use crate::dom2::{Document, NodeId, NodeKind};
use crate::error::{Error, RenderStage, Result};
use crate::js::DomHost;
use crate::js::ScriptType;
use crate::render_control::{record_stage, StageGuard, StageHeartbeat};
use serde::{Deserialize, Serialize};
use std::cell::{Ref, RefCell, RefMut};
use std::collections::VecDeque;
use std::rc::Rc;

/// Host-side bookkeeping for `Document.currentScript`.
///
/// The HTML Standard's ["execute the script block"](https://html.spec.whatwg.org/) algorithm
/// temporarily sets the document's `currentScript` to the currently executing classic script
/// element (but only when that element is in the document tree; classic scripts in shadow trees and
/// module scripts observe `null`), and then restores it afterward.
///
/// This is observable in real-world scripts (`document.currentScript`) and is a prerequisite for
/// wiring up Web IDL bindings.
///
/// Note: This state lives outside `dom2::Document` because `dom2` currently represents only the DOM
/// tree structure, not the full HTML Document object state.
#[derive(Debug, Default, Clone)]
pub struct CurrentScriptState {
  /// Equivalent to `Document.currentScript` (as a `dom2::NodeId` handle).
  pub current_script: Option<NodeId>,
  previous_current_script: Vec<Option<NodeId>>,
}

/// Shared, host-managed handle for [`CurrentScriptState`].
///
/// This is the intended bridge between the host script execution pipeline (e.g. [`ScriptOrchestrator`])
/// and the JS DOM bindings layer (`document.currentScript`).
///
/// The bindings must **not** own this state; they only read it. The HTML orchestration code (host)
/// mutates it as scripts execute.
#[derive(Debug, Clone, Default)]
pub struct CurrentScriptStateHandle(Rc<RefCell<CurrentScriptState>>);

impl CurrentScriptStateHandle {
  pub fn borrow(&self) -> Ref<'_, CurrentScriptState> {
    self.0.borrow()
  }

  pub fn borrow_mut(&self) -> RefMut<'_, CurrentScriptState> {
    self.0.borrow_mut()
  }

  /// Clears `Document.currentScript` bookkeeping state.
  ///
  /// This is intended for navigation/reset: the handle itself is stable and may be cloned into JS
  /// bindings realms, so callers must not replace the handle when resetting script state.
  pub fn reset(&self) {
    let mut state = self.borrow_mut();
    state.current_script = None;
    state.previous_current_script.clear();
  }
}

impl CurrentScriptState {
  fn push(&mut self, script: Option<NodeId>) -> Result<()> {
    self
      .previous_current_script
      .try_reserve(1)
      .map_err(|err| Error::Other(format!("currentScript stack allocation failed: {err}")))?;
    self.previous_current_script.push(self.current_script);
    self.current_script = script;
    Ok(())
  }

  fn pop(&mut self) -> Result<()> {
    let previous = self
      .previous_current_script
      .pop()
      .ok_or_else(|| Error::Other("currentScript stack underflow".to_string()))?;
    self.current_script = previous;
    Ok(())
  }

  #[cfg(test)]
  fn stack_depth(&self) -> usize {
    self.previous_current_script.len()
  }
}

/// Debug record of a script execution.
///
/// This is intended for tooling and unit tests that need to understand script ordering and the
/// host's `document.currentScript` bookkeeping.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScriptExecutionLogEntry {
  /// `dom2::NodeId::index()` of the `<script>` element being executed.
  pub script_id: usize,
  #[serde(flatten)]
  pub source: ScriptSourceSnapshot,
  /// `dom2::NodeId::index()` observed as `document.currentScript` during execution.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub current_script_node_id: Option<usize>,
}

/// Snapshot of whether a script is external (`src=`) or inline.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum ScriptSourceSnapshot {
  Inline,
  Url { url: String },
}

/// Bounded FIFO log of executed scripts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScriptExecutionLog {
  capacity: usize,
  entries: VecDeque<ScriptExecutionLogEntry>,
}

impl ScriptExecutionLog {
  pub fn new(capacity: usize) -> Self {
    Self {
      capacity: capacity.max(1),
      entries: VecDeque::new(),
    }
  }

  pub fn entries(&self) -> &VecDeque<ScriptExecutionLogEntry> {
    &self.entries
  }

  pub fn record(&mut self, entry: ScriptExecutionLogEntry) {
    while self.entries.len() >= self.capacity {
      self.entries.pop_front();
    }
    self.entries.push_back(entry);
  }
}

/// Trait for host types that carry `Document.currentScript` state.
pub trait CurrentScriptHost {
  fn current_script_state(&self) -> &CurrentScriptStateHandle;

  fn current_script(&self) -> Option<NodeId> {
    self.current_script_state().borrow().current_script
  }

  fn script_execution_log(&self) -> Option<&ScriptExecutionLog> {
    None
  }

  fn script_execution_log_mut(&mut self) -> Option<&mut ScriptExecutionLog> {
    None
  }
}

/// Script execution adapter invoked by [`ScriptOrchestrator`].
pub trait ScriptBlockExecutor<Host: CurrentScriptHost> {
  fn execute_script(
    &mut self,
    host: &mut Host,
    orchestrator: &mut ScriptOrchestrator,
    script: NodeId,
    script_type: ScriptType,
  ) -> Result<()>;
}

/// Minimal script execution orchestrator.
///
/// For now, this focuses solely on spec-shaped `Document.currentScript` bookkeeping around
/// "execute the script block" (classic scripts). Future tasks will extend this with async/defer,
/// module loading, and event loop integration.
#[derive(Debug, Default)]
pub struct ScriptOrchestrator;

impl ScriptOrchestrator {
  pub fn new() -> Self {
    Self
  }

  /// Execute `f` while temporarily overriding `Document.currentScript` bookkeeping.
  ///
  /// This is a lower-level helper used by embeddings that need to run script code while:
  /// - setting `Document.currentScript` to a precomputed value, and
  /// - ensuring the previous value is always restored (even on error).
  ///
  /// Callers are responsible for computing `new_current_script` according to the HTML Standard
  /// (e.g. whether the script element is connected and in the document tree vs a shadow tree).
  pub fn execute_with_current_script_state_resolved(
    &mut self,
    current_script_state: &CurrentScriptStateHandle,
    new_current_script: Option<NodeId>,
    f: impl FnOnce() -> Result<()>,
  ) -> Result<()> {
    current_script_state.borrow_mut().push(new_current_script)?;
    let result = f();
    let pop_result = current_script_state.borrow_mut().pop();

    match (result, pop_result) {
      (Ok(()), Ok(())) => Ok(()),
      (Err(err), Ok(())) => Err(err),
      (Ok(()), Err(pop_err)) => Err(pop_err),
      (Err(err), Err(pop_err)) => Err(Error::Other(format!(
        "script execution failed ({err}); additionally failed to restore Document.currentScript ({pop_err})"
      ))),
    }
  }

