use super::*;
use crate::dom::{DomNode, DomNodeType};
use crate::dom2::{Document, NodeId};
use selectors::context::QuirksMode;
use std::collections::HashMap;

fn element(tag_name: &str, children: Vec<DomNode>) -> DomNode {
  DomNode {
    node_type: DomNodeType::Element {
      tag_name: tag_name.to_string(),
      namespace: String::new(),
      attributes: Vec::new(),
    },
    children,
  }
}

fn make_dom_abc() -> (Document, NodeId, NodeId, NodeId) {
  // Document → <a> → <b> → <c>
  let root = DomNode {
    node_type: DomNodeType::Document {
      quirks_mode: QuirksMode::NoQuirks,
    },
    children: vec![element("a", vec![element("b", vec![element("c", vec![])])])],
  };
  let doc = Document::from_renderer_dom(&root);

  // `dom2::NodeId` is opaque (constructor is private); use the known tree shape to grab IDs.
  let root_id = doc.root();
  let a = doc.node(root_id).children[0];
  let b = doc.node(a).children[0];
  let c = doc.node(b).children[0];
  (doc, a, b, c)
}

#[derive(Debug, Clone, Copy)]
enum Action {
  None,
  StopPropagation,
  StopImmediatePropagation,
  RemoveListener {
    target: EventTargetId,
    type_: &'static str,
    listener_id: ListenerId,
    capture: bool,
    expect_removed: Option<bool>,
  },
}

#[derive(Debug, Clone, Copy)]
struct Behavior {
  label: &'static str,
  expected_phase: EventPhase,
  expected_current_target: EventTargetId,
  action: Action,
}

struct RecordingInvoker<'a> {
  registry: &'a EventListenerRegistry,
  dispatch_target: EventTargetId,
  calls: Vec<&'static str>,
  behaviors: HashMap<ListenerId, Behavior>,
}

impl<'a> RecordingInvoker<'a> {
  fn new(
    registry: &'a EventListenerRegistry,
    dispatch_target: EventTargetId,
    behaviors: impl IntoIterator<Item = (ListenerId, Behavior)>,
  ) -> Self {
    Self {
      registry,
      dispatch_target,
      calls: Vec::new(),
      behaviors: behaviors.into_iter().collect(),
    }
  }
}

impl EventListenerInvoker for RecordingInvoker<'_> {
  fn invoke(&mut self, listener_id: ListenerId, event: &mut Event) -> Result<(), DomError> {
    let behavior = *self
      .behaviors
      .get(&listener_id)
      .unwrap_or_else(|| panic!("unknown listener_id: {listener_id:?}"));
    assert_eq!(event.target, Some(self.dispatch_target));
    assert_eq!(event.current_target, Some(behavior.expected_current_target));
    assert_eq!(event.event_phase, behavior.expected_phase);

    self.calls.push(behavior.label);

    match behavior.action {
      Action::None => {}
      Action::StopPropagation => event.stop_propagation(),
      Action::StopImmediatePropagation => event.stop_immediate_propagation(),
      Action::RemoveListener {
        target,
        type_,
        listener_id,
        capture,
        expect_removed,
      } => {
        let removed = self
          .registry
          .remove_event_listener(target, type_, listener_id, capture);
        if let Some(expect_removed) = expect_removed {
          assert_eq!(removed, expect_removed);
        }
      }
    }

    Ok(())
  }
}

