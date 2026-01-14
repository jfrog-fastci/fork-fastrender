#![cfg(test)]

use super::*;
use crate::dom::{parse_html, DomNode, DomNodeType};
use crate::dom2::{Document, NodeId, NodeKind};
use crate::web::dom::DomException;
use selectors::context::QuirksMode;
use std::collections::HashMap;
use vm_js::HeapLimits;
use vm_js::Value as JsValue;

#[test]
fn listener_id_new_roundtrips_through_get() {
  let id = ListenerId::new(42);
  assert_eq!(id.get(), 42);
}

#[test]
fn sweep_dead_opaque_target_clears_listener_and_parent_metadata() {
  let registry = EventListenerRegistry::new();
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 512 * 1024));

  let target_id: u64;
  {
    let mut scope = heap.scope();
    let obj = scope.alloc_object().unwrap();
    target_id = (obj.index() as u64) | ((obj.generation() as u64) << 32);

    registry.register_opaque_target(target_id, WeakGcObject::new(obj));
    registry.set_opaque_parent(target_id, Some(EventTargetId::Window));
    registry.add_event_listener(
      EventTargetId::Opaque(target_id),
      "x",
      ListenerId::new(1),
      AddEventListenerOptions::default(),
    );

    assert!(registry.opaque_targets.borrow().contains_key(&target_id));
    assert!(registry.opaque_parents.borrow().contains_key(&target_id));
    assert!(
      registry
        .listeners
        .borrow()
        .contains_key(&EventTargetId::Opaque(target_id))
    );

    // Drop all roots and force a GC cycle so the wrapper dies.
    scope.heap_mut().collect_garbage();
  }

  registry.sweep_dead_opaque_targets(&heap);

  assert!(!registry.opaque_targets.borrow().contains_key(&target_id));
  assert!(!registry.opaque_parents.borrow().contains_key(&target_id));
  assert!(
    !registry
      .listeners
      .borrow()
      .contains_key(&EventTargetId::Opaque(target_id))
  );
}

#[test]
fn sweep_drops_opaque_parent_links_that_point_to_dead_targets() {
  let registry = EventListenerRegistry::new();
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 512 * 1024));

  let mut scope = heap.scope();
  let parent_obj = scope.alloc_object().unwrap();
  let child_obj = scope.alloc_object().unwrap();

  let parent_id = (parent_obj.index() as u64) | ((parent_obj.generation() as u64) << 32);
  let child_id = (child_obj.index() as u64) | ((child_obj.generation() as u64) << 32);

  registry.register_opaque_target(parent_id, WeakGcObject::new(parent_obj));
  registry.register_opaque_target(child_id, WeakGcObject::new(child_obj));
  registry.set_opaque_parent(child_id, Some(EventTargetId::Opaque(parent_id)));

  registry.add_event_listener(
    EventTargetId::Opaque(parent_id),
    "x",
    ListenerId::new(1),
    AddEventListenerOptions::default(),
  );
  registry.add_event_listener(
    EventTargetId::Opaque(child_id),
    "x",
    ListenerId::new(2),
    AddEventListenerOptions::default(),
  );

  // Keep the child wrapper alive; allow the parent to be collected.
  scope.push_root(JsValue::Object(child_obj)).unwrap();
  scope.heap_mut().collect_garbage();

  registry.sweep_dead_opaque_targets(scope.heap());

  assert!(!registry.opaque_targets.borrow().contains_key(&parent_id));
  assert!(registry.opaque_targets.borrow().contains_key(&child_id));

  // The child's parent link should be dropped rather than pointing at a dead opaque id.
  assert!(!registry.opaque_parents.borrow().contains_key(&child_id));

  assert!(
    !registry
      .listeners
      .borrow()
      .contains_key(&EventTargetId::Opaque(parent_id))
  );
  assert!(
    registry
      .listeners
      .borrow()
      .contains_key(&EventTargetId::Opaque(child_id))
  );
}

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

fn first_element_child(doc: &Document, parent: NodeId) -> NodeId {
  doc
    .node(parent)
    .children
    .iter()
    .copied()
    .find(|&child| matches!(doc.node(child).kind, NodeKind::Element { .. }))
    .unwrap_or_else(|| panic!("expected {parent:?} to have an element child"))
}

fn make_dom_abc() -> (Document, NodeId, NodeId, NodeId) {
  // Document → <a> → <b> → <c>
  let root = DomNode {
    node_type: DomNodeType::Document {
      quirks_mode: QuirksMode::NoQuirks,
      scripting_enabled: true,
      is_html_document: true,
    },
    children: vec![element("a", vec![element("b", vec![element("c", vec![])])])],
  };
  let doc = Document::from_renderer_dom(&root);

  // `dom2::NodeId` is opaque (constructor is private); use the known tree shape to grab IDs.
  let root_id = doc.root();
  // `Document::from_renderer_dom` may materialize a synthetic doctype node; skip to the first
  // element.
  let a = first_element_child(&doc, root_id);
  let b = first_element_child(&doc, a);
  let c = first_element_child(&doc, b);
  (doc, a, b, c)
}

fn find_node_id_anywhere(doc: &Document, id: &str) -> Option<NodeId> {
  for node_id in doc.subtree_preorder(doc.root()) {
    let (namespace, attributes) = match &doc.node(node_id).kind {
      NodeKind::Element {
        namespace,
        attributes,
        ..
      }
      | NodeKind::Slot {
        namespace,
        attributes,
        ..
      } => (namespace.as_str(), attributes.as_slice()),
      _ => continue,
    };
    let is_html = doc.is_html_case_insensitive_namespace(namespace);
    if attributes
      .iter()
      .any(|attr| attr.qualified_name_matches("id", is_html) && attr.value == id)
    {
      return Some(node_id);
    }
  }
  None
}

