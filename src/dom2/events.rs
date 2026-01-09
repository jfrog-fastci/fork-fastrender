//! DOM Events foundations (WHATWG DOM).
//!
//! FastRender previously had two event dispatch foundations:
//! - `dom2::events` (integrated with the mutable `dom2::Document`)
//! - `web::events` (a standalone registry + dispatch algorithm used only by its own tests)
//!
//! `dom2::events` is the canonical implementation going forward so JS bindings can target a single
//! spec-shaped foundation and share listener dispatch semantics with the mutable DOM.

use std::collections::HashMap;

use crate::Result;

use super::{Document, NodeId};

/// Event phase values match the Web IDL `EventPhase` enum.
///
/// In particular, the discriminant values are stable for JS bindings:
/// - `0` = NONE
/// - `1` = CAPTURING_PHASE
/// - `2` = AT_TARGET
/// - `3` = BUBBLING_PHASE
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventPhase {
  None = 0,
  CapturingPhase = 1,
  AtTarget = 2,
  BubblingPhase = 3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventTargetId {
  Window,
  Document,
  Node(NodeId),
  // Future: ShadowRoot, etc.
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventInit {
  pub bubbles: bool,
  pub cancelable: bool,
  pub composed: bool,
}

impl Default for EventInit {
  fn default() -> Self {
    Self {
      bubbles: false,
      cancelable: false,
      composed: false,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ListenerId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventListenerOptions {
  pub capture: bool,
  pub once: bool,
  pub passive: bool,
}

impl Default for EventListenerOptions {
  fn default() -> Self {
    Self {
      capture: false,
      once: false,
      passive: false,
    }
  }
}

#[derive(Debug, Clone)]
pub struct Event {
  pub type_: String,
  pub bubbles: bool,
  pub cancelable: bool,
  pub composed: bool,
  pub default_prevented: bool,
  pub event_phase: EventPhase,
  pub target: Option<EventTargetId>,
  pub current_target: Option<EventTargetId>,
  pub stop_propagation: bool,
  pub stop_immediate_propagation: bool,

  // Internal "in passive listener" flag that affects `prevent_default`.
  in_passive_listener: bool,
}

impl Event {
  pub fn new(type_: impl Into<String>, init: EventInit) -> Self {
    Self {
      type_: type_.into(),
      bubbles: init.bubbles,
      cancelable: init.cancelable,
      composed: init.composed,
      default_prevented: false,
      event_phase: EventPhase::None,
      target: None,
      current_target: None,
      stop_propagation: false,
      stop_immediate_propagation: false,
      in_passive_listener: false,
    }
  }

  pub fn stop_propagation(&mut self) {
    self.stop_propagation = true;
  }

  pub fn stop_immediate_propagation(&mut self) {
    // DOM semantics: stopImmediatePropagation implies stopPropagation.
    self.stop_propagation = true;
    self.stop_immediate_propagation = true;
  }

  pub fn prevent_default(&mut self) {
    if !self.cancelable {
      return;
    }
    if self.in_passive_listener {
      return;
    }
    self.default_prevented = true;
  }
}

pub trait EventListenerInvoker {
  fn invoke(
    &mut self,
    listener_id: ListenerId,
    event: &mut Event,
    ctx: &mut dyn EventListenerContext,
  ) -> Result<()>;
}

/// Minimal host context exposed to event listeners.
///
/// This allows callbacks to remove listeners during dispatch without needing to re-borrow the whole
/// [`Document`].
pub trait EventListenerContext {
  fn remove_event_listener(
    &mut self,
    target: EventTargetId,
    type_: &str,
    listener_id: ListenerId,
    capture: bool,
  ) -> bool;
}

/// DOM-like dispatch return value:
/// - `Ok(true)` if `prevent_default` was not successfully called.
/// - `Ok(false)` if the event was cancelable and `prevent_default` was called from a non-passive
///   listener.
pub type DispatchResult = Result<bool>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RegisteredListener {
  record_id: u64,
  id: ListenerId,
  options: EventListenerOptions,
}

#[derive(Debug, Clone, Default)]
pub struct EventListenerRegistry {
  next_record_id: u64,
  // target -> type -> listeners (in insertion order)
  listeners: HashMap<EventTargetId, HashMap<String, Vec<RegisteredListener>>>,
}

impl EventListenerRegistry {
  pub fn add_event_listener(
    &mut self,
    target: EventTargetId,
    type_: &str,
    listener_id: ListenerId,
    options: EventListenerOptions,
  ) -> bool {
    let by_type = self.listeners.entry(target).or_default();
    let listeners = by_type.entry(type_.to_string()).or_default();

    if listeners
      .iter()
      .any(|l| l.id == listener_id && l.options.capture == options.capture)
    {
      return false;
    }

    let record_id = self.next_record_id;
    self.next_record_id = self.next_record_id.wrapping_add(1);
    listeners.push(RegisteredListener {
      record_id,
      id: listener_id,
      options,
    });
    true
  }

  pub fn remove_event_listener(
    &mut self,
    target: EventTargetId,
    type_: &str,
    listener_id: ListenerId,
    capture: bool,
  ) -> bool {
    let Some(by_type) = self.listeners.get_mut(&target) else {
      return false;
    };
    let Some(listeners) = by_type.get_mut(type_) else {
      return false;
    };

    let Some(idx) = listeners
      .iter()
      .position(|l| l.id == listener_id && l.options.capture == capture)
    else {
      return false;
    };

    listeners.remove(idx);
    if listeners.is_empty() {
      by_type.remove(type_);
      if by_type.is_empty() {
        self.listeners.remove(&target);
      }
    }
    true
  }

  fn listeners_snapshot(&self, target: EventTargetId, type_: &str) -> Vec<RegisteredListener> {
    self
      .listeners
      .get(&target)
      .and_then(|by_type| by_type.get(type_))
      .cloned()
      .unwrap_or_default()
  }

  fn listener_record_registered(
    &self,
    target: EventTargetId,
    type_: &str,
    record_id: u64,
  ) -> bool {
    self
      .listeners
      .get(&target)
      .and_then(|by_type| by_type.get(type_))
      .is_some_and(|listeners| {
        listeners.iter().any(|l| l.record_id == record_id)
      })
  }
}

impl EventListenerContext for EventListenerRegistry {
  fn remove_event_listener(
    &mut self,
    target: EventTargetId,
    type_: &str,
    listener_id: ListenerId,
    capture: bool,
  ) -> bool {
    EventListenerRegistry::remove_event_listener(self, target.normalize(), type_, listener_id, capture)
  }
}

impl EventTargetId {
  fn to_node_id(self) -> Option<NodeId> {
    match self.normalize() {
      EventTargetId::Window => None,
      EventTargetId::Document => Some(NodeId(0)),
      EventTargetId::Node(id) => Some(id),
    }
  }

  fn normalize(self) -> Self {
    match self {
      EventTargetId::Node(NodeId(0)) => EventTargetId::Document,
      other => other,
    }
  }
}

impl Document {
  pub fn add_event_listener(
    &mut self,
    target: EventTargetId,
    type_: &str,
    listener_id: ListenerId,
    options: EventListenerOptions,
  ) -> bool {
    self
      .events
      .add_event_listener(target.normalize(), type_, listener_id, options)
  }

  pub fn remove_event_listener(
    &mut self,
    target: EventTargetId,
    type_: &str,
    listener_id: ListenerId,
    capture: bool,
  ) -> bool {
    self
      .events
      .remove_event_listener(target.normalize(), type_, listener_id, capture)
  }

  pub fn dispatch_event(
    &mut self,
    target: EventTargetId,
    event: &mut Event,
    invoker: &mut dyn EventListenerInvoker,
  ) -> DispatchResult {
    // Build path up-front so we can freely borrow `self.events` mutably during dispatch.
    let path = self.event_path(target.normalize());

    // Reset dispatch-time state.
    event.target = Some(target.normalize());
    event.current_target = None;
    event.event_phase = EventPhase::None;
    event.stop_propagation = false;
    event.stop_immediate_propagation = false;
    event.in_passive_listener = false;

    // Capturing: root -> parent of target
    if path.len() > 1 {
      event.event_phase = EventPhase::CapturingPhase;
      // `path` is `target -> ... -> root`. Reverse it to walk `root -> ... -> target`, then exclude
      // the target itself.
      for &current in path.iter().rev().take(path.len() - 1) {
        event.current_target = Some(current);
        self.invoke_listeners(current, event, invoker, /* capture */ true)?;
        if event.stop_propagation {
          break;
        }
      }
    }

    // If propagation was stopped during capture, the event never reaches the target.
    if event.stop_propagation {
      event.current_target = None;
      event.event_phase = EventPhase::None;
      return Ok(!event.default_prevented);
    }

    // At-target: capture listeners, then bubble listeners.
    event.event_phase = EventPhase::AtTarget;
    let target_id = *path.first().expect("path must contain at least the target");
    event.current_target = Some(target_id);
    self.invoke_listeners(target_id, event, invoker, /* capture */ true)?;

    // `stopPropagation()` does not stop listeners on the current target; only
    // `stopImmediatePropagation()` does.
    if !event.stop_immediate_propagation {
      self.invoke_listeners(target_id, event, invoker, /* capture */ false)?;
    }

    // Bubbling: parent of target -> root (only if event.bubbles)
    if event.bubbles && !event.stop_propagation && path.len() > 1 {
      event.event_phase = EventPhase::BubblingPhase;
      for &current in path.iter().skip(1) {
        event.current_target = Some(current);
        self.invoke_listeners(current, event, invoker, /* capture */ false)?;
        if event.stop_propagation {
          break;
        }
      }
    }

    event.current_target = None;
    event.event_phase = EventPhase::None;
    Ok(!event.default_prevented)
  }

  fn event_path(&self, target: EventTargetId) -> Vec<EventTargetId> {
    // MVP: composed path == ancestor chain (no Shadow DOM retargeting).
    //
    // Path is from target up to root.
    let target = target.normalize();
    if target == EventTargetId::Window {
      return vec![EventTargetId::Window];
    }

    let mut path = Vec::new();
    let mut current = target;
    loop {
      let normalized = current.normalize();
      path.push(normalized);

      if normalized == EventTargetId::Document {
        break;
      }

      let Some(node_id) = normalized.to_node_id() else {
        break;
      };
      let parent = self.node(node_id).parent;
      let Some(parent_id) = parent else {
        break;
      };

      current = if parent_id == NodeId(0) {
        EventTargetId::Document
      } else {
        EventTargetId::Node(parent_id)
      };
    }

    // DOM's event path is rooted at Window.
    path.push(EventTargetId::Window);
    path
  }

  fn invoke_listeners(
    &mut self,
    target: EventTargetId,
    event: &mut Event,
    invoker: &mut dyn EventListenerInvoker,
    capture: bool,
  ) -> Result<()> {
    let listeners = self.events.listeners_snapshot(target, &event.type_);
    for listener in listeners {
      if event.stop_immediate_propagation {
        break;
      }
      if listener.options.capture != capture {
        continue;
      }

      // Honor removals that occur during dispatch (DOM clones the listener list, but retains shared
      // "removed" state).
      if !self
        .events
        .listener_record_registered(target, &event.type_, listener.record_id)
      {
        continue;
      }

      if listener.options.once {
        self.events.remove_event_listener(
          target,
          &event.type_,
          listener.id,
          listener.options.capture,
        );
      }

      let prev_passive = event.in_passive_listener;
      event.in_passive_listener = listener.options.passive;
      let res = invoker.invoke(listener.id, event, &mut self.events);
      event.in_passive_listener = prev_passive;
      res?;
    }
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use selectors::context::QuirksMode;

  use crate::dom2::NodeKind;

  #[derive(Debug, Clone, Copy)]
  enum Action {
    None,
    StopPropagation,
    StopImmediatePropagation,
    PreventDefault,
  }

  #[derive(Debug, Clone, Copy)]
  struct Behavior {
    label: &'static str,
    action: Action,
    expected_phase: Option<EventPhase>,
    expected_target: Option<EventTargetId>,
    expected_current_target: Option<EventTargetId>,
  }

  impl Behavior {
    fn new(label: &'static str, action: Action) -> Self {
      Self {
        label,
        action,
        expected_phase: None,
        expected_target: None,
        expected_current_target: None,
      }
    }

    fn with_expectations(
      mut self,
      expected_phase: EventPhase,
      expected_target: EventTargetId,
      expected_current_target: EventTargetId,
    ) -> Self {
      self.expected_phase = Some(expected_phase);
      self.expected_target = Some(expected_target);
      self.expected_current_target = Some(expected_current_target);
      self
    }
  }

  struct RecordingInvoker {
    calls: Vec<&'static str>,
    behaviors: HashMap<ListenerId, Behavior>,
  }

  impl RecordingInvoker {
    fn new(behaviors: impl IntoIterator<Item = (ListenerId, Behavior)>) -> Self {
      Self {
        calls: Vec::new(),
        behaviors: behaviors.into_iter().collect(),
      }
    }
  }

  impl EventListenerInvoker for RecordingInvoker {
    fn invoke(
      &mut self,
      listener_id: ListenerId,
      event: &mut Event,
      _ctx: &mut dyn EventListenerContext,
    ) -> Result<()> {
      let behavior = *self
        .behaviors
        .get(&listener_id)
        .unwrap_or_else(|| panic!("unknown listener_id: {listener_id:?}"));
      if let Some(expected) = behavior.expected_phase {
        assert_eq!(event.event_phase, expected);
      }
      if let Some(expected) = behavior.expected_target {
        assert_eq!(event.target, Some(expected));
      }
      if let Some(expected) = behavior.expected_current_target {
        assert_eq!(event.current_target, Some(expected));
      }
      self.calls.push(behavior.label);
      match behavior.action {
        Action::None => {}
        Action::StopPropagation => event.stop_propagation(),
        Action::StopImmediatePropagation => event.stop_immediate_propagation(),
        Action::PreventDefault => event.prevent_default(),
      }
      Ok(())
    }
  }

  fn make_three_level_doc() -> (
    Document,
    EventTargetId,
    EventTargetId,
    EventTargetId,
    EventTargetId,
  ) {
    let mut doc = Document::new(QuirksMode::NoQuirks);
    let root = doc.root();
    let parent = doc.push_node(
      NodeKind::Element {
        tag_name: "div".to_string(),
        namespace: "".to_string(),
        attributes: Vec::new(),
      },
      Some(root),
      /* inert_subtree */ false,
    );
    let target = doc.push_node(
      NodeKind::Element {
        tag_name: "span".to_string(),
        namespace: "".to_string(),
        attributes: Vec::new(),
      },
      Some(parent),
      /* inert_subtree */ false,
    );

    (
      doc,
      EventTargetId::Window,
      EventTargetId::Document,
      EventTargetId::Node(parent),
      EventTargetId::Node(target),
    )
  }

  #[test]
  fn capture_order_vs_bubble_order_on_three_level_tree() {
    let (mut doc, window_id, doc_id, parent_id, target_id) = make_three_level_doc();

    let mut invoker = RecordingInvoker::new([
      (
        ListenerId(1),
        Behavior::new("window-capture", Action::None).with_expectations(
          EventPhase::CapturingPhase,
          target_id,
          window_id,
        ),
      ),
      (
        ListenerId(2),
        Behavior::new("doc-capture", Action::None).with_expectations(
          EventPhase::CapturingPhase,
          target_id,
          doc_id,
        ),
      ),
      (
        ListenerId(3),
        Behavior::new("parent-capture", Action::None).with_expectations(
          EventPhase::CapturingPhase,
          target_id,
          parent_id,
        ),
      ),
      (
        ListenerId(4),
        Behavior::new("target-capture", Action::None)
          .with_expectations(EventPhase::AtTarget, target_id, target_id),
      ),
      (
        ListenerId(5),
        Behavior::new("target-bubble", Action::None)
          .with_expectations(EventPhase::AtTarget, target_id, target_id),
      ),
      (
        ListenerId(6),
        Behavior::new("parent-bubble", Action::None).with_expectations(
          EventPhase::BubblingPhase,
          target_id,
          parent_id,
        ),
      ),
      (
        ListenerId(7),
        Behavior::new("doc-bubble", Action::None).with_expectations(
          EventPhase::BubblingPhase,
          target_id,
          doc_id,
        ),
      ),
      (
        ListenerId(8),
        Behavior::new("window-bubble", Action::None).with_expectations(
          EventPhase::BubblingPhase,
          target_id,
          window_id,
        ),
      ),
    ]);

    let type_ = "test";
    assert!(doc.add_event_listener(
      window_id,
      type_,
      ListenerId(1),
      EventListenerOptions {
        capture: true,
        ..Default::default()
      }
    ));
    assert!(doc.add_event_listener(
      doc_id,
      type_,
      ListenerId(2),
      EventListenerOptions {
        capture: true,
        ..Default::default()
      }
    ));
    assert!(doc.add_event_listener(
      parent_id,
      type_,
      ListenerId(3),
      EventListenerOptions {
        capture: true,
        ..Default::default()
      }
    ));
    assert!(doc.add_event_listener(
      target_id,
      type_,
      ListenerId(4),
      EventListenerOptions {
        capture: true,
        ..Default::default()
      }
    ));

    assert!(doc.add_event_listener(
      target_id,
      type_,
      ListenerId(5),
      EventListenerOptions::default()
    ));
    assert!(doc.add_event_listener(
      parent_id,
      type_,
      ListenerId(6),
      EventListenerOptions::default()
    ));
    assert!(doc.add_event_listener(
      doc_id,
      type_,
      ListenerId(7),
      EventListenerOptions::default()
    ));
    assert!(doc.add_event_listener(
      window_id,
      type_,
      ListenerId(8),
      EventListenerOptions::default()
    ));

    let mut event = Event::new(
      type_,
      EventInit {
        bubbles: true,
        ..Default::default()
      },
    );

    doc
      .dispatch_event(target_id, &mut event, &mut invoker)
      .unwrap();
    assert_eq!(event.event_phase, EventPhase::None);
    assert_eq!(event.current_target, None);

    assert_eq!(
      invoker.calls,
      vec![
        "window-capture",
        "doc-capture",
        "parent-capture",
        "target-capture",
        "target-bubble",
        "parent-bubble",
        "doc-bubble",
        "window-bubble"
      ]
    );
  }

  #[test]
  fn stop_propagation_stops_moving_to_next_target() {
    let (mut doc, _window_id, doc_id, parent_id, target_id) = make_three_level_doc();

    let mut invoker = RecordingInvoker::new([
      (
        ListenerId(1),
        Behavior::new("target-stop", Action::StopPropagation),
      ),
      (
        ListenerId(2),
        Behavior::new("parent-bubble", Action::None),
      ),
      (
        ListenerId(3),
        Behavior::new("doc-bubble", Action::None),
      ),
    ]);

    let type_ = "test";
    doc.add_event_listener(
      target_id,
      type_,
      ListenerId(1),
      EventListenerOptions::default(),
    );
    doc.add_event_listener(
      parent_id,
      type_,
      ListenerId(2),
      EventListenerOptions::default(),
    );
    doc.add_event_listener(
      doc_id,
      type_,
      ListenerId(3),
      EventListenerOptions::default(),
    );

    let mut event = Event::new(
      type_,
      EventInit {
        bubbles: true,
        ..Default::default()
      },
    );

    doc
      .dispatch_event(target_id, &mut event, &mut invoker)
      .unwrap();

    assert_eq!(invoker.calls, vec!["target-stop"]);
  }

  #[test]
  fn stop_propagation_in_target_capture_stops_ancestors_but_not_target_listeners() {
    let (mut doc, _window_id, _doc_id, _parent_id, target_id) = make_three_level_doc();

    let mut invoker = RecordingInvoker::new([
      (
        ListenerId(1),
        Behavior::new("target-capture-stop", Action::StopPropagation),
      ),
      (
        ListenerId(2),
        Behavior::new("target-capture-2", Action::None),
      ),
      (
        ListenerId(3),
        Behavior::new("target-bubble", Action::None),
      ),
    ]);

    let type_ = "test";
    doc.add_event_listener(
      target_id,
      type_,
      ListenerId(1),
      EventListenerOptions {
        capture: true,
        ..Default::default()
      },
    );
    doc.add_event_listener(
      target_id,
      type_,
      ListenerId(2),
      EventListenerOptions {
        capture: true,
        ..Default::default()
      },
    );
    doc.add_event_listener(
      target_id,
      type_,
      ListenerId(3),
      EventListenerOptions::default(),
    );

    let mut event = Event::new(
      type_,
      EventInit {
        bubbles: true,
        ..Default::default()
      },
    );

    doc
      .dispatch_event(target_id, &mut event, &mut invoker)
      .unwrap();

    assert_eq!(
      invoker.calls,
      vec!["target-capture-stop", "target-capture-2", "target-bubble"]
    );
  }

  #[test]
  fn stop_immediate_propagation_stops_later_listeners_on_same_target() {
    let (mut doc, _window_id, _doc_id, parent_id, target_id) = make_three_level_doc();

    let mut invoker = RecordingInvoker::new([
      (
        ListenerId(1),
        Behavior::new("target-1", Action::StopImmediatePropagation),
      ),
      (
        ListenerId(2),
        Behavior::new("target-2", Action::None),
      ),
      (
        ListenerId(3),
        Behavior::new("parent-bubble", Action::None),
      ),
    ]);

    let type_ = "test";
    doc.add_event_listener(
      target_id,
      type_,
      ListenerId(1),
      EventListenerOptions::default(),
    );
    doc.add_event_listener(
      target_id,
      type_,
      ListenerId(2),
      EventListenerOptions::default(),
    );
    doc.add_event_listener(
      parent_id,
      type_,
      ListenerId(3),
      EventListenerOptions::default(),
    );

    let mut event = Event::new(
      type_,
      EventInit {
        bubbles: true,
        ..Default::default()
      },
    );

    doc
      .dispatch_event(target_id, &mut event, &mut invoker)
      .unwrap();

    assert_eq!(invoker.calls, vec!["target-1"]);
  }

  #[test]
  fn once_listeners_removed_after_first_dispatch() {
    let (mut doc, _window_id, _doc_id, _parent_id, target_id) = make_three_level_doc();

    let mut invoker = RecordingInvoker::new([(
      ListenerId(1),
      Behavior::new("once", Action::None),
    )]);

    let type_ = "test";
    doc.add_event_listener(
      target_id,
      type_,
      ListenerId(1),
      EventListenerOptions {
        once: true,
        ..Default::default()
      },
    );

    let mut event = Event::new(
      type_,
      EventInit {
        bubbles: true,
        ..Default::default()
      },
    );
    doc
      .dispatch_event(target_id, &mut event, &mut invoker)
      .unwrap();

    let mut event2 = Event::new(
      type_,
      EventInit {
        bubbles: true,
        ..Default::default()
      },
    );
    doc
      .dispatch_event(target_id, &mut event2, &mut invoker)
      .unwrap();

    assert_eq!(invoker.calls, vec!["once"]);
  }

  #[test]
  fn remove_event_listener_prevents_invocation() {
    let (mut doc, _window_id, _doc_id, _parent_id, target_id) = make_three_level_doc();

    let mut invoker = RecordingInvoker::new([(
      ListenerId(1),
      Behavior::new("should-not-run", Action::None),
    )]);

    let type_ = "test";
    doc.add_event_listener(
      target_id,
      type_,
      ListenerId(1),
      EventListenerOptions::default(),
    );
    assert!(doc.remove_event_listener(target_id, type_, ListenerId(1), false));

    let mut event = Event::new(
      type_,
      EventInit {
        bubbles: true,
        ..Default::default()
      },
    );
    doc
      .dispatch_event(target_id, &mut event, &mut invoker)
      .unwrap();

    assert!(invoker.calls.is_empty());
  }

  #[test]
  fn passive_listeners_cannot_set_default_prevented() {
    let (mut doc, _window_id, _doc_id, _parent_id, target_id) = make_three_level_doc();

    let mut invoker = RecordingInvoker::new([(
      ListenerId(1),
      Behavior::new("passive", Action::PreventDefault),
    )]);

    let type_ = "test";
    doc.add_event_listener(
      target_id,
      type_,
      ListenerId(1),
      EventListenerOptions {
        passive: true,
        ..Default::default()
      },
    );

    let mut event = Event::new(
      type_,
      EventInit {
        bubbles: true,
        cancelable: true,
        ..Default::default()
      },
    );
    let res = doc
      .dispatch_event(target_id, &mut event, &mut invoker)
      .unwrap();
    assert!(res, "dispatchEvent should return true if not canceled");
    assert!(
      !event.default_prevented,
      "passive listeners must not set defaultPrevented"
    );
  }

  #[test]
  fn event_phase_discriminants_match_dom() {
    assert_eq!(EventPhase::None as u8, 0);
    assert_eq!(EventPhase::CapturingPhase as u8, 1);
    assert_eq!(EventPhase::AtTarget as u8, 2);
    assert_eq!(EventPhase::BubblingPhase as u8, 3);
  }

  #[test]
  fn remove_event_listener_during_dispatch_prevents_future_invocation() {
    let (mut doc, _window_id, _doc_id, _parent_id, target_id) = make_three_level_doc();

    struct RemovingInvoker {
      calls: Vec<&'static str>,
      remove_id: ListenerId,
      target: EventTargetId,
      removal_attempts: usize,
    }

    impl EventListenerInvoker for RemovingInvoker {
      fn invoke(
        &mut self,
        listener_id: ListenerId,
        _event: &mut Event,
        ctx: &mut dyn EventListenerContext,
      ) -> Result<()> {
        if listener_id == ListenerId(1) {
          self.calls.push("l1");
          let removed = ctx.remove_event_listener(self.target, "test", self.remove_id, false);
          if self.removal_attempts == 0 {
            assert!(removed, "expected first removal to succeed");
          } else {
            assert!(!removed, "expected subsequent removals to be no-ops");
          }
          self.removal_attempts += 1;
        } else if listener_id == self.remove_id {
          self.calls.push("l2");
        } else {
          panic!("unexpected listener: {listener_id:?}");
        }
        Ok(())
      }
    }

    doc.add_event_listener(
      target_id,
      "test",
      ListenerId(1),
      EventListenerOptions::default(),
    );
    doc.add_event_listener(
      target_id,
      "test",
      ListenerId(2),
      EventListenerOptions::default(),
    );

    let mut invoker = RemovingInvoker {
      calls: Vec::new(),
      remove_id: ListenerId(2),
      target: target_id,
      removal_attempts: 0,
    };

    let mut event = Event::new(
      "test",
      EventInit {
        bubbles: true,
        ..Default::default()
      },
    );
    doc
      .dispatch_event(target_id, &mut event, &mut invoker)
      .unwrap();

    let mut event2 = Event::new("test", EventInit::default());
    doc
      .dispatch_event(target_id, &mut event2, &mut invoker)
      .unwrap();

    assert_eq!(invoker.calls, vec!["l1", "l1"]);
  }
}