#[test]
fn capture_and_bubble_ordering_across_tree() {
  let (doc, a, b, c) = make_dom_abc();
  let registry = EventListenerRegistry::new();

  let type_ = "x";

  let id_window_capture = ListenerId::new(1);
  let id_document_capture = ListenerId::new(2);
  let id_a_capture = ListenerId::new(3);
  let id_b_capture = ListenerId::new(4);
  let id_c_capture = ListenerId::new(5);

  let id_c_bubble = ListenerId::new(6);
  let id_b_bubble = ListenerId::new(7);
  let id_a_bubble = ListenerId::new(8);
  let id_document_bubble = ListenerId::new(9);
  let id_window_bubble = ListenerId::new(10);

  assert!(registry.add_event_listener(
    EventTargetId::Window,
    type_,
    id_window_capture,
    AddEventListenerOptions {
      capture: true,
      ..Default::default()
    }
  ));
  assert!(registry.add_event_listener(
    EventTargetId::Document,
    type_,
    id_document_capture,
    AddEventListenerOptions {
      capture: true,
      ..Default::default()
    }
  ));
  assert!(registry.add_event_listener(
    EventTargetId::Node(a),
    type_,
    id_a_capture,
    AddEventListenerOptions {
      capture: true,
      ..Default::default()
    }
  ));
  assert!(registry.add_event_listener(
    EventTargetId::Node(b),
    type_,
    id_b_capture,
    AddEventListenerOptions {
      capture: true,
      ..Default::default()
    }
  ));
  assert!(registry.add_event_listener(
    EventTargetId::Node(c),
    type_,
    id_c_capture,
    AddEventListenerOptions {
      capture: true,
      ..Default::default()
    }
  ));

  assert!(registry.add_event_listener(
    EventTargetId::Node(c),
    type_,
    id_c_bubble,
    AddEventListenerOptions::default()
  ));
  assert!(registry.add_event_listener(
    EventTargetId::Node(b),
    type_,
    id_b_bubble,
    AddEventListenerOptions::default()
  ));
  assert!(registry.add_event_listener(
    EventTargetId::Node(a),
    type_,
    id_a_bubble,
    AddEventListenerOptions::default()
  ));
  assert!(registry.add_event_listener(
    EventTargetId::Document,
    type_,
    id_document_bubble,
    AddEventListenerOptions::default()
  ));
  assert!(registry.add_event_listener(
    EventTargetId::Window,
    type_,
    id_window_bubble,
    AddEventListenerOptions::default()
  ));

  let mut invoker = RecordingInvoker::new(
    &registry,
    EventTargetId::Node(c),
    [
      (
        id_window_capture,
        Behavior {
          label: "window_capture",
          expected_phase: EventPhase::Capturing,
          expected_current_target: EventTargetId::Window,
          action: Action::None,
        },
      ),
      (
        id_document_capture,
        Behavior {
          label: "document_capture",
          expected_phase: EventPhase::Capturing,
          expected_current_target: EventTargetId::Document,
          action: Action::None,
        },
      ),
      (
        id_a_capture,
        Behavior {
          label: "a_capture",
          expected_phase: EventPhase::Capturing,
          expected_current_target: EventTargetId::Node(a),
          action: Action::None,
        },
      ),
      (
        id_b_capture,
        Behavior {
          label: "b_capture",
          expected_phase: EventPhase::Capturing,
          expected_current_target: EventTargetId::Node(b),
          action: Action::None,
        },
      ),
      (
        id_c_capture,
        Behavior {
          label: "c_capture",
          expected_phase: EventPhase::AtTarget,
          expected_current_target: EventTargetId::Node(c),
          action: Action::None,
        },
      ),
      (
        id_c_bubble,
        Behavior {
          label: "c_bubble",
          expected_phase: EventPhase::AtTarget,
          expected_current_target: EventTargetId::Node(c),
          action: Action::None,
        },
      ),
      (
        id_b_bubble,
        Behavior {
          label: "b_bubble",
          expected_phase: EventPhase::Bubbling,
          expected_current_target: EventTargetId::Node(b),
          action: Action::None,
        },
      ),
      (
        id_a_bubble,
        Behavior {
          label: "a_bubble",
          expected_phase: EventPhase::Bubbling,
          expected_current_target: EventTargetId::Node(a),
          action: Action::None,
        },
      ),
      (
        id_document_bubble,
        Behavior {
          label: "document_bubble",
          expected_phase: EventPhase::Bubbling,
          expected_current_target: EventTargetId::Document,
          action: Action::None,
        },
      ),
      (
        id_window_bubble,
        Behavior {
          label: "window_bubble",
          expected_phase: EventPhase::Bubbling,
          expected_current_target: EventTargetId::Window,
          action: Action::None,
        },
      ),
    ],
  );

  let mut event = Event::new(
    type_,
    EventInit {
      bubbles: true,
      ..Default::default()
    },
  );
  assert!(
    dispatch_event(
      EventTargetId::Node(c),
      &mut event,
      &doc,
      &registry,
      &mut invoker
    )
    .unwrap()
  );

  assert_eq!(
    invoker.calls.as_slice(),
    &[
      "window_capture",
      "document_capture",
      "a_capture",
      "b_capture",
      "c_capture",
      "c_bubble",
      "b_bubble",
      "a_bubble",
      "document_bubble",
      "window_bubble"
    ]
  );
}

