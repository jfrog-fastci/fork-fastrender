use std::collections::HashMap;

use crate::Result;

use super::{Document, NodeId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventPhase {
  CapturingPhase,
  AtTarget,
  BubblingPhase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventTargetId {
  Document,
  Node(NodeId),
  // Future: Window, ShadowRoot, etc.
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
  pub fn new(type_: impl Into<String>) -> Self {
    Self {
      type_: type_.into(),
      // Mirror DOM defaults.
      bubbles: false,
      cancelable: false,
      default_prevented: false,
      // Spec has an explicit NONE phase, but we only model the phases used during dispatch.
      event_phase: EventPhase::AtTarget,
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
  fn invoke(&mut self, listener_id: ListenerId, event: &mut Event) -> Result<()>;
}

/// DOM-like dispatch return value:
/// - `Ok(true)` if `prevent_default` was not successfully called.
/// - `Ok(false)` if the event was cancelable and `prevent_default` was called from a non-passive
///   listener.
pub type DispatchResult = Result<bool>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RegisteredListener {
  id: ListenerId,
  options: EventListenerOptions,
}

#[derive(Debug, Clone, Default)]
pub struct EventListenerRegistry {
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

    listeners.push(RegisteredListener {
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

  fn listener_registered(
    &self,
    target: EventTargetId,
    type_: &str,
    listener_id: ListenerId,
    capture: bool,
  ) -> bool {
    self
      .listeners
      .get(&target)
      .and_then(|by_type| by_type.get(type_))
      .is_some_and(|listeners| {
        listeners
          .iter()
          .any(|l| l.id == listener_id && l.options.capture == capture)
      })
  }
}

impl EventTargetId {
  fn to_node_id(self) -> NodeId {
    match self {
      EventTargetId::Document => NodeId(0),
      EventTargetId::Node(id) => id,
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
      return Ok(!event.default_prevented);
    }

    // At-target: capture listeners, then bubble listeners.
    event.event_phase = EventPhase::AtTarget;
    let target_id = *path.first().expect("path must contain at least the target");
    event.current_target = Some(target_id);
    self.invoke_listeners(target_id, event, invoker, /* capture */ true)?;

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
    Ok(!event.default_prevented)
  }

  fn event_path(&self, target: EventTargetId) -> Vec<EventTargetId> {
    // MVP: composed path == ancestor chain (no Shadow DOM retargeting).
    let mut path = Vec::new();

    // Path is from target up to root.
    let mut current = target;
    loop {
      let normalized = current.normalize();
      path.push(normalized);

      let node_id = normalized.to_node_id();
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
      if !self.events.listener_registered(
        target,
        &event.type_,
        listener.id,
        listener.options.capture,
      ) {
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
      let res = invoker.invoke(listener.id, event);
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
    fn invoke(&mut self, listener_id: ListenerId, event: &mut Event) -> Result<()> {
      let behavior = *self
        .behaviors
        .get(&listener_id)
        .unwrap_or_else(|| panic!("unknown listener_id: {listener_id:?}"));
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

  fn make_three_level_doc() -> (Document, EventTargetId, EventTargetId, EventTargetId) {
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
      EventTargetId::Document,
      EventTargetId::Node(parent),
      EventTargetId::Node(target),
    )
  }

  #[test]
  fn capture_order_vs_bubble_order_on_three_level_tree() {
    let (mut doc, doc_id, parent_id, target_id) = make_three_level_doc();

    let mut invoker = RecordingInvoker::new([
      (
        ListenerId(1),
        Behavior {
          label: "doc-capture",
          action: Action::None,
        },
      ),
      (
        ListenerId(2),
        Behavior {
          label: "parent-capture",
          action: Action::None,
        },
      ),
      (
        ListenerId(3),
        Behavior {
          label: "target-capture",
          action: Action::None,
        },
      ),
      (
        ListenerId(4),
        Behavior {
          label: "target-bubble",
          action: Action::None,
        },
      ),
      (
        ListenerId(5),
        Behavior {
          label: "parent-bubble",
          action: Action::None,
        },
      ),
      (
        ListenerId(6),
        Behavior {
          label: "doc-bubble",
          action: Action::None,
        },
      ),
    ]);

    let type_ = "test";
    assert!(doc.add_event_listener(
      doc_id,
      type_,
      ListenerId(1),
      EventListenerOptions {
        capture: true,
        ..Default::default()
      }
    ));
    assert!(doc.add_event_listener(
      parent_id,
      type_,
      ListenerId(2),
      EventListenerOptions {
        capture: true,
        ..Default::default()
      }
    ));
    assert!(doc.add_event_listener(
      target_id,
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
      EventListenerOptions::default()
    ));
    assert!(doc.add_event_listener(
      parent_id,
      type_,
      ListenerId(5),
      EventListenerOptions::default()
    ));
    assert!(doc.add_event_listener(
      doc_id,
      type_,
      ListenerId(6),
      EventListenerOptions::default()
    ));

    let mut event = Event::new(type_);
    event.bubbles = true;

    doc
      .dispatch_event(target_id, &mut event, &mut invoker)
      .unwrap();

    assert_eq!(
      invoker.calls,
      vec![
        "doc-capture",
        "parent-capture",
        "target-capture",
        "target-bubble",
        "parent-bubble",
        "doc-bubble"
      ]
    );
  }

  #[test]
  fn stop_propagation_stops_moving_to_next_target() {
    let (mut doc, doc_id, parent_id, target_id) = make_three_level_doc();

    let mut invoker = RecordingInvoker::new([
      (
        ListenerId(1),
        Behavior {
          label: "target-stop",
          action: Action::StopPropagation,
        },
      ),
      (
        ListenerId(2),
        Behavior {
          label: "parent-bubble",
          action: Action::None,
        },
      ),
      (
        ListenerId(3),
        Behavior {
          label: "doc-bubble",
          action: Action::None,
        },
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

    let mut event = Event::new(type_);
    event.bubbles = true;

    doc
      .dispatch_event(target_id, &mut event, &mut invoker)
      .unwrap();

    assert_eq!(invoker.calls, vec!["target-stop"]);
  }

  #[test]
  fn stop_immediate_propagation_stops_later_listeners_on_same_target() {
    let (mut doc, _doc_id, parent_id, target_id) = make_three_level_doc();

    let mut invoker = RecordingInvoker::new([
      (
        ListenerId(1),
        Behavior {
          label: "target-1",
          action: Action::StopImmediatePropagation,
        },
      ),
      (
        ListenerId(2),
        Behavior {
          label: "target-2",
          action: Action::None,
        },
      ),
      (
        ListenerId(3),
        Behavior {
          label: "parent-bubble",
          action: Action::None,
        },
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

    let mut event = Event::new(type_);
    event.bubbles = true;

    doc
      .dispatch_event(target_id, &mut event, &mut invoker)
      .unwrap();

    assert_eq!(invoker.calls, vec!["target-1"]);
  }

  #[test]
  fn once_listeners_removed_after_first_dispatch() {
    let (mut doc, _doc_id, _parent_id, target_id) = make_three_level_doc();

    let mut invoker = RecordingInvoker::new([(
      ListenerId(1),
      Behavior {
        label: "once",
        action: Action::None,
      },
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

    let mut event = Event::new(type_);
    event.bubbles = true;
    doc
      .dispatch_event(target_id, &mut event, &mut invoker)
      .unwrap();

    let mut event2 = Event::new(type_);
    event2.bubbles = true;
    doc
      .dispatch_event(target_id, &mut event2, &mut invoker)
      .unwrap();

    assert_eq!(invoker.calls, vec!["once"]);
  }

  #[test]
  fn remove_event_listener_prevents_invocation() {
    let (mut doc, _doc_id, _parent_id, target_id) = make_three_level_doc();

    let mut invoker = RecordingInvoker::new([(
      ListenerId(1),
      Behavior {
        label: "should-not-run",
        action: Action::None,
      },
    )]);

    let type_ = "test";
    doc.add_event_listener(
      target_id,
      type_,
      ListenerId(1),
      EventListenerOptions::default(),
    );
    assert!(doc.remove_event_listener(target_id, type_, ListenerId(1), false));

    let mut event = Event::new(type_);
    event.bubbles = true;
    doc
      .dispatch_event(target_id, &mut event, &mut invoker)
      .unwrap();

    assert!(invoker.calls.is_empty());
  }

  #[test]
  fn passive_listeners_cannot_set_default_prevented() {
    let (mut doc, _doc_id, _parent_id, target_id) = make_three_level_doc();

    let mut invoker = RecordingInvoker::new([(
      ListenerId(1),
      Behavior {
        label: "passive",
        action: Action::PreventDefault,
      },
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

    let mut event = Event::new(type_);
    event.bubbles = true;
    event.cancelable = true;
    let res = doc
      .dispatch_event(target_id, &mut event, &mut invoker)
      .unwrap();
    assert!(res, "dispatchEvent should return true if not canceled");
    assert!(
      !event.default_prevented,
      "passive listeners must not set defaultPrevented"
    );
  }
}
