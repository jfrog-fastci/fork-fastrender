use fastrender::dom::{DomNode, DomNodeType};
use fastrender::dom2::{Document, NodeId};
use fastrender::js::events_bindings::DomEventsRealm;
use fastrender::js::webidl::VmJsRuntime;
use selectors::context::QuirksMode;
use std::cell::RefCell;
use std::rc::Rc;
use vm_js::{PropertyKey, Value};
use webidl_js_runtime::JsRuntime as _;

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

fn make_doc_body_target() -> (Document, NodeId, NodeId) {
  // Document → <body> → <div>
  let root = DomNode {
    node_type: DomNodeType::Document {
      quirks_mode: QuirksMode::NoQuirks,
    },
    children: vec![element("body", vec![element("div", vec![])])],
  };
  let doc = Document::from_renderer_dom(&root);
  let root_id = doc.root();
  let body = doc.node(root_id).children[0];
  let target = doc.node(body).children[0];
  (doc, body, target)
}

fn key(rt: &mut VmJsRuntime, name: &str) -> PropertyKey {
  let v = rt.alloc_string_value(name).expect("alloc key string");
  let Value::String(s) = v else {
    panic!("expected string value for property key");
  };
  PropertyKey::String(s)
}

fn bool_value(v: Value) -> bool {
  match v {
    Value::Bool(b) => b,
    other => panic!("expected bool, got {other:?}"),
  }
}

#[test]
fn capture_and_bubble_listener_order_document_body_target() {
  let (dom, body_id, target_id) = make_doc_body_target();
  let mut rt = VmJsRuntime::new();
  let realm = DomEventsRealm::new(&mut rt, dom).expect("install realm");

  let body = realm
    .create_node_wrapper(&mut rt, body_id)
    .expect("body wrapper");
  let target = realm
    .create_node_wrapper(&mut rt, target_id)
    .expect("target wrapper");

  let log: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));

  let add_listener = |rt: &mut VmJsRuntime,
                      target_obj: Value,
                      label: &'static str,
                      capture: bool,
                      log: Rc<RefCell<Vec<&'static str>>>| {
    let cb = rt
      .alloc_function_value(move |_rt, _this, _args| {
        log.borrow_mut().push(label);
        Ok(Value::Undefined)
      })
      .expect("callback fn");
    let add_key = key(rt, "addEventListener");
    let add = rt.get(target_obj, add_key).expect("get addEventListener");
    let type_ = rt.alloc_string_value("x").expect("type string");
    let mut args = vec![type_, cb];
    if capture {
      args.push(Value::Bool(true));
    }
    rt.call_function(add, target_obj, &args)
      .expect("addEventListener call");
  };

  add_listener(&mut rt, realm.window, "window_capture", true, log.clone());
  add_listener(&mut rt, realm.document, "document_capture", true, log.clone());
  add_listener(&mut rt, body, "body_capture", true, log.clone());
  add_listener(&mut rt, target, "target_capture", true, log.clone());

  add_listener(&mut rt, target, "target_bubble", false, log.clone());
  add_listener(&mut rt, body, "body_bubble", false, log.clone());
  add_listener(&mut rt, realm.document, "document_bubble", false, log.clone());
  add_listener(&mut rt, realm.window, "window_bubble", false, log.clone());

  // Create a bubbling event.
  let init = rt.alloc_object_value().expect("init");
  let bubbles_key = key(&mut rt, "bubbles");
  rt.define_data_property(init, bubbles_key, Value::Bool(true), true)
    .expect("init.bubbles");
  let type_ = rt.alloc_string_value("x").unwrap();
  let event = rt
    .call_function(realm.event_constructor, Value::Undefined, &[type_, init])
    .expect("new Event");

  let dispatch_key = key(&mut rt, "dispatchEvent");
  let dispatch = rt.get(target, dispatch_key).expect("get dispatchEvent");
  let res = rt
    .call_function(dispatch, target, &[event])
    .expect("dispatchEvent");
  assert!(bool_value(res));

  assert_eq!(
    log.borrow().as_slice(),
    &[
      "window_capture",
      "document_capture",
      "body_capture",
      "target_capture",
      "target_bubble",
      "body_bubble",
      "document_bubble",
      "window_bubble",
    ]
  );
}

#[test]
fn once_option_removes_after_first_dispatch() {
  let (dom, _body_id, target_id) = make_doc_body_target();
  let mut rt = VmJsRuntime::new();
  let realm = DomEventsRealm::new(&mut rt, dom).expect("install realm");

  let target = realm
    .create_node_wrapper(&mut rt, target_id)
    .expect("target wrapper");

  let log: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));
  let cb_log = log.clone();
  let cb = rt
    .alloc_function_value(move |_rt, _this, _args| {
      cb_log.borrow_mut().push("once");
      Ok(Value::Undefined)
    })
    .expect("callback fn");

  let options = rt.alloc_object_value().expect("options");
  let once_key = key(&mut rt, "once");
  rt.define_data_property(options, once_key, Value::Bool(true), true)
    .expect("options.once");

  let add_key = key(&mut rt, "addEventListener");
  let add = rt.get(target, add_key).expect("get addEventListener");
  let type_ = rt.alloc_string_value("x").unwrap();
  rt.call_function(add, target, &[type_, cb, options])
    .expect("addEventListener");

  for _ in 0..2 {
    let type_ = rt.alloc_string_value("x").unwrap();
    let event = rt
      .call_function(realm.event_constructor, Value::Undefined, &[type_])
      .expect("new Event");
    let dispatch_key = key(&mut rt, "dispatchEvent");
    let dispatch = rt.get(target, dispatch_key).expect("dispatchEvent getter");
    rt.call_function(dispatch, target, &[event])
      .expect("dispatchEvent");
  }

  assert_eq!(log.borrow().as_slice(), &["once"]);
}

