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
  PreventDefault,
  RemoveListener {
    target: EventTargetId,
    type_: &'static str,
    capture: bool,
    listener_id: ListenerId,
  },
}

#[derive(Debug, Clone, Copy, Default)]
struct Expectations {
  target: Option<EventTargetId>,
  current_target: Option<EventTargetId>,
  event_phase: Option<EventPhase>,
}

#[derive(Debug, Clone, Copy)]
struct Behavior {
  label: &'static str,
  expectations: Expectations,
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
  fn invoke(
    &mut self,
    listener_id: ListenerId,
    event: &mut Event,
    ctx: &mut dyn EventListenerContext,
  ) -> crate::Result<()> {
    let behavior = *self
      .behaviors
      .get(&listener_id)
      .unwrap_or_else(|| panic!("unknown listener_id: {listener_id:?}"));

    self.calls.push(behavior.label);

    if let Some(expected) = behavior.expectations.target {
      assert_eq!(event.target, Some(expected));
    }
    if let Some(expected) = behavior.expectations.current_target {
      assert_eq!(event.current_target, Some(expected));
    }
    if let Some(expected) = behavior.expectations.event_phase {
      assert_eq!(event.event_phase, expected);
    }

    match behavior.action {
      Action::None => {}
      Action::StopPropagation => event.stop_propagation(),
      Action::StopImmediatePropagation => event.stop_immediate_propagation(),
      Action::PreventDefault => event.prevent_default(),
      Action::RemoveListener {
        target,
        type_,
        capture,
        listener_id,
      } => {
        ctx.remove_event_listener(target, type_, listener_id, capture);
      }
    }
    Ok(())
  }
}