fn make_dom_div_target() -> (Document, NodeId) {
  let root = DomNode {
    node_type: DomNodeType::Document {
      quirks_mode: QuirksMode::NoQuirks,
      scripting_enabled: true,
      is_html_document: true,
    },
    children: vec![DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: String::new(),
        attributes: vec![("id".to_string(), "target".to_string())],
      },
      children: vec![],
    }],
  };
  let doc = Document::from_renderer_dom(&root);
  let target = first_element_child(&doc, doc.root());
  (doc, target)
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
    listener_id: ListenerId,
    capture: bool,
    expect_removed: Option<bool>,
  },
  RemoveAndReaddListener {
    target: EventTargetId,
    type_: &'static str,
    listener_id: ListenerId,
    capture: bool,
    options: AddEventListenerOptions,
    expect_removed: Option<bool>,
    expect_added: Option<bool>,
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
      Action::PreventDefault => event.prevent_default(),
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
      Action::RemoveAndReaddListener {
        target,
        type_,
        listener_id,
        capture,
        options,
        expect_removed,
        expect_added,
      } => {
        let removed = self
          .registry
          .remove_event_listener(target, type_, listener_id, capture);
        if let Some(expect_removed) = expect_removed {
          assert_eq!(removed, expect_removed);
        }

        let added = self
          .registry
          .add_event_listener(target, type_, listener_id, options);
        if let Some(expect_added) = expect_added {
          assert_eq!(added, expect_added);
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
  assert!(dispatch_event(
    EventTargetId::Node(c),
    &mut event,
    &doc,
    &registry,
    &mut invoker
  )
  .unwrap());

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
fn load_event_path_does_not_propagate_to_window() {
  let (doc, target) = make_dom_div_target();
  let registry = EventListenerRegistry::new();
  let type_ = "load";

  let id_window_capture = ListenerId::new(1);
  let id_document_capture = ListenerId::new(2);
  let id_document_bubble = ListenerId::new(3);
  let id_window_bubble = ListenerId::new(4);

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
    EventTargetId::Node(target),
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
  dispatch_event(
    EventTargetId::Node(target),
    &mut event,
    &doc,
    &registry,
    &mut invoker,
  )
  .unwrap();

  assert_eq!(invoker.calls.as_slice(), &["document_capture", "document_bubble"]);
}

#[test]
fn non_load_event_path_still_propagates_to_window() {
  let (doc, target) = make_dom_div_target();
  let registry = EventListenerRegistry::new();
  let type_ = "x";

  let id_window_capture = ListenerId::new(1);
  let id_document_capture = ListenerId::new(2);
  let id_document_bubble = ListenerId::new(3);
  let id_window_bubble = ListenerId::new(4);

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
    EventTargetId::Node(target),
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
  dispatch_event(
    EventTargetId::Node(target),
    &mut event,
    &doc,
    &registry,
    &mut invoker,
  )
  .unwrap();

  assert_eq!(
    invoker.calls.as_slice(),
    &["window_capture", "document_capture", "document_bubble", "window_bubble"]
  );
}

#[test]
fn has_listeners_for_dispatch_respects_load_event_document_parent_special_case() {
  let (doc, target) = make_dom_div_target();
  let registry = EventListenerRegistry::new();

  let id_window_capture_load = ListenerId::new(1);
  assert!(registry.add_event_listener(
    EventTargetId::Window,
    "load",
    id_window_capture_load,
    AddEventListenerOptions {
      capture: true,
      ..Default::default()
    },
  ));
  assert!(
    !registry.has_listeners_for_dispatch(EventTargetId::Node(target), "load", &doc, true, false),
    "load events should not propagate from document to window"
  );

  let id_window_capture_x = ListenerId::new(2);
  assert!(registry.add_event_listener(
    EventTargetId::Window,
    "x",
    id_window_capture_x,
    AddEventListenerOptions {
      capture: true,
      ..Default::default()
    },
  ));
  assert!(
    registry.has_listeners_for_dispatch(EventTargetId::Node(target), "x", &doc, true, false),
    "non-load events should still propagate from document to window"
  );
}

#[test]
fn document_bubbling_event_reaches_window_by_default() {
  let doc = Document::new(QuirksMode::NoQuirks);
  assert!(doc.has_window_event_parent());
  let registry = EventListenerRegistry::new();

  let type_ = "x";
  let id_document = ListenerId::new(1);
  let id_window = ListenerId::new(2);

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
    EventTargetId::Document,
    [
      (
        id_document,
        Behavior {
          label: "document",
          expected_phase: EventPhase::AtTarget,
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
  assert_eq!(
    build_event_path(EventTargetId::Document, &event, &doc, &registry)
      .into_iter()
      .map(|entry| entry.invocation_target)
      .collect::<Vec<_>>(),
    vec![EventTargetId::Document, EventTargetId::Window],
    "expected document events to reach window when `has_window_event_parent` is enabled"
  );
  dispatch_event(
    EventTargetId::Document,
    &mut event,
    &doc,
    &registry,
    &mut invoker,
  )
  .unwrap();

  assert_eq!(invoker.calls.as_slice(), &["document", "window_bubble"]);
}

#[test]
fn document_without_window_event_parent_does_not_reach_window() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  doc.set_has_window_event_parent(false);
  assert!(!doc.has_window_event_parent());
  let registry = EventListenerRegistry::new();

  let type_ = "x";
  let id_document = ListenerId::new(1);
  let id_window = ListenerId::new(2);

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
    EventTargetId::Document,
    [(
      id_document,
      Behavior {
        label: "document",
        expected_phase: EventPhase::AtTarget,
        expected_current_target: EventTargetId::Document,
        action: Action::None,
      },
    )],
  );

  let mut event = Event::new(
    type_,
    EventInit {
      bubbles: true,
      ..Default::default()
    },
  );
  assert_eq!(
    build_event_path(EventTargetId::Document, &event, &doc, &registry)
      .into_iter()
      .map(|entry| entry.invocation_target)
      .collect::<Vec<_>>(),
    vec![EventTargetId::Document],
    "document events must not reach window when `has_window_event_parent` is disabled"
  );
  dispatch_event(
    EventTargetId::Document,
    &mut event,
    &doc,
    &registry,
    &mut invoker,
  )
  .unwrap();

  assert_eq!(invoker.calls.as_slice(), &["document"]);
}

#[test]
fn connected_node_event_does_not_reach_window_without_window_event_parent() {
  let (mut doc, a, b, c) = make_dom_abc();
  doc.set_has_window_event_parent(false);
  let registry = EventListenerRegistry::new();

  let type_ = "x";
  let id_document = ListenerId::new(1);
  let id_window = ListenerId::new(2);

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
    [(
      id_document,
      Behavior {
        label: "document_bubble",
        expected_phase: EventPhase::Bubbling,
        expected_current_target: EventTargetId::Document,
        action: Action::None,
      },
    )],
  );

  let mut event = Event::new(
    type_,
    EventInit {
      bubbles: true,
      ..Default::default()
    },
  );
  assert_eq!(
    build_event_path(EventTargetId::Node(c), &event, &doc, &registry)
      .into_iter()
      .map(|entry| entry.invocation_target)
      .collect::<Vec<_>>(),
    vec![
      EventTargetId::Node(c),
      EventTargetId::Node(b),
      EventTargetId::Node(a),
      EventTargetId::Document,
    ],
    "connected nodes must not include window in their event path when `has_window_event_parent` is disabled"
  );
  dispatch_event(
    EventTargetId::Node(c),
    &mut event,
    &doc,
    &registry,
    &mut invoker,
  )
  .unwrap();

  assert_eq!(invoker.calls.as_slice(), &["document_bubble"]);
}

#[test]
fn capture_and_bubble_ordering_across_opaque_parent_chain() {
  let doc = Document::new(QuirksMode::NoQuirks);
  let registry = EventListenerRegistry::new();

  let type_ = "x";
  let root = EventTargetId::Opaque(1);
  let parent = EventTargetId::Opaque(2);
  let target = EventTargetId::Opaque(3);

  registry.set_opaque_parent(2, Some(root));
  registry.set_opaque_parent(3, Some(parent));

  let id_root_capture = ListenerId::new(1);
  let id_parent_capture = ListenerId::new(2);
  let id_target_capture = ListenerId::new(3);
  let id_target_bubble = ListenerId::new(4);
  let id_parent_bubble = ListenerId::new(5);
  let id_root_bubble = ListenerId::new(6);

  assert!(registry.add_event_listener(
    root,
    type_,
    id_root_capture,
    AddEventListenerOptions {
      capture: true,
      ..Default::default()
    }
  ));
  assert!(registry.add_event_listener(
    parent,
    type_,
    id_parent_capture,
    AddEventListenerOptions {
      capture: true,
      ..Default::default()
    }
  ));
  assert!(registry.add_event_listener(
    target,
    type_,
    id_target_capture,
    AddEventListenerOptions {
      capture: true,
      ..Default::default()
    }
  ));
  assert!(registry.add_event_listener(
    target,
    type_,
    id_target_bubble,
    AddEventListenerOptions::default()
  ));
  assert!(registry.add_event_listener(
    parent,
    type_,
    id_parent_bubble,
    AddEventListenerOptions::default()
  ));
  assert!(registry.add_event_listener(
    root,
    type_,
    id_root_bubble,
    AddEventListenerOptions::default()
  ));

  let mut invoker = RecordingInvoker::new(
    &registry,
    target,
    [
      (
        id_root_capture,
        Behavior {
          label: "root_capture",
          expected_phase: EventPhase::Capturing,
          expected_current_target: root,
          action: Action::None,
        },
      ),
      (
        id_parent_capture,
        Behavior {
          label: "parent_capture",
          expected_phase: EventPhase::Capturing,
          expected_current_target: parent,
          action: Action::None,
        },
      ),
      (
        id_target_capture,
        Behavior {
          label: "target_capture",
          expected_phase: EventPhase::AtTarget,
          expected_current_target: target,
          action: Action::None,
        },
      ),
      (
        id_target_bubble,
        Behavior {
          label: "target_bubble",
          expected_phase: EventPhase::AtTarget,
          expected_current_target: target,
          action: Action::None,
        },
      ),
      (
        id_parent_bubble,
        Behavior {
          label: "parent_bubble",
          expected_phase: EventPhase::Bubbling,
          expected_current_target: parent,
          action: Action::None,
        },
      ),
      (
        id_root_bubble,
        Behavior {
          label: "root_bubble",
          expected_phase: EventPhase::Bubbling,
          expected_current_target: root,
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
  assert!(dispatch_event(target, &mut event, &doc, &registry, &mut invoker).unwrap());
  assert_eq!(
    invoker.calls.as_slice(),
    &[
      "root_capture",
      "parent_capture",
      "target_capture",
      "target_bubble",
      "parent_bubble",
      "root_bubble"
    ]
  );
}

#[test]
fn opaque_parent_chain_preserves_target_and_event_phase() {
  let doc = Document::new(QuirksMode::NoQuirks);
  let registry = EventListenerRegistry::new();

  let type_ = "x";
  let root = EventTargetId::Opaque(11);
  let parent = EventTargetId::Opaque(12);
  let target = EventTargetId::Opaque(13);

  registry.set_opaque_parent(12, Some(root));
  registry.set_opaque_parent(13, Some(parent));

  let id_root_capture = ListenerId::new(1);
  let id_parent_capture = ListenerId::new(2);
  let id_target_capture = ListenerId::new(3);
  let id_target_bubble = ListenerId::new(4);
  let id_parent_bubble = ListenerId::new(5);
  let id_root_bubble = ListenerId::new(6);

  assert!(registry.add_event_listener(
    root,
    type_,
    id_root_capture,
    AddEventListenerOptions {
      capture: true,
      ..Default::default()
    }
  ));
  assert!(registry.add_event_listener(
    parent,
    type_,
    id_parent_capture,
    AddEventListenerOptions {
      capture: true,
      ..Default::default()
    }
  ));
  assert!(registry.add_event_listener(
    target,
    type_,
    id_target_capture,
    AddEventListenerOptions {
      capture: true,
      ..Default::default()
    }
  ));
  assert!(registry.add_event_listener(
    target,
    type_,
    id_target_bubble,
    AddEventListenerOptions::default()
  ));
  assert!(registry.add_event_listener(
    parent,
    type_,
    id_parent_bubble,
    AddEventListenerOptions::default()
  ));
  assert!(registry.add_event_listener(
    root,
    type_,
    id_root_bubble,
    AddEventListenerOptions::default()
  ));

  struct Invoker {
    calls: Vec<&'static str>,
    root: EventTargetId,
    parent: EventTargetId,
    target: EventTargetId,
    id_root_capture: ListenerId,
    id_parent_capture: ListenerId,
    id_target_capture: ListenerId,
    id_target_bubble: ListenerId,
    id_parent_bubble: ListenerId,
    id_root_bubble: ListenerId,
  }

  impl EventListenerInvoker for Invoker {
    fn invoke(&mut self, listener_id: ListenerId, event: &mut Event) -> Result<(), DomError> {
      // Opaque parent chains must not participate in Shadow DOM retargeting: `event.target` should
      // remain the original dispatch target for all listeners.
      assert_eq!(event.target, Some(self.target));

      match listener_id {
        id if id == self.id_root_capture => {
          assert_eq!(event.current_target, Some(self.root));
          assert_eq!(event.event_phase, EventPhase::Capturing);
          self.calls.push("root_capture");
        }
        id if id == self.id_parent_capture => {
          assert_eq!(event.current_target, Some(self.parent));
          assert_eq!(event.event_phase, EventPhase::Capturing);
          self.calls.push("parent_capture");
        }
        id if id == self.id_target_capture => {
          assert_eq!(event.current_target, Some(self.target));
          assert_eq!(event.event_phase, EventPhase::AtTarget);
          self.calls.push("target_capture");
        }
        id if id == self.id_target_bubble => {
          assert_eq!(event.current_target, Some(self.target));
          assert_eq!(event.event_phase, EventPhase::AtTarget);
          self.calls.push("target_bubble");
        }
        id if id == self.id_parent_bubble => {
          assert_eq!(event.current_target, Some(self.parent));
          assert_eq!(event.event_phase, EventPhase::Bubbling);
          self.calls.push("parent_bubble");
        }
        id if id == self.id_root_bubble => {
          assert_eq!(event.current_target, Some(self.root));
          assert_eq!(event.event_phase, EventPhase::Bubbling);
          self.calls.push("root_bubble");
        }
        _ => panic!("unexpected listener_id: {listener_id:?}"),
      }

      Ok(())
    }
  }

  let mut invoker = Invoker {
    calls: Vec::new(),
    root,
    parent,
    target,
    id_root_capture,
    id_parent_capture,
    id_target_capture,
    id_target_bubble,
    id_parent_bubble,
    id_root_bubble,
  };

  let mut event = Event::new(
    type_,
    EventInit {
      bubbles: true,
      ..Default::default()
    },
  );
  dispatch_event(target, &mut event, &doc, &registry, &mut invoker).unwrap();

  assert_eq!(
    invoker.calls.as_slice(),
    &[
      "root_capture",
      "parent_capture",
      "target_capture",
      "target_bubble",
      "parent_bubble",
      "root_bubble"
    ]
  );
}

#[test]
fn stop_propagation_prevents_subsequent_targets() {
  let (doc, a, b, c) = make_dom_abc();
  let registry = EventListenerRegistry::new();

  let type_ = "x";

  let id_stop = ListenerId::new(1);
  let id_b2 = ListenerId::new(2);
  let id_a = ListenerId::new(3);
  let id_document = ListenerId::new(4);
  let id_window = ListenerId::new(5);

  assert!(registry.add_event_listener(
    EventTargetId::Node(b),
    type_,
    id_stop,
    AddEventListenerOptions::default()
  ));
  assert!(registry.add_event_listener(
    EventTargetId::Node(b),
    type_,
    id_b2,
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
        id_b2,
        Behavior {
          label: "b_bubble_2",
          expected_phase: EventPhase::Bubbling,
          expected_current_target: EventTargetId::Node(b),
          action: Action::None,
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

  assert_eq!(invoker.calls.as_slice(), &["b_bubble_stop", "b_bubble_2"]);
  assert!(
    !event.propagation_stopped,
    "propagation_stopped must be cleared after dispatch returns"
  );
  assert!(
    !event.immediate_propagation_stopped,
    "immediate_propagation_stopped must be cleared after dispatch returns"
  );
  assert!(
    event.path.is_empty(),
    "event.path must be cleared after dispatch returns"
  );
}

#[test]
fn propagation_flags_are_cleared_after_dispatch() {
  let doc = Document::new(QuirksMode::NoQuirks);
  let registry = EventListenerRegistry::new();

  let type_ = "x";
  let id_stop = ListenerId::new(1);

  assert!(registry.add_event_listener(
    EventTargetId::Document,
    type_,
    id_stop,
    AddEventListenerOptions::default()
  ));

  let mut invoker = RecordingInvoker::new(
    &registry,
    EventTargetId::Document,
    [(
      id_stop,
      Behavior {
        label: "stop",
        expected_phase: EventPhase::AtTarget,
        expected_current_target: EventTargetId::Document,
        action: Action::StopPropagation,
      },
    )],
  );

  let mut event = Event::new(
    type_,
    EventInit {
      bubbles: true,
      ..Default::default()
    },
  );
  dispatch_event(
    EventTargetId::Document,
    &mut event,
    &doc,
    &registry,
    &mut invoker,
  )
  .unwrap();

  assert_eq!(invoker.calls.as_slice(), &["stop"]);
  assert!(!event.propagation_stopped);
  assert!(!event.immediate_propagation_stopped);
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
  assert!(
    !event.propagation_stopped,
    "propagation_stopped must be cleared after dispatch returns"
  );
  assert!(
    !event.immediate_propagation_stopped,
    "immediate_propagation_stopped must be cleared after dispatch returns"
  );
  assert!(
    event.path.is_empty(),
    "event.path must be cleared after dispatch returns"
  );
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
fn remove_and_readd_listener_during_dispatch_does_not_fire_in_current_dispatch() {
  let (doc, _a, _b, c) = make_dom_abc();
  let registry = EventListenerRegistry::new();

  let type_ = "x";
  let id_remove_and_readd = ListenerId::new(1);
  let id_removed = ListenerId::new(2);

  // First listener removes and re-adds the second listener while dispatch is in progress.
  //
  // DOM's algorithm snapshots the listener list but keeps a shared "removed" flag. This means the
  // re-added listener must not run until a subsequent dispatch.
  assert!(registry.add_event_listener(
    EventTargetId::Node(c),
    type_,
    id_remove_and_readd,
    AddEventListenerOptions {
      once: true,
      ..Default::default()
    }
  ));
  assert!(registry.add_event_listener(
    EventTargetId::Node(c),
    type_,
    id_removed,
    AddEventListenerOptions::default()
  ));

  let mut invoker = RecordingInvoker::new(
    &registry,
    EventTargetId::Node(c),
    [
      (
        id_remove_and_readd,
        Behavior {
          label: "remove_and_readd",
          expected_phase: EventPhase::AtTarget,
          expected_current_target: EventTargetId::Node(c),
          action: Action::RemoveAndReaddListener {
            target: EventTargetId::Node(c),
            type_,
            listener_id: id_removed,
            capture: false,
            options: AddEventListenerOptions::default(),
            expect_removed: Some(true),
            expect_added: Some(true),
          },
        },
      ),
      (
        id_removed,
        Behavior {
          label: "removed",
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

  assert_eq!(invoker.calls.as_slice(), &["remove_and_readd", "removed"]);
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
fn detached_node_event_path_does_not_include_document_or_window() {
  let (mut doc, _a, b, detached) = make_dom_abc();
  doc.node_mut(b).children.retain(|&child| child != detached);
  doc.node_mut(detached).parent = None;

  let registry = EventListenerRegistry::new();
  let type_ = "x";
  let id_window_capture = ListenerId::new(1);
  let id_document_capture = ListenerId::new(2);
  let id_window_bubble = ListenerId::new(3);
  let id_document_bubble = ListenerId::new(4);
  let id_node = ListenerId::new(5);

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
    EventTargetId::Window,
    type_,
    id_window_bubble,
    AddEventListenerOptions::default()
  ));
  assert!(registry.add_event_listener(
    EventTargetId::Document,
    type_,
    id_document_bubble,
    AddEventListenerOptions::default()
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
        id_node,
        Behavior {
          label: "node",
          expected_phase: EventPhase::AtTarget,
          expected_current_target: EventTargetId::Node(detached),
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

#[test]
fn template_contents_event_path_does_not_include_template_document_or_window() {
  let root =
    parse_html("<!doctype html><template><div id=in></div></template><div id=out></div>").unwrap();
  let doc = Document::from_renderer_dom(&root);

  let mut template_id: Option<NodeId> = None;
  let mut in_id: Option<NodeId> = None;
  let mut out_id: Option<NodeId> = None;

  for id in doc.subtree_preorder(doc.root()) {
    let NodeKind::Element {
      tag_name,
      namespace,
      attributes,
      ..
    } = &doc.node(id).kind
    else {
      continue;
    };

    if tag_name.eq_ignore_ascii_case("template") {
      template_id = Some(id);
    }

    if tag_name.eq_ignore_ascii_case("div") {
      let is_html = doc.is_html_case_insensitive_namespace(namespace);
      let id_attr = attributes
        .iter()
        .find(|attr| attr.qualified_name_matches("id", is_html))
        .map(|attr| attr.value.as_str());
      match id_attr {
        Some("in") => in_id = Some(id),
        Some("out") => out_id = Some(id),
        _ => {}
      }
    }
  }

  let template_id = template_id.expect("template element not found");
  let in_id = in_id.expect("template content node not found");
  let out_id = out_id.expect("outside node not found");

  assert!(
    doc.node(template_id).inert_subtree,
    "<template> should mark inert_subtree"
  );
  assert_eq!(
    doc.node(in_id).parent,
    Some(template_id),
    "expected template content node to be a child of the <template> element"
  );
  assert!(doc.is_descendant_of_inert_template(in_id));
  assert!(!doc.is_descendant_of_inert_template(out_id));

  let registry = EventListenerRegistry::new();
  let type_ = "x";

  let id_window_capture = ListenerId::new(1);
  let id_document_capture = ListenerId::new(2);
  let id_template_capture = ListenerId::new(3);
  let id_in = ListenerId::new(4);
  let id_template_bubble = ListenerId::new(5);
  let id_document_bubble = ListenerId::new(6);
  let id_window_bubble = ListenerId::new(7);

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
    EventTargetId::Node(template_id),
    type_,
    id_template_capture,
    AddEventListenerOptions {
      capture: true,
      ..Default::default()
    }
  ));
  assert!(registry.add_event_listener(
    EventTargetId::Node(in_id),
    type_,
    id_in,
    AddEventListenerOptions::default()
  ));
  assert!(registry.add_event_listener(
    EventTargetId::Node(template_id),
    type_,
    id_template_bubble,
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
    EventTargetId::Node(in_id),
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
        id_template_capture,
        Behavior {
          label: "template_capture",
          expected_phase: EventPhase::Capturing,
          expected_current_target: EventTargetId::Node(template_id),
          action: Action::None,
        },
      ),
      (
        id_in,
        Behavior {
          label: "in",
          expected_phase: EventPhase::AtTarget,
          expected_current_target: EventTargetId::Node(in_id),
          action: Action::None,
        },
      ),
      (
        id_template_bubble,
        Behavior {
          label: "template_bubble",
          expected_phase: EventPhase::Bubbling,
          expected_current_target: EventTargetId::Node(template_id),
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
  dispatch_event(
    EventTargetId::Node(in_id),
    &mut event,
    &doc,
    &registry,
    &mut invoker,
  )
  .unwrap();

  assert_eq!(invoker.calls.as_slice(), &["in"]);
}

#[test]
fn shadow_root_event_path_respects_composed_flag() {
  let html = "<!doctype html><div id=host><template shadowroot=open><span id=inner></span></template><p id=light></p></div>";
  let doc = crate::dom2::parse_html(html).unwrap();
  let host = doc.get_element_by_id("host").expect("host element not found");
  let shadow_root = doc
    .node(host)
    .children
    .iter()
    .copied()
    .find(|&child| matches!(doc.node(child).kind, NodeKind::ShadowRoot { .. }))
    .expect("expected host to have an attached shadow root");
  let inner = find_node_id_anywhere(&doc, "inner").expect("shadow node not found");
  assert_eq!(doc.containing_shadow_root(inner), Some(shadow_root));

  let registry = EventListenerRegistry::new();
  let type_ = "x";

  let id_window_capture = ListenerId::new(1);
  let id_document_capture = ListenerId::new(2);
  let id_host_capture = ListenerId::new(3);
  let id_shadow_root_capture = ListenerId::new(4);
  let id_inner_capture = ListenerId::new(5);

  let id_inner_bubble = ListenerId::new(6);
  let id_shadow_root_bubble = ListenerId::new(7);
  let id_host_bubble = ListenerId::new(8);
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
    EventTargetId::Node(host),
    type_,
    id_host_capture,
    AddEventListenerOptions {
      capture: true,
      ..Default::default()
    }
  ));
  assert!(registry.add_event_listener(
    EventTargetId::Node(shadow_root),
    type_,
    id_shadow_root_capture,
    AddEventListenerOptions {
      capture: true,
      ..Default::default()
    }
  ));
  assert!(registry.add_event_listener(
    EventTargetId::Node(inner),
    type_,
    id_inner_capture,
    AddEventListenerOptions {
      capture: true,
      ..Default::default()
    }
  ));

  assert!(registry.add_event_listener(
    EventTargetId::Node(inner),
    type_,
    id_inner_bubble,
    AddEventListenerOptions::default()
  ));
  assert!(registry.add_event_listener(
    EventTargetId::Node(shadow_root),
    type_,
    id_shadow_root_bubble,
    AddEventListenerOptions::default()
  ));
  assert!(registry.add_event_listener(
    EventTargetId::Node(host),
    type_,
    id_host_bubble,
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

  #[derive(Debug, Clone, Copy)]
  struct Expectation {
    label: &'static str,
    expected_phase: EventPhase,
    expected_current_target: EventTargetId,
    expected_target: EventTargetId,
  }

  struct ExpectingInvoker {
    calls: Vec<&'static str>,
    expectations: HashMap<ListenerId, Expectation>,
  }

  impl ExpectingInvoker {
    fn new(expectations: impl IntoIterator<Item = (ListenerId, Expectation)>) -> Self {
      Self {
        calls: Vec::new(),
        expectations: expectations.into_iter().collect(),
      }
    }
  }

  impl EventListenerInvoker for ExpectingInvoker {
    fn invoke(&mut self, listener_id: ListenerId, event: &mut Event) -> Result<(), DomError> {
      let expect = *self.expectations.get(&listener_id).unwrap_or_else(|| {
        panic!("unexpected listener_id during dispatch: {listener_id:?}");
      });
      assert_eq!(event.target, Some(expect.expected_target));
      assert_eq!(event.current_target, Some(expect.expected_current_target));
      assert_eq!(event.event_phase, expect.expected_phase);
      self.calls.push(expect.label);
      Ok(())
    }
  }

  // Non-composed events must not escape from the shadow root to the host/document/window. Since the
  // event never leaves the shadow tree, no retargeting occurs.
  let mut invoker = ExpectingInvoker::new([
    (
      id_shadow_root_capture,
      Expectation {
        label: "shadow_root_capture",
        expected_phase: EventPhase::Capturing,
        expected_current_target: EventTargetId::Node(shadow_root),
        expected_target: EventTargetId::Node(inner),
      },
    ),
    (
      id_inner_capture,
      Expectation {
        label: "inner_capture",
        expected_phase: EventPhase::AtTarget,
        expected_current_target: EventTargetId::Node(inner),
        expected_target: EventTargetId::Node(inner),
      },
    ),
    (
      id_inner_bubble,
      Expectation {
        label: "inner_bubble",
        expected_phase: EventPhase::AtTarget,
        expected_current_target: EventTargetId::Node(inner),
        expected_target: EventTargetId::Node(inner),
      },
    ),
    (
      id_shadow_root_bubble,
      Expectation {
        label: "shadow_root_bubble",
        expected_phase: EventPhase::Bubbling,
        expected_current_target: EventTargetId::Node(shadow_root),
        expected_target: EventTargetId::Node(inner),
      },
    ),
  ]);
  let mut event = Event::new(
    type_,
    EventInit {
      bubbles: true,
      composed: false,
      ..Default::default()
    },
  );
  dispatch_event(
    EventTargetId::Node(inner),
    &mut event,
    &doc,
    &registry,
    &mut invoker,
  )
  .unwrap();
  assert_eq!(
    invoker.calls.as_slice(),
    &[
      "shadow_root_capture",
      "inner_capture",
      "inner_bubble",
      "shadow_root_bubble"
    ]
  );

  // Composed events do escape the shadow root. Targets outside the shadow tree observe the host as
  // `event.target` (retargeting).
  //
  // Additionally, the shadow-adjusted target causes the host to observe `AT_TARGET` in both capture
  // and bubble passes (this is different from the normal capturing/bubbling phases).
  let mut invoker = ExpectingInvoker::new([
    (
      id_window_capture,
      Expectation {
        label: "window_capture",
        expected_phase: EventPhase::Capturing,
        expected_current_target: EventTargetId::Window,
        expected_target: EventTargetId::Node(host),
      },
    ),
    (
      id_document_capture,
      Expectation {
        label: "document_capture",
        expected_phase: EventPhase::Capturing,
        expected_current_target: EventTargetId::Document,
        expected_target: EventTargetId::Node(host),
      },
    ),
    (
      id_host_capture,
      Expectation {
        label: "host_capture",
        expected_phase: EventPhase::AtTarget,
        expected_current_target: EventTargetId::Node(host),
        expected_target: EventTargetId::Node(host),
      },
    ),
    (
      id_shadow_root_capture,
      Expectation {
        label: "shadow_root_capture",
        expected_phase: EventPhase::Capturing,
        expected_current_target: EventTargetId::Node(shadow_root),
        expected_target: EventTargetId::Node(inner),
      },
    ),
    (
      id_inner_capture,
      Expectation {
        label: "inner_capture",
        expected_phase: EventPhase::AtTarget,
        expected_current_target: EventTargetId::Node(inner),
        expected_target: EventTargetId::Node(inner),
      },
    ),
    (
      id_inner_bubble,
      Expectation {
        label: "inner_bubble",
        expected_phase: EventPhase::AtTarget,
        expected_current_target: EventTargetId::Node(inner),
        expected_target: EventTargetId::Node(inner),
      },
    ),
    (
      id_shadow_root_bubble,
      Expectation {
        label: "shadow_root_bubble",
        expected_phase: EventPhase::Bubbling,
        expected_current_target: EventTargetId::Node(shadow_root),
        expected_target: EventTargetId::Node(inner),
      },
    ),
    (
      id_host_bubble,
      Expectation {
        label: "host_bubble",
        expected_phase: EventPhase::AtTarget,
        expected_current_target: EventTargetId::Node(host),
        expected_target: EventTargetId::Node(host),
      },
    ),
    (
      id_document_bubble,
      Expectation {
        label: "document_bubble",
        expected_phase: EventPhase::Bubbling,
        expected_current_target: EventTargetId::Document,
        expected_target: EventTargetId::Node(host),
      },
    ),
    (
      id_window_bubble,
      Expectation {
        label: "window_bubble",
        expected_phase: EventPhase::Bubbling,
        expected_current_target: EventTargetId::Window,
        expected_target: EventTargetId::Node(host),
      },
    ),
  ]);
  let mut event = Event::new(
    type_,
    EventInit {
      bubbles: true,
      composed: true,
      ..Default::default()
    },
  );
  dispatch_event(
    EventTargetId::Node(inner),
    &mut event,
    &doc,
    &registry,
    &mut invoker,
  )
  .unwrap();
  assert_eq!(
    invoker.calls.as_slice(),
    &[
      "window_capture",
      "document_capture",
      "host_capture",
      "shadow_root_capture",
      "inner_capture",
      "inner_bubble",
      "shadow_root_bubble",
      "host_bubble",
      "document_bubble",
      "window_bubble"
    ]
  );
}

#[test]
fn closed_shadow_root_composed_path_hides_internal_nodes_from_outside() {
  let html =
    "<!doctype html><div id=host><template shadowroot=closed><span id=inner></span></template></div>";
  let doc = crate::dom2::parse_html(html).unwrap();
  let host = doc.get_element_by_id("host").expect("host element not found");
  let shadow_root = doc
    .node(host)
    .children
    .iter()
    .copied()
    .find(|&child| {
      matches!(
        doc.node(child).kind,
        NodeKind::ShadowRoot {
          mode: crate::dom::ShadowRootMode::Closed,
          ..
        }
      )
    })
    .expect("expected host to have an attached closed shadow root");
  let inner = find_node_id_anywhere(&doc, "inner").expect("shadow node not found");
  assert_eq!(doc.containing_shadow_root(inner), Some(shadow_root));

  let registry = EventListenerRegistry::new();
  let type_ = "x";

  let id_document_capture = ListenerId::new(1);
  let id_inner = ListenerId::new(2);

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
    EventTargetId::Node(inner),
    type_,
    id_inner,
    AddEventListenerOptions::default()
  ));

  struct ComposedPathInvoker {
    calls: Vec<&'static str>,
    paths: HashMap<&'static str, Vec<EventTargetId>>,
    host: NodeId,
    shadow_root: NodeId,
    inner: NodeId,
    id_document_capture: ListenerId,
    id_inner: ListenerId,
  }

  impl EventListenerInvoker for ComposedPathInvoker {
    fn invoke(&mut self, listener_id: ListenerId, event: &mut Event) -> Result<(), DomError> {
      if listener_id == self.id_document_capture {
        assert_eq!(event.current_target, Some(EventTargetId::Document));
        assert_eq!(event.event_phase, EventPhase::Capturing);
        assert_eq!(event.target, Some(EventTargetId::Node(self.host)));
        self.calls.push("document_capture");
        self.paths.insert("document_capture", event.composed_path());
        return Ok(());
      }
      if listener_id == self.id_inner {
        assert_eq!(event.current_target, Some(EventTargetId::Node(self.inner)));
        assert_eq!(event.event_phase, EventPhase::AtTarget);
        assert_eq!(event.target, Some(EventTargetId::Node(self.inner)));
        self.calls.push("inner");
        self.paths.insert("inner", event.composed_path());
        return Ok(());
      }

      panic!("unexpected listener_id during dispatch: {listener_id:?}");
    }
  }

  let mut invoker = ComposedPathInvoker {
    calls: Vec::new(),
    paths: HashMap::new(),
    host,
    shadow_root,
    inner,
    id_document_capture,
    id_inner,
  };

  let mut event = Event::new(
    type_,
    EventInit {
      bubbles: true,
      composed: true,
      ..Default::default()
    },
  );
  dispatch_event(
    EventTargetId::Node(inner),
    &mut event,
    &doc,
    &registry,
    &mut invoker,
  )
  .unwrap();
  assert_eq!(invoker.calls.as_slice(), &["document_capture", "inner"]);

  let outside = invoker
    .paths
    .get("document_capture")
    .expect("document_capture should record composed path");
  assert_eq!(
    outside.first().copied(),
    Some(EventTargetId::Node(host)),
    "outside listeners should observe the host as the first entry"
  );
  assert!(
    !outside.contains(&EventTargetId::Node(inner)),
    "outside listeners must not observe nodes inside a closed shadow root"
  );
  assert!(
    !outside.contains(&EventTargetId::Node(shadow_root)),
    "outside listeners must not observe a closed shadow root node in composed_path()"
  );
  assert!(outside.contains(&EventTargetId::Document));
  assert!(outside.contains(&EventTargetId::Window));

  let inside = invoker
    .paths
    .get("inner")
    .expect("inner should record composed path");
  assert_eq!(
    inside.get(0).copied(),
    Some(EventTargetId::Node(inner)),
    "inside listeners should observe the original target first"
  );
  assert_eq!(
    inside.get(1).copied(),
    Some(EventTargetId::Node(shadow_root)),
    "inside listeners should observe the closed shadow root in composed_path()"
  );
  assert!(inside.contains(&EventTargetId::Node(host)));
  assert!(inside.contains(&EventTargetId::Document));
  assert!(inside.contains(&EventTargetId::Window));
}

#[test]
fn slot_in_closed_tree_allows_composed_path_to_include_slotted_nodes() {
  // When a slottable is assigned into a closed shadow tree, the event path includes internal nodes
  // (slot + shadow root). `Event.composedPath()` must hide those internal nodes from outside
  // listeners *without* hiding the slotted node itself.
  let html = "<!doctype html>\
    <div id=host>\
      <template shadowroot=closed><slot id=slot></slot></template>\
      <span id=slotted></span>\
    </div>";
  let doc = crate::dom2::parse_html(html).unwrap();
  let host = find_node_by_id(&doc, doc.root(), "host").expect("host not found");
  let shadow_root = find_shadow_root(&doc, host).expect("shadow root not found");
  let slot = find_node_by_id(&doc, shadow_root, "slot").expect("slot not found");
  let slotted = find_node_by_id(&doc, host, "slotted").expect("slotted node not found");

  let registry = EventListenerRegistry::new();
  let type_ = "x";
  let id_document_capture = ListenerId::new(1);
  let id_slot_bubble = ListenerId::new(2);

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
    EventTargetId::Node(slot),
    type_,
    id_slot_bubble,
    AddEventListenerOptions::default()
  ));

  let mut invoker = TraceInvoker {
    labels: HashMap::from([
      (id_document_capture, "document_capture"),
      (id_slot_bubble, "slot_bubble"),
    ]),
    calls: Vec::new(),
  };

  let mut event = Event::new(
    type_,
    EventInit {
      bubbles: true,
      composed: true,
      ..Default::default()
    },
  );
  dispatch_event(
    EventTargetId::Node(slotted),
    &mut event,
    &doc,
    &registry,
    &mut invoker,
  )
  .unwrap();

  assert_eq!(
    invoker.calls.iter().map(|c| c.label).collect::<Vec<_>>(),
    vec!["document_capture", "slot_bubble"],
    "expected document capture + slot bubble listeners to be invoked"
  );

  let document_call = invoker
    .calls
    .iter()
    .find(|c| c.label == "document_capture")
    .expect("document_capture should record composed path");
  assert_eq!(document_call.target, Some(EventTargetId::Node(slotted)));
  assert_eq!(document_call.event_phase, EventPhase::Capturing);
  assert_eq!(document_call.current_target, Some(EventTargetId::Document));

  // Outside listeners should not see closed shadow internals in composed_path(), but must still see
  // the slotted node.
  assert_eq!(
    document_call.composed_path.first().copied(),
    Some(EventTargetId::Node(slotted))
  );
  assert!(document_call.composed_path.contains(&EventTargetId::Node(host)));
  assert!(document_call.composed_path.contains(&EventTargetId::Document));
  assert!(document_call.composed_path.contains(&EventTargetId::Window));
  assert!(
    !document_call.composed_path.contains(&EventTargetId::Node(slot)),
    "outside listeners must not observe nodes inside a closed shadow root"
  );
  assert!(
    !document_call
      .composed_path
      .contains(&EventTargetId::Node(shadow_root)),
    "outside listeners must not observe a closed shadow root node in composed_path()"
  );

  // Inside listeners (slot) should be able to see closed tree internals.
  let slot_call = invoker
    .calls
    .iter()
    .find(|c| c.label == "slot_bubble")
    .expect("slot_bubble should record composed path");
  assert_eq!(slot_call.current_target, Some(EventTargetId::Node(slot)));
  assert_eq!(slot_call.event_phase, EventPhase::Bubbling);
  assert!(slot_call.composed_path.contains(&EventTargetId::Node(slot)));
  assert!(slot_call.composed_path.contains(&EventTargetId::Node(shadow_root)));
  assert!(slot_call.composed_path.contains(&EventTargetId::Node(slotted)));
}

#[test]
fn has_listeners_for_dispatch_respects_composed_shadow_boundary() {
  let html = "<!doctype html><div id=host><template shadowroot=open><span id=inner></span></template><p id=light></p></div>";
  let doc = crate::dom2::parse_html(html).unwrap();
  let host = doc.get_element_by_id("host").expect("host element not found");
  let inner = find_node_id_anywhere(&doc, "inner").expect("shadow node not found");

  let registry = EventListenerRegistry::new();
  let type_ = "x";
  assert!(registry.add_event_listener(
    EventTargetId::Node(host),
    type_,
    ListenerId::new(1),
    AddEventListenerOptions::default()
  ));

  assert!(
    !registry.has_listeners_for_dispatch(
      EventTargetId::Node(inner),
      type_,
      &doc,
      /* bubbles */ true,
      /* composed */ false
    ),
    "non-composed events inside a shadow tree must not see light-DOM host listeners"
  );
  assert!(
    registry.has_listeners_for_dispatch(
      EventTargetId::Node(inner),
      type_,
      &doc,
      /* bubbles */ true,
      /* composed */ true
    ),
    "composed events should be able to reach the host's listeners"
  );
}

#[test]
fn debug_does_not_borrow_listener_map() {
  let registry = EventListenerRegistry::new();

  // Hold a mutable borrow of the underlying listener map, then ensure Debug formatting does not
  // panic (Debug must not borrow the RefCell).
  let _guard = registry.listeners.borrow_mut();
  let _formatted = format!("{registry:?}");
}

#[test]
fn passive_listeners_cannot_set_default_prevented() {
  let (doc, _a, _b, c) = make_dom_abc();
  let registry = EventListenerRegistry::new();

  let type_ = "x";
  let id_passive = ListenerId::new(1);

  assert!(registry.add_event_listener(
    EventTargetId::Node(c),
    type_,
    id_passive,
    AddEventListenerOptions {
      passive: true,
      ..Default::default()
    }
  ));

  let mut invoker = RecordingInvoker::new(
    &registry,
    EventTargetId::Node(c),
    [(
      id_passive,
      Behavior {
        label: "passive",
        expected_phase: EventPhase::AtTarget,
        expected_current_target: EventTargetId::Node(c),
        action: Action::PreventDefault,
      },
    )],
  );

  let mut event = Event::new(
    type_,
    EventInit {
      bubbles: true,
      cancelable: true,
      ..Default::default()
    },
  );
  let res = dispatch_event(
    EventTargetId::Node(c),
    &mut event,
    &doc,
    &registry,
    &mut invoker,
  )
  .unwrap();

  assert!(res, "dispatchEvent should return true if not canceled");
  assert!(
    !event.default_prevented,
    "passive listeners must not set defaultPrevented"
  );
}

#[test]
fn duplicate_add_event_listener_is_noop() {
  let (doc, _a, _b, c) = make_dom_abc();
  let registry = EventListenerRegistry::new();

  let type_ = "x";
  let id = ListenerId::new(1);

  assert!(registry.add_event_listener(
    EventTargetId::Node(c),
    type_,
    id,
    AddEventListenerOptions::default()
  ));
  assert!(
    !registry.add_event_listener(
      EventTargetId::Node(c),
      type_,
      id,
      AddEventListenerOptions::default()
    ),
    "duplicate addEventListener should be ignored"
  );

  let mut invoker = RecordingInvoker::new(
    &registry,
    EventTargetId::Node(c),
    [(
      id,
      Behavior {
        label: "listener",
        expected_phase: EventPhase::AtTarget,
        expected_current_target: EventTargetId::Node(c),
        action: Action::None,
      },
    )],
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

  assert_eq!(invoker.calls.as_slice(), &["listener"]);
}

#[test]
fn document_node_id_normalizes_to_document() {
  let (doc, _a, _b, _c) = make_dom_abc();
  let registry = EventListenerRegistry::new();

  let type_ = "x";
  let id = ListenerId::new(1);
  let doc_node_id = doc.root();

  // Registering on the document node itself must behave like registering on `Document`.
  assert!(registry.add_event_listener(
    EventTargetId::Node(doc_node_id),
    type_,
    id,
    AddEventListenerOptions::default()
  ));

  let mut invoker = RecordingInvoker::new(
    &registry,
    EventTargetId::Document,
    [(
      id,
      Behavior {
        label: "document_listener",
        expected_phase: EventPhase::AtTarget,
        expected_current_target: EventTargetId::Document,
        action: Action::None,
      },
    )],
  );

  let mut event = Event::new(type_, EventInit::default());
  dispatch_event(
    EventTargetId::Node(doc_node_id),
    &mut event,
    &doc,
    &registry,
    &mut invoker,
  )
  .unwrap();

  assert_eq!(invoker.calls.as_slice(), &["document_listener"]);
}

#[test]
fn document_create_event_and_init_event_do_not_crash() {
  let doc = Document::new(QuirksMode::NoQuirks);
  let mut e = doc.create_event("Event").expect("createEvent(Event)");
  assert_eq!(e.type_, "");
  assert!(!e.bubbles);
  assert!(!e.cancelable);
  assert_eq!(e.detail, None);

  e.init_event("x", true, true);
  assert_eq!(e.type_, "x");
  assert!(e.bubbles);
  assert!(e.cancelable);
}

#[test]
fn document_create_event_and_init_custom_event_set_detail() {
  let doc = Document::new(QuirksMode::NoQuirks);
  let mut ce = doc
    .create_event("CustomEvent")
    .expect("createEvent(CustomEvent)");

  assert_eq!(ce.detail, Some(JsValue::Null));

  ce.init_custom_event("x", true, true, JsValue::Number(123.0));
  assert_eq!(ce.type_, "x");
  assert!(ce.bubbles);
  assert!(ce.cancelable);
  assert_eq!(ce.detail, Some(JsValue::Number(123.0)));
}

#[test]
fn custom_event_constructor_sets_detail() {
  let ce = Event::new_custom_event(
    "x",
    CustomEventInit {
      detail: JsValue::Number(1.0),
      ..Default::default()
    },
  );
  assert_eq!(ce.type_, "x");
  assert_eq!(ce.detail, Some(JsValue::Number(1.0)));
}

#[test]
fn create_event_rejects_unsupported_interfaces_with_not_supported_error() {
  let doc = Document::new(QuirksMode::NoQuirks);
  let err = doc
    .create_event("KeyboardEvent")
    .expect_err("unsupported createEvent should error");
  assert!(
    matches!(err, DomException::NotSupportedError { .. }),
    "expected NotSupportedError, got {err:?}"
  );
}

#[test]
fn dispatch_event_returns_false_if_prevent_default_called() {
  let doc = Document::new(QuirksMode::NoQuirks);
  let registry = EventListenerRegistry::new();

  let listener_id = ListenerId::new(1);
  assert!(registry.add_event_listener(
    EventTargetId::Document,
    "x",
    listener_id,
    AddEventListenerOptions::default(),
  ));

  let mut invoker = RecordingInvoker::new(
    &registry,
    EventTargetId::Document,
    [(
      listener_id,
      Behavior {
        label: "document_listener",
        expected_phase: EventPhase::AtTarget,
        expected_current_target: EventTargetId::Document,
        action: Action::PreventDefault,
      },
    )],
  );

  let mut event = Event::new(
    "x",
    EventInit {
      cancelable: true,
      ..Default::default()
    },
  );

  let ok = dispatch_event(
    EventTargetId::Document,
    &mut event,
    &doc,
    &registry,
    &mut invoker,
  )
  .unwrap();
  assert!(!ok, "dispatchEvent should return false when canceled");
  assert!(event.default_prevented);
  assert_eq!(invoker.calls.as_slice(), &["document_listener"]);
}

#[test]
fn dispatch_event_clears_event_path_after_dispatch() {
  let doc = Document::new(QuirksMode::NoQuirks);
  let registry = EventListenerRegistry::new();

  let type_ = "x";
  let listener_id = ListenerId::new(1);
  assert!(registry.add_event_listener(
    EventTargetId::Document,
    type_,
    listener_id,
    AddEventListenerOptions::default(),
  ));

  struct PathAssertingInvoker {
    invoked: bool,
  }

  impl EventListenerInvoker for PathAssertingInvoker {
    fn invoke(&mut self, expected_id: ListenerId, event: &mut Event) -> Result<(), DomError> {
      assert_eq!(expected_id, ListenerId::new(1));
      assert!(
        !event.path.is_empty(),
        "event.path must be populated during listener invocation"
      );
      self.invoked = true;
      Ok(())
    }
  }

  let mut invoker = PathAssertingInvoker { invoked: false };
  let mut event = Event::new(type_, EventInit::default());
  assert!(
    dispatch_event(
      EventTargetId::Document,
      &mut event,
      &doc,
      &registry,
      &mut invoker
    )
    .unwrap(),
    "dispatchEvent should return true when not canceled"
  );
  assert!(invoker.invoked, "expected the listener to be invoked");
  assert!(
    event.path.is_empty(),
    "event.path must be cleared after dispatch returns"
  );
}

fn node_id_attribute(kind: &NodeKind) -> Option<&str> {
  match kind {
    NodeKind::Element { attributes, .. } | NodeKind::Slot { attributes, .. } => attributes
      .iter()
      .find(|attr| attr.qualified_name_matches("id", /* is_html */ true))
      .map(|attr| attr.value.as_str()),
    _ => None,
  }
}

fn find_node_by_id(doc: &Document, root: NodeId, id: &str) -> Option<NodeId> {
  doc.subtree_preorder(root).find(|&node_id| node_id_attribute(&doc.node(node_id).kind) == Some(id))
}

fn find_shadow_root(doc: &Document, host: NodeId) -> Option<NodeId> {
  doc
    .node(host)
    .children
    .iter()
    .copied()
    .find(|&child| doc.node(child).parent == Some(host) && matches!(doc.node(child).kind, NodeKind::ShadowRoot { .. }))
}

#[derive(Debug)]
struct TraceCall {
  label: &'static str,
  target: Option<EventTargetId>,
  current_target: Option<EventTargetId>,
  event_phase: EventPhase,
  composed_path: Vec<EventTargetId>,
}

struct TraceInvoker {
  labels: HashMap<ListenerId, &'static str>,
  calls: Vec<TraceCall>,
}

impl EventListenerInvoker for TraceInvoker {
  fn invoke(&mut self, listener_id: ListenerId, event: &mut Event) -> Result<(), DomError> {
    let label = *self
      .labels
      .get(&listener_id)
      .unwrap_or_else(|| panic!("unknown listener_id: {listener_id:?}"));
    self.calls.push(TraceCall {
      label,
      target: event.target,
      current_target: event.current_target,
      event_phase: event.event_phase,
      composed_path: event.composed_path(),
    });
    Ok(())
  }
}

#[test]
fn shadow_dom_composed_false_does_not_cross_shadow_root() {
  let html = "<!doctype html><div id=host><template shadowroot=open><span id=inner></span></template></div>";
  let doc = crate::dom2::parse_html(html).unwrap();
  let host = find_node_by_id(&doc, doc.root(), "host").expect("host not found");
  let shadow_root = find_shadow_root(&doc, host).expect("shadow root not found");
  let inner = find_node_by_id(&doc, shadow_root, "inner").expect("inner not found");

  let registry = EventListenerRegistry::new();
  let type_ = "x";
  let id_inner = ListenerId::new(1);
  let id_shadow_root = ListenerId::new(2);
  let id_host = ListenerId::new(3);
  let id_document = ListenerId::new(4);
  let id_window = ListenerId::new(5);

  assert!(registry.add_event_listener(
    EventTargetId::Node(inner),
    type_,
    id_inner,
    AddEventListenerOptions::default()
  ));
  assert!(registry.add_event_listener(
    EventTargetId::Node(shadow_root),
    type_,
    id_shadow_root,
    AddEventListenerOptions::default()
  ));
  assert!(registry.add_event_listener(
    EventTargetId::Node(host),
    type_,
    id_host,
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

  let mut invoker = TraceInvoker {
    labels: HashMap::from([
      (id_inner, "inner"),
      (id_shadow_root, "shadow_root"),
      (id_host, "host"),
      (id_document, "document"),
      (id_window, "window"),
    ]),
    calls: Vec::new(),
  };

  let mut event = Event::new(
    type_,
    EventInit {
      bubbles: true,
      composed: false,
      ..Default::default()
    },
  );
  dispatch_event(
    EventTargetId::Node(inner),
    &mut event,
    &doc,
    &registry,
    &mut invoker,
  )
  .unwrap();

  // `composed=false` must not invoke listeners outside the shadow root.
  assert_eq!(
    invoker.calls.iter().map(|c| c.label).collect::<Vec<_>>(),
    vec!["inner", "shadow_root"]
  );
  assert_eq!(invoker.calls[0].target, Some(EventTargetId::Node(inner)));
  assert_eq!(invoker.calls[0].current_target, Some(EventTargetId::Node(inner)));
  assert_eq!(invoker.calls[0].event_phase, EventPhase::AtTarget);

  assert_eq!(invoker.calls[1].target, Some(EventTargetId::Node(inner)));
  assert_eq!(
    invoker.calls[1].current_target,
    Some(EventTargetId::Node(shadow_root))
  );
  assert_eq!(invoker.calls[1].event_phase, EventPhase::Bubbling);
}

#[test]
fn shadow_dom_composed_true_retargets_target_outside_shadow_tree() {
  let html = "<!doctype html><div id=host><template shadowroot=open><span id=inner></span></template></div>";
  let doc = crate::dom2::parse_html(html).unwrap();
  let host = find_node_by_id(&doc, doc.root(), "host").expect("host not found");
  let shadow_root = find_shadow_root(&doc, host).expect("shadow root not found");
  let inner = find_node_by_id(&doc, shadow_root, "inner").expect("inner not found");

  let registry = EventListenerRegistry::new();
  let type_ = "x";
  let id_inner = ListenerId::new(1);
  let id_shadow_root = ListenerId::new(2);
  let id_host = ListenerId::new(3);
  let id_document = ListenerId::new(4);

  assert!(registry.add_event_listener(
    EventTargetId::Node(inner),
    type_,
    id_inner,
    AddEventListenerOptions::default()
  ));
  assert!(registry.add_event_listener(
    EventTargetId::Node(shadow_root),
    type_,
    id_shadow_root,
    AddEventListenerOptions::default()
  ));
  assert!(registry.add_event_listener(
    EventTargetId::Node(host),
    type_,
    id_host,
    AddEventListenerOptions::default()
  ));
  assert!(registry.add_event_listener(
    EventTargetId::Document,
    type_,
    id_document,
    AddEventListenerOptions::default()
  ));

  let mut invoker = TraceInvoker {
    labels: HashMap::from([
      (id_inner, "inner"),
      (id_shadow_root, "shadow_root"),
      (id_host, "host"),
      (id_document, "document"),
    ]),
    calls: Vec::new(),
  };

  let mut event = Event::new(
    type_,
    EventInit {
      bubbles: true,
      composed: true,
      ..Default::default()
    },
  );
  dispatch_event(
    EventTargetId::Node(inner),
    &mut event,
    &doc,
    &registry,
    &mut invoker,
  )
  .unwrap();

  // `composed=true` must invoke listeners outside the shadow root and retarget `event.target` to the
  // host for those listeners.
  assert!(
    invoker.calls.iter().any(|c| c.label == "host"),
    "expected host listener to be invoked"
  );
  assert!(
    invoker.calls.iter().any(|c| c.label == "document"),
    "expected document listener to be invoked"
  );

  let inner_call = invoker
    .calls
    .iter()
    .find(|c| c.label == "inner")
    .expect("inner listener not invoked");
  assert_eq!(inner_call.target, Some(EventTargetId::Node(inner)));
  assert_eq!(inner_call.event_phase, EventPhase::AtTarget);

  let shadow_root_call = invoker
    .calls
    .iter()
    .find(|c| c.label == "shadow_root")
    .expect("shadow root listener not invoked");
  assert_eq!(shadow_root_call.target, Some(EventTargetId::Node(inner)));
  assert_eq!(shadow_root_call.event_phase, EventPhase::Bubbling);

  let host_call = invoker
    .calls
    .iter()
    .find(|c| c.label == "host")
    .expect("host listener not invoked");
  assert_eq!(host_call.target, Some(EventTargetId::Node(host)));
  assert_eq!(host_call.event_phase, EventPhase::AtTarget);

  let document_call = invoker
    .calls
    .iter()
    .find(|c| c.label == "document")
    .expect("document listener not invoked");
  assert_eq!(document_call.target, Some(EventTargetId::Node(host)));
  assert_eq!(document_call.event_phase, EventPhase::Bubbling);

  // For open shadow roots, composedPath() is allowed to expose the shadow tree to outside listeners.
  assert_eq!(
    document_call.composed_path.first(),
    Some(&EventTargetId::Node(inner))
  );
  assert!(
    document_call
      .composed_path
      .contains(&EventTargetId::Node(shadow_root)),
    "expected composedPath() to include the shadow root for open mode"
  );
  assert_eq!(
    document_call.composed_path.last(),
    Some(&EventTargetId::Window),
    "expected composedPath() to end at Window"
  );
}

#[test]
fn transfer_node_listeners_moves_listeners_between_registries_and_remaps_node_ids() {
  let (src_doc, _a, _b, old_node_id) = make_dom_abc();

  // Create a second document with a different node indexing so `old_node_id != new_node_id`.
  // Document → <a> → <b> → <x> → <c>
  let root = DomNode {
    node_type: DomNodeType::Document {
      quirks_mode: QuirksMode::NoQuirks,
      scripting_enabled: true,
      is_html_document: true,
    },
    children: vec![element(
      "a",
      vec![element("b", vec![element("x", vec![element("c", vec![])])])],
    )],
  };
  let dst_doc = Document::from_renderer_dom(&root);
  let dst_root_id = dst_doc.root();
  let dst_a = first_element_child(&dst_doc, dst_root_id);
  let dst_b = first_element_child(&dst_doc, dst_a);
  let dst_x = first_element_child(&dst_doc, dst_b);
  let new_node_id = first_element_child(&dst_doc, dst_x);

  assert_ne!(
    old_node_id, new_node_id,
    "expected different node IDs across documents to validate remapping"
  );

  let src = EventListenerRegistry::new();
  let dst = EventListenerRegistry::new();

  let type_x = "x";
  let type_y = "y";

  let id_bubble_1 = ListenerId::new(1);
  let id_capture_1 = ListenerId::new(2);
  let id_bubble_2 = ListenerId::new(3);
  let id_capture_once = ListenerId::new(4);
  let id_other_type = ListenerId::new(5);

  // Add a mix of listeners to the source registry.
  assert!(src.add_event_listener(
    EventTargetId::Node(old_node_id),
    type_x,
    id_bubble_1,
    AddEventListenerOptions::default(),
  ));
  assert!(src.add_event_listener(
    EventTargetId::Node(old_node_id),
    type_x,
    id_capture_1,
    AddEventListenerOptions {
      capture: true,
      ..Default::default()
    },
  ));
  assert!(src.add_event_listener(
    EventTargetId::Node(old_node_id),
    type_x,
    id_bubble_2,
    AddEventListenerOptions::default(),
  ));
  assert!(src.add_event_listener(
    EventTargetId::Node(old_node_id),
    type_x,
    id_capture_once,
    AddEventListenerOptions {
      capture: true,
      once: true,
      ..Default::default()
    },
  ));
  assert!(src.add_event_listener(
    EventTargetId::Node(old_node_id),
    type_y,
    id_other_type,
    AddEventListenerOptions::default(),
  ));

  // Snapshot the source registry so we can compare dispatch ordering without mutating the live
  // registry (some listeners are `once`).
  let src_snapshot = src.clone();
  let mut invoker_before = RecordingInvoker::new(
    &src_snapshot,
    EventTargetId::Node(old_node_id),
    [
      (
        id_bubble_1,
        Behavior {
          label: "bubble_1",
          expected_phase: EventPhase::AtTarget,
          expected_current_target: EventTargetId::Node(old_node_id),
          action: Action::None,
        },
      ),
      (
        id_capture_1,
        Behavior {
          label: "capture_1",
          expected_phase: EventPhase::AtTarget,
          expected_current_target: EventTargetId::Node(old_node_id),
          action: Action::None,
        },
      ),
      (
        id_bubble_2,
        Behavior {
          label: "bubble_2",
          expected_phase: EventPhase::AtTarget,
          expected_current_target: EventTargetId::Node(old_node_id),
          action: Action::None,
        },
      ),
      (
        id_capture_once,
        Behavior {
          label: "capture_once",
          expected_phase: EventPhase::AtTarget,
          expected_current_target: EventTargetId::Node(old_node_id),
          action: Action::None,
        },
      ),
    ],
  );

  let mut event = Event::new(
    type_x,
    EventInit {
      bubbles: true,
      ..Default::default()
    },
  );
  dispatch_event(
    EventTargetId::Node(old_node_id),
    &mut event,
    &src_doc,
    &src_snapshot,
    &mut invoker_before,
  )
  .unwrap();
  let calls_before = invoker_before.calls.clone();

  // Transfer listeners to the destination registry.
  src.transfer_node_listeners(&dst, &[(old_node_id, new_node_id)]);

  assert!(
    !src.has_event_listeners(EventTargetId::Node(old_node_id), type_x),
    "source registry should no longer have listeners for transferred node/type"
  );
  assert!(
    !src.has_event_listeners(EventTargetId::Node(old_node_id), type_y),
    "source registry should no longer have listeners for transferred node/other-type"
  );
  assert!(
    dst.has_event_listeners(EventTargetId::Node(new_node_id), type_x),
    "destination registry should have listeners for new node/type"
  );
  assert!(
    dst.has_event_listeners(EventTargetId::Node(new_node_id), type_y),
    "destination registry should have listeners for new node/other-type"
  );

  // Dispatch the same event in the destination document. Callback ordering should match the
  // pre-transfer snapshot.
  let mut invoker_after = RecordingInvoker::new(
    &dst,
    EventTargetId::Node(new_node_id),
    [
      (
        id_bubble_1,
        Behavior {
          label: "bubble_1",
          expected_phase: EventPhase::AtTarget,
          expected_current_target: EventTargetId::Node(new_node_id),
          action: Action::None,
        },
      ),
      (
        id_capture_1,
        Behavior {
          label: "capture_1",
          expected_phase: EventPhase::AtTarget,
          expected_current_target: EventTargetId::Node(new_node_id),
          action: Action::None,
        },
      ),
      (
        id_bubble_2,
        Behavior {
          label: "bubble_2",
          expected_phase: EventPhase::AtTarget,
          expected_current_target: EventTargetId::Node(new_node_id),
          action: Action::None,
        },
      ),
      (
        id_capture_once,
        Behavior {
          label: "capture_once",
          expected_phase: EventPhase::AtTarget,
          expected_current_target: EventTargetId::Node(new_node_id),
          action: Action::None,
        },
      ),
    ],
  );

  let mut event = Event::new(
    type_x,
    EventInit {
      bubbles: true,
      ..Default::default()
    },
  );
  dispatch_event(
    EventTargetId::Node(new_node_id),
    &mut event,
    &dst_doc,
    &dst,
    &mut invoker_after,
  )
  .unwrap();

  assert_eq!(
    invoker_after.calls.as_slice(),
    calls_before.as_slice(),
    "listener invocation order must be preserved across transfer"
  );

  // The `once` listener should have been removed by the prior dispatch.
  let mut invoker_once_check = RecordingInvoker::new(
    &dst,
    EventTargetId::Node(new_node_id),
    [
      (
        id_capture_1,
        Behavior {
          label: "capture_1",
          expected_phase: EventPhase::AtTarget,
          expected_current_target: EventTargetId::Node(new_node_id),
          action: Action::None,
        },
      ),
      (
        id_bubble_1,
        Behavior {
          label: "bubble_1",
          expected_phase: EventPhase::AtTarget,
          expected_current_target: EventTargetId::Node(new_node_id),
          action: Action::None,
        },
      ),
      (
        id_bubble_2,
        Behavior {
          label: "bubble_2",
          expected_phase: EventPhase::AtTarget,
          expected_current_target: EventTargetId::Node(new_node_id),
          action: Action::None,
        },
      ),
    ],
  );

  let mut event = Event::new(type_x, EventInit::default());
  dispatch_event(
    EventTargetId::Node(new_node_id),
    &mut event,
    &dst_doc,
    &dst,
    &mut invoker_once_check,
  )
  .unwrap();
  assert_eq!(
    invoker_once_check.calls.as_slice(),
    &["capture_1", "bubble_1", "bubble_2"]
  );

  // Adding new listeners after transfer must not break removal-during-dispatch semantics (guards
  // against `record_id` collisions in the destination registry).
  let id_new_1 = ListenerId::new(10);
  let id_new_2 = ListenerId::new(11);
  let id_new_3 = ListenerId::new(12);
  let id_new_4 = ListenerId::new(13);
  let id_new_5 = ListenerId::new(14);

  for id in [id_new_1, id_new_2, id_new_3, id_new_4, id_new_5] {
    assert!(dst.add_event_listener(
      EventTargetId::Node(new_node_id),
      type_x,
      id,
      AddEventListenerOptions::default(),
    ));
  }

  let mut invoker_collision_check = RecordingInvoker::new(
    &dst,
    EventTargetId::Node(new_node_id),
    [
      (
        id_capture_1,
        Behavior {
          label: "capture_1",
          expected_phase: EventPhase::AtTarget,
          expected_current_target: EventTargetId::Node(new_node_id),
          action: Action::None,
        },
      ),
      (
        id_bubble_1,
        Behavior {
          label: "bubble_1",
          expected_phase: EventPhase::AtTarget,
          expected_current_target: EventTargetId::Node(new_node_id),
          action: Action::RemoveListener {
            target: EventTargetId::Node(new_node_id),
            type_: type_x,
            listener_id: id_bubble_2,
            capture: false,
            expect_removed: Some(true),
          },
        },
      ),
      (
        id_bubble_2,
        Behavior {
          label: "bubble_2",
          expected_phase: EventPhase::AtTarget,
          expected_current_target: EventTargetId::Node(new_node_id),
          action: Action::None,
        },
      ),
      (
        id_new_1,
        Behavior {
          label: "new_1",
          expected_phase: EventPhase::AtTarget,
          expected_current_target: EventTargetId::Node(new_node_id),
          action: Action::None,
        },
      ),
      (
        id_new_2,
        Behavior {
          label: "new_2",
          expected_phase: EventPhase::AtTarget,
          expected_current_target: EventTargetId::Node(new_node_id),
          action: Action::None,
        },
      ),
      (
        id_new_3,
        Behavior {
          label: "new_3",
          expected_phase: EventPhase::AtTarget,
          expected_current_target: EventTargetId::Node(new_node_id),
          action: Action::None,
        },
      ),
      (
        id_new_4,
        Behavior {
          label: "new_4",
          expected_phase: EventPhase::AtTarget,
          expected_current_target: EventTargetId::Node(new_node_id),
          action: Action::None,
        },
      ),
      (
        id_new_5,
        Behavior {
          label: "new_5",
          expected_phase: EventPhase::AtTarget,
          expected_current_target: EventTargetId::Node(new_node_id),
          action: Action::None,
        },
      ),
    ],
  );

  let mut event = Event::new(type_x, EventInit::default());
  dispatch_event(
    EventTargetId::Node(new_node_id),
    &mut event,
    &dst_doc,
    &dst,
    &mut invoker_collision_check,
  )
  .unwrap();
  assert_eq!(
    invoker_collision_check.calls.as_slice(),
    &["capture_1", "bubble_1", "new_1", "new_2", "new_3", "new_4", "new_5"]
  );
}

#[test]
fn composed_path_hides_closed_shadow_tree_from_outside() {
  let html = "<!doctype html><div id=host><template shadowroot=closed><span id=inner></span></template></div>";
  let doc = crate::dom2::parse_html(html).unwrap();
  let host = find_node_by_id(&doc, doc.root(), "host").expect("host not found");
  let shadow_root = find_shadow_root(&doc, host).expect("shadow root not found");
  let inner = find_node_by_id(&doc, shadow_root, "inner").expect("inner not found");

  let registry = EventListenerRegistry::new();
  let type_ = "x";
  let id_document = ListenerId::new(1);
  assert!(registry.add_event_listener(
    EventTargetId::Document,
    type_,
    id_document,
    AddEventListenerOptions::default()
  ));

  let mut invoker = TraceInvoker {
    labels: HashMap::from([(id_document, "document")]),
    calls: Vec::new(),
  };

  let mut event = Event::new(
    type_,
    EventInit {
      bubbles: true,
      composed: true,
      ..Default::default()
    },
  );
  dispatch_event(
    EventTargetId::Node(inner),
    &mut event,
    &doc,
    &registry,
    &mut invoker,
  )
  .unwrap();

  let document_call = invoker.calls.first().expect("document listener not invoked");
  assert_eq!(document_call.target, Some(EventTargetId::Node(host)));
  assert_eq!(document_call.event_phase, EventPhase::Bubbling);

  assert_eq!(
    document_call.composed_path.first(),
    Some(&EventTargetId::Node(host)),
    "closed shadow tree nodes should be hidden from outside listeners"
  );
  assert!(
    !document_call
      .composed_path
      .contains(&EventTargetId::Node(shadow_root)),
    "closed shadow root should be hidden from outside listeners"
  );
  assert!(
    !document_call.composed_path.contains(&EventTargetId::Node(inner)),
    "closed shadow tree target should be hidden from outside listeners"
  );
}

#[test]
fn composed_path_is_stable_across_capture_target_and_bubble_for_open_shadow_roots() {
  let html = "<!doctype html><div id=host><template shadowroot=open><span id=inner></span></template></div>";
  let doc = crate::dom2::parse_html(html).unwrap();
  let host = find_node_by_id(&doc, doc.root(), "host").expect("host not found");
  let shadow_root = find_shadow_root(&doc, host).expect("shadow root not found");
  let inner = find_node_by_id(&doc, shadow_root, "inner").expect("inner not found");

  let registry = EventListenerRegistry::new();
  let type_ = "x";

  let id_window_capture = ListenerId::new(1);
  let id_document_capture = ListenerId::new(2);
  let id_host_capture = ListenerId::new(3);
  let id_shadow_root_capture = ListenerId::new(4);
  let id_inner_capture = ListenerId::new(5);
  let id_inner_bubble = ListenerId::new(6);
  let id_shadow_root_bubble = ListenerId::new(7);
  let id_host_bubble = ListenerId::new(8);
  let id_document_bubble = ListenerId::new(9);
  let id_window_bubble = ListenerId::new(10);

  for (id, target, capture) in [
    (id_window_capture, EventTargetId::Window, true),
    (id_document_capture, EventTargetId::Document, true),
    (id_host_capture, EventTargetId::Node(host), true),
    (id_shadow_root_capture, EventTargetId::Node(shadow_root), true),
    (id_inner_capture, EventTargetId::Node(inner), true),
    (id_inner_bubble, EventTargetId::Node(inner), false),
    (id_shadow_root_bubble, EventTargetId::Node(shadow_root), false),
    (id_host_bubble, EventTargetId::Node(host), false),
    (id_document_bubble, EventTargetId::Document, false),
    (id_window_bubble, EventTargetId::Window, false),
  ] {
    assert!(registry.add_event_listener(
      target,
      type_,
      id,
      AddEventListenerOptions {
        capture,
        ..Default::default()
      }
    ));
  }

  let mut invoker = TraceInvoker {
    labels: HashMap::from([
      (id_window_capture, "window_capture"),
      (id_document_capture, "document_capture"),
      (id_host_capture, "host_capture"),
      (id_shadow_root_capture, "shadow_root_capture"),
      (id_inner_capture, "inner_capture"),
      (id_inner_bubble, "inner_bubble"),
      (id_shadow_root_bubble, "shadow_root_bubble"),
      (id_host_bubble, "host_bubble"),
      (id_document_bubble, "document_bubble"),
      (id_window_bubble, "window_bubble"),
    ]),
    calls: Vec::new(),
  };

  let mut event = Event::new(
    type_,
    EventInit {
      bubbles: true,
      composed: true,
      ..Default::default()
    },
  );
  dispatch_event(
    EventTargetId::Node(inner),
    &mut event,
    &doc,
    &registry,
    &mut invoker,
  )
  .unwrap();

  assert!(
    invoker.calls.iter().any(|c| c.event_phase == EventPhase::Capturing),
    "expected at least one capturing-phase invocation"
  );
  assert!(
    invoker.calls.iter().any(|c| c.event_phase == EventPhase::AtTarget),
    "expected at least one at-target invocation"
  );
  assert!(
    invoker.calls.iter().any(|c| c.event_phase == EventPhase::Bubbling),
    "expected at least one bubbling-phase invocation"
  );

  let first = invoker
    .calls
    .first()
    .expect("expected at least one listener invocation");
  let baseline = first.composed_path.clone();
  assert_eq!(
    baseline.first().copied(),
    Some(EventTargetId::Node(inner)),
    "expected composedPath to start at the original target for open shadow roots"
  );
  assert!(
    baseline.contains(&EventTargetId::Node(shadow_root)),
    "expected composedPath to include the shadow root for open mode"
  );
  assert_eq!(
    baseline.last().copied(),
    Some(EventTargetId::Window),
    "expected composedPath to end at Window"
  );

  for call in &invoker.calls {
    assert_eq!(
      call.composed_path, baseline,
      "composedPath should be stable across capture/target/bubble for open shadow roots (call: {})",
      call.label
    );
  }
}
