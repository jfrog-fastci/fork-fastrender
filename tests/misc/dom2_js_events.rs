use fastrender::dom2;
use fastrender::js::JsDomEvents;
use fastrender::web::events::{AddEventListenerOptions, Event, EventInit, EventTargetId};
use fastrender::Result;
use std::cell::RefCell;
use std::rc::Rc;
use vm_js::{PropertyKey, Value, VmError};
use webidl_js_runtime::{JsPropertyKind, JsRuntime as _};

#[derive(Debug, Clone, Copy)]
enum Action {
  None,
  StopPropagation,
  StopImmediatePropagation,
  PreventDefault,
}

fn key(rt: &mut fastrender::js::webidl::VmJsRuntime, name: &str) -> PropertyKey {
  let v = rt.alloc_string_value(name).expect("alloc string");
  let Value::String(s) = v else {
    panic!("expected string");
  };
  PropertyKey::String(s)
}

fn as_utf8_lossy(rt: &fastrender::js::webidl::VmJsRuntime, v: Value) -> String {
  let Value::String(s) = v else {
    panic!("expected string");
  };
  rt.heap().get_string(s).unwrap().to_utf8_lossy()
}

fn assert_js_value_eq(rt: &fastrender::js::webidl::VmJsRuntime, got: Value, expected: Value) {
  match (got, expected) {
    (Value::String(_), Value::String(_)) => {
      assert_eq!(as_utf8_lossy(rt, got), as_utf8_lossy(rt, expected));
    }
    _ => assert_eq!(got, expected),
  }
}

fn event_accessor_setter(
  rt: &mut fastrender::js::webidl::VmJsRuntime,
  event: Value,
  key: PropertyKey,
) -> std::result::Result<Value, VmError> {
  let Value::Object(obj) = event else {
    panic!("expected Event object");
  };
  let proto = rt
    .heap()
    .object_prototype(obj)?
    .expect("Event object is missing prototype");
  let desc = rt
    .get_own_property(Value::Object(proto), key)?
    .expect("Event prototype is missing property");
  let JsPropertyKind::Accessor { set, .. } = desc.kind else {
    panic!("expected accessor property");
  };
  Ok(set)
}

fn make_listener(
  js: &mut JsDomEvents,
  log: Rc<RefCell<Vec<&'static str>>>,
  label: &'static str,
  action: Action,
  keys: ListenerKeys,
) -> Value {
  js.runtime_mut()
    .alloc_function_value(move |rt, this, args| {
      let event = args.get(0).copied().unwrap_or(Value::Undefined);
      log.borrow_mut().push(label);

      // Basic Event wrapper smoke: read a few properties.
      let got_type = rt.get(event, keys.type_)?;
      assert_eq!(as_utf8_lossy(rt, got_type), "test");

      let got_target = rt.get(event, keys.target)?;
      assert_js_value_eq(rt, got_target, keys.expected_target);

      let got_current = rt.get(event, keys.current_target)?;
      assert_js_value_eq(rt, got_current, keys.expected_current_target);
      // Callable listeners are invoked with `this = event.currentTarget`.
      assert_js_value_eq(rt, this, keys.expected_current_target);

      let got_phase = rt.get(event, keys.event_phase)?;
      assert_eq!(got_phase, Value::Number(keys.expected_phase));

      let bubbles = rt.get(event, keys.bubbles)?;
      assert_eq!(bubbles, Value::Bool(keys.expected_bubbles));
      let cancelable = rt.get(event, keys.cancelable)?;
      assert_eq!(cancelable, Value::Bool(keys.expected_cancelable));
      let composed = rt.get(event, keys.composed)?;
      assert_eq!(composed, Value::Bool(keys.expected_composed));
      let is_trusted = rt.get(event, keys.is_trusted)?;
      assert_eq!(is_trusted, Value::Bool(keys.expected_is_trusted));

      match action {
        Action::None => {}
        Action::StopPropagation => {
          let f = rt.get(event, keys.stop_propagation)?;
          rt.call_function(f, event, &[])?;
        }
        Action::StopImmediatePropagation => {
          let f = rt.get(event, keys.stop_immediate_propagation)?;
          rt.call_function(f, event, &[])?;
        }
        Action::PreventDefault => {
          let f = rt.get(event, keys.prevent_default)?;
          rt.call_function(f, event, &[])?;
        }
      }
      Ok(Value::Undefined)
    })
    .expect("alloc function")
}