#[test]
fn stop_propagation_prevents_subsequent_targets() {
  let (doc, a, b, c) = make_dom_abc();
  let registry = EventListenerRegistry::new();

  let type_ = "x";

  let id_stop = ListenerId::new(1);
  let id_a = ListenerId::new(2);
  let id_document = ListenerId::new(3);
  let id_window = ListenerId::new(4);

  assert!(registry.add_event_listener(
    EventTargetId::Node(b),
    type_,
    id_stop,
    AddEventListenerOptions::default()
  ));
  assert!(registry.add_event_listener(
    EventTargetId::Node(a),
    type_,
    id_a,
    AddEventListenerOptions::default()
  ));
  assert!(registry.add_event_listener(
    EventTargetId::Document,
    type_,
    id_document,
    AddEventListenerOptions::default()
  ));
  assert!(registry.add_event_listener(
    EventTargetId::Window,
    type_,
    id_window,
    AddEventListenerOptions::default()
  ));

  let mut invoker = RecordingInvoker::new(
    &registry,
    EventTargetId::Node(c),
    [
      (
        id_stop,
        Behavior {
          label: "b_bubble_stop",
          expected_phase: EventPhase::Bubbling,
          expected_current_target: EventTargetId::Node(b),
          action: Action::StopPropagation,
        },
      ),
      (
        id_a,
        Behavior {
          label: "a_bubble",
          expected_phase: EventPhase::Bubbling,
          expected_current_target: EventTargetId::Node(a),
          action: Action::None,
        },
      ),
      (
        id_document,
        Behavior {
          label: "document_bubble",
          expected_phase: EventPhase::Bubbling,
          expected_current_target: EventTargetId::Document,
          action: Action::None,
        },
      ),
      (
        id_window,
        Behavior {
          label: "window_bubble",
          expected_phase: EventPhase::Bubbling,
          expected_current_target: EventTargetId::Window,
          action: Action::None,
        },
      ),
    ],
  );

  let mut event = Event::new(
    type_,
    EventInit {
      bubbles: true,
      ..Default::default()
    },
  );
  dispatch_event(
    EventTargetId::Node(c),
    &mut event,
    &doc,
    &registry,
    &mut invoker,
  )
  .unwrap();

  assert_eq!(invoker.calls.as_slice(), &["b_bubble_stop"]);
}

