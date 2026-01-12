use std::ptr::NonNull;
use std::rc::Rc;

use crate::dom2;
use crate::js::{CurrentScriptHost, CurrentScriptStateHandle, DomHost, ScriptExecutionLog};
use crate::web::events;

#[derive(Debug, Default)]
pub(crate) struct ActiveEventStack {
  stack: Vec<ActiveEventEntry>,
}

#[derive(Debug, Clone, Copy)]
struct ActiveEventEntry {
  event_id: u64,
  event: NonNull<events::Event>,
}

#[derive(Debug)]
pub(crate) struct ActiveEventGuard {
  stack: *mut ActiveEventStack,
  event_id: u64,
}

impl Drop for ActiveEventGuard {
  fn drop(&mut self) {
    // SAFETY: `ActiveEventGuard` is only created by `ActiveEventStack::push`, which stores a raw
    // pointer to the originating stack. The stack outlives the guard because it is owned by the
    // `VmHost` passed into JS execution (and the guard is dropped before returning from that call).
    let stack = unsafe { &mut *self.stack };
    if let Some(last) = stack.stack.last() {
      if last.event_id == self.event_id {
        stack.stack.pop();
        return;
      }
    }
    // Out-of-order drop should not happen, but avoid leaking active-event state if it does.
    if let Some(pos) = stack
      .stack
      .iter()
      .rposition(|entry| entry.event_id == self.event_id)
    {
      stack.stack.remove(pos);
    }
  }
}

impl ActiveEventStack {
  pub(crate) fn push(&mut self, event_id: u64, event: &mut events::Event) -> ActiveEventGuard {
    self.stack.push(ActiveEventEntry {
      event_id,
      event: NonNull::from(event),
    });
    ActiveEventGuard {
      stack: self as *mut _,
      event_id,
    }
  }

  pub(crate) fn with_event<R>(
    &mut self,
    event_id: u64,
    f: impl FnOnce(&mut events::Event) -> R,
  ) -> Option<R> {
    let ptr = self
      .stack
      .iter()
      .rfind(|entry| entry.event_id == event_id)?
      .event;
    // SAFETY: entries in `ActiveEventStack` are only created for the dynamic extent of dispatch.
    Some(unsafe { f(&mut *ptr.as_ptr()) })
  }
}

/// Host-owned document state that composes:
/// - the mutable DOM tree (`dom2`),
/// - a shared DOM Events listener registry, and
/// - host bookkeeping for `Document.currentScript` (plus optional execution logging).
///
/// This is intentionally renderer/engine agnostic so that JS/WebIDL bindings can depend on it
/// without embedding rendering pipeline details.
pub struct HostDocumentState {
  dom: dom2::Document,
  events: Rc<events::EventListenerRegistry>,
  current_script: CurrentScriptStateHandle,
  script_log: Option<ScriptExecutionLog>,
  active_events: ActiveEventStack,
}

/// Backwards-compatible alias retained for older call sites.
pub type DocumentHostState = HostDocumentState;

impl std::fmt::Debug for HostDocumentState {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("HostDocumentState")
      .field("dom", &self.dom)
      .field("events", &self.events)
      .field("current_script", &self.current_script)
      .field("script_log", &self.script_log)
      .finish()
  }
}