#[derive(Clone, Copy)]
struct ListenerKeys {
  type_: PropertyKey,
  bubbles: PropertyKey,
  cancelable: PropertyKey,
  composed: PropertyKey,
  target: PropertyKey,
  current_target: PropertyKey,
  event_phase: PropertyKey,
  is_trusted: PropertyKey,
  stop_propagation: PropertyKey,
  stop_immediate_propagation: PropertyKey,
  prevent_default: PropertyKey,
  expected_target: Value,
  expected_current_target: Value,
  expected_phase: f64,
  expected_bubbles: bool,
  expected_cancelable: bool,
  expected_composed: bool,
  expected_is_trusted: bool,
}

fn build_doc() -> (dom2::Document, dom2::NodeId, dom2::NodeId) {
  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><div id=parent><span id=target></span></div>")
      .unwrap();
  let mut doc = dom2::Document::from_renderer_dom(&renderer_dom);
  let parent = doc.query_selector("#parent", None).unwrap().unwrap();
  let target = doc.query_selector("#target", None).unwrap().unwrap();
  (doc, parent, target)
}

#[test]
fn js_listeners_capture_and_bubble_in_dom_order() -> Result<()> {
  let (mut doc, parent, target) = build_doc();
  let mut js = JsDomEvents::new()?;

  let key_type = key(js.runtime_mut(), "type");
  let key_bubbles = key(js.runtime_mut(), "bubbles");
  let key_cancelable = key(js.runtime_mut(), "cancelable");
  let key_composed = key(js.runtime_mut(), "composed");
  let key_target = key(js.runtime_mut(), "target");
  let key_current_target = key(js.runtime_mut(), "currentTarget");
  let key_event_phase = key(js.runtime_mut(), "eventPhase");
  let key_is_trusted = key(js.runtime_mut(), "isTrusted");
  let key_stop_propagation = key(js.runtime_mut(), "stopPropagation");
  let key_stop_immediate_propagation = key(js.runtime_mut(), "stopImmediatePropagation");
  let key_prevent_default = key(js.runtime_mut(), "preventDefault");

  let target_value = Value::Number(target.index() as f64);
  let parent_value = Value::Number(parent.index() as f64);
  let doc_value = js
    .runtime_mut()
    .alloc_string_value("document")
    .expect("alloc string");

  let log: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));

  let doc_capture = make_listener(
    &mut js,
    log.clone(),
    "doc-capture",
    Action::None,
    ListenerKeys {
      type_: key_type,
      bubbles: key_bubbles,
      cancelable: key_cancelable,
      composed: key_composed,
      target: key_target,
      current_target: key_current_target,
      event_phase: key_event_phase,
      is_trusted: key_is_trusted,
      stop_propagation: key_stop_propagation,
      stop_immediate_propagation: key_stop_immediate_propagation,
      prevent_default: key_prevent_default,
      expected_target: target_value,
      expected_current_target: doc_value,
      expected_phase: 1.0,
      expected_bubbles: true,
      expected_cancelable: false,
      expected_composed: false,
      expected_is_trusted: false,
    },
  );
  let parent_capture = make_listener(
    &mut js,
    log.clone(),
    "parent-capture",
    Action::None,
    ListenerKeys {
      expected_current_target: parent_value,
      ..ListenerKeys {
        type_: key_type,
        bubbles: key_bubbles,
        cancelable: key_cancelable,
        composed: key_composed,
        target: key_target,
        current_target: key_current_target,
        event_phase: key_event_phase,
        is_trusted: key_is_trusted,
        stop_propagation: key_stop_propagation,
        stop_immediate_propagation: key_stop_immediate_propagation,
        prevent_default: key_prevent_default,
        expected_target: target_value,
        expected_current_target: parent_value,
        expected_phase: 1.0,
        expected_bubbles: true,
        expected_cancelable: false,
        expected_composed: false,
        expected_is_trusted: false,
      }
    },
  );

  let target_capture = make_listener(
    &mut js,
    log.clone(),
    "target-capture",
    Action::None,
    ListenerKeys {
      expected_current_target: Value::Number(target.index() as f64),
      expected_phase: 2.0,
      ..ListenerKeys {
        type_: key_type,
        bubbles: key_bubbles,
        cancelable: key_cancelable,
        composed: key_composed,
        target: key_target,
        current_target: key_current_target,
        event_phase: key_event_phase,
        is_trusted: key_is_trusted,
        stop_propagation: key_stop_propagation,
        stop_immediate_propagation: key_stop_immediate_propagation,
        prevent_default: key_prevent_default,
        expected_target: target_value,
        expected_current_target: Value::Number(target.index() as f64),
        expected_phase: 2.0,
        expected_bubbles: true,
        expected_cancelable: false,
        expected_composed: false,
        expected_is_trusted: false,
      }
    },
  );

  let target_bubble = make_listener(
    &mut js,
    log.clone(),
    "target-bubble",
    Action::None,
    ListenerKeys {
      expected_current_target: Value::Number(target.index() as f64),
      expected_phase: 2.0,
      ..ListenerKeys {
        type_: key_type,
        bubbles: key_bubbles,
        cancelable: key_cancelable,
        composed: key_composed,
        target: key_target,
        current_target: key_current_target,
        event_phase: key_event_phase,
        is_trusted: key_is_trusted,
        stop_propagation: key_stop_propagation,
        stop_immediate_propagation: key_stop_immediate_propagation,
        prevent_default: key_prevent_default,
        expected_target: target_value,
        expected_current_target: Value::Number(target.index() as f64),
        expected_phase: 2.0,
        expected_bubbles: true,
        expected_cancelable: false,
        expected_composed: false,
        expected_is_trusted: false,
      }
    },
  );

  let parent_bubble = make_listener(
    &mut js,
    log.clone(),
    "parent-bubble",
    Action::None,
    ListenerKeys {
      expected_current_target: parent_value,
      expected_phase: 3.0,
      ..ListenerKeys {
        type_: key_type,
        bubbles: key_bubbles,
        cancelable: key_cancelable,
        composed: key_composed,
        target: key_target,
        current_target: key_current_target,
        event_phase: key_event_phase,
        is_trusted: key_is_trusted,
        stop_propagation: key_stop_propagation,
        stop_immediate_propagation: key_stop_immediate_propagation,
        prevent_default: key_prevent_default,
        expected_target: target_value,
        expected_current_target: parent_value,
        expected_phase: 3.0,
        expected_bubbles: true,
        expected_cancelable: false,
        expected_composed: false,
        expected_is_trusted: false,
      }
    },
  );

  let doc_bubble = make_listener(
    &mut js,
    log.clone(),
    "doc-bubble",
    Action::None,
    ListenerKeys {
      expected_current_target: doc_value,
      expected_phase: 3.0,
      ..ListenerKeys {
        type_: key_type,
        bubbles: key_bubbles,
        cancelable: key_cancelable,
        composed: key_composed,
        target: key_target,
        current_target: key_current_target,
        event_phase: key_event_phase,
        is_trusted: key_is_trusted,
        stop_propagation: key_stop_propagation,
        stop_immediate_propagation: key_stop_immediate_propagation,
        prevent_default: key_prevent_default,
        expected_target: target_value,
        expected_current_target: doc_value,
        expected_phase: 3.0,
        expected_bubbles: true,
        expected_cancelable: false,
        expected_composed: false,
        expected_is_trusted: false,
      }
    },
  );

  let type_ = "test";
  let _ = js.add_js_event_listener(
    EventTargetId::Document,
    type_,
    doc_capture,
    AddEventListenerOptions {
      capture: true,
      ..Default::default()
    },
  )?;
  let _ = js.add_js_event_listener(
    EventTargetId::Node(parent),
    type_,
    parent_capture,
    AddEventListenerOptions {
      capture: true,
      ..Default::default()
    },
  )?;
  let _ = js.add_js_event_listener(
    EventTargetId::Node(target),
    type_,
    target_capture,
    AddEventListenerOptions {
      capture: true,
      ..Default::default()
    },
  )?;

  let _ = js.add_js_event_listener(
    EventTargetId::Node(target),
    type_,
    target_bubble,
    AddEventListenerOptions::default(),
  )?;
  let _ = js.add_js_event_listener(
    EventTargetId::Node(parent),
    type_,
    parent_bubble,
    AddEventListenerOptions::default(),
  )?;
  let _ = js.add_js_event_listener(
    EventTargetId::Document,
    type_,
    doc_bubble,
    AddEventListenerOptions::default(),
  )?;

  let mut event = Event::new(
    type_,
    EventInit {
      bubbles: true,
      ..Default::default()
    },
  );
  js.dispatch_dom_event(&doc, EventTargetId::Node(target), &mut event)?;

  assert_eq!(
    *log.borrow(),
    vec![
      "doc-capture",
      "parent-capture",
      "target-capture",
      "target-bubble",
      "parent-bubble",
      "doc-bubble"
    ]
  );
  Ok(())
}

