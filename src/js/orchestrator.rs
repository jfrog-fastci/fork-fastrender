use crate::dom2::{Document, NodeId, NodeKind};
use crate::error::Result;
use crate::js::ScriptType;
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
}

impl CurrentScriptState {
  fn push(&mut self, script: Option<NodeId>) {
    self.previous_current_script.push(self.current_script);
    self.current_script = script;
  }

  fn pop(&mut self) {
    let previous = self
      .previous_current_script
      .pop()
      .expect("currentScript stack underflow");
    self.current_script = previous;
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
    dom: &Document,
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
    dom: &Document,
    script: NodeId,
    script_type: ScriptType,
    executor: &mut Exec,
  ) -> Result<()>
  where
    Host: CurrentScriptHost,
    Exec: ScriptBlockExecutor<Host>,
  {
    // HTML: "prepare a script" early-outs when the script element is not connected.
    //
    // In `dom2`, `<template>` contents are represented as inert subtrees (the nodes remain in the
    // tree for snapshotting/traversal, but should not be treated as connected for scripting).
    // Scripts that have been detached from the document must also be skipped.
    if !dom.is_connected_for_scripting(script) {
      return Ok(());
    }

    let new_current_script = match script_type {
      ScriptType::Classic => (!node_root_is_shadow_root(dom, script)).then_some(script),
      // `Document.currentScript` is null for module scripts.
      ScriptType::Module => None,
      // Import maps and unknown script types are not executed (currentScript remains null).
      ScriptType::ImportMap | ScriptType::Unknown => None,
    };

    host
      .current_script_state()
      .borrow_mut()
      .push(new_current_script);
    if let Some(log) = host.script_execution_log_mut() {
      log.record(ScriptExecutionLogEntry {
        script_id: script.index(),
        source: script_source_snapshot(dom, script),
        current_script_node_id: new_current_script.map(|id| id.index()),
      });
    }
    let result = executor.execute_script(host, self, dom, script, script_type);
    host.current_script_state().borrow_mut().pop();
    result
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
    .find(|(k, _)| k.eq_ignore_ascii_case("src"))
    .map(|(_, v)| v.to_string());
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

  #[derive(Default)]
  struct Host {
    script_state: CurrentScriptStateHandle,
    log: Option<ScriptExecutionLog>,
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
      _dom: &Dom2Document,
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

    let mut host = Host::default();
    let mut orchestrator = ScriptOrchestrator::new();
    let mut executor = RecordingExecutor::default();

    orchestrator.execute_script_element(
      &mut host,
      &dom,
      scripts[0],
      ScriptType::Classic,
      &mut executor,
    )?;
    assert_eq!(host.current_script(), None);

    orchestrator.execute_script_element(
      &mut host,
      &dom,
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
      dom: &Dom2Document,
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
        orchestrator.execute_script_element(host, dom, self.script_b, ScriptType::Classic, self)?;
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

    let mut host = Host::default();
    let mut orchestrator = ScriptOrchestrator::new();
    let mut executor = NestedExecutor::new(script_a, script_b);

    orchestrator.execute_script_element(
      &mut host,
      &dom,
      script_a,
      ScriptType::Classic,
      &mut executor,
    )?;

    assert_eq!(
      executor.observed,
      vec![Some(script_a), Some(script_b), Some(script_a)]
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
      _dom: &Dom2Document,
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

    let mut host = Host::default();
    // Simulate an outer (already executing) script.
    let outer_current = dom.root();
    host.script_state.borrow_mut().current_script = Some(outer_current);

    let mut orchestrator = ScriptOrchestrator::new();
    let mut executor = ErroringExecutor;
    let err = orchestrator
      .execute_script_element(&mut host, &dom, script, ScriptType::Classic, &mut executor)
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

    let mut host = Host::default();
    let mut orchestrator = ScriptOrchestrator::new();
    let mut executor = RecordingExecutor::default();

    orchestrator.execute_script_element(
      &mut host,
      &dom,
      inert_script,
      ScriptType::Classic,
      &mut executor,
    )?;
    assert_eq!(host.current_script(), None);
    assert_eq!(executor.observed, Vec::<Option<NodeId>>::new());

    orchestrator.execute_script_element(
      &mut host,
      &dom,
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
    let renderer_dom = crate::dom::parse_html("<!doctype html><script></script><script></script>")
      .unwrap();
    let dom = Dom2Document::from_renderer_dom(&renderer_dom);
    let scripts = find_script_elements(&dom);
    assert_eq!(scripts.len(), 2);

    let mut host = Host::default();
    host.log = Some(ScriptExecutionLog::new(16));
    let mut orchestrator = ScriptOrchestrator::new();
    let mut executor = RecordingExecutor::default();

    orchestrator.execute_script_element(
      &mut host,
      &dom,
      scripts[0],
      ScriptType::Classic,
      &mut executor,
    )?;
    orchestrator.execute_script_element(
      &mut host,
      &dom,
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
    let renderer_dom = crate::dom::parse_html("<!doctype html><script></script><script></script>")
      .unwrap();
    let dom = Dom2Document::from_renderer_dom(&renderer_dom);
    let scripts = find_script_elements(&dom);
    assert_eq!(scripts.len(), 2);

    let mut host = Host::default();
    host.log = Some(ScriptExecutionLog::new(1));
    let mut orchestrator = ScriptOrchestrator::new();
    let mut executor = RecordingExecutor::default();

    orchestrator.execute_script_element(
      &mut host,
      &dom,
      scripts[0],
      ScriptType::Classic,
      &mut executor,
    )?;
    orchestrator.execute_script_element(
      &mut host,
      &dom,
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
}