  /// Execute a script element that has already been "prepared" (HTML `prepare a script`).
  ///
  /// This differs from [`ScriptOrchestrator::execute_script_element`] in one key way:
  /// `script_already_started` is **not** consulted or mutated here.
  ///
  /// HTML sets the per-element "already started" flag during *preparation* (e.g. during DOM
  /// insertion steps for dynamically inserted scripts, and when a parser-inserted script finishes
  /// parsing). External scripts may be prepared long before they execute (after a fetch completes),
  /// so the execution phase must not treat `script_already_started=true` as a reason to skip.
  ///
  /// Callers are still responsible for ensuring the element is prepared at most once.
  pub fn execute_prepared_script_element<Host, Exec>(
    &mut self,
    host: &mut Host,
    script: NodeId,
    script_type: ScriptType,
    executor: &mut Exec,
  ) -> Result<()>
  where
    Host: CurrentScriptHost + DomHost,
    Exec: ScriptBlockExecutor<Host>,
  {
    // Even for a previously-prepared script, avoid executing when the node is no longer connected
    // for scripting (e.g. it was removed from the document, or moved into inert <template>
    // contents).
    if !host.with_dom(|dom| dom.is_connected_for_scripting(script)) {
      return Ok(());
    }

    let existing_current_script = host.current_script();
    let new_current_script = match script_type {
      ScriptType::Classic => {
        (!host.with_dom(|dom| node_root_is_shadow_root(dom, script))).then_some(script)
      }
      // `Document.currentScript` is null for module scripts.
      ScriptType::Module => None,
      // Import map scripts do not set `Document.currentScript`; preserve the existing value.
      ScriptType::ImportMap => existing_current_script,
      // Unknown script types should be ignored by "prepare a script" (currentScript remains null).
      ScriptType::Unknown => None,
    };

    let source_snapshot = host.with_dom(|dom| script_source_snapshot(dom, script));

    host
      .current_script_state()
      .borrow_mut()
      .push(new_current_script)?;
    if let Some(log) = host.script_execution_log_mut() {
      log.record(ScriptExecutionLogEntry {
        script_id: script.index(),
        source: source_snapshot,
        current_script_node_id: new_current_script.map(|id| id.index()),
      });
    }
    let result = {
      let _stage_guard = StageGuard::install(Some(RenderStage::Script));
      record_stage(StageHeartbeat::Script);
      executor.execute_script(host, self, script, script_type)
    };
    let pop_result = host.current_script_state().borrow_mut().pop();

    match (result, pop_result) {
      (Ok(()), Ok(())) => Ok(()),
      (Err(err), Ok(())) => Err(err),
      (Ok(()), Err(pop_err)) => Err(pop_err),
      (Err(err), Err(pop_err)) => Err(Error::Other(format!(
        "script execution failed ({err}); additionally failed to restore Document.currentScript ({pop_err})"
      ))),
    }
  }

  /// Execute a script element while performing `Document.currentScript` bookkeeping using an
  /// explicit [`CurrentScriptStateHandle`] handle.
  ///
  /// This is a lower-level variant of [`ScriptOrchestrator::execute_script_element`] that avoids
  /// requiring a host type implementing [`CurrentScriptHost`]. It exists primarily so embeddings
  /// can keep the live `dom2::Document` and the `currentScript` state in separate structs without
  /// running afoul of Rust's borrow checker.
  ///
  /// Callers provide a closure that is invoked while `current_script_state.borrow().current_script`
  /// has been updated for the duration of execution. The state borrow is not held across the call,
  /// so JS bindings can observe `document.currentScript` during execution.
  pub fn execute_with_current_script_state(
    &mut self,
    current_script_state: &CurrentScriptStateHandle,
    dom: &Document,
    script: NodeId,
    script_type: ScriptType,
    f: impl FnOnce() -> Result<()>,
  ) -> Result<()> {
    // HTML: "prepare a script" early-outs when the script element is not connected.
    if !dom.is_connected_for_scripting(script) {
      return Ok(());
    }

    let existing_current_script = current_script_state.borrow().current_script;
    let new_current_script = match script_type {
      ScriptType::Classic => (!node_root_is_shadow_root(dom, script)).then_some(script),
      ScriptType::Module => None,
      // Import map scripts do not set `Document.currentScript`; preserve the existing value.
      ScriptType::ImportMap => existing_current_script,
      ScriptType::Unknown => None,
    };

    self.execute_with_current_script_state_resolved(current_script_state, new_current_script, f)
  }

  /// Execute a script element while performing `Document.currentScript` bookkeeping.
  ///
  /// - For classic scripts in the document tree, `current_script` is set to `script` for the
  ///   duration of execution.
  /// - For module scripts (and classic scripts in a shadow tree), `current_script` is set to `None`
  ///   for the duration of execution.
  /// - The previous value is always restored afterward, even on error.
  pub fn execute_script_element<Host, Exec>(
    &mut self,
    host: &mut Host,
    script: NodeId,
    script_type: ScriptType,
    executor: &mut Exec,
  ) -> Result<()>
  where
    Host: CurrentScriptHost + DomHost,
    Exec: ScriptBlockExecutor<Host>,
  {
    // HTML: "prepare a script" returns early when:
    // - the script element is not connected, or
    // - the script element has already started.
    //
    // In `dom2`, `<template>` contents are represented as inert subtrees (the nodes remain in the
    // tree for snapshotting/traversal, but should not be treated as connected for scripting).
    // Scripts that have been detached from the document must also be skipped.
    //
    // `script_already_started` also serves as our guard against accidentally executing the same
    // `<script>` element twice (e.g. if it is moved/reinserted, or if execution is re-entrant).
    let should_execute = host.with_dom(|dom| {
      dom.is_connected_for_scripting(script) && !dom.node(script).script_already_started
    });
    if !should_execute {
      return Ok(());
    }

    // Mark the script as started before executing so re-entrant execution attempts short-circuit.
    //
    // Note: this is a DOM mutation, but does not affect rendering. Hosts can treat `changed=false`
    // as meaning "no style/layout invalidation required".
    host
      .mutate_dom(|dom| (dom.set_script_already_started(script, true), false))
      .map_err(|err| {
        Error::Other(format!(
          "failed to set script_already_started for node {}: {err}",
          script.index()
        ))
      })?;

    let existing_current_script = host.current_script();
    let new_current_script = match script_type {
      ScriptType::Classic => {
        (!host.with_dom(|dom| node_root_is_shadow_root(dom, script))).then_some(script)
      }
      // `Document.currentScript` is null for module scripts.
      ScriptType::Module => None,
      // Import map scripts do not set `Document.currentScript`; preserve the existing value.
      ScriptType::ImportMap => existing_current_script,
      // Unknown script types should be ignored by "prepare a script" (currentScript remains null).
      ScriptType::Unknown => None,
    };

    let source_snapshot = host.with_dom(|dom| script_source_snapshot(dom, script));

    host
      .current_script_state()
      .borrow_mut()
      .push(new_current_script)?;
    if let Some(log) = host.script_execution_log_mut() {
      log.record(ScriptExecutionLogEntry {
        script_id: script.index(),
        source: source_snapshot,
        current_script_node_id: new_current_script.map(|id| id.index()),
      });
    }
    let result = {
      let _stage_guard = StageGuard::install(Some(RenderStage::Script));
      record_stage(StageHeartbeat::Script);
      executor.execute_script(host, self, script, script_type)
    };
    let pop_result = host.current_script_state().borrow_mut().pop();

    match (result, pop_result) {
      (Ok(()), Ok(())) => Ok(()),
      (Err(err), Ok(())) => Err(err),
      (Ok(()), Err(pop_err)) => Err(pop_err),
      (Err(err), Err(pop_err)) => Err(Error::Other(format!(
        "script execution failed ({err}); additionally failed to restore Document.currentScript ({pop_err})"
      ))),
    }
  }
}