#[test]
fn js_stop_propagation_is_observed_by_dispatch() -> Result<()> {
  let (mut doc, parent, target) = build_doc();
  let mut js = JsDomEvents::new()?;

  let log: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));

  let key_type = key(js.runtime_mut(), "type");
  let key_bubbles = key(js.runtime_mut(), "bubbles");
  let key_cancelable = key(js.runtime_mut(), "cancelable");
  let key_composed = key(js.runtime_mut(), "composed");
  let key_target = key(js.runtime_mut(), "target");
  let key_current_target = key(js.runtime_mut(), "currentTarget");
  let key_event_phase = key(js.runtime_mut(), "eventPhase");
  let key_is_trusted = key(js.runtime_mut(), "isTrusted");
  let key_stop_propagation = key(js.runtime_mut(), "stopPropagation");
  let key_stop_immediate_propagation = key(js.runtime_mut(), "stopImmediatePropagation");
  let key_prevent_default = key(js.runtime_mut(), "preventDefault");

  let target_value = Value::Number(target.index() as f64);

  let stopper = make_listener(
    &mut js,
    log.clone(),
    "target-stop",
    Action::StopPropagation,
    ListenerKeys {
      type_: key_type,
      bubbles: key_bubbles,
      cancelable: key_cancelable,
      composed: key_composed,
      target: key_target,
      current_target: key_current_target,
      event_phase: key_event_phase,
      is_trusted: key_is_trusted,
      stop_propagation: key_stop_propagation,
      stop_immediate_propagation: key_stop_immediate_propagation,
      prevent_default: key_prevent_default,
      expected_target: target_value,
      expected_current_target: target_value,
      expected_phase: 2.0,
      expected_bubbles: true,
      expected_cancelable: false,
      expected_composed: false,
      expected_is_trusted: false,
    },
  );

  let parent_bubble = make_listener(
    &mut js,
    log.clone(),
    "parent-bubble",
    Action::None,
    ListenerKeys {
      expected_current_target: Value::Number(parent.index() as f64),
      expected_phase: 3.0,
      ..ListenerKeys {
        type_: key_type,
        bubbles: key_bubbles,
        cancelable: key_cancelable,
        composed: key_composed,
        target: key_target,
        current_target: key_current_target,
        event_phase: key_event_phase,
        is_trusted: key_is_trusted,
        stop_propagation: key_stop_propagation,
        stop_immediate_propagation: key_stop_immediate_propagation,
        prevent_default: key_prevent_default,
        expected_target: target_value,
        expected_current_target: Value::Number(parent.index() as f64),
        expected_phase: 3.0,
        expected_bubbles: true,
        expected_cancelable: false,
        expected_composed: false,
        expected_is_trusted: false,
      }
    },
  );

  let _ = js.add_js_event_listener(
    EventTargetId::Node(target),
    "test",
    stopper,
    AddEventListenerOptions::default(),
  )?;
  let _ = js.add_js_event_listener(
    EventTargetId::Node(parent),
    "test",
    parent_bubble,
    AddEventListenerOptions::default(),
  )?;

  let mut event = Event::new(
    "test",
    EventInit {
      bubbles: true,
      ..Default::default()
    },
  );
  js.dispatch_dom_event(&doc, EventTargetId::Node(target), &mut event)?;

  assert_eq!(*log.borrow(), vec!["target-stop"]);
  Ok(())
}

