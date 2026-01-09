use fastrender::dom2;
use fastrender::js::webidl::JsRuntime as WebIdlJsRuntime;
use fastrender::js::JsDomEvents;
use fastrender::web::events::{AddEventListenerOptions, Event, EventInit, EventTargetId};
use fastrender::Result;
use std::cell::RefCell;
use std::rc::Rc;
use vm_js::{PropertyKey, Value};

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

fn make_listener(
  js: &mut JsDomEvents,
  log: Rc<RefCell<Vec<&'static str>>>,
  label: &'static str,
  action: Action,
  keys: ListenerKeys,
) -> Value {
  js.runtime_mut()
    .alloc_function_value(move |rt, _this, args| {
      let event = args.get(0).copied().unwrap_or(Value::Undefined);
      log.borrow_mut().push(label);

      // Basic Event wrapper smoke: read a few properties.
      let got_type = rt.get(event, keys.type_)?;
      assert_eq!(as_utf8_lossy(rt, got_type), "test");
      let got_target = rt.get(event, keys.target)?;
      assert_js_value_eq(rt, got_target, keys.expected_target);

      let got_current = rt.get(event, keys.current_target)?;
      assert_js_value_eq(rt, got_current, keys.expected_current_target);

      let got_phase = rt.get(event, keys.event_phase)?;
      assert_eq!(got_phase, Value::Number(keys.expected_phase));

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
  target: PropertyKey,
  current_target: PropertyKey,
  event_phase: PropertyKey,
  stop_propagation: PropertyKey,
  stop_immediate_propagation: PropertyKey,
  prevent_default: PropertyKey,
  expected_target: Value,
  expected_current_target: Value,
  expected_phase: f64,
}

fn build_doc() -> (dom2::Document, dom2::NodeId, dom2::NodeId) {
  let renderer_dom = fastrender::dom::parse_html(
    "<!doctype html><div id=parent><span id=target></span></div>",
  )
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
  let key_target = key(js.runtime_mut(), "target");
  let key_current_target = key(js.runtime_mut(), "currentTarget");
  let key_event_phase = key(js.runtime_mut(), "eventPhase");
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
      target: key_target,
      current_target: key_current_target,
      event_phase: key_event_phase,
      stop_propagation: key_stop_propagation,
      stop_immediate_propagation: key_stop_immediate_propagation,
      prevent_default: key_prevent_default,
      expected_target: target_value,
      expected_current_target: doc_value,
      expected_phase: 1.0,
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
        target: key_target,
        current_target: key_current_target,
        event_phase: key_event_phase,
        stop_propagation: key_stop_propagation,
        stop_immediate_propagation: key_stop_immediate_propagation,
        prevent_default: key_prevent_default,
        expected_target: target_value,
        expected_current_target: parent_value,
        expected_phase: 1.0,
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
        target: key_target,
        current_target: key_current_target,
        event_phase: key_event_phase,
        stop_propagation: key_stop_propagation,
        stop_immediate_propagation: key_stop_immediate_propagation,
        prevent_default: key_prevent_default,
        expected_target: target_value,
        expected_current_target: Value::Number(target.index() as f64),
        expected_phase: 2.0,
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
        target: key_target,
        current_target: key_current_target,
        event_phase: key_event_phase,
        stop_propagation: key_stop_propagation,
        stop_immediate_propagation: key_stop_immediate_propagation,
        prevent_default: key_prevent_default,
        expected_target: target_value,
        expected_current_target: Value::Number(target.index() as f64),
        expected_phase: 2.0,
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
        target: key_target,
        current_target: key_current_target,
        event_phase: key_event_phase,
        stop_propagation: key_stop_propagation,
        stop_immediate_propagation: key_stop_immediate_propagation,
        prevent_default: key_prevent_default,
        expected_target: target_value,
        expected_current_target: parent_value,
        expected_phase: 3.0,
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
        target: key_target,
        current_target: key_current_target,
        event_phase: key_event_phase,
        stop_propagation: key_stop_propagation,
        stop_immediate_propagation: key_stop_immediate_propagation,
        prevent_default: key_prevent_default,
        expected_target: target_value,
        expected_current_target: doc_value,
        expected_phase: 3.0,
      }
    },
  );

  let type_ = "test";
  js.add_js_event_listener(
    EventTargetId::Document,
    type_,
    doc_capture,
    AddEventListenerOptions {
      capture: true,
      ..Default::default()
    },
  )?;
  js.add_js_event_listener(
    EventTargetId::Node(parent),
    type_,
    parent_capture,
    AddEventListenerOptions {
      capture: true,
      ..Default::default()
    },
  )?;
  js.add_js_event_listener(
    EventTargetId::Node(target),
    type_,
    target_capture,
    AddEventListenerOptions {
      capture: true,
      ..Default::default()
    },
  )?;

  js.add_js_event_listener(
    EventTargetId::Node(target),
    type_,
    target_bubble,
    AddEventListenerOptions::default(),
  )?;
  js.add_js_event_listener(
    EventTargetId::Node(parent),
    type_,
    parent_bubble,
    AddEventListenerOptions::default(),
  )?;
  js.add_js_event_listener(
    EventTargetId::Document,
    type_,
    doc_bubble,
    AddEventListenerOptions::default(),
  )?;

  let mut event = Event::new(type_, EventInit { bubbles: true, ..Default::default() });
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
  let key_target = key(js.runtime_mut(), "target");
  let key_current_target = key(js.runtime_mut(), "currentTarget");
  let key_event_phase = key(js.runtime_mut(), "eventPhase");
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
      target: key_target,
      current_target: key_current_target,
      event_phase: key_event_phase,
      stop_propagation: key_stop_propagation,
      stop_immediate_propagation: key_stop_immediate_propagation,
      prevent_default: key_prevent_default,
      expected_target: target_value,
      expected_current_target: target_value,
      expected_phase: 2.0,
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
        target: key_target,
        current_target: key_current_target,
        event_phase: key_event_phase,
        stop_propagation: key_stop_propagation,
        stop_immediate_propagation: key_stop_immediate_propagation,
        prevent_default: key_prevent_default,
        expected_target: target_value,
        expected_current_target: Value::Number(parent.index() as f64),
        expected_phase: 3.0,
      }
    },
  );

  js.add_js_event_listener(
    EventTargetId::Node(target),
    "test",
    stopper,
    AddEventListenerOptions::default(),
  )?;
  js.add_js_event_listener(
    EventTargetId::Node(parent),
    "test",
    parent_bubble,
    AddEventListenerOptions::default(),
  )?;

  let mut event = Event::new("test", EventInit { bubbles: true, ..Default::default() });
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
  let key_target = key(js.runtime_mut(), "target");
  let key_current_target = key(js.runtime_mut(), "currentTarget");
  let key_event_phase = key(js.runtime_mut(), "eventPhase");
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
      target: key_target,
      current_target: key_current_target,
      event_phase: key_event_phase,
      stop_propagation: key_stop_propagation,
      stop_immediate_propagation: key_stop_immediate_propagation,
      prevent_default: key_prevent_default,
      expected_target: target_value,
      expected_current_target: target_value,
      expected_phase: 2.0,
    },
  );

  let second = make_listener(
    &mut js,
    log.clone(),
    "second",
    Action::None,
    ListenerKeys {
      type_: key_type,
      target: key_target,
      current_target: key_current_target,
      event_phase: key_event_phase,
      stop_propagation: key_stop_propagation,
      stop_immediate_propagation: key_stop_immediate_propagation,
      prevent_default: key_prevent_default,
      expected_target: target_value,
      expected_current_target: target_value,
      expected_phase: 2.0,
    },
  );

  js.add_js_event_listener(
    EventTargetId::Node(target),
    "test",
    first,
    AddEventListenerOptions::default(),
  )?;
  js.add_js_event_listener(
    EventTargetId::Node(target),
    "test",
    second,
    AddEventListenerOptions::default(),
  )?;

  let mut event = Event::new("test", EventInit { bubbles: true, ..Default::default() });
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
  let key_target = key(js.runtime_mut(), "target");
  let key_current_target = key(js.runtime_mut(), "currentTarget");
  let key_event_phase = key(js.runtime_mut(), "eventPhase");
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
      target: key_target,
      current_target: key_current_target,
      event_phase: key_event_phase,
      stop_propagation: key_stop_propagation,
      stop_immediate_propagation: key_stop_immediate_propagation,
      prevent_default: key_prevent_default,
      expected_target: target_value,
      expected_current_target: target_value,
      expected_phase: 2.0,
    },
  );

  js.add_js_event_listener(
    EventTargetId::Node(target),
    "test",
    once,
    AddEventListenerOptions {
      once: true,
      ..Default::default()
    },
  )?;

  let mut event = Event::new("test", EventInit { bubbles: true, ..Default::default() });
  js.dispatch_dom_event(&doc, EventTargetId::Node(target), &mut event)?;

  let mut event2 = Event::new("test", EventInit { bubbles: true, ..Default::default() });
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
  let key_target = key(js.runtime_mut(), "target");
  let key_current_target = key(js.runtime_mut(), "currentTarget");
  let key_event_phase = key(js.runtime_mut(), "eventPhase");
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
      target: key_target,
      current_target: key_current_target,
      event_phase: key_event_phase,
      stop_propagation: key_stop_propagation,
      stop_immediate_propagation: key_stop_immediate_propagation,
      prevent_default: key_prevent_default,
      expected_target: target_value,
      expected_current_target: target_value,
      expected_phase: 2.0,
    },
  );

  js.add_js_event_listener(
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
  let listener = js
    .runtime_mut()
    .alloc_function_value(move |rt, _this, args| {
      let event = args.get(0).copied().unwrap_or(Value::Undefined);
      log_for_cb.borrow_mut().push("listener");

      let f = rt.get(event, key_prevent_default)?;
      rt.call_function(f, event, &[])?;

      let prevented = rt.get(event, key_default_prevented)?;
      assert_eq!(prevented, Value::Bool(true));

      Ok(Value::Undefined)
    })
    .expect("alloc function");

  js.add_js_event_listener(
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

  assert!(!dispatch_ok, "dispatchEvent should return false when canceled");
  assert!(event.default_prevented);
  assert_eq!(*log.borrow(), vec!["listener"]);
  Ok(())
}