fn script_source_snapshot(dom: &Document, script: NodeId) -> ScriptSourceSnapshot {
  let node = dom.node(script);
  let NodeKind::Element { attributes, .. } = &node.kind else {
    return ScriptSourceSnapshot::Inline;
  };

  // HTML attributes are case-insensitive for script elements; treat any `src` attribute as
  // identifying an external script (even if empty, as the fetch would still resolve).
  let src = attributes
    .iter()
    .find(|attr| attr.namespace == crate::dom2::NULL_NAMESPACE && attr.local_name.eq_ignore_ascii_case("src"))
    .map(|attr| attr.value.to_string());
  match src {
    Some(url) => ScriptSourceSnapshot::Url { url },
    None => ScriptSourceSnapshot::Inline,
  }
}

fn node_root_is_shadow_root(dom: &Document, mut node: NodeId) -> bool {
  loop {
    match &dom.node(node).kind {
      NodeKind::ShadowRoot { .. } => return true,
      NodeKind::Document { .. } => return false,
      _ => {}
    }

    // DOM's "root" concept treats ShadowRoot as the root of a separate tree (i.e. its parent is
    // null). `dom2` currently stores ShadowRoot nodes in the main tree with a parent pointer (the
    // host element) so that the renderer can traverse them. For `currentScript`, we still need the
    // DOM notion of root, so we stop when we see a ShadowRoot.
    let Some(parent) = dom.node(node).parent else {
      return false;
    };
    node = parent;
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::dom2::Document as Dom2Document;
  use crate::error::Error;

  struct Host {
    dom: Dom2Document,
    script_state: CurrentScriptStateHandle,
    log: Option<ScriptExecutionLog>,
  }

  impl DomHost for Host {
    fn with_dom<R, F>(&self, f: F) -> R
    where
      F: FnOnce(&Dom2Document) -> R,
    {
      f(&self.dom)
    }

    fn mutate_dom<R, F>(&mut self, f: F) -> R
    where
      F: FnOnce(&mut Dom2Document) -> (R, bool),
    {
      let (result, _changed) = f(&mut self.dom);
      result
    }
  }

  impl CurrentScriptHost for Host {
    fn current_script_state(&self) -> &CurrentScriptStateHandle {
      &self.script_state
    }

    fn script_execution_log(&self) -> Option<&ScriptExecutionLog> {
      self.log.as_ref()
    }

    fn script_execution_log_mut(&mut self) -> Option<&mut ScriptExecutionLog> {
      self.log.as_mut()
    }
  }

  fn find_script_elements(dom: &Dom2Document) -> Vec<NodeId> {
    let mut out = Vec::new();
    let mut stack = vec![dom.root()];
    while let Some(id) = stack.pop() {
      let node = dom.node(id);
      if let NodeKind::Element { tag_name, .. } = &node.kind {
        if tag_name.eq_ignore_ascii_case("script") {
          out.push(id);
        }
      }
      for &child in node.children.iter().rev() {
        stack.push(child);
      }
    }
    out
  }

  #[derive(Default)]
  struct RecordingExecutor {
    observed: Vec<Option<NodeId>>,
  }

  impl ScriptBlockExecutor<Host> for RecordingExecutor {
    fn execute_script(
      &mut self,
      host: &mut Host,
      _orchestrator: &mut ScriptOrchestrator,
      _script: NodeId,
      _script_type: ScriptType,
    ) -> Result<()> {
      self.observed.push(host.current_script());
      Ok(())
    }
  }

  #[test]
  fn sets_current_script_for_sequential_classic_scripts() -> Result<()> {
    let renderer_dom =
      crate::dom::parse_html("<!doctype html><script></script><script></script>").unwrap();
    let dom = Dom2Document::from_renderer_dom(&renderer_dom);
    let scripts = find_script_elements(&dom);
    assert_eq!(scripts.len(), 2);

    let mut host = Host {
      dom,
      script_state: CurrentScriptStateHandle::default(),
      log: None,
    };
    let mut orchestrator = ScriptOrchestrator::new();
    let mut executor = RecordingExecutor::default();

    orchestrator.execute_script_element(
      &mut host,
      scripts[0],
      ScriptType::Classic,
      &mut executor,
    )?;
    assert_eq!(host.current_script(), None);

    orchestrator.execute_script_element(
      &mut host,
      scripts[1],
      ScriptType::Classic,
      &mut executor,
    )?;
    assert_eq!(host.current_script(), None);

    assert_eq!(executor.observed, vec![Some(scripts[0]), Some(scripts[1])]);
    assert_eq!(host.script_state.borrow().stack_depth(), 0);
    Ok(())
  }

  struct NestedExecutor {
    script_a: NodeId,
    script_b: NodeId,
    observed: Vec<Option<NodeId>>,
    did_nested: bool,
  }

  impl NestedExecutor {
    fn new(script_a: NodeId, script_b: NodeId) -> Self {
      Self {
        script_a,
        script_b,
        observed: Vec::new(),
        did_nested: false,
      }
    }
  }

  impl ScriptBlockExecutor<Host> for NestedExecutor {
    fn execute_script(
      &mut self,
      host: &mut Host,
      orchestrator: &mut ScriptOrchestrator,
      script: NodeId,
      _script_type: ScriptType,
    ) -> Result<()> {
      self.observed.push(host.current_script());
      if script == self.script_a {
        assert!(
          !self.did_nested,
          "nested executor should run nested script only once"
        );
        self.did_nested = true;
        orchestrator.execute_script_element(host, self.script_b, ScriptType::Classic, self)?;
        self.observed.push(host.current_script());
      }
      Ok(())
    }
  }

  #[test]
  fn restores_current_script_for_nested_classic_execution() -> Result<()> {
    let renderer_dom =
      crate::dom::parse_html("<!doctype html><script id=a></script><script id=b></script>")
        .unwrap();
    let dom = Dom2Document::from_renderer_dom(&renderer_dom);
    let scripts = find_script_elements(&dom);
    assert_eq!(scripts.len(), 2);
    let script_a = scripts[0];
    let script_b = scripts[1];

    let mut host = Host {
      dom,
      script_state: CurrentScriptStateHandle::default(),
      log: None,
    };
    let mut orchestrator = ScriptOrchestrator::new();
    let mut executor = NestedExecutor::new(script_a, script_b);

    orchestrator.execute_script_element(&mut host, script_a, ScriptType::Classic, &mut executor)?;

    assert_eq!(
      executor.observed,
      vec![Some(script_a), Some(script_b), Some(script_a)]
    );
    assert_eq!(host.current_script(), None);
    assert_eq!(host.script_state.borrow().stack_depth(), 0);
    Ok(())
  }

  struct NestedImportMapExecutor {
    outer_script: NodeId,
    import_map_script: NodeId,
    observed: Vec<Option<NodeId>>,
    did_nested: bool,
  }

  impl NestedImportMapExecutor {
    fn new(outer_script: NodeId, import_map_script: NodeId) -> Self {
      Self {
        outer_script,
        import_map_script,
        observed: Vec::new(),
        did_nested: false,
      }
    }
  }

  impl ScriptBlockExecutor<Host> for NestedImportMapExecutor {
    fn execute_script(
      &mut self,
      host: &mut Host,
      orchestrator: &mut ScriptOrchestrator,
      script: NodeId,
      script_type: ScriptType,
    ) -> Result<()> {
      self.observed.push(host.current_script());
      if script == self.outer_script {
        assert_eq!(
          script_type,
          ScriptType::Classic,
          "outer script should be executed as a classic script"
        );
        assert!(
          !self.did_nested,
          "nested executor should run nested script only once"
        );
        self.did_nested = true;

        orchestrator.execute_script_element(
          host,
          self.import_map_script,
          ScriptType::ImportMap,
          self,
        )?;

        // Import map scripts do not set `document.currentScript`, so it must remain the classic
        // script element that was already executing.
        assert_eq!(
          host.current_script(),
          Some(self.outer_script),
          "import map execution must not clobber document.currentScript"
        );
        self.observed.push(host.current_script());
      } else if script == self.import_map_script {
        assert_eq!(
          script_type,
          ScriptType::ImportMap,
          "expected nested script to be executed as an import map"
        );
        assert_eq!(
          host.current_script(),
          Some(self.outer_script),
          "import maps must not change document.currentScript (should remain the outer classic script)"
        );
      }
      Ok(())
    }
  }

  #[test]
  fn nested_import_map_execution_preserves_current_script() -> Result<()> {
    let renderer_dom = crate::dom::parse_html(
      r#"<!doctype html><script id=outer></script><script type=importmap>{"imports":{}}</script>"#,
    )
    .unwrap();
    let dom = Dom2Document::from_renderer_dom(&renderer_dom);
    let scripts = find_script_elements(&dom);
    assert_eq!(scripts.len(), 2);
    let outer_script = scripts[0];
    let import_map_script = scripts[1];

    let mut host = Host {
      dom,
      script_state: CurrentScriptStateHandle::default(),
      log: None,
    };
    let mut orchestrator = ScriptOrchestrator::new();
    let mut executor = NestedImportMapExecutor::new(outer_script, import_map_script);

    orchestrator.execute_script_element(
      &mut host,
      outer_script,
      ScriptType::Classic,
      &mut executor,
    )?;

    assert_eq!(
      executor.observed,
      vec![Some(outer_script), Some(outer_script), Some(outer_script)],
      "import map execution should observe the currently executing classic script as document.currentScript"
    );
    assert_eq!(host.current_script(), None);
    assert_eq!(host.script_state.borrow().stack_depth(), 0);
    Ok(())
  }

  struct ErroringExecutor;

  impl ScriptBlockExecutor<Host> for ErroringExecutor {
    fn execute_script(
      &mut self,
      host: &mut Host,
      _orchestrator: &mut ScriptOrchestrator,
      _script: NodeId,
      _script_type: ScriptType,
    ) -> Result<()> {
      assert!(
        host.current_script().is_some(),
        "expected current_script to be set before the executor runs"
      );
      Err(Error::Other("boom".to_string()))
    }
  }

  #[test]
  fn restores_current_script_on_error() {
    let renderer_dom = crate::dom::parse_html("<!doctype html><script></script>").unwrap();
    let dom = Dom2Document::from_renderer_dom(&renderer_dom);
    let scripts = find_script_elements(&dom);
    assert_eq!(scripts.len(), 1);
    let script = scripts[0];

    let mut host = Host {
      dom,
      script_state: CurrentScriptStateHandle::default(),
      log: None,
    };
    // Simulate an outer (already executing) script.
    let outer_current = host.dom.root();
    host.script_state.borrow_mut().current_script = Some(outer_current);

    let mut orchestrator = ScriptOrchestrator::new();
    let mut executor = ErroringExecutor;
    let err = orchestrator
      .execute_script_element(&mut host, script, ScriptType::Classic, &mut executor)
      .expect_err("expected script execution to fail");

    assert!(matches!(err, Error::Other(msg) if msg == "boom"));
    assert_eq!(host.current_script(), Some(outer_current));
    assert_eq!(host.script_state.borrow().stack_depth(), 0);
  }

  #[test]
  fn skips_execution_for_scripts_not_connected_for_scripting() -> Result<()> {
    let renderer_dom = crate::dom::parse_html(
      "<!doctype html><body><template><script id=inert></script></template><script id=live></script>",
    )
    .unwrap();
    let dom = Dom2Document::from_renderer_dom(&renderer_dom);

    let scripts = find_script_elements(&dom);
    assert_eq!(scripts.len(), 2);

    let mut inert_script: Option<NodeId> = None;
    let mut live_script: Option<NodeId> = None;
    for &script in &scripts {
      if dom.is_connected_for_scripting(script) {
        live_script = Some(script);
      } else {
        inert_script = Some(script);
      }
    }
    let inert_script = inert_script.expect("expected a script inside <template>");
    let live_script = live_script.expect("expected a live script");

    let mut host = Host {
      dom,
      script_state: CurrentScriptStateHandle::default(),
      log: None,
    };
    let mut orchestrator = ScriptOrchestrator::new();
    let mut executor = RecordingExecutor::default();

    orchestrator.execute_script_element(
      &mut host,
      inert_script,
      ScriptType::Classic,
      &mut executor,
    )?;
    assert_eq!(host.current_script(), None);
    assert_eq!(executor.observed, Vec::<Option<NodeId>>::new());

    orchestrator.execute_script_element(
      &mut host,
      live_script,
      ScriptType::Classic,
      &mut executor,
    )?;
    assert_eq!(host.current_script(), None);
    assert_eq!(executor.observed, vec![Some(live_script)]);
    assert_eq!(host.script_state.borrow().stack_depth(), 0);
    Ok(())
  }

  #[test]
  fn records_script_execution_log_with_current_script() -> Result<()> {
    let renderer_dom =
      crate::dom::parse_html("<!doctype html><script></script><script></script>").unwrap();
    let dom = Dom2Document::from_renderer_dom(&renderer_dom);
    let scripts = find_script_elements(&dom);
    assert_eq!(scripts.len(), 2);

    let mut host = Host {
      dom,
      script_state: CurrentScriptStateHandle::default(),
      log: Some(ScriptExecutionLog::new(16)),
    };
    let mut orchestrator = ScriptOrchestrator::new();
    let mut executor = RecordingExecutor::default();

    orchestrator.execute_script_element(
      &mut host,
      scripts[0],
      ScriptType::Classic,
      &mut executor,
    )?;
    orchestrator.execute_script_element(
      &mut host,
      scripts[1],
      ScriptType::Classic,
      &mut executor,
    )?;

    let log = host.log.as_ref().expect("log enabled");
    assert_eq!(
      log.entries().iter().cloned().collect::<Vec<_>>(),
      vec![
        ScriptExecutionLogEntry {
          script_id: scripts[0].index(),
          source: ScriptSourceSnapshot::Inline,
          current_script_node_id: Some(scripts[0].index()),
        },
        ScriptExecutionLogEntry {
          script_id: scripts[1].index(),
          source: ScriptSourceSnapshot::Inline,
          current_script_node_id: Some(scripts[1].index()),
        },
      ]
    );
    Ok(())
  }

  #[test]
  fn script_execution_log_is_bounded_fifo() -> Result<()> {
    let renderer_dom =
      crate::dom::parse_html("<!doctype html><script></script><script></script>").unwrap();
    let dom = Dom2Document::from_renderer_dom(&renderer_dom);
    let scripts = find_script_elements(&dom);
    assert_eq!(scripts.len(), 2);

    let mut host = Host {
      dom,
      script_state: CurrentScriptStateHandle::default(),
      log: Some(ScriptExecutionLog::new(1)),
    };
    let mut orchestrator = ScriptOrchestrator::new();
    let mut executor = RecordingExecutor::default();

    orchestrator.execute_script_element(
      &mut host,
      scripts[0],
      ScriptType::Classic,
      &mut executor,
    )?;
    orchestrator.execute_script_element(
      &mut host,
      scripts[1],
      ScriptType::Classic,
      &mut executor,
    )?;

    let log = host.log.as_ref().expect("log enabled");
    let entries = log.entries().iter().cloned().collect::<Vec<_>>();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].script_id, scripts[1].index());
    Ok(())
  }

  #[test]
  fn classic_script_in_shadow_root_is_connected_but_current_script_is_null() -> Result<()> {
    // Scripts inside declaratively attached shadow roots should be treated as connected for script
    // preparation, but `document.currentScript` must remain null for classic scripts in shadow
    // trees.
    let dom = crate::dom2::parse_html(
      r#"<div id="host"><template shadowroot="open"><script id="shadow"></script></template></div>"#,
    )
    .unwrap();

    let scripts = find_script_elements(&dom);
    assert_eq!(scripts.len(), 1);
    let script = scripts[0];
    assert!(
      dom.is_connected_for_scripting(script),
      "script inside attached shadow root should be connected for scripting"
    );

    let mut host = Host {
      dom,
      script_state: CurrentScriptStateHandle::default(),
      log: None,
    };
    let mut orchestrator = ScriptOrchestrator::new();
    let mut executor = RecordingExecutor::default();
    orchestrator.execute_script_element(&mut host, script, ScriptType::Classic, &mut executor)?;

    assert_eq!(
      executor.observed,
      vec![None],
      "currentScript must be null for classic scripts in shadow trees"
    );
    Ok(())
  }

  #[test]
  fn script_already_started_prevents_double_execution() -> Result<()> {
    let renderer_dom = crate::dom::parse_html("<!doctype html><script></script>").unwrap();
    let dom = Dom2Document::from_renderer_dom(&renderer_dom);
    let scripts = find_script_elements(&dom);
    assert_eq!(scripts.len(), 1);
    let script = scripts[0];
    assert!(
      !dom.node(script).script_already_started,
      "imported DOM should start with script_already_started=false"
    );

    let mut host = Host {
      dom,
      script_state: CurrentScriptStateHandle::default(),
      log: None,
    };
    let mut orchestrator = ScriptOrchestrator::new();
    let mut executor = RecordingExecutor::default();

    orchestrator.execute_script_element(&mut host, script, ScriptType::Classic, &mut executor)?;
    assert!(
      host.dom.node(script).script_already_started,
      "executed scripts must be marked already started"
    );
    assert_eq!(executor.observed, vec![Some(script)]);
    assert_eq!(host.current_script(), None);
    assert_eq!(host.script_state.borrow().stack_depth(), 0);

    // Second execution attempt should no-op.
    orchestrator.execute_script_element(&mut host, script, ScriptType::Classic, &mut executor)?;
    assert_eq!(
      executor.observed,
      vec![Some(script)],
      "already-started scripts must not execute twice"
    );
    assert_eq!(host.current_script(), None);
    assert_eq!(host.script_state.borrow().stack_depth(), 0);
    Ok(())
  }
}