#[test]
fn js_stop_immediate_propagation_skips_later_listeners_on_same_target() -> Result<()> {
  let (mut doc, _parent, target) = build_doc();
  let mut js = JsDomEvents::new()?;

  let log: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));

  let key_type = key(js.runtime_mut(), "type");
  let key_bubbles = key(js.runtime_mut(), "bubbles");
  let key_cancelable = key(js.runtime_mut(), "cancelable");
  let key_composed = key(js.runtime_mut(), "composed");
  let key_target = key(js.runtime_mut(), "target");
  let key_current_target = key(js.runtime_mut(), "currentTarget");
  let key_event_phase = key(js.runtime_mut(), "eventPhase");
  let key_is_trusted = key(js.runtime_mut(), "isTrusted");
  let key_stop_propagation = key(js.runtime_mut(), "stopPropagation");
  let key_stop_immediate_propagation = key(js.runtime_mut(), "stopImmediatePropagation");
  let key_prevent_default = key(js.runtime_mut(), "preventDefault");

  let target_value = Value::Number(target.index() as f64);

  let first = make_listener(
    &mut js,
    log.clone(),
    "first",
    Action::StopImmediatePropagation,
    ListenerKeys {
      type_: key_type,
      bubbles: key_bubbles,
      cancelable: key_cancelable,
      composed: key_composed,
      target: key_target,
      current_target: key_current_target,
      event_phase: key_event_phase,
      is_trusted: key_is_trusted,
      stop_propagation: key_stop_propagation,
      stop_immediate_propagation: key_stop_immediate_propagation,
      prevent_default: key_prevent_default,
      expected_target: target_value,
      expected_current_target: target_value,
      expected_phase: 2.0,
      expected_bubbles: true,
      expected_cancelable: false,
      expected_composed: false,
      expected_is_trusted: false,
    },
  );

  let second = make_listener(
    &mut js,
    log.clone(),
    "second",
    Action::None,
    ListenerKeys {
      type_: key_type,
      bubbles: key_bubbles,
      cancelable: key_cancelable,
      composed: key_composed,
      target: key_target,
      current_target: key_current_target,
      event_phase: key_event_phase,
      is_trusted: key_is_trusted,
      stop_propagation: key_stop_propagation,
      stop_immediate_propagation: key_stop_immediate_propagation,
      prevent_default: key_prevent_default,
      expected_target: target_value,
      expected_current_target: target_value,
      expected_phase: 2.0,
      expected_bubbles: true,
      expected_cancelable: false,
      expected_composed: false,
      expected_is_trusted: false,
    },
  );

  let _ = js.add_js_event_listener(
    EventTargetId::Node(target),
    "test",
    first,
    AddEventListenerOptions::default(),
  )?;
  let _ = js.add_js_event_listener(
    EventTargetId::Node(target),
    "test",
    second,
    AddEventListenerOptions::default(),
  )?;

  let mut event = Event::new(
    "test",
    EventInit {
      bubbles: true,
      ..Default::default()
    },
  );
  js.dispatch_dom_event(&doc, EventTargetId::Node(target), &mut event)?;

  assert_eq!(*log.borrow(), vec!["first"]);
  Ok(())
}

