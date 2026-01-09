use webidl_ir::{IdlType, NamedType, NamedTypeKind, TypeAnnotation};
use webidl_js_runtime::conversions::{convert_to_callback, invoke_callback_interface, to_callback_function};
use webidl_js_runtime::{JsRuntime, VmJsRuntime};
use vm_js::Value;

fn callback_interface_type(name: &str) -> IdlType {
  IdlType::Named(NamedType {
    name: name.to_string(),
    kind: NamedTypeKind::CallbackInterface,
  })
}

fn callback_function_type(name: &str) -> IdlType {
  IdlType::Named(NamedType {
    name: name.to_string(),
    kind: NamedTypeKind::CallbackFunction,
  })
}

#[test]
fn callback_interface_conversion_accepts_function() {
  let mut rt = VmJsRuntime::new();
  let func = rt
    .alloc_function_value(|_rt, _this, _args| Ok(Value::Undefined))
    .unwrap();

  let ty = callback_interface_type("EventListener");
  let got = convert_to_callback(&mut rt, func, &ty).unwrap();
  assert_eq!(got, func);
}

#[test]
fn callback_interface_conversion_accepts_object_with_handle_event_method() {
  let mut rt = VmJsRuntime::new();
  let handle_event = rt
    .alloc_function_value(|_rt, _this, _args| Ok(Value::Undefined))
    .unwrap();

  let obj = rt.alloc_object_value().unwrap();
  let key = rt.property_key_from_str("handleEvent").unwrap();
  rt.define_data_property(obj, key, handle_event, true).unwrap();

  let ty = callback_interface_type("EventListener");
  let got = convert_to_callback(&mut rt, obj, &ty).unwrap();
  assert_eq!(got, obj);
}

#[test]
fn callback_interface_conversion_rejects_non_callable_primitives() {
  let mut rt = VmJsRuntime::new();
  let ty = callback_interface_type("EventListener");
  assert!(convert_to_callback(&mut rt, Value::Number(1.0), &ty).is_err());
  let s = rt.alloc_string_value("x").unwrap();
  assert!(convert_to_callback(&mut rt, s, &ty).is_err());
}

#[test]
fn callback_function_conversion_rejects_non_callable_without_legacy_treat_non_object_as_null() {
  let mut rt = VmJsRuntime::new();
  assert!(to_callback_function(&mut rt, Value::Number(1.0), false).is_err());

  let ty = callback_function_type("VoidFunction");
  assert!(convert_to_callback(&mut rt, Value::Number(1.0), &ty).is_err());
}

#[test]
fn callback_function_conversion_legacy_treat_non_object_as_null_coerces_primitives_to_null() {
  let mut rt = VmJsRuntime::new();
  let got = to_callback_function(&mut rt, Value::Number(1.0), true).unwrap();
  assert_eq!(got, Value::Null);

  let ty = IdlType::Annotated {
    annotations: vec![TypeAnnotation::LegacyTreatNonObjectAsNull],
    inner: Box::new(callback_function_type("VoidFunction")),
  };
  let got = convert_to_callback(&mut rt, Value::Number(1.0), &ty).unwrap();
  assert_eq!(got, Value::Null);
}

#[test]
fn invoke_callback_interface_calls_function_with_undefined_this() {
  let mut rt = VmJsRuntime::new();
  let event = Value::Number(123.0);

  let func = rt
    .alloc_function_value(move |_rt, this, args| {
      assert_eq!(this, Value::Undefined);
      assert_eq!(args, &[event]);
      Ok(Value::Number(1.0))
    })
    .unwrap();

  let result = invoke_callback_interface(&mut rt, func, &[event]).unwrap();
  assert_eq!(result, Value::Number(1.0));
}

#[test]
fn invoke_callback_interface_calls_handle_event_with_object_this() {
  let mut rt = VmJsRuntime::new();
  let event = Value::Number(123.0);

  let obj = rt.alloc_object_value().unwrap();
  let expected_this = obj;

  let handle_event = rt
    .alloc_function_value(move |_rt, this, args| {
      assert_eq!(this, expected_this);
      assert_eq!(args, &[event]);
      Ok(Value::Number(2.0))
    })
    .unwrap();

  let key = rt.property_key_from_str("handleEvent").unwrap();
  rt.define_data_property(obj, key, handle_event, true).unwrap();

  let result = invoke_callback_interface(&mut rt, obj, &[event]).unwrap();
  assert_eq!(result, Value::Number(2.0));
}