#[cfg(all(test, feature = "quickjs"))]
mod quickjs_current_script_tests {
  use super::{CurrentScriptHost, CurrentScriptStateHandle, ScriptBlockExecutor, ScriptOrchestrator};

  use crate::dom2::{Document, NodeId};
  use crate::js::{
    EventLoop, HtmlScriptId, HtmlScriptScheduler, HtmlScriptSchedulerAction, HtmlScriptWork,
    RunLimits, ScriptElementSpec, ScriptType, TaskSource,
  };
  use crate::{Error, Result};
  use rquickjs::{Context, Ctx, Object, Runtime, Value};
  use std::collections::HashMap;

  fn find_element_by_id(dom: &Document, id: &str) -> NodeId {
    for node_id in dom.subtree_preorder(dom.root()) {
      if dom.get_attribute(node_id, "id").ok().flatten() == Some(id) {
        return node_id;
      }
    }
    panic!("element id={id} not found");
  }

  fn element_id_attr(dom: &Document, node_id: NodeId) -> Option<&str> {
    dom.get_attribute(node_id, "id").unwrap_or(None)
  }

  fn init_js_realm(dom: &Document, script_nodes: &[NodeId]) -> Result<(Runtime, Context)> {
    let rt = Runtime::new().map_err(|e| Error::Other(e.to_string()))?;
    let ctx = Context::full(&rt).map_err(|e| Error::Other(e.to_string()))?;

    ctx.with(|ctx| -> Result<()> {
      let globals = ctx.globals();

      // A minimal `document` object with a `currentScript` Web-compat getter.
      let document = Object::new(ctx.clone()).map_err(|e| Error::Other(e.to_string()))?;
      globals
        .set("document", document.clone())
        .map_err(|e| Error::Other(e.to_string()))?;

      // `document.__currentScript` is the backing slot; `document.currentScript` is a getter.
      //
      // We also maintain a JS-side stack so nested script execution can restore the previous
      // `currentScript` without requiring Rust to hold JS `Value` handles across `Context::with`
      // boundaries (rquickjs values are lifetime-tied to the `Ctx` borrow).
      ctx
        .eval::<(), _>("globalThis.document.__currentScript = null;")
        .map_err(|e| Error::Other(e.to_string()))?;
      ctx
        .eval::<(), _>("globalThis.document.__currentScriptStack = [];")
        .map_err(|e| Error::Other(e.to_string()))?;
      ctx
        .eval::<(), _>(
          r#"
          Object.defineProperty(globalThis.document, "currentScript", {
            get() { return this.__currentScript; },
            configurable: true,
          });

          globalThis.document.__pushCurrentScript = function (v) {
            this.__currentScriptStack.push(this.__currentScript);
            this.__currentScript = v;
          };

          globalThis.document.__popCurrentScript = function () {
            if (this.__currentScriptStack.length === 0) {
              throw new Error("currentScript JS stack underflow");
            }
            this.__currentScript = this.__currentScriptStack.pop();
          };
        "#,
        )
        .map_err(|e| Error::Other(e.to_string()))?;

      // A stable mapping from dom2 NodeId → wrapper object so JS can observe identity.
      let by_node_id = Object::new(ctx.clone()).map_err(|e| Error::Other(e.to_string()))?;
      globals
        .set("__scriptByNodeId", by_node_id.clone())
        .map_err(|e| Error::Other(e.to_string()))?;

      for &node_id in script_nodes {
        let wrapper = Object::new(ctx.clone()).map_err(|e| Error::Other(e.to_string()))?;
        wrapper
          .set("nodeId", node_id.index() as i32)
          .map_err(|e| Error::Other(e.to_string()))?;
        if let Some(id) = element_id_attr(dom, node_id) {
          wrapper
            .set("id", id)
            .map_err(|e| Error::Other(e.to_string()))?;
        }
        by_node_id
          .set(node_id.index().to_string(), wrapper)
          .map_err(|e| Error::Other(e.to_string()))?;
      }

      // Convenience log used by tests.
      ctx
        .eval::<(), _>("globalThis.log = [];")
        .map_err(|e| Error::Other(e.to_string()))?;

      Ok(())
    })?;

    Ok((rt, ctx))
  }

