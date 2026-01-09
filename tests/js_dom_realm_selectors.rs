use fastrender::dom::parse_html;
use fastrender::dom2::Document as Dom2Document;
use fastrender::js::DomJsRealm;
use fastrender::js::webidl::VmJsRuntime;
use vm_js::{Value, VmError};
use webidl_js_runtime::JsRuntime as _;

fn as_utf8_lossy(rt: &VmJsRuntime, v: Value) -> String {
  let Value::String(s) = v else {
    panic!("expected string, got {v:?}");
  };
  rt.heap().get_string(s).unwrap().to_utf8_lossy()
}

fn call_method(
  rt: &mut VmJsRuntime,
  receiver: Value,
  name: &str,
  args: &[Value],
) -> Result<Value, VmError> {
  let key = rt.prop_key(name)?;
  let func = rt.get(receiver, key)?;
  rt.call_function(func, receiver, args)
}

fn make_realm(html: &str) -> DomJsRealm {
  let renderer_dom = parse_html(html).expect("parse_html");
  let dom = Dom2Document::from_renderer_dom(&renderer_dom);
  DomJsRealm::new(dom).expect("DomJsRealm::new")
}

#[test]
fn dom_realm_query_selector_and_query_selector_all_return_wrapped_nodes() -> Result<(), VmError> {
  let mut realm = make_realm(
    r#"<!doctype html>
      <div id="outer">
        <span class="inner"></span>
        <span class="inner"></span>
      </div>
    "#,
  );
  let document = realm.document();
  let rt = realm.runtime_mut();

  let sel_outer = rt.alloc_string_value("#outer")?;
  let outer_a = call_method(rt, document, "querySelector", &[sel_outer])?;
  let sel_outer = rt.alloc_string_value("#outer")?;
  let outer_b = call_method(rt, document, "querySelector", &[sel_outer])?;
  assert_eq!(outer_a, outer_b, "wrapper identity should be stable for the same NodeId");

  let attr_id = rt.alloc_string_value("id")?;
  let outer_id = call_method(rt, outer_a, "getAttribute", &[attr_id])?;
  let outer_id = rt.to_string(outer_id)?;
  assert_eq!(as_utf8_lossy(rt, outer_id), "outer");

  let sel_inner = rt.alloc_string_value(".inner")?;
  let list = call_method(rt, document, "querySelectorAll", &[sel_inner])?;
  assert!(rt.is_object(list));
  let k_length = rt.prop_key("length")?;
  let len = rt.get(list, k_length)?;
  let Value::Number(len) = len else {
    panic!("expected length to be a number, got {len:?}");
  };
  assert_eq!(len, 2.0);

  let k_0 = rt.prop_key("0")?;
  let first = rt.get(list, k_0)?;
  assert!(rt.is_object(first));
  let attr_class = rt.alloc_string_value("class")?;
  let class = call_method(rt, first, "getAttribute", &[attr_class])?;
  let class = rt.to_string(class)?;
  assert_eq!(as_utf8_lossy(rt, class), "inner");

  Ok(())
}

#[test]
fn dom_realm_matches_and_closest_work() -> Result<(), VmError> {
  let mut realm = make_realm(
    r#"<!doctype html>
      <div id="outer">
        <span class="inner"></span>
      </div>
    "#,
  );
  let document = realm.document();
  let rt = realm.runtime_mut();

  let sel_inner = rt.alloc_string_value(".inner")?;
  let inner = call_method(rt, document, "querySelector", &[sel_inner])?;
  assert!(rt.is_object(inner));

  let selectors = rt.alloc_string_value(".inner")?;
  let matched = call_method(rt, inner, "matches", &[selectors])?;
  assert_eq!(matched, Value::Bool(true));

  let selectors = rt.alloc_string_value("#outer")?;
  let matched = call_method(rt, inner, "matches", &[selectors])?;
  assert_eq!(matched, Value::Bool(false));

  let selectors = rt.alloc_string_value("#outer")?;
  let closest = call_method(rt, inner, "closest", &[selectors])?;
  let sel_outer = rt.alloc_string_value("#outer")?;
  let outer = call_method(rt, document, "querySelector", &[sel_outer])?;
  assert_eq!(closest, outer);

  Ok(())
}

#[test]
fn dom_realm_invalid_selector_throws_domexception_syntaxerror() -> Result<(), VmError> {
  let mut realm = make_realm(r#"<!doctype html><div></div>"#);
  let document = realm.document();
  let rt = realm.runtime_mut();

  let invalid = rt.alloc_string_value("[")?;
  let err = call_method(rt, document, "querySelector", &[invalid]).unwrap_err();
  let Some(thrown) = err.thrown_value() else {
    panic!("expected thrown error, got {err:?}");
  };

  let k_name = rt.prop_key("name")?;
  let name = rt.get(thrown, k_name)?;
  let name = rt.to_string(name)?;
  assert_eq!(as_utf8_lossy(rt, name), "SyntaxError");

  let k_message = rt.prop_key("message")?;
  let message = rt.get(thrown, k_message)?;
  let message = rt.to_string(message)?;
  assert!(
    as_utf8_lossy(rt, message).contains("Invalid selector"),
    "expected message to mention invalid selector"
  );

  Ok(())
}