#[test]
fn js_once_listener_runs_only_once() -> Result<()> {
  let (mut doc, _parent, target) = build_doc();
  let mut js = JsDomEvents::new()?;

  let log: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));

  let key_type = key(js.runtime_mut(), "type");
  let key_bubbles = key(js.runtime_mut(), "bubbles");
  let key_cancelable = key(js.runtime_mut(), "cancelable");
  let key_composed = key(js.runtime_mut(), "composed");
  let key_target = key(js.runtime_mut(), "target");
  let key_current_target = key(js.runtime_mut(), "currentTarget");
  let key_event_phase = key(js.runtime_mut(), "eventPhase");
  let key_is_trusted = key(js.runtime_mut(), "isTrusted");
  let key_stop_propagation = key(js.runtime_mut(), "stopPropagation");
  let key_stop_immediate_propagation = key(js.runtime_mut(), "stopImmediatePropagation");
  let key_prevent_default = key(js.runtime_mut(), "preventDefault");

  let target_value = Value::Number(target.index() as f64);

  let once = make_listener(
    &mut js,
    log.clone(),
    "once",
    Action::None,
    ListenerKeys {
      type_: key_type,
      bubbles: key_bubbles,
      cancelable: key_cancelable,
      composed: key_composed,
      target: key_target,
      current_target: key_current_target,
      event_phase: key_event_phase,
      is_trusted: key_is_trusted,
      stop_propagation: key_stop_propagation,
      stop_immediate_propagation: key_stop_immediate_propagation,
      prevent_default: key_prevent_default,
      expected_target: target_value,
      expected_current_target: target_value,
      expected_phase: 2.0,
      expected_bubbles: true,
      expected_cancelable: false,
      expected_composed: false,
      expected_is_trusted: false,
    },
  );

  let _ = js.add_js_event_listener(
    EventTargetId::Node(target),
    "test",
    once,
    AddEventListenerOptions {
      once: true,
      ..Default::default()
    },
  )?;

  let mut event = Event::new(
    "test",
    EventInit {
      bubbles: true,
      ..Default::default()
    },
  );
  js.dispatch_dom_event(&doc, EventTargetId::Node(target), &mut event)?;

  let mut event2 = Event::new(
    "test",
    EventInit {
      bubbles: true,
      ..Default::default()
    },
  );
  js.dispatch_dom_event(&doc, EventTargetId::Node(target), &mut event2)?;

  assert_eq!(*log.borrow(), vec!["once"]);
  Ok(())
}