  struct JsHost {
    dom: Document,
    js_rt: Runtime,
    js_ctx: Context,

    script_state: CurrentScriptStateHandle,
    orchestrator: ScriptOrchestrator,

    // Script source text, keyed by the script element NodeId.
    script_segments: HashMap<NodeId, Vec<String>>,
    // Optional nested execution plan used by the nested-currentScript test.
    nested: Option<(NodeId, NodeId)>,
  }

  impl JsHost {
    fn new(dom: Document, script_nodes: &[NodeId]) -> Result<Self> {
      let (js_rt, js_ctx) = init_js_realm(&dom, script_nodes)?;
      Ok(Self {
        dom,
        js_rt,
        js_ctx,
        script_state: CurrentScriptStateHandle::default(),
        orchestrator: ScriptOrchestrator::new(),
        script_segments: HashMap::new(),
        nested: None,
      })
    }

    fn set_script_source(&mut self, node_id: NodeId, source: &str) {
      self
        .script_segments
        .insert(node_id, vec![source.to_string()]);
    }

    fn set_script_segments(&mut self, node_id: NodeId, segments: Vec<&str>) {
      self.script_segments.insert(
        node_id,
        segments.into_iter().map(|s| s.to_string()).collect(),
      );
    }

    fn set_nested(&mut self, outer: NodeId, inner: NodeId) {
      self.nested = Some((outer, inner));
    }

