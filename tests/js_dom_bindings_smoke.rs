use fastrender::js::bindings::{install_dom_bindings, DomHost};
use fastrender::js::webidl::VmJsRuntime;
use vm_js::{Value, VmError};
use webidl_js_runtime::JsRuntime as _;

struct TestHost {
  global: Value,
}

impl DomHost for TestHost {
  fn global_object(&mut self) -> Value {
    self.global
  }
}

fn as_utf8_lossy(rt: &VmJsRuntime, v: Value) -> String {
  let Value::String(s) = v else {
    panic!("expected string");
  };
  rt.heap().get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn installs_dom_bindings_and_exposes_constructors_and_document() {
  let mut rt = VmJsRuntime::new();
  let global = rt.alloc_object_value().unwrap();
  let mut host = TestHost { global };

  install_dom_bindings(&mut rt, &mut host).unwrap();

  let k_window = rt.prop_key("Window").unwrap();
  let ctor_window = rt.get(global, k_window).unwrap();
  assert!(rt.is_callable(ctor_window));

  let k_document = rt.prop_key("document").unwrap();
  let document = rt.get(global, k_document).unwrap();
  assert!(rt.is_object(document));

  // Ensure `Document.prototype.createElement` was installed.
  let k_create_element = rt.prop_key("createElement").unwrap();
  let create_element = rt
    .get(document, k_create_element)
    .unwrap();
  assert!(rt.is_callable(create_element));

  // `Element.prototype.querySelector` should exist.
  let k_element = rt.prop_key("Element").unwrap();
  let ctor_element = rt.get(global, k_element).unwrap();
  let k_prototype = rt.prop_key("prototype").unwrap();
  let element_proto = rt
    .get(ctor_element, k_prototype)
    .unwrap();
  let k_query_selector = rt.prop_key("querySelector").unwrap();
  let qs_desc = rt
    .get_own_property(element_proto, k_query_selector)
    .unwrap();
  assert!(qs_desc.is_some());
}

#[test]
fn unimplemented_methods_throw_type_error_and_validate_required_args() {
  let mut rt = VmJsRuntime::new();
  let global = rt.alloc_object_value().unwrap();
  let mut host = TestHost { global };
  install_dom_bindings(&mut rt, &mut host).unwrap();

  let k_document = rt.prop_key("document").unwrap();
  let document = rt.get(global, k_document).unwrap();
  let k_create_element = rt.prop_key("createElement").unwrap();
  let create_element = rt
    .get(document, k_create_element)
    .unwrap();

  // Missing required argument should throw a TypeError with a deterministic message.
  let err = rt.call_function(create_element, document, &[]).unwrap_err();
  let thrown = match err {
    VmError::Throw(v) => v,
    other => panic!("expected VmError::Throw, got {other:?}"),
  };
  let msg = rt.to_string(thrown).unwrap();
  assert!(
    as_utf8_lossy(&rt, msg).contains("Document.createElement: expected at least 1 arguments"),
    "got: {}",
    as_utf8_lossy(&rt, msg)
  );

  // With the required argument present, the stub body should still throw a TypeError.
  let arg0 = rt.alloc_string_value("div").unwrap();
  let err = rt
    .call_function(create_element, document, &[arg0])
    .unwrap_err();
  let thrown = match err {
    VmError::Throw(v) => v,
    other => panic!("expected VmError::Throw, got {other:?}"),
  };
  let msg = rt.to_string(thrown).unwrap();
  assert!(
    as_utf8_lossy(&rt, msg).contains("TypeError: not implemented"),
    "got: {}",
    as_utf8_lossy(&rt, msg)
  );
}

#[test]
fn query_selector_invalid_selector_throws_domexception_syntaxerror() {
  let mut rt = VmJsRuntime::new();
  let global = rt.alloc_object_value().unwrap();
  let mut host = TestHost { global };
  install_dom_bindings(&mut rt, &mut host).unwrap();

  let k_document = rt.prop_key("document").unwrap();
  let document = rt.get(global, k_document).unwrap();
  let k_query_selector = rt.prop_key("querySelector").unwrap();
  let query_selector = rt.get(document, k_query_selector).unwrap();
  assert!(rt.is_callable(query_selector));

  let arg0 = rt.alloc_string_value("body").unwrap();
  let out = rt.call_function(query_selector, document, &[arg0]).unwrap();
  assert!(matches!(out, Value::Null));

  let arg0 = rt.alloc_string_value("[").unwrap();
  let err = rt.call_function(query_selector, document, &[arg0]).unwrap_err();
  let thrown = match err {
    VmError::Throw(v) => v,
    other => panic!("expected VmError::Throw, got {other:?}"),
  };

  let k_name = rt.prop_key("name").unwrap();
  let name = rt.get(thrown, k_name).unwrap();
  let name = rt.to_string(name).unwrap();
  assert_eq!(as_utf8_lossy(&rt, name), "SyntaxError");

  let k_message = rt.prop_key("message").unwrap();
  let message = rt.get(thrown, k_message).unwrap();
  let message = rt.to_string(message).unwrap();
  let message = as_utf8_lossy(&rt, message);
  assert!(
    message.contains("Invalid selector"),
    "expected message to mention invalid selector, got {message:?}"
  );
}
