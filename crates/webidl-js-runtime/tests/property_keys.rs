use vm_js::{PropertyKey, Value, VmError};
use webidl_js_runtime::{JsRuntime, VmJsRuntime};

fn as_utf8_lossy(rt: &VmJsRuntime, v: Value) -> String {
  let Value::String(s) = v else {
    panic!("expected string");
  };
  rt.heap().get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn property_key_from_str_roundtrips_with_get() -> Result<(), VmError> {
  let mut rt = VmJsRuntime::new();
  let obj = rt.alloc_object_value()?;

  let key = rt.property_key_from_str("x")?;
  assert!(rt.property_key_is_string(key));
  assert!(!rt.property_key_is_symbol(key));

  rt.define_data_property(obj, key, Value::Number(42.0), true)?;
  let got = rt.get(obj, key)?;
  assert_eq!(got, Value::Number(42.0));

  let keys = rt.own_property_keys(obj)?;
  assert_eq!(keys, vec![key]);

  let key_as_string = rt.property_key_to_js_string(key)?;
  assert_eq!(as_utf8_lossy(&rt, key_as_string), "x");
  Ok(())
}

#[test]
fn property_key_to_js_string_throws_type_error_for_symbol_keys() -> Result<(), VmError> {
  let mut rt = VmJsRuntime::new();

  let desc = rt.alloc_string_value("sym")?;
  let Value::String(desc) = desc else {
    panic!("expected string");
  };
  let sym = rt.heap_mut().symbol_for(desc)?;
  let key = PropertyKey::Symbol(sym);
  assert!(rt.property_key_is_symbol(key));
  assert!(!rt.property_key_is_string(key));

  let err = rt.property_key_to_js_string(key).unwrap_err();
  let thrown = match err {
    VmError::Throw(v) => v,
    other => panic!("expected VmError::Throw, got {other:?}"),
  };

  let name_key = rt.property_key_from_str("name")?;
  let name = rt.get(thrown, name_key)?;
  assert_eq!(as_utf8_lossy(&rt, name), "TypeError");
  Ok(())
}