    fn eval_bool(&self, expr: &str) -> Result<bool> {
      self.js_ctx.with(|ctx| {
        ctx
          .eval::<bool, _>(expr)
          .map_err(|e| Error::Other(e.to_string()))
      })
    }

    fn run_script_element(&mut self, node_id: NodeId, script_type: ScriptType) -> Result<()> {
      let mut exec = JsExecutor::default();
      let mut orchestrator = std::mem::take(&mut self.orchestrator);
      let result = orchestrator.execute_script_element(self, node_id, script_type, &mut exec);
      self.orchestrator = orchestrator;
      result
    }
  }

  impl CurrentScriptHost for JsHost {
    fn current_script_state(&self) -> &CurrentScriptStateHandle {
      &self.script_state
    }
  }

  impl crate::js::DomHost for JsHost {
    fn with_dom<R, F>(&self, f: F) -> R
    where
      F: FnOnce(&Document) -> R,
    {
      f(&self.dom)
    }

    fn mutate_dom<R, F>(&mut self, f: F) -> R
    where
      F: FnOnce(&mut Document) -> (R, bool),
    {
      let (result, _changed) = f(&mut self.dom);
      result
    }
  }

  #[derive(Default)]
  struct JsExecutor {
    did_nested: bool,
  }

  impl JsExecutor {
    fn js_push_current_script<'js>(ctx: Ctx<'js>, new_current_script: Option<NodeId>) -> Result<()> {
      match new_current_script {
        None => ctx
          .eval::<(), _>("document.__pushCurrentScript(null);")
          .map_err(|e| Error::Other(e.to_string()))?,
        Some(node_id) => ctx
          .eval::<(), _>(format!(
            "document.__pushCurrentScript(__scriptByNodeId[\"{}\"]);",
            node_id.index()
          ))
          .map_err(|e| Error::Other(e.to_string()))?,
      }
      Ok(())
    }

