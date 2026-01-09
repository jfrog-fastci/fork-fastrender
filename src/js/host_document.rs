use crate::dom2;
use crate::web::events;

/// Host-owned document state that composes the mutable DOM tree (`dom2`), the event listener
/// registry (`web::events`), and bookkeeping for `Document.currentScript`.
///
/// This is intentionally renderer/engine agnostic so that JS/WebIDL bindings can depend on it
/// without embedding rendering pipeline details.
pub struct DocumentHostState {
  dom: dom2::Document,
  current_script: crate::js::CurrentScriptStateHandle,
}

impl std::fmt::Debug for DocumentHostState {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("DocumentHostState")
      .field("dom", &self.dom)
      .field("current_script", &self.current_script)
      .finish()
  }
}

impl DocumentHostState {
  pub fn new(dom: dom2::Document) -> Self {
    Self {
      dom,
      current_script: crate::js::CurrentScriptStateHandle::default(),
    }
  }

  pub fn from_renderer_dom(root: &crate::dom::DomNode) -> Self {
    Self::new(dom2::Document::from_renderer_dom(root))
  }

  pub fn dom(&self) -> &dom2::Document {
    &self.dom
  }

  pub fn dom_mut(&mut self) -> &mut dom2::Document {
    &mut self.dom
  }

  pub fn events(&self) -> &events::EventListenerRegistry {
    self.dom.events()
  }

  pub fn events_mut(&mut self) -> &mut events::EventListenerRegistry {
    self.dom.events_mut()
  }

  /// Convenience passthrough for `Document.currentScript` as a `dom2::NodeId` handle.
  pub fn current_script(&self) -> Option<dom2::NodeId> {
    self.current_script.borrow().current_script
  }
}

impl crate::js::CurrentScriptHost for DocumentHostState {
  fn current_script_state(&self) -> &crate::js::CurrentScriptStateHandle {
    &self.current_script
  }
}

/// Map a `dom2` node id into a stable `EventTargetId`.
///
/// The document root is normalized to `EventTargetId::Document` so that host code doesn't end up
/// with two distinct IDs for the same target (`Document` vs `Node(dom.root())`).
pub fn event_target_for_node(node: dom2::NodeId) -> events::EventTargetId {
  if node.index() == 0 {
    events::EventTargetId::Document
  } else {
    events::EventTargetId::Node(node)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::dom2::{Document, NodeId, NodeKind};
  use crate::error::{Error, Result};
  use crate::js::{CurrentScriptHost, ScriptBlockExecutor, ScriptOrchestrator, ScriptType};

  fn find_script_elements(dom: &Document) -> Vec<NodeId> {
    dom
      .subtree_preorder(dom.root())
      .filter(|&id| match &dom.node(id).kind {
        NodeKind::Element { tag_name, .. } if tag_name.eq_ignore_ascii_case("script") => true,
        _ => false,
      })
      .collect()
  }

  struct RecordingExecutor {
    observed: Vec<Option<NodeId>>,
    fail: bool,
  }

  impl RecordingExecutor {
    fn new() -> Self {
      Self {
        observed: Vec::new(),
        fail: false,
      }
    }

    fn failing() -> Self {
      Self {
        observed: Vec::new(),
        fail: true,
      }
    }
  }

  impl ScriptBlockExecutor<DocumentHostState> for RecordingExecutor {
    fn execute_script(
      &mut self,
      host: &mut DocumentHostState,
      _orchestrator: &mut ScriptOrchestrator,
      _dom: &Document,
      _script: NodeId,
      _script_type: ScriptType,
    ) -> Result<()> {
      self.observed.push(host.current_script());
      if self.fail {
        return Err(Error::Other("boom".to_string()));
      }
      Ok(())
    }
  }

  #[test]
  fn current_script_is_set_and_restored_via_document_host_state() -> Result<()> {
    let renderer_dom = crate::dom::parse_html("<!doctype html><script></script>").unwrap();
    let mut host = DocumentHostState::from_renderer_dom(&renderer_dom);
    let dom = host.dom().clone();

    let scripts = find_script_elements(&dom);
    assert_eq!(scripts.len(), 1);
    let script = scripts[0];

    // Simulate an outer (already executing) script.
    let outer_current = dom.root();
    host
      .current_script_state()
      .borrow_mut()
      .current_script = Some(outer_current);

    let mut orchestrator = ScriptOrchestrator::new();
    let mut executor = RecordingExecutor::new();
    orchestrator.execute_script_element(
      &mut host,
      &dom,
      script,
      ScriptType::Classic,
      &mut executor,
    )?;

    assert_eq!(executor.observed, vec![Some(script)]);
    assert_eq!(host.current_script(), Some(outer_current));
    Ok(())
  }

  #[test]
  fn current_script_is_restored_on_error_via_document_host_state() {
    let renderer_dom = crate::dom::parse_html("<!doctype html><script></script>").unwrap();
    let mut host = DocumentHostState::from_renderer_dom(&renderer_dom);
    let dom = host.dom().clone();

    let scripts = find_script_elements(&dom);
    assert_eq!(scripts.len(), 1);
    let script = scripts[0];

    let outer_current = dom.root();
    host
      .current_script_state()
      .borrow_mut()
      .current_script = Some(outer_current);

    let mut orchestrator = ScriptOrchestrator::new();
    let mut executor = RecordingExecutor::failing();
    let err = orchestrator
      .execute_script_element(&mut host, &dom, script, ScriptType::Classic, &mut executor)
      .expect_err("expected script execution to fail");

    assert!(matches!(err, Error::Other(msg) if msg == "boom"));
    assert_eq!(host.current_script(), Some(outer_current));
  }

  #[test]
  fn event_target_for_node_maps_document_root_to_document() {
    let renderer_dom = crate::dom::parse_html("<!doctype html><div></div>").unwrap();
    let host = DocumentHostState::from_renderer_dom(&renderer_dom);

    let root = host.dom().root();
    assert_eq!(event_target_for_node(root), events::EventTargetId::Document);
  }

  #[test]
  fn document_host_state_event_registry_is_document_owned() {
    let renderer_dom = crate::dom::parse_html("<!doctype html><div></div>").unwrap();
    let mut host = DocumentHostState::from_renderer_dom(&renderer_dom);

    let target = events::EventTargetId::Document;
    let listener_id = events::ListenerId::new(1);
    let options = events::AddEventListenerOptions::default();

    assert!(
      host
        .events_mut()
        .add_event_listener(target, "click", listener_id, options),
      "listener should be newly inserted"
    );

    assert!(
      host
        .dom()
        .events()
        .remove_event_listener(target, "click", listener_id, options.capture),
      "listener should be removable through the dom2::Document registry"
    );
  }
}
