use super::*;
use crate::dom::{DomNode, DomNodeType};
use crate::dom2::{Document, NodeId};
use selectors::context::QuirksMode;
use std::cell::RefCell;
use std::rc::Rc;

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

#[test]
fn capture_and_bubble_ordering_across_tree() {
  let (doc, a, b, c) = make_dom_abc();
  let mut registry = EventListenerRegistry::new();
  let log: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));

  let add = |registry: &mut EventListenerRegistry,
             target: EventTargetId,
             label: &'static str,
             expected_phase: EventPhase,
             expected_current: EventTargetId,
             options: AddEventListenerOptions,
             log: Rc<RefCell<Vec<&'static str>>>| {
    registry.add_event_listener(target, "x", options, move |event, _ctx| {
      assert_eq!(event.target, Some(EventTargetId::Node(c)));
      assert_eq!(event.current_target, Some(expected_current));
      assert_eq!(event.event_phase, expected_phase);
      log.borrow_mut().push(label);
      Ok(())
    });
  };

  add(
    &mut registry,
    EventTargetId::Window,
    "window_capture",
    EventPhase::Capturing,
    EventTargetId::Window,
    AddEventListenerOptions {
      capture: true,
      ..Default::default()
    },
    log.clone(),
  );
  add(
    &mut registry,
    EventTargetId::Document,
    "document_capture",
    EventPhase::Capturing,
    EventTargetId::Document,
    AddEventListenerOptions {
      capture: true,
      ..Default::default()
    },
    log.clone(),
  );
  add(
    &mut registry,
    EventTargetId::Node(a),
    "a_capture",
    EventPhase::Capturing,
    EventTargetId::Node(a),
    AddEventListenerOptions {
      capture: true,
      ..Default::default()
    },
    log.clone(),
  );
  add(
    &mut registry,
    EventTargetId::Node(b),
    "b_capture",
    EventPhase::Capturing,
    EventTargetId::Node(b),
    AddEventListenerOptions {
      capture: true,
      ..Default::default()
    },
    log.clone(),
  );
  add(
    &mut registry,
    EventTargetId::Node(c),
    "c_capture",
    EventPhase::AtTarget,
    EventTargetId::Node(c),
    AddEventListenerOptions {
      capture: true,
      ..Default::default()
    },
    log.clone(),
  );

  add(
    &mut registry,
    EventTargetId::Node(c),
    "c_bubble",
    EventPhase::AtTarget,
    EventTargetId::Node(c),
    AddEventListenerOptions {
      capture: false,
      ..Default::default()
    },
    log.clone(),
  );
  add(
    &mut registry,
    EventTargetId::Node(b),
    "b_bubble",
    EventPhase::Bubbling,
    EventTargetId::Node(b),
    AddEventListenerOptions {
      capture: false,
      ..Default::default()
    },
    log.clone(),
  );
  add(
    &mut registry,
    EventTargetId::Node(a),
    "a_bubble",
    EventPhase::Bubbling,
    EventTargetId::Node(a),
    AddEventListenerOptions {
      capture: false,
      ..Default::default()
    },
    log.clone(),
  );
  add(
    &mut registry,
    EventTargetId::Document,
    "document_bubble",
    EventPhase::Bubbling,
    EventTargetId::Document,
    AddEventListenerOptions {
      capture: false,
      ..Default::default()
    },
    log.clone(),
  );
  add(
    &mut registry,
    EventTargetId::Window,
    "window_bubble",
    EventPhase::Bubbling,
    EventTargetId::Window,
    AddEventListenerOptions {
      capture: false,
      ..Default::default()
    },
    log.clone(),
  );

  let mut event = Event::new(
    "x",
    EventInit {
      bubbles: true,
      ..Default::default()
    },
  );
  assert!(dispatch_event(EventTargetId::Node(c), &mut event, &doc, &mut registry).unwrap());

  assert_eq!(
    log.borrow().as_slice(),
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
  let (doc, _a, b, c) = make_dom_abc();
  let mut registry = EventListenerRegistry::new();
  let log: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));

  let log1 = log.clone();
  registry.add_event_listener(
    EventTargetId::Node(b),
    "x",
    AddEventListenerOptions::default(),
    move |event, _ctx| {
      assert_eq!(event.event_phase, EventPhase::Bubbling);
      log1.borrow_mut().push("b_bubble_stop");
      event.stop_propagation();
      Ok(())
    },
  );

  for label in ["a_bubble", "document_bubble", "window_bubble"] {
    let logn = log.clone();
    let target = match label {
      "a_bubble" => EventTargetId::Node(_a),
      "document_bubble" => EventTargetId::Document,
      "window_bubble" => EventTargetId::Window,
      _ => unreachable!(),
    };
    registry.add_event_listener(target, "x", AddEventListenerOptions::default(), move |_, _| {
      logn.borrow_mut().push(label);
      Ok(())
    });
  }

  let mut event = Event::new(
    "x",
    EventInit {
      bubbles: true,
      ..Default::default()
    },
  );
  dispatch_event(EventTargetId::Node(c), &mut event, &doc, &mut registry).unwrap();

  assert_eq!(log.borrow().as_slice(), &["b_bubble_stop"]);
}