impl HostDocumentState {
  pub fn new(dom: dom2::Document) -> Self {
    Self {
      dom,
      events: Rc::new(events::EventListenerRegistry::new()),
      current_script: CurrentScriptStateHandle::default(),
      script_log: None,
      active_events: ActiveEventStack::default(),
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

  pub fn events(&self) -> Rc<events::EventListenerRegistry> {
    Rc::clone(&self.events)
  }

  pub fn events_ref(&self) -> &events::EventListenerRegistry {
    self.events.as_ref()
  }

  pub fn current_script_handle(&self) -> &CurrentScriptStateHandle {
    &self.current_script
  }

  pub fn enable_script_execution_log(&mut self, capacity: usize) {
    self.script_log = Some(ScriptExecutionLog::new(capacity));
  }

  pub fn event_target_for_node(&self, node: dom2::NodeId) -> events::EventTargetId {
    // `dom2::NodeId` is an opaque index, but the document node is always index 0.
    if node == self.dom.root() || node.index() == 0 {
      return events::EventTargetId::Document;
    }
    events::EventTargetId::Node(node)
  }

  pub(crate) fn push_active_event(
    &mut self,
    event_id: u64,
    event: &mut events::Event,
  ) -> ActiveEventGuard {
    self.active_events.push(event_id, event)
  }

  pub(crate) fn with_active_event<R>(
    &mut self,
    event_id: u64,
    f: impl FnOnce(&mut events::Event) -> R,
  ) -> Option<R> {
    self.active_events.with_event(event_id, f)
  }
}

impl CurrentScriptHost for HostDocumentState {
  fn current_script_state(&self) -> &CurrentScriptStateHandle {
    &self.current_script
  }

  fn script_execution_log(&self) -> Option<&ScriptExecutionLog> {
    self.script_log.as_ref()
  }

  fn script_execution_log_mut(&mut self) -> Option<&mut ScriptExecutionLog> {
    self.script_log.as_mut()
  }
}

impl DomHost for HostDocumentState {
  fn with_dom<R, F>(&self, f: F) -> R
  where
    F: FnOnce(&dom2::Document) -> R,
  {
    f(&self.dom)
  }

  fn mutate_dom<R, F>(&mut self, f: F) -> R
  where
    F: FnOnce(&mut dom2::Document) -> (R, bool),
  {
    let (result, changed) = f(&mut self.dom);
    // `dom2::Document` records mutation metadata for incremental invalidation. `HostDocumentState`
    // has no renderer, but callers (tests / tooling / future invalidation) still expect mutation
    // logs to be scoped to each `mutate_dom` turn rather than accumulating indefinitely.
    if changed {
      let _ = self.dom.take_mutations();
    } else {
      self.dom.clear_mutations();
    }
    result
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::dom2::{Document, NodeId, NodeKind};
  use crate::error::{Error, Result};
  use crate::js::{ScriptBlockExecutor, ScriptOrchestrator, ScriptType};

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

  impl ScriptBlockExecutor<HostDocumentState> for RecordingExecutor {
    fn execute_script(
      &mut self,
      host: &mut HostDocumentState,
      _orchestrator: &mut ScriptOrchestrator,
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
    let mut host = HostDocumentState::from_renderer_dom(&renderer_dom);

    let scripts = find_script_elements(host.dom());
    assert_eq!(scripts.len(), 1);
    let script = scripts[0];

    // Simulate an outer (already executing) script.
    let outer_current = host.dom().root();
    host.current_script_handle().borrow_mut().current_script = Some(outer_current);

    let mut orchestrator = ScriptOrchestrator::new();
    let mut executor = RecordingExecutor::new();
    orchestrator.execute_script_element(&mut host, script, ScriptType::Classic, &mut executor)?;

    assert_eq!(executor.observed, vec![Some(script)]);
    assert_eq!(host.current_script(), Some(outer_current));
    Ok(())
  }

  #[test]
  fn current_script_is_restored_on_error_via_document_host_state() {
    let renderer_dom = crate::dom::parse_html("<!doctype html><script></script>").unwrap();
    let mut host = HostDocumentState::from_renderer_dom(&renderer_dom);

    let scripts = find_script_elements(host.dom());
    assert_eq!(scripts.len(), 1);
    let script = scripts[0];

    let outer_current = host.dom().root();
    host.current_script_handle().borrow_mut().current_script = Some(outer_current);

    let mut orchestrator = ScriptOrchestrator::new();
    let mut executor = RecordingExecutor::failing();
    let err = orchestrator
      .execute_script_element(&mut host, script, ScriptType::Classic, &mut executor)
      .expect_err("expected script execution to fail");

    assert!(matches!(err, Error::Other(msg) if msg == "boom"));
    assert_eq!(host.current_script(), Some(outer_current));
  }

  #[test]
  fn event_target_for_node_maps_document_root_to_document() {
    let renderer_dom = crate::dom::parse_html("<!doctype html><div></div>").unwrap();
    let host = HostDocumentState::from_renderer_dom(&renderer_dom);

    let root = host.dom().root();
    assert_eq!(
      host.event_target_for_node(root),
      events::EventTargetId::Document
    );
  }

  #[test]
  fn document_host_state_event_registry_is_document_owned() {
    let renderer_dom = crate::dom::parse_html("<!doctype html><div></div>").unwrap();
    let host = DocumentHostState::from_renderer_dom(&renderer_dom);

    let target = events::EventTargetId::Document;
    let listener_id = events::ListenerId::new(1);
    let options = events::AddEventListenerOptions::default();

    assert!(
      host
        .events_ref()
        .add_event_listener(target, "click", listener_id, options),
      "listener should be newly inserted"
    );

    assert!(
      host
        .events_ref()
        .remove_event_listener(target, "click", listener_id, options.capture),
      "listener should be removable through the host-owned registry"
    );
  }

  #[test]
  fn mutate_dom_no_change_clears_dom2_mutation_log() {
    let renderer_dom = crate::dom::parse_html("<!doctype html><div id=a></div>").unwrap();
    let mut host = HostDocumentState::from_renderer_dom(&renderer_dom);

    let div = host
      .dom()
      .get_element_by_id("a")
      .expect("expected element with id=a");

    // Seed the mutation log.
    assert!(host
      .dom_mut()
      .set_attribute(div, "data-test", "1")
      .expect("set_attribute should succeed"));
    let seeded = host.dom_mut().take_mutations();
    assert!(
      !seeded.is_empty(),
      "expected mutation log to contain the seeded mutation"
    );

    // Seed it again, then ensure `mutate_dom` clears the pending log even when the closure reports
    // `changed == false`.
    assert!(host
      .dom_mut()
      .set_attribute(div, "data-test", "2")
      .expect("set_attribute should succeed"));
    host.mutate_dom(|_| ((), false));

    let after = host.dom_mut().take_mutations();
    assert!(
      after.is_empty(),
      "expected HostDocumentState::mutate_dom to clear dom2 mutation logs"
    );
  }

  #[test]
  fn mutate_dom_change_clears_dom2_mutation_log() {
    let renderer_dom = crate::dom::parse_html("<!doctype html><div id=a></div>").unwrap();
    let mut host = HostDocumentState::from_renderer_dom(&renderer_dom);

    host.mutate_dom(|dom| {
      let div = dom
        .get_element_by_id("a")
        .expect("expected element with id=a");
      let changed = dom
        .set_attribute(div, "data-test", "1")
        .expect("set_attribute should succeed");
      ((), changed)
    });

    let after = host.dom_mut().take_mutations();
    assert!(
      after.is_empty(),
      "expected HostDocumentState::mutate_dom to clear dom2 mutation logs"
    );
  }
}