#[test]
fn js_passive_listener_cannot_prevent_default() -> Result<()> {
  let (mut doc, _parent, target) = build_doc();
  let mut js = JsDomEvents::new()?;

  let log: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));

  let key_type = key(js.runtime_mut(), "type");
  let key_bubbles = key(js.runtime_mut(), "bubbles");
  let key_cancelable = key(js.runtime_mut(), "cancelable");
  let key_composed = key(js.runtime_mut(), "composed");
  let key_target = key(js.runtime_mut(), "target");
  let key_current_target = key(js.runtime_mut(), "currentTarget");
  let key_event_phase = key(js.runtime_mut(), "eventPhase");
  let key_is_trusted = key(js.runtime_mut(), "isTrusted");
  let key_stop_propagation = key(js.runtime_mut(), "stopPropagation");
  let key_stop_immediate_propagation = key(js.runtime_mut(), "stopImmediatePropagation");
  let key_prevent_default = key(js.runtime_mut(), "preventDefault");

  let target_value = Value::Number(target.index() as f64);

  let passive = make_listener(
    &mut js,
    log.clone(),
    "passive",
    Action::PreventDefault,
    ListenerKeys {
      type_: key_type,
      bubbles: key_bubbles,
      cancelable: key_cancelable,
      composed: key_composed,
      target: key_target,
      current_target: key_current_target,
      event_phase: key_event_phase,
      is_trusted: key_is_trusted,
      stop_propagation: key_stop_propagation,
      stop_immediate_propagation: key_stop_immediate_propagation,
      prevent_default: key_prevent_default,
      expected_target: target_value,
      expected_current_target: target_value,
      expected_phase: 2.0,
      expected_bubbles: true,
      expected_cancelable: true,
      expected_composed: false,
      expected_is_trusted: false,
    },
  );

  let _ = js.add_js_event_listener(
    EventTargetId::Node(target),
    "test",
    passive,
    AddEventListenerOptions {
      passive: true,
      ..Default::default()
    },
  )?;

  let mut event = Event::new(
    "test",
    EventInit {
      bubbles: true,
      cancelable: true,
      ..Default::default()
    },
  );
  let res = js.dispatch_dom_event(&doc, EventTargetId::Node(target), &mut event)?;
  assert!(res, "dispatchEvent should return true if not canceled");
  assert!(
    !event.default_prevented,
    "passive listeners must not set defaultPrevented"
  );

  assert_eq!(*log.borrow(), vec!["passive"]);
  Ok(())
}

#[test]
fn js_prevent_default_sets_default_prevented_property() -> Result<()> {
  let (mut doc, _parent, target) = build_doc();
  let mut js = JsDomEvents::new()?;

  let log: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));

  let key_default_prevented = key(js.runtime_mut(), "defaultPrevented");
  let key_prevent_default = key(js.runtime_mut(), "preventDefault");

  let log_for_cb = log.clone();
  let expected_this = Value::Number(target.index() as f64);
  let listener = js
    .runtime_mut()
    .alloc_function_value(move |rt, this, args| {
      assert_eq!(this, expected_this);
      let event = args.get(0).copied().unwrap_or(Value::Undefined);
      log_for_cb.borrow_mut().push("listener");

      let f = rt.get(event, key_prevent_default)?;
      rt.call_function(f, event, &[])?;

      let prevented = rt.get(event, key_default_prevented)?;
      assert_eq!(prevented, Value::Bool(true));

      Ok(Value::Undefined)
    })
    .expect("alloc function");

  let _ = js.add_js_event_listener(
    EventTargetId::Node(target),
    "test",
    listener,
    AddEventListenerOptions::default(),
  )?;

  let mut event = Event::new(
    "test",
    EventInit {
      bubbles: true,
      cancelable: true,
      ..Default::default()
    },
  );
  let dispatch_ok = js.dispatch_dom_event(&doc, EventTargetId::Node(target), &mut event)?;

  assert!(
    !dispatch_ok,
    "dispatchEvent should return false when canceled"
  );
  assert!(event.default_prevented);
  assert_eq!(*log.borrow(), vec!["listener"]);
  Ok(())
}

