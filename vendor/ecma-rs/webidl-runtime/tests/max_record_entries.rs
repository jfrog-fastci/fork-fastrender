use vm_js::{PropertyKey, Value};
use webidl::ir::{IdlType, NumericType, StringType, TypeContext};
use webidl_runtime::{
  convert_to_idl, resolve_overload, Optionality, OverloadArg, OverloadSig, JsRuntime as _,
  VmJsRuntime, WebIdlLimits,
};

fn thrown_to_string(rt: &mut VmJsRuntime, thrown: Value) -> String {
  rt
    .with_stack_roots(&[thrown], |rt| {
      let s = rt.to_string(thrown)?;
      let Value::String(handle) = s else {
        panic!("expected string from Error.prototype.toString");
      };
      Ok(rt.heap().get_string(handle).unwrap().to_utf8_lossy())
    })
    .unwrap()
}

fn thrown_message(rt: &mut VmJsRuntime, thrown: Value) -> String {
  rt
    .with_stack_roots(&[thrown], |rt| {
      let Value::Object(obj) = thrown else {
        panic!("expected object");
      };
      let key_value = rt.alloc_string_value("message")?;
      let Value::String(key) = key_value else {
        panic!("expected string value for key");
      };
      let msg = rt.get(Value::Object(obj), PropertyKey::String(key))?;
      let msg = rt.to_string(msg)?;
      let Value::String(msg) = msg else {
        panic!("expected string message");
      };
      Ok(rt.heap().get_string(msg).unwrap().to_utf8_lossy())
    })
    .unwrap()
}

#[test]
fn max_record_entries_is_enforced_in_both_conversion_paths() {
  let limits = WebIdlLimits {
    max_string_code_units: 1024,
    max_sequence_length: 1024,
    max_record_entries: 1,
  };

  let record_ty = IdlType::Record(
    Box::new(IdlType::String(StringType::DomString)),
    Box::new(IdlType::Numeric(NumericType::Long)),
  );

  // `convert_to_idl` (bindings conversions).
  {
    let mut rt = VmJsRuntime::new();
    rt.set_webidl_limits(limits);
    let ctx = TypeContext::default();

    let obj = rt.alloc_object_value().unwrap();
    let a_key = rt.property_key_from_str("a").unwrap();
    let b_key = rt.property_key_from_str("b").unwrap();
    rt.define_data_property(obj, a_key, Value::Number(1.0), true)
      .unwrap();
    rt.define_data_property(obj, b_key, Value::Number(2.0), true)
      .unwrap();

    let err = convert_to_idl(&mut rt, obj, &record_ty, &ctx).unwrap_err();
    let Some(thrown) = err.thrown_value() else {
      panic!("expected throw");
    };
    assert_eq!(thrown_message(&mut rt, thrown), "record exceeds maximum entry count");
    assert!(
      thrown_to_string(&mut rt, thrown).starts_with("RangeError"),
      "expected RangeError"
    );
  }

  // `resolve_overload` (overload resolution conversions).
  {
    let mut rt = VmJsRuntime::new();
    rt.set_webidl_limits(limits);

    let obj = rt.alloc_object_value().unwrap();
    let a_key = rt.property_key_from_str("a").unwrap();
    let b_key = rt.property_key_from_str("b").unwrap();
    rt.define_data_property(obj, a_key, Value::Number(1.0), true)
      .unwrap();
    rt.define_data_property(obj, b_key, Value::Number(2.0), true)
      .unwrap();

    let overloads = vec![OverloadSig {
      args: vec![OverloadArg {
        ty: record_ty.clone(),
        optionality: Optionality::Required,
        default: None,
      }],
      decl_index: 0,
      distinguishing_arg_index_by_arg_count: None,
    }];

    let err = resolve_overload(&mut rt, &overloads, &[obj]).unwrap_err();
    let Some(thrown) = err.thrown_value() else {
      panic!("expected throw");
    };
    assert_eq!(thrown_message(&mut rt, thrown), "record exceeds maximum entry count");
    assert!(
      thrown_to_string(&mut rt, thrown).starts_with("RangeError"),
      "expected RangeError"
    );
  }
}