#[test]
fn passive_option_ignores_prevent_default() {
  let (dom, _body_id, target_id) = make_doc_body_target();
  let mut rt = VmJsRuntime::new();
  let realm = DomEventsRealm::new(&mut rt, dom).expect("install realm");

  let target = realm
    .create_node_wrapper(&mut rt, target_id)
    .expect("target wrapper");

  let cb = rt
    .alloc_function_value(move |rt, _this, args| {
      let event = args.get(0).copied().unwrap_or(Value::Undefined);
      let prevent_key = key(rt, "preventDefault");
      let prevent = rt.get(event, prevent_key)?;
      rt.call_function(prevent, event, &[])?;
      Ok(Value::Undefined)
    })
    .expect("callback fn");

  let options = rt.alloc_object_value().expect("options");
  let passive_key = key(&mut rt, "passive");
  rt.define_data_property(options, passive_key, Value::Bool(true), true)
    .expect("options.passive");

  let add_key = key(&mut rt, "addEventListener");
  let add = rt.get(target, add_key).expect("get addEventListener");
  let type_ = rt.alloc_string_value("x").unwrap();
  rt.call_function(add, target, &[type_, cb, options])
    .expect("addEventListener");

  let init = rt.alloc_object_value().expect("init");
  let cancelable_key = key(&mut rt, "cancelable");
  rt.define_data_property(init, cancelable_key, Value::Bool(true), true)
    .expect("init.cancelable");
  let type_ = rt.alloc_string_value("x").unwrap();
  let event = rt
    .call_function(realm.event_constructor, Value::Undefined, &[type_, init])
    .expect("new Event");

  let dispatch_key = key(&mut rt, "dispatchEvent");
  let dispatch = rt.get(target, dispatch_key).expect("get dispatchEvent");
  let res = rt
    .call_function(dispatch, target, &[event])
    .expect("dispatchEvent");
  assert!(bool_value(res));

  let default_prevented_key = key(&mut rt, "defaultPrevented");
  let default_prevented = rt
    .get(event, default_prevented_key)
    .expect("get defaultPrevented");
  assert!(!bool_value(default_prevented));
}

#[test]
fn stop_immediate_propagation_stops_later_listeners_on_same_target() {
  let (dom, _body_id, target_id) = make_doc_body_target();
  let mut rt = VmJsRuntime::new();
  let realm = DomEventsRealm::new(&mut rt, dom).expect("install realm");

  let target = realm
    .create_node_wrapper(&mut rt, target_id)
    .expect("target wrapper");

  let log: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));

  let log_first = log.clone();
  let first = rt
    .alloc_function_value(move |rt, _this, args| {
      log_first.borrow_mut().push("first");
      let event = args.get(0).copied().unwrap_or(Value::Undefined);
      let stop_key = key(rt, "stopImmediatePropagation");
      let stop = rt.get(event, stop_key)?;
      rt.call_function(stop, event, &[])?;
      Ok(Value::Undefined)
    })
    .expect("first fn");

  let log_second = log.clone();
  let second = rt
    .alloc_function_value(move |_rt, _this, _args| {
      log_second.borrow_mut().push("second");
      Ok(Value::Undefined)
    })
    .expect("second fn");

  let add_key = key(&mut rt, "addEventListener");
  let add = rt.get(target, add_key).expect("get addEventListener");
  let type_ = rt.alloc_string_value("x").unwrap();
  rt.call_function(add, target, &[type_, first])
    .expect("add first");
  let add_key = key(&mut rt, "addEventListener");
  let add = rt.get(target, add_key).expect("get addEventListener");
  let type_ = rt.alloc_string_value("x").unwrap();
  rt.call_function(add, target, &[type_, second])
    .expect("add second");

  let init = rt.alloc_object_value().expect("init");
  let bubbles_key = key(&mut rt, "bubbles");
  rt.define_data_property(init, bubbles_key, Value::Bool(true), true)
    .expect("init.bubbles");
  let type_ = rt.alloc_string_value("x").unwrap();
  let event = rt
    .call_function(realm.event_constructor, Value::Undefined, &[type_, init])
    .expect("new Event");

  let dispatch_key = key(&mut rt, "dispatchEvent");
  let dispatch = rt.get(target, dispatch_key).expect("get dispatchEvent");
  rt.call_function(dispatch, target, &[event])
    .expect("dispatchEvent");

  assert_eq!(log.borrow().as_slice(), &["first"]);
}