#[test]
fn js_callback_interface_listener_object_invokes_handle_event() -> Result<()> {
  let (mut doc, _parent, target) = build_doc();
  let mut js = JsDomEvents::new()?;

  let log: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));

  let key_type = key(js.runtime_mut(), "type");
  let key_bubbles = key(js.runtime_mut(), "bubbles");
  let key_cancelable = key(js.runtime_mut(), "cancelable");
  let key_composed = key(js.runtime_mut(), "composed");
  let key_target = key(js.runtime_mut(), "target");
  let key_current_target = key(js.runtime_mut(), "currentTarget");
  let key_event_phase = key(js.runtime_mut(), "eventPhase");
  let key_is_trusted = key(js.runtime_mut(), "isTrusted");

  let target_value = Value::Number(target.index() as f64);

  // Listener object implements the EventListener callback interface by exposing a callable
  // `handleEvent` method.
  let listener_obj = js.runtime_mut().alloc_object_value().expect("alloc listener object");
  let listener_obj_for_assert = listener_obj;
  let log_for_cb = log.clone();

  let keys = ListenerKeys {
    type_: key_type,
    bubbles: key_bubbles,
    cancelable: key_cancelable,
    composed: key_composed,
    target: key_target,
    current_target: key_current_target,
    event_phase: key_event_phase,
    is_trusted: key_is_trusted,
    // Unused for this test, but required by the struct.
    stop_propagation: key(js.runtime_mut(), "stopPropagation"),
    stop_immediate_propagation: key(js.runtime_mut(), "stopImmediatePropagation"),
    prevent_default: key(js.runtime_mut(), "preventDefault"),
    expected_target: target_value,
    expected_current_target: target_value,
    expected_phase: 2.0,
    expected_bubbles: true,
    expected_cancelable: false,
    expected_composed: false,
    expected_is_trusted: false,
  };

  let handle_event = js
    .runtime_mut()
    .alloc_function_value(move |rt, this, args| {
      // Per WebIDL "call a user object's operation", handleEvent is called with `this = listener`.
      assert_eq!(this, listener_obj_for_assert);

      let event = args.get(0).copied().unwrap_or(Value::Undefined);
      log_for_cb.borrow_mut().push("handleEvent");

      let got_type = rt.get(event, keys.type_)?;
      assert_eq!(as_utf8_lossy(rt, got_type), "test");
      let got_target = rt.get(event, keys.target)?;
      assert_eq!(got_target, keys.expected_target);
      let got_current = rt.get(event, keys.current_target)?;
      assert_eq!(got_current, keys.expected_current_target);
      let got_phase = rt.get(event, keys.event_phase)?;
      assert_eq!(got_phase, Value::Number(keys.expected_phase));

      let bubbles = rt.get(event, keys.bubbles)?;
      assert_eq!(bubbles, Value::Bool(keys.expected_bubbles));
      let cancelable = rt.get(event, keys.cancelable)?;
      assert_eq!(cancelable, Value::Bool(keys.expected_cancelable));
      let composed = rt.get(event, keys.composed)?;
      assert_eq!(composed, Value::Bool(keys.expected_composed));
      let is_trusted = rt.get(event, keys.is_trusted)?;
      assert_eq!(is_trusted, Value::Bool(keys.expected_is_trusted));

      Ok(Value::Undefined)
    })
    .expect("alloc handleEvent");

  let handle_event_key = js
    .runtime_mut()
    .property_key_from_str("handleEvent")
    .expect("intern handleEvent key");
  js.runtime_mut()
    .define_data_property(listener_obj, handle_event_key, handle_event, true)
    .expect("define handleEvent");

  let _ = js.add_js_event_listener(
    EventTargetId::Node(target),
    "test",
    listener_obj,
    AddEventListenerOptions::default(),
  )?;

  let mut event = Event::new("test", EventInit { bubbles: true, ..Default::default() });
  js.dispatch_dom_event(&doc, EventTargetId::Node(target), &mut event)?;

  assert_eq!(*log.borrow(), vec!["handleEvent"]);
  Ok(())
}

#[test]
fn js_cancel_bubble_setter_stops_propagation() -> Result<()> {
  let (doc, parent, target) = build_doc();
  let mut js = JsDomEvents::new()?;

  let log: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));

  let key_cancel_bubble = key(js.runtime_mut(), "cancelBubble");

  let log_for_target = log.clone();
  let target_listener = js
    .runtime_mut()
    .alloc_function_value(move |rt, _this, args| {
      let event = args.get(0).copied().unwrap_or(Value::Undefined);
      log_for_target.borrow_mut().push("target");

      let initial = rt.get(event, key_cancel_bubble)?;
      assert_eq!(initial, Value::Bool(false));

      let setter = event_accessor_setter(rt, event, key_cancel_bubble)?;
      rt.call_function(setter, event, &[Value::Bool(true)])?;

      let updated = rt.get(event, key_cancel_bubble)?;
      assert_eq!(updated, Value::Bool(true));

      Ok(Value::Undefined)
    })
    .expect("alloc function");

  let log_for_parent = log.clone();
  let parent_listener = js
    .runtime_mut()
    .alloc_function_value(move |_rt, _this, _args| {
      log_for_parent.borrow_mut().push("parent");
      Ok(Value::Undefined)
    })
    .expect("alloc function");

  let _ = js.add_js_event_listener(
    EventTargetId::Node(target),
    "test",
    target_listener,
    AddEventListenerOptions::default(),
  )?;
  let _ = js.add_js_event_listener(
    EventTargetId::Node(parent),
    "test",
    parent_listener,
    AddEventListenerOptions::default(),
  )?;

  let mut event = Event::new("test", EventInit { bubbles: true, ..Default::default() });
  js.dispatch_dom_event(&doc, EventTargetId::Node(target), &mut event)?;

  assert_eq!(*log.borrow(), vec!["target"]);
  Ok(())
}