#[test]
fn capture_and_bubble_ordering_across_tree() {
  let (mut doc, a, b, c) = make_dom_abc();

  let type_ = "x";

  let id_window_capture = ListenerId(1);
  let id_document_capture = ListenerId(2);
  let id_a_capture = ListenerId(3);
  let id_b_capture = ListenerId(4);
  let id_c_capture = ListenerId(5);

  let id_c_bubble = ListenerId(6);
  let id_b_bubble = ListenerId(7);
  let id_a_bubble = ListenerId(8);
  let id_document_bubble = ListenerId(9);
  let id_window_bubble = ListenerId(10);

  assert!(doc.add_event_listener(
    EventTargetId::Window,
    type_,
    id_window_capture,
    EventListenerOptions {
      capture: true,
      ..Default::default()
    }
  ));
  assert!(doc.add_event_listener(
    EventTargetId::Document,
    type_,
    id_document_capture,
    EventListenerOptions {
      capture: true,
      ..Default::default()
    }
  ));
  assert!(doc.add_event_listener(
    EventTargetId::Node(a),
    type_,
    id_a_capture,
    EventListenerOptions {
      capture: true,
      ..Default::default()
    }
  ));
  assert!(doc.add_event_listener(
    EventTargetId::Node(b),
    type_,
    id_b_capture,
    EventListenerOptions {
      capture: true,
      ..Default::default()
    }
  ));
  assert!(doc.add_event_listener(
    EventTargetId::Node(c),
    type_,
    id_c_capture,
    EventListenerOptions {
      capture: true,
      ..Default::default()
    }
  ));

  assert!(doc.add_event_listener(
    EventTargetId::Node(c),
    type_,
    id_c_bubble,
    EventListenerOptions::default()
  ));
  assert!(doc.add_event_listener(
    EventTargetId::Node(b),
    type_,
    id_b_bubble,
    EventListenerOptions::default()
  ));
  assert!(doc.add_event_listener(
    EventTargetId::Node(a),
    type_,
    id_a_bubble,
    EventListenerOptions::default()
  ));
  assert!(doc.add_event_listener(
    EventTargetId::Document,
    type_,
    id_document_bubble,
    EventListenerOptions::default()
  ));
  assert!(doc.add_event_listener(
    EventTargetId::Window,
    type_,
    id_window_bubble,
    EventListenerOptions::default()
  ));

  let dispatch_target = EventTargetId::Node(c);
  let mut invoker = RecordingInvoker::new([
    (
      id_window_capture,
      Behavior {
        label: "window_capture",
        expectations: Expectations {
          target: Some(dispatch_target),
          current_target: Some(EventTargetId::Window),
          event_phase: Some(EventPhase::CapturingPhase),
        },
        action: Action::None,
      },
    ),
    (
      id_document_capture,
      Behavior {
        label: "document_capture",
        expectations: Expectations {
          target: Some(dispatch_target),
          current_target: Some(EventTargetId::Document),
          event_phase: Some(EventPhase::CapturingPhase),
        },
        action: Action::None,
      },
    ),
    (
      id_a_capture,
      Behavior {
        label: "a_capture",
        expectations: Expectations {
          target: Some(dispatch_target),
          current_target: Some(EventTargetId::Node(a)),
          event_phase: Some(EventPhase::CapturingPhase),
        },
        action: Action::None,
      },
    ),
    (
      id_b_capture,
      Behavior {
        label: "b_capture",
        expectations: Expectations {
          target: Some(dispatch_target),
          current_target: Some(EventTargetId::Node(b)),
          event_phase: Some(EventPhase::CapturingPhase),
        },
        action: Action::None,
      },
    ),
    (
      id_c_capture,
      Behavior {
        label: "c_capture",
        expectations: Expectations {
          target: Some(dispatch_target),
          current_target: Some(dispatch_target),
          event_phase: Some(EventPhase::AtTarget),
        },
        action: Action::None,
      },
    ),
    (
      id_c_bubble,
      Behavior {
        label: "c_bubble",
        expectations: Expectations {
          target: Some(dispatch_target),
          current_target: Some(dispatch_target),
          event_phase: Some(EventPhase::AtTarget),
        },
        action: Action::None,
      },
    ),
    (
      id_b_bubble,
      Behavior {
        label: "b_bubble",
        expectations: Expectations {
          target: Some(dispatch_target),
          current_target: Some(EventTargetId::Node(b)),
          event_phase: Some(EventPhase::BubblingPhase),
        },
        action: Action::None,
      },
    ),
    (
      id_a_bubble,
      Behavior {
        label: "a_bubble",
        expectations: Expectations {
          target: Some(dispatch_target),
          current_target: Some(EventTargetId::Node(a)),
          event_phase: Some(EventPhase::BubblingPhase),
        },
        action: Action::None,
      },
    ),
    (
      id_document_bubble,
      Behavior {
        label: "document_bubble",
        expectations: Expectations {
          target: Some(dispatch_target),
          current_target: Some(EventTargetId::Document),
          event_phase: Some(EventPhase::BubblingPhase),
        },
        action: Action::None,
      },
    ),
    (
      id_window_bubble,
      Behavior {
        label: "window_bubble",
        expectations: Expectations {
          target: Some(dispatch_target),
          current_target: Some(EventTargetId::Window),
          event_phase: Some(EventPhase::BubblingPhase),
        },
        action: Action::None,
      },
    ),
  ]);

  let mut event = Event::new(
    type_,
    EventInit {
      bubbles: true,
      ..Default::default()
    },
  );
  let not_canceled = doc
    .dispatch_event(dispatch_target, &mut event, &mut invoker)
    .unwrap();
  assert!(not_canceled);

  assert_eq!(
    invoker.calls,
    vec![
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
  let (mut doc, a, b, c) = make_dom_abc();

  let type_ = "x";
  let id_b_stop = ListenerId(1);
  let id_a = ListenerId(2);
  let id_document = ListenerId(3);
  let id_window = ListenerId(4);

  assert!(doc.add_event_listener(
    EventTargetId::Node(b),
    type_,
    id_b_stop,
    EventListenerOptions::default()
  ));
  assert!(doc.add_event_listener(
    EventTargetId::Node(a),
    type_,
    id_a,
    EventListenerOptions::default()
  ));
  assert!(doc.add_event_listener(
    EventTargetId::Document,
    type_,
    id_document,
    EventListenerOptions::default()
  ));
  assert!(doc.add_event_listener(
    EventTargetId::Window,
    type_,
    id_window,
    EventListenerOptions::default()
  ));

  let dispatch_target = EventTargetId::Node(c);
  let mut invoker = RecordingInvoker::new([
    (
      id_b_stop,
      Behavior {
        label: "b_bubble_stop",
        expectations: Expectations {
          target: Some(dispatch_target),
          current_target: Some(EventTargetId::Node(b)),
          event_phase: Some(EventPhase::BubblingPhase),
        },
        action: Action::StopPropagation,
      },
    ),
    (
      id_a,
      Behavior {
        label: "a_bubble",
        expectations: Expectations::default(),
        action: Action::None,
      },
    ),
    (
      id_document,
      Behavior {
        label: "document_bubble",
        expectations: Expectations::default(),
        action: Action::None,
      },
    ),
    (
      id_window,
      Behavior {
        label: "window_bubble",
        expectations: Expectations::default(),
        action: Action::None,
      },
    ),
  ]);

  let mut event = Event::new(
    type_,
    EventInit {
      bubbles: true,
      ..Default::default()
    },
  );
  doc
    .dispatch_event(dispatch_target, &mut event, &mut invoker)
    .unwrap();

  assert_eq!(invoker.calls, vec!["b_bubble_stop"]);
}

#[test]
fn stop_immediate_propagation_stops_other_listeners_on_same_target() {
  let (mut doc, _a, b, c) = make_dom_abc();

  let type_ = "x";
  let id_first = ListenerId(1);
  let id_second = ListenerId(2);
  let id_parent = ListenerId(3);

  assert!(doc.add_event_listener(
    EventTargetId::Node(c),
    type_,
    id_first,
    EventListenerOptions::default()
  ));
  assert!(doc.add_event_listener(
    EventTargetId::Node(c),
    type_,
    id_second,
    EventListenerOptions::default()
  ));
  assert!(doc.add_event_listener(
    EventTargetId::Node(b),
    type_,
    id_parent,
    EventListenerOptions::default()
  ));

  let dispatch_target = EventTargetId::Node(c);
  let mut invoker = RecordingInvoker::new([
    (
      id_first,
      Behavior {
        label: "first",
        expectations: Expectations::default(),
        action: Action::StopImmediatePropagation,
      },
    ),
    (
      id_second,
      Behavior {
        label: "second",
        expectations: Expectations::default(),
        action: Action::None,
      },
    ),
    (
      id_parent,
      Behavior {
        label: "parent_bubble",
        expectations: Expectations::default(),
        action: Action::None,
      },
    ),
  ]);

  let mut event = Event::new(
    type_,
    EventInit {
      bubbles: true,
      ..Default::default()
    },
  );
  doc
    .dispatch_event(dispatch_target, &mut event, &mut invoker)
    .unwrap();

  assert_eq!(invoker.calls, vec!["first"]);
}

#[test]
fn once_listeners_only_fire_once() {
  let (mut doc, _a, _b, c) = make_dom_abc();

  let type_ = "x";
  let id_once = ListenerId(1);

  assert!(doc.add_event_listener(
    EventTargetId::Node(c),
    type_,
    id_once,
    EventListenerOptions {
      once: true,
      ..Default::default()
    }
  ));

  let mut invoker = RecordingInvoker::new([(
    id_once,
    Behavior {
      label: "once",
      expectations: Expectations::default(),
      action: Action::None,
    },
  )]);

  for _ in 0..2 {
    let mut event = Event::new(type_, EventInit::default());
    doc
      .dispatch_event(EventTargetId::Node(c), &mut event, &mut invoker)
      .unwrap();
  }

  assert_eq!(invoker.calls, vec!["once"]);
}

#[test]
fn remove_event_listener_during_dispatch_skips_removed_listener() {
  let (mut doc, _a, _b, c) = make_dom_abc();

  let type_ = "x";
  let id_l1 = ListenerId(1);
  let id_l2 = ListenerId(2);
  let dispatch_target = EventTargetId::Node(c);

  assert!(doc.add_event_listener(
    dispatch_target,
    type_,
    id_l1,
    EventListenerOptions::default()
  ));
  assert!(doc.add_event_listener(
    dispatch_target,
    type_,
    id_l2,
    EventListenerOptions::default()
  ));

  let mut invoker = RecordingInvoker::new([
    (
      id_l1,
      Behavior {
        label: "l1",
        expectations: Expectations::default(),
        action: Action::RemoveListener {
          target: dispatch_target,
          type_,
          capture: false,
          listener_id: id_l2,
        },
      },
    ),
    (
      id_l2,
      Behavior {
        label: "l2",
        expectations: Expectations::default(),
        action: Action::None,
      },
    ),
  ]);

  let mut event = Event::new(type_, EventInit::default());
  doc
    .dispatch_event(dispatch_target, &mut event, &mut invoker)
    .unwrap();

  let mut event2 = Event::new(type_, EventInit::default());
  doc
    .dispatch_event(dispatch_target, &mut event2, &mut invoker)
    .unwrap();

  assert_eq!(invoker.calls, vec!["l1", "l1"]);
}

#[test]
fn passive_listeners_cannot_set_default_prevented() {
  let (mut doc, _a, _b, c) = make_dom_abc();

  let type_ = "x";
  let id_passive = ListenerId(1);

  assert!(doc.add_event_listener(
    EventTargetId::Node(c),
    type_,
    id_passive,
    EventListenerOptions {
      passive: true,
      ..Default::default()
    }
  ));

  let mut invoker = RecordingInvoker::new([(
    id_passive,
    Behavior {
      label: "passive",
      expectations: Expectations::default(),
      action: Action::PreventDefault,
    },
  )]);

  let mut event = Event::new(
    type_,
    EventInit {
      bubbles: true,
      cancelable: true,
      ..Default::default()
    },
  );
  let not_canceled = doc
    .dispatch_event(EventTargetId::Node(c), &mut event, &mut invoker)
    .unwrap();

  assert!(not_canceled, "dispatchEvent should return true if not canceled");
  assert!(
    !event.default_prevented,
    "passive listeners must not set defaultPrevented"
  );
}