#[test]
fn stop_immediate_propagation_stops_other_listeners_on_same_target() {
  let (doc, _a, b, c) = make_dom_abc();
  let registry = EventListenerRegistry::new();

  let type_ = "x";

  let id_first = ListenerId::new(1);
  let id_second = ListenerId::new(2);
  let id_parent = ListenerId::new(3);

  assert!(registry.add_event_listener(
    EventTargetId::Node(c),
    type_,
    id_first,
    AddEventListenerOptions::default()
  ));
  assert!(registry.add_event_listener(
    EventTargetId::Node(c),
    type_,
    id_second,
    AddEventListenerOptions::default()
  ));
  assert!(registry.add_event_listener(
    EventTargetId::Node(b),
    type_,
    id_parent,
    AddEventListenerOptions::default()
  ));

  let mut invoker = RecordingInvoker::new(
    &registry,
    EventTargetId::Node(c),
    [
      (
        id_first,
        Behavior {
          label: "first",
          expected_phase: EventPhase::AtTarget,
          expected_current_target: EventTargetId::Node(c),
          action: Action::StopImmediatePropagation,
        },
      ),
      (
        id_second,
        Behavior {
          label: "second",
          expected_phase: EventPhase::AtTarget,
          expected_current_target: EventTargetId::Node(c),
          action: Action::None,
        },
      ),
      (
        id_parent,
        Behavior {
          label: "parent_bubble",
          expected_phase: EventPhase::Bubbling,
          expected_current_target: EventTargetId::Node(b),
          action: Action::None,
        },
      ),
    ],
  );

  let mut event = Event::new(
    type_,
    EventInit {
      bubbles: true,
      ..Default::default()
    },
  );
  dispatch_event(
    EventTargetId::Node(c),
    &mut event,
    &doc,
    &registry,
    &mut invoker,
  )
  .unwrap();

  assert_eq!(invoker.calls.as_slice(), &["first"]);
}

#[test]
fn once_listeners_only_fire_once() {
  let (doc, _a, _b, c) = make_dom_abc();
  let registry = EventListenerRegistry::new();
  let type_ = "x";

  let id_once = ListenerId::new(1);
  assert!(registry.add_event_listener(
    EventTargetId::Node(c),
    type_,
    id_once,
    AddEventListenerOptions {
      once: true,
      ..Default::default()
    },
  ));

  let mut invoker = RecordingInvoker::new(
    &registry,
    EventTargetId::Node(c),
    [(
      id_once,
      Behavior {
        label: "once",
        expected_phase: EventPhase::AtTarget,
        expected_current_target: EventTargetId::Node(c),
        action: Action::RemoveListener {
          target: EventTargetId::Node(c),
          type_,
          listener_id: id_once,
          capture: false,
          expect_removed: Some(false),
        },
      },
    )],
  );

  for _ in 0..2 {
    let mut event = Event::new(type_, EventInit::default());
    dispatch_event(
      EventTargetId::Node(c),
      &mut event,
      &doc,
      &registry,
      &mut invoker,
    )
    .unwrap();
  }

  assert_eq!(invoker.calls.as_slice(), &["once"]);
}

#[test]
fn remove_event_listener_during_dispatch_prevents_later_listener_in_current_dispatch() {
  let (doc, _a, _b, c) = make_dom_abc();
  let registry = EventListenerRegistry::new();

  let type_ = "x";
  let id1 = ListenerId::new(1);
  let id2 = ListenerId::new(2);

  // Listener that removes `l2` while dispatch is in progress.
  assert!(registry.add_event_listener(
    EventTargetId::Node(c),
    type_,
    id1,
    AddEventListenerOptions::default()
  ));
  assert!(registry.add_event_listener(
    EventTargetId::Node(c),
    type_,
    id2,
    AddEventListenerOptions::default()
  ));

  let mut invoker = RecordingInvoker::new(
    &registry,
    EventTargetId::Node(c),
    [
      (
        id1,
        Behavior {
          label: "l1",
          expected_phase: EventPhase::AtTarget,
          expected_current_target: EventTargetId::Node(c),
          action: Action::RemoveListener {
            target: EventTargetId::Node(c),
            type_,
            listener_id: id2,
            capture: false,
            expect_removed: None,
          },
        },
      ),
      (
        id2,
        Behavior {
          label: "l2",
          expected_phase: EventPhase::AtTarget,
          expected_current_target: EventTargetId::Node(c),
          action: Action::None,
        },
      ),
    ],
  );

  let mut event = Event::new(type_, EventInit::default());
  dispatch_event(
    EventTargetId::Node(c),
    &mut event,
    &doc,
    &registry,
    &mut invoker,
  )
  .unwrap();

  let mut event = Event::new(type_, EventInit::default());
  dispatch_event(
    EventTargetId::Node(c),
    &mut event,
    &doc,
    &registry,
    &mut invoker,
  )
  .unwrap();

  assert_eq!(invoker.calls.as_slice(), &["l1", "l1"]);
}

