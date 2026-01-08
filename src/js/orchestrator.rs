use crate::dom2::{Document, NodeId, NodeKind};
use crate::error::Result;
use crate::js::ScriptType;

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

/// Trait for host types that carry `Document.currentScript` state.
pub trait CurrentScriptHost {
  fn current_script_state(&self) -> &CurrentScriptState;
  fn current_script_state_mut(&mut self) -> &mut CurrentScriptState;

  fn current_script(&self) -> Option<NodeId> {
    self.current_script_state().current_script
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
    let new_current_script = match script_type {
      ScriptType::Classic => (!node_root_is_shadow_root(dom, script)).then_some(script),
      // `Document.currentScript` is null for module scripts.
      ScriptType::Module => None,
      // Import maps and unknown script types are not executed (currentScript remains null).
      ScriptType::ImportMap | ScriptType::Unknown => None,
    };

    host.current_script_state_mut().push(new_current_script);
    let result = executor.execute_script(host, self, dom, script, script_type);
    host.current_script_state_mut().pop();
    result
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
    script_state: CurrentScriptState,
  }

  impl CurrentScriptHost for Host {
    fn current_script_state(&self) -> &CurrentScriptState {
      &self.script_state
    }

    fn current_script_state_mut(&mut self) -> &mut CurrentScriptState {
      &mut self.script_state
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
    assert_eq!(host.script_state.stack_depth(), 0);
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
    assert_eq!(host.script_state.stack_depth(), 0);
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
    host.script_state.current_script = Some(outer_current);

    let mut orchestrator = ScriptOrchestrator::new();
    let mut executor = ErroringExecutor;
    let err = orchestrator
      .execute_script_element(&mut host, &dom, script, ScriptType::Classic, &mut executor)
      .expect_err("expected script execution to fail");

    assert!(matches!(err, Error::Other(msg) if msg == "boom"));
    assert_eq!(host.current_script(), Some(outer_current));
    assert_eq!(host.script_state.stack_depth(), 0);
  }
}