#[test]
fn js_return_value_setter_false_calls_prevent_default() -> Result<()> {
  let (doc, _parent, target) = build_doc();
  let mut js = JsDomEvents::new()?;

  let log: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));

  let key_return_value = key(js.runtime_mut(), "returnValue");
  let key_default_prevented = key(js.runtime_mut(), "defaultPrevented");

  let log_for_cb = log.clone();
  let expected_this = Value::Number(target.index() as f64);
  let listener = js
    .runtime_mut()
    .alloc_function_value(move |rt, this, args| {
      assert_eq!(this, expected_this);
      let event = args.get(0).copied().unwrap_or(Value::Undefined);
      log_for_cb.borrow_mut().push("listener");

      let initial = rt.get(event, key_return_value)?;
      assert_eq!(initial, Value::Bool(true));
      let initial_prevented = rt.get(event, key_default_prevented)?;
      assert_eq!(initial_prevented, Value::Bool(false));

      let setter = event_accessor_setter(rt, event, key_return_value)?;
      rt.call_function(setter, event, &[Value::Bool(false)])?;

      let updated = rt.get(event, key_return_value)?;
      assert_eq!(updated, Value::Bool(false));
      let updated_prevented = rt.get(event, key_default_prevented)?;
      assert_eq!(updated_prevented, Value::Bool(true));

      Ok(Value::Undefined)
    })
    .expect("alloc function");

  let _ = js.add_js_event_listener(
    EventTargetId::Node(target),
    "test",
    listener,
    AddEventListenerOptions::default(),
  )?;

  let mut event = Event::new(
    "test",
    EventInit {
      bubbles: true,
      cancelable: true,
      ..Default::default()
    },
  );
  let dispatch_ok = js.dispatch_dom_event(&doc, EventTargetId::Node(target), &mut event)?;

  assert!(
    !dispatch_ok,
    "dispatchEvent should return false when canceled via returnValue"
  );
  assert!(event.default_prevented);
  assert_eq!(*log.borrow(), vec!["listener"]);
  Ok(())
}

#[test]
fn js_composed_path_and_src_element_reflect_dispatch_path() -> Result<()> {
  let (doc, _parent, target) = build_doc();
  let mut js = JsDomEvents::new()?;

  let key_composed_path = key(js.runtime_mut(), "composedPath");
  let key_length = key(js.runtime_mut(), "length");
  let key_src_element = key(js.runtime_mut(), "srcElement");

  let window_value = js
    .runtime_mut()
    .alloc_string_value("window")
    .expect("alloc window string");
  let document_value = js
    .runtime_mut()
    .alloc_string_value("document")
    .expect("alloc document string");

  let mut expected: Vec<Value> = Vec::new();
  expected.push(Value::Number(target.index() as f64));
  let mut current = target;
  loop {
    let Some(parent) = doc.node(current).parent else {
      break;
    };
    if matches!(doc.node(parent).kind, dom2::NodeKind::Document { .. }) {
      break;
    }
    expected.push(Value::Number(parent.index() as f64));
    current = parent;
  }
  expected.push(document_value);
  expected.push(window_value);

  let expected_for_cb = expected.clone();
  let listener = js
    .runtime_mut()
    .alloc_function_value(move |rt, _this, args| {
      let event = args.get(0).copied().unwrap_or(Value::Undefined);

      let src_element = rt.get(event, key_src_element)?;
      assert_eq!(src_element, expected_for_cb[0]);

      let composed_path_fn = rt.get(event, key_composed_path)?;
      let path = rt.call_function(composed_path_fn, event, &[])?;

      let len = rt.get(path, key_length)?;
      assert_eq!(len, Value::Number(expected_for_cb.len() as f64));

      for (idx, expected_value) in expected_for_cb.iter().copied().enumerate() {
        let key = rt.property_key_from_u32(idx as u32)?;
        let got = rt.get(path, key)?;
        assert_js_value_eq(rt, got, expected_value);
      }

      Ok(Value::Undefined)
    })
    .expect("alloc function");

  let _ = js.add_js_event_listener(
    EventTargetId::Node(target),
    "test",
    listener,
    AddEventListenerOptions::default(),
  )?;

  let mut event = Event::new("test", EventInit { bubbles: true, ..Default::default() });
  js.dispatch_dom_event(&doc, EventTargetId::Node(target), &mut event)?;
  Ok(())
}

#[test]
fn js_time_stamp_is_number() -> Result<()> {
  let (doc, _parent, target) = build_doc();
  let mut js = JsDomEvents::new()?;

  let key_time_stamp = key(js.runtime_mut(), "timeStamp");

  let listener = js
    .runtime_mut()
    .alloc_function_value(move |rt, _this, args| {
      let event = args.get(0).copied().unwrap_or(Value::Undefined);
      let ts = rt.get(event, key_time_stamp)?;
      let Value::Number(n) = ts else {
        panic!("expected timeStamp to be a number");
      };
      assert!(n.is_finite());
      assert!(n >= 0.0);
      Ok(Value::Undefined)
    })
    .expect("alloc function");

  let _ = js.add_js_event_listener(
    EventTargetId::Node(target),
    "test",
    listener,
    AddEventListenerOptions::default(),
  )?;

  let mut event = Event::new("test", EventInit { bubbles: true, ..Default::default() });
  js.dispatch_dom_event(&doc, EventTargetId::Node(target), &mut event)?;
  Ok(())
}
