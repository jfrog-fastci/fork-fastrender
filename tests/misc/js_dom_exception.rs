use fastrender::dom::parse_html;
use fastrender::dom2::Document;
use fastrender::js::bindings::{install_document_query_selector_bindings, DomExceptionClass};
use fastrender::js::webidl::VmJsRuntime;
use fastrender::web::dom::DomException;
use std::cell::RefCell;
use std::rc::Rc;
use webidl_js_runtime::runtime::JsRuntime as _;
use vm_js::{PropertyKey, Value, VmError};

fn prop_key(rt: &mut VmJsRuntime, s: &str) -> PropertyKey {
  let v = rt.alloc_string_value(s).expect("alloc string");
  let Value::String(handle) = v else {
    panic!("expected string");
  };
  PropertyKey::String(handle)
}

fn to_rust_string(rt: &mut VmJsRuntime, v: Value) -> String {
  let v = rt.to_string(v).expect("to_string");
  let Value::String(s) = v else {
    panic!("expected string");
  };
  rt.heap().get_string(s).expect("get_string").to_utf8_lossy()
}

#[test]
fn query_selector_invalid_selector_throws_domexception_syntaxerror() {
  let dom = parse_html("<!doctype html><div></div>").unwrap();
  let mut doc = Document::from_renderer_dom(&dom);

  let expected_message = match doc.query_selector("[", None).unwrap_err() {
    DomException::SyntaxError { message } => message,
  };

  let doc = Rc::new(RefCell::new(doc));

  let mut rt = VmJsRuntime::new();
  let global = rt.alloc_object_value().expect("global object");

  let dom_exception = DomExceptionClass::install(&mut rt, global).expect("install DOMException");
  let document =
    install_document_query_selector_bindings(&mut rt, global, Rc::clone(&doc), dom_exception)
      .expect("install document bindings");

  let key_query_selector = prop_key(&mut rt, "querySelector");
  let query_selector = rt
    .get(document, key_query_selector)
    .expect("get querySelector");

  let selector_arg = rt.alloc_string_value("[").expect("selector string");
  let result = rt.call(
    query_selector,
    document,
    &[selector_arg],
  );

  let thrown = match result {
    Ok(_) => panic!("expected querySelector to throw"),
    Err(VmError::Throw(v)) => v,
    Err(other) => panic!("expected Throw, got {other:?}"),
  };

  let key_name = prop_key(&mut rt, "name");
  let name = rt.get(thrown, key_name).expect("get name");
  assert_eq!(to_rust_string(&mut rt, name), "SyntaxError");

  let key_message = prop_key(&mut rt, "message");
  let message = rt.get(thrown, key_message).expect("get message");
  assert_eq!(to_rust_string(&mut rt, message), expected_message);

  // Ensure the instance inherits `toString` from `DOMException.prototype`.
  let key_to_string = prop_key(&mut rt, "toString");
  let to_string = rt.get(thrown, key_to_string).expect("get toString");
  assert!(rt.is_callable(to_string));

  let rendered = rt.call(to_string, thrown, &[]).expect("call toString");
  let rendered = to_rust_string(&mut rt, rendered);
  assert!(rendered.contains("SyntaxError"));
  assert!(rendered.contains(&expected_message));
}

#[test]
fn domexception_constructor_defaults_name_to_error() {
  let mut rt = VmJsRuntime::new();
  let global = rt.alloc_object_value().expect("global object");
  let class = DomExceptionClass::install(&mut rt, global).expect("install DOMException");

  let message_arg = rt.alloc_string_value("boom").expect("message");
  let instance = rt
    .call(
      class.constructor,
      Value::Undefined,
      &[message_arg],
    )
    .expect("construct DOMException");

  let key_name = prop_key(&mut rt, "name");
  let key_message = prop_key(&mut rt, "message");
  let name = rt.get(instance, key_name).expect("get name");
  let message = rt.get(instance, key_message).expect("get message");
  assert_eq!(to_rust_string(&mut rt, name), "Error");
  assert_eq!(to_rust_string(&mut rt, message), "boom");
}