    fn js_pop_current_script<'js>(ctx: Ctx<'js>) -> Result<()> {
      ctx
        .eval::<(), _>("document.__popCurrentScript();")
        .map_err(|e| Error::Other(e.to_string()))?;
      Ok(())
    }
  }

  impl ScriptBlockExecutor<JsHost> for JsExecutor {
    fn execute_script(
      &mut self,
      host: &mut JsHost,
      orchestrator: &mut ScriptOrchestrator,
      script: NodeId,
      _script_type: ScriptType,
    ) -> Result<()> {
      let Some(segments) = host.script_segments.get(&script).cloned() else {
        return Err(Error::Other(format!(
          "missing script source for node_id={}",
          script.index()
        )));
      };

      // Mirror host currentScript state into the JS `document.currentScript` getter.
      let new_current_script = host.current_script();
      let nested_inner = (!self.did_nested)
        .then(|| {
          host
            .nested
            .and_then(|(outer, inner)| (outer == script).then_some(inner))
        })
        .flatten();

      host
        .js_ctx
        .with(|ctx| Self::js_push_current_script(ctx, new_current_script))?;

      // Always restore `document.currentScript` (via a JS-side stack), even when execution throws.
      let exec_result = (|| -> Result<()> {
        for (idx, seg) in segments.iter().enumerate() {
          host.js_ctx.with(|ctx| {
            ctx
              .eval::<(), _>(seg.as_str())
              .map_err(|e| Error::Other(e.to_string()))
          })?;

          // Simulate a nested script execution boundary between the first and second segments.
          if idx == 0 {
            if let Some(inner) = nested_inner {
              self.did_nested = true;
              orchestrator.execute_script_element(host, inner, ScriptType::Classic, self)?;
            }
          }
        }
        Ok(())
      })();

      let restore_result = host.js_ctx.with(|ctx| Self::js_pop_current_script(ctx));

      restore_result?;
      exec_result
    }
  }

  fn assert_log_eq_script_sequence(host: &JsHost, expected: &[NodeId]) -> Result<()> {
    host.js_ctx.with(|ctx| -> Result<()> {
      let globals = ctx.globals();
      let log: Vec<Value<'_>> = globals
        .get("log")
        .map_err(|e| Error::Other(e.to_string()))?;
      assert_eq!(
        log.len(),
        expected.len(),
        "log length mismatch: got {} expected {}",
        log.len(),
        expected.len()
      );

      // Compare each log entry to the wrapper object for the expected node id.
      for (idx, &node_id) in expected.iter().enumerate() {
        let expr = format!("log[{idx}] === __scriptByNodeId[\"{}\"]", node_id.index());
        let equal = ctx
          .eval::<bool, _>(expr.as_str())
          .map_err(|e| Error::Other(e.to_string()))?;
        assert!(equal, "log[{idx}] did not match node_id={}", node_id.index());
      }
      Ok(())
    })
  }

  #[test]
  fn document_current_script_restores_for_nested_execution() -> Result<()> {
    let renderer_dom =
      crate::dom::parse_html("<!doctype html><script id=a></script><script id=b></script>")?;
    let dom = Document::from_renderer_dom(&renderer_dom);
    let script_a = find_element_by_id(&dom, "a");
    let script_b = find_element_by_id(&dom, "b");

    let mut host = JsHost::new(dom, &[script_a, script_b])?;
    host.set_script_segments(
      script_a,
      vec!["log.push(document.currentScript);", "log.push(document.currentScript);"],
    );
    host.set_script_source(script_b, "log.push(document.currentScript);");
    host.set_nested(script_a, script_b);

    host.run_script_element(script_a, ScriptType::Classic)?;

    assert_log_eq_script_sequence(&host, &[script_a, script_b, script_a])?;
    assert!(host.eval_bool("document.currentScript === null")?);
    Ok(())
  }

  #[test]
  fn document_current_script_is_set_for_parser_blocking_async_and_defer() -> Result<()> {
    let renderer_dom = crate::dom::parse_html(
      r#"<!doctype html>
        <script id=a src="a.js"></script>
        <script id=b></script>
        <script id=c src="c.js" async></script>
        <script id=d src="d.js" defer></script>
      "#,
    )?;
    let dom = Document::from_renderer_dom(&renderer_dom);

    let a = find_element_by_id(&dom, "a");
    let b = find_element_by_id(&dom, "b");
    let c = find_element_by_id(&dom, "c");
    let d = find_element_by_id(&dom, "d");

    let mut host = JsHost::new(dom, &[a, b, c, d])?;
    let mut event_loop = EventLoop::<JsHost>::new();
    let mut scheduler = HtmlScriptScheduler::<NodeId>::new();

    let mut blocked_parser_on: Option<HtmlScriptId> = None;

    let mut apply_actions =
      |blocked_parser_on: &mut Option<HtmlScriptId>,
       host: &mut JsHost,
       event_loop: &mut EventLoop<JsHost>,
       actions: Vec<HtmlScriptSchedulerAction<NodeId>>|
       -> Result<()> {
        for action in actions {
          match action {
            HtmlScriptSchedulerAction::StartClassicFetch { .. } => {}
            HtmlScriptSchedulerAction::StartModuleGraphFetch { .. } => {}
            HtmlScriptSchedulerAction::StartInlineModuleGraphFetch { .. } => {}
            HtmlScriptSchedulerAction::BlockParserUntilExecuted { script_id, .. } => {
              *blocked_parser_on = Some(script_id);
            }
            HtmlScriptSchedulerAction::ExecuteNow {
              script_id,
              node_id,
              work,
            } => {
              if let HtmlScriptWork::Classic { source_text } = work {
                let Some(source_text) = source_text else {
                  continue;
                };
                host.set_script_source(node_id, &source_text);
                host.run_script_element(node_id, ScriptType::Classic)?;
                event_loop.perform_microtask_checkpoint(host)?;
                if *blocked_parser_on == Some(script_id) {
                  *blocked_parser_on = None;
                }
              }
            }
            HtmlScriptSchedulerAction::QueueTask { node_id, work, .. } => {
              if let HtmlScriptWork::Classic { source_text } = work {
                let Some(source_text) = source_text else {
                  continue;
                };
                event_loop.queue_task(TaskSource::Script, move |host, _event_loop| {
                  host.set_script_source(node_id, &source_text);
                  host.run_script_element(node_id, ScriptType::Classic)
                })?;
              }
            }
            HtmlScriptSchedulerAction::QueueScriptEventTask { .. } => {
              // These tasks fire `load`/`error` events at `<script>` elements as required by the HTML
              // script processing model. The `currentScript` tests don't model DOM events, so we can
              // safely ignore them here.
            }
          }
        }
        Ok(())
      };

    // Discover + execute a parser-blocking external script (no async/defer).
    let discovered = scheduler.discovered_parser_script(
      ScriptElementSpec {
        base_url: None,
        src: Some("https://example.com/a.js".to_string()),
        src_attr_present: true,
        inline_text: String::new(),
        async_attr: false,
        defer_attr: false,
        nomodule_attr: false,
        crossorigin: None,
        integrity_attr_present: false,
        integrity: None,
        referrer_policy: None,
        fetch_priority: None,
        parser_inserted: true,
        force_async: false,
        node_id: Some(a),
        script_type: ScriptType::Classic,
      },
      a,
      None,
    )?;
    let a_id = discovered.id;
    apply_actions(
      &mut blocked_parser_on,
      &mut host,
      &mut event_loop,
      discovered.actions,
    )?;
    assert_eq!(blocked_parser_on, Some(a_id));

    // Fetch completes; the blocking script executes synchronously and unblocks the parser.
    let actions = scheduler.classic_fetch_completed(
      a_id,
      "log.push(document.currentScript);".to_string(),
    )?;
    apply_actions(&mut blocked_parser_on, &mut host, &mut event_loop, actions)?;
    assert_eq!(blocked_parser_on, None);

    // Now the parser can continue and execute an inline script.
    let discovered = scheduler.discovered_parser_script(
      ScriptElementSpec {
        base_url: None,
        src: None,
        src_attr_present: false,
        inline_text: "log.push(document.currentScript);".to_string(),
        async_attr: false,
        defer_attr: false,
        nomodule_attr: false,
        crossorigin: None,
        integrity_attr_present: false,
        integrity: None,
        referrer_policy: None,
        fetch_priority: None,
        parser_inserted: true,
        force_async: false,
        node_id: Some(b),
        script_type: ScriptType::Classic,
      },
      b,
      None,
    )?;
    apply_actions(
      &mut blocked_parser_on,
      &mut host,
      &mut event_loop,
      discovered.actions,
    )?;

    // Discover async + defer external scripts.
    let discovered = scheduler.discovered_parser_script(
      ScriptElementSpec {
        base_url: None,
        src: Some("https://example.com/c.js".to_string()),
        src_attr_present: true,
        inline_text: String::new(),
        async_attr: true,
        defer_attr: false,
        nomodule_attr: false,
        crossorigin: None,
        integrity_attr_present: false,
        integrity: None,
        referrer_policy: None,
        fetch_priority: None,
        parser_inserted: true,
        force_async: false,
        node_id: Some(c),
        script_type: ScriptType::Classic,
      },
      c,
      None,
    )?;
    let c_id = discovered.id;
    apply_actions(
      &mut blocked_parser_on,
      &mut host,
      &mut event_loop,
      discovered.actions,
    )?;

    let discovered = scheduler.discovered_parser_script(
      ScriptElementSpec {
        base_url: None,
        src: Some("https://example.com/d.js".to_string()),
        src_attr_present: true,
        inline_text: String::new(),
        async_attr: false,
        defer_attr: true,
        nomodule_attr: false,
        crossorigin: None,
        integrity_attr_present: false,
        integrity: None,
        referrer_policy: None,
        fetch_priority: None,
        parser_inserted: true,
        force_async: false,
        node_id: Some(d),
        script_type: ScriptType::Classic,
      },
      d,
      None,
    )?;
    let d_id = discovered.id;
    apply_actions(
      &mut blocked_parser_on,
      &mut host,
      &mut event_loop,
      discovered.actions,
    )?;

    // Parsing completes (allows defer scripts to queue once ready).
    apply_actions(
      &mut blocked_parser_on,
      &mut host,
      &mut event_loop,
      scheduler.parsing_completed()?,
    )?;

    // Complete fetches (queue tasks).
    apply_actions(
      &mut blocked_parser_on,
      &mut host,
      &mut event_loop,
      scheduler.classic_fetch_completed(c_id, "log.push(document.currentScript);".to_string())?,
    )?;
    apply_actions(
      &mut blocked_parser_on,
      &mut host,
      &mut event_loop,
      scheduler.classic_fetch_completed(d_id, "log.push(document.currentScript);".to_string())?,
    )?;

    // Drain event loop tasks (async/defer scripts run here).
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    // Expected order: a (blocking external), b (inline), c (async), d (defer).
    assert_log_eq_script_sequence(&host, &[a, b, c, d])?;
    assert!(host.eval_bool("document.currentScript === null")?);
    Ok(())
  }

  #[test]
  fn document_current_script_is_null_for_shadow_root_scripts() -> Result<()> {
    let renderer_dom = crate::dom::parse_html(
      r#"<!doctype html>
        <div id=host>
          <template shadowroot="open">
            <script id=shadow></script>
          </template>
        </div>
      "#,
    )?;
    let dom = Document::from_renderer_dom(&renderer_dom);
    let shadow_script = find_element_by_id(&dom, "shadow");

    let mut host = JsHost::new(dom, &[shadow_script])?;
    host.set_script_source(
      shadow_script,
      "globalThis.shadowObserved = document.currentScript;",
    );

    host.run_script_element(shadow_script, ScriptType::Classic)?;
    assert!(host.eval_bool("shadowObserved === null")?);
    Ok(())
  }

  #[test]
  fn document_current_script_is_null_for_module_scripts() -> Result<()> {
    let renderer_dom = crate::dom::parse_html("<!doctype html><script id=mod></script>")?;
    let dom = Document::from_renderer_dom(&renderer_dom);
    let module_script = find_element_by_id(&dom, "mod");

    let mut host = JsHost::new(dom, &[module_script])?;
    host.set_script_source(
      module_script,
      "globalThis.modObserved = document.currentScript;",
    );

    host.run_script_element(module_script, ScriptType::Module)?;
    assert!(host.eval_bool("modObserved === null")?);
    assert!(host.eval_bool("document.currentScript === null")?);
    Ok(())
  }

  #[test]
  fn current_script_is_restored_on_js_exception() -> Result<()> {
    let renderer_dom = crate::dom::parse_html("<!doctype html><script id=boom></script>")?;
    let dom = Document::from_renderer_dom(&renderer_dom);
    let boom = find_element_by_id(&dom, "boom");

    let mut host = JsHost::new(dom, &[boom])?;
    host.set_script_source(boom, "throw new Error('boom');");

    let err = host
      .run_script_element(boom, ScriptType::Classic)
      .expect_err("expected script execution to throw");
    assert!(matches!(err, Error::Other(_)));
    assert!(host.eval_bool("document.currentScript === null")?);
    Ok(())
  }

  #[test]
  fn disconnected_scripts_do_not_execute_and_do_not_affect_current_script() -> Result<()> {
    let renderer_dom = crate::dom::parse_html(
      "<!doctype html><template><script id=inert></script></template><script id=live></script>",
    )?;
    let dom = Document::from_renderer_dom(&renderer_dom);
    let inert = find_element_by_id(&dom, "inert");
    let live = find_element_by_id(&dom, "live");

    let mut host = JsHost::new(dom, &[inert, live])?;
    host.set_script_source(inert, "globalThis.inertRan = true;");
    host.set_script_source(live, "globalThis.liveObserved = document.currentScript;");

    host.run_script_element(inert, ScriptType::Classic)?;
    host.run_script_element(live, ScriptType::Classic)?;

    // The inert script should not have run.
    assert!(host.eval_bool("typeof inertRan === 'undefined'")?);
    // The live script should observe itself as currentScript.
    assert!(host.eval_bool(&format!(
      "liveObserved === __scriptByNodeId[\"{}\"]",
      live.index()
    ))?);
    // And `currentScript` should remain null after execution.
    assert!(host.eval_bool("document.currentScript === null")?);
    Ok(())
  }

  #[test]
  fn disconnected_scripts_do_not_modify_current_script_when_already_set() -> Result<()> {
    let renderer_dom = crate::dom::parse_html(
      "<!doctype html><template><script id=inert></script></template><script id=live></script>",
    )?;
    let dom = Document::from_renderer_dom(&renderer_dom);
    let inert = find_element_by_id(&dom, "inert");
    let live = find_element_by_id(&dom, "live");

    let mut host = JsHost::new(dom, &[inert, live])?;

    // Simulate an already-executing script (both host-side and in the JS `document`).
    host.script_state.borrow_mut().current_script = Some(live);
    host.js_ctx.with(|ctx| -> Result<()> {
      ctx
        .eval::<(), _>(format!(
          "document.__currentScript = __scriptByNodeId[\"{}\"]; document.__currentScriptStack = [];",
          live.index()
        ))
        .map_err(|e| Error::Other(e.to_string()))?;
      Ok(())
    })?;

    host.run_script_element(inert, ScriptType::Classic)?;

    // Inert scripts must not execute and must not affect currentScript.
    assert_eq!(host.current_script(), Some(live));
    assert!(host.eval_bool(&format!(
      "document.currentScript === __scriptByNodeId[\"{}\"]",
      live.index()
    ))?);
    assert!(host.eval_bool("document.__currentScriptStack.length === 0")?);
    Ok(())
  }
}