#[test]
fn stop_immediate_propagation_stops_other_listeners_on_same_target() {
  let (doc, _a, b, c) = make_dom_abc();
  let mut registry = EventListenerRegistry::new();
  let log: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));

  let log_first = log.clone();
  registry.add_event_listener(
    EventTargetId::Node(c),
    "x",
    AddEventListenerOptions::default(),
    move |event, _ctx| {
      log_first.borrow_mut().push("first");
      event.stop_immediate_propagation();
      Ok(())
    },
  );

  let log_second = log.clone();
  registry.add_event_listener(
    EventTargetId::Node(c),
    "x",
    AddEventListenerOptions::default(),
    move |_event, _ctx| {
      log_second.borrow_mut().push("second");
      Ok(())
    },
  );

  let log_parent = log.clone();
  registry.add_event_listener(
    EventTargetId::Node(b),
    "x",
    AddEventListenerOptions::default(),
    move |_event, _ctx| {
      log_parent.borrow_mut().push("parent_bubble");
      Ok(())
    },
  );

  let mut event = Event::new(
    "x",
    EventInit {
      bubbles: true,
      ..Default::default()
    },
  );
  dispatch_event(EventTargetId::Node(c), &mut event, &doc, &mut registry).unwrap();

  assert_eq!(log.borrow().as_slice(), &["first"]);
}

#[test]
fn once_listeners_only_fire_once() {
  let (doc, _a, _b, c) = make_dom_abc();
  let mut registry = EventListenerRegistry::new();
  let count: Rc<RefCell<u32>> = Rc::new(RefCell::new(0));

  let count1 = count.clone();
  registry.add_event_listener(
    EventTargetId::Node(c),
    "x",
    AddEventListenerOptions {
      once: true,
      ..Default::default()
    },
    move |_event, _ctx| {
      *count1.borrow_mut() += 1;
      Ok(())
    },
  );

  for _ in 0..2 {
    let mut event = Event::new("x", EventInit::default());
    dispatch_event(EventTargetId::Node(c), &mut event, &doc, &mut registry).unwrap();
  }

  assert_eq!(*count.borrow(), 1);
}

#[test]
fn remove_event_listener_during_dispatch_does_not_affect_current_snapshot() {
  let (doc, _a, _b, c) = make_dom_abc();
  let mut registry = EventListenerRegistry::new();
  let log: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));

  let log1 = log.clone();
  let listener_to_remove: Rc<RefCell<Option<ListenerId>>> = Rc::new(RefCell::new(None));
  let listener_to_remove_cb = listener_to_remove.clone();

  // Listener that removes `l2` while dispatch is in progress.
  registry.add_event_listener(
    EventTargetId::Node(c),
    "x",
    AddEventListenerOptions::default(),
    move |_event, ctx| {
      log1.borrow_mut().push("l1");
      let id2 = listener_to_remove_cb.borrow().expect("missing id2");
      ctx.remove_event_listener(EventTargetId::Node(c), "x", false, id2);
      Ok(())
    },
  );

  let log2 = log.clone();
  let id2 = registry.add_event_listener(
    EventTargetId::Node(c),
    "x",
    AddEventListenerOptions::default(),
    move |_event, _ctx| {
      log2.borrow_mut().push("l2");
      Ok(())
    },
  );

  // Publish `id2` so `l1` can remove it.
  *listener_to_remove.borrow_mut() = Some(id2);

  // First dispatch: both should run due to snapshotting.
  let mut event = Event::new("x", EventInit::default());
  dispatch_event(EventTargetId::Node(c), &mut event, &doc, &mut registry).unwrap();

  // Second dispatch: l2 should be removed.
  let mut event = Event::new("x", EventInit::default());
  dispatch_event(EventTargetId::Node(c), &mut event, &doc, &mut registry).unwrap();

  assert_eq!(log.borrow().as_slice(), &["l1", "l2", "l1"]);
}