#[test]
fn stop_propagation_in_target_capture_skips_target_bubble_listeners() {
  let (doc, _a, _b, c) = make_dom_abc();
  let registry = EventListenerRegistry::new();

  let type_ = "x";
  let id_capture_1 = ListenerId::new(1);
  let id_capture_2 = ListenerId::new(2);
  let id_bubble = ListenerId::new(3);

  assert!(registry.add_event_listener(
    EventTargetId::Node(c),
    type_,
    id_capture_1,
    AddEventListenerOptions {
      capture: true,
      ..Default::default()
    }
  ));
  assert!(registry.add_event_listener(
    EventTargetId::Node(c),
    type_,
    id_capture_2,
    AddEventListenerOptions {
      capture: true,
      ..Default::default()
    }
  ));
  assert!(registry.add_event_listener(
    EventTargetId::Node(c),
    type_,
    id_bubble,
    AddEventListenerOptions::default()
  ));

  let mut invoker = RecordingInvoker::new(
    &registry,
    EventTargetId::Node(c),
    [
      (
        id_capture_1,
        Behavior {
          label: "capture_stop",
          expected_phase: EventPhase::AtTarget,
          expected_current_target: EventTargetId::Node(c),
          action: Action::StopPropagation,
        },
      ),
      (
        id_capture_2,
        Behavior {
          label: "capture_2",
          expected_phase: EventPhase::AtTarget,
          expected_current_target: EventTargetId::Node(c),
          action: Action::None,
        },
      ),
      (
        id_bubble,
        Behavior {
          label: "bubble",
          expected_phase: EventPhase::AtTarget,
          expected_current_target: EventTargetId::Node(c),
          action: Action::None,
        },
      ),
    ],
  );

  let mut event = Event::new(
    type_,
    EventInit {
      bubbles: true,
      ..Default::default()
    },
  );
  dispatch_event(
    EventTargetId::Node(c),
    &mut event,
    &doc,
    &registry,
    &mut invoker,
  )
  .unwrap();

  assert_eq!(invoker.calls.as_slice(), &["capture_stop", "capture_2"]);
}

#[test]
fn detached_node_event_path_does_not_include_window() {
  let (mut doc, _a, b, detached) = make_dom_abc();
  doc.node_mut(b).children.retain(|&child| child != detached);
  doc.node_mut(detached).parent = None;

  let registry = EventListenerRegistry::new();
  let type_ = "x";
  let id_window = ListenerId::new(1);
  let id_node = ListenerId::new(2);

  assert!(registry.add_event_listener(
    EventTargetId::Window,
    type_,
    id_window,
    AddEventListenerOptions {
      capture: true,
      ..Default::default()
    }
  ));
  assert!(registry.add_event_listener(
    EventTargetId::Node(detached),
    type_,
    id_node,
    AddEventListenerOptions::default()
  ));

  let mut invoker = RecordingInvoker::new(
    &registry,
    EventTargetId::Node(detached),
    [
      (
        id_window,
        Behavior {
          label: "window_capture",
          expected_phase: EventPhase::Capturing,
          expected_current_target: EventTargetId::Window,
          action: Action::None,
        },
      ),
      (
        id_node,
        Behavior {
          label: "node",
          expected_phase: EventPhase::AtTarget,
          expected_current_target: EventTargetId::Node(detached),
          action: Action::None,
        },
      ),
    ],
  );

  let mut event = Event::new(type_, EventInit::default());
  dispatch_event(
    EventTargetId::Node(detached),
    &mut event,
    &doc,
    &registry,
    &mut invoker,
  )
  .unwrap();

  assert_eq!(invoker.calls.as_slice(), &["node"]);
}
