use std::cell::Cell;
use std::rc::Rc;
use vm_js::{PropertyKey, Value};
use webidl::ir::{IdlType, NumericType, StringType, TypeContext};
use webidl_runtime::{
  convert_to_idl, resolve_overload, JsRuntime as _, Optionality, OverloadArg, OverloadSig,
  VmJsRuntime, WebIdlLimits,
};

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
fn record_key_conversion_happens_before_get() {
  let limits = WebIdlLimits {
    max_string_code_units: 2,
    max_sequence_length: 1024,
    max_record_entries: 1024,
  };

  let ctx = TypeContext::default();
  let record_ty = IdlType::Record(
    Box::new(IdlType::String(StringType::DomString)),
    Box::new(IdlType::Numeric(NumericType::Long)),
  );

  let key = "aaa"; // 3 UTF-16 code units > `max_string_code_units`

  // `convert_to_idl` (bindings conversions).
  {
    let mut rt = VmJsRuntime::new();
    rt.set_webidl_limits(limits);

    let getter_called = Rc::new(Cell::new(false));
    let getter_called_for_fn = getter_called.clone();
    let getter = rt
      .alloc_function_value(move |_rt, _this, _args| {
        getter_called_for_fn.set(true);
        Ok(Value::Number(1.0))
      })
      .unwrap();

    let obj = rt.alloc_object_value().unwrap();
    let key_pk = rt.property_key_from_str(key).unwrap();
    rt.define_accessor_property(obj, key_pk, getter, Value::Undefined, true)
      .unwrap();

    let err = convert_to_idl(&mut rt, obj, &record_ty, &ctx).unwrap_err();
    let Some(thrown) = err.thrown_value() else {
      panic!("expected throw");
    };
    assert_eq!(thrown_message(&mut rt, thrown), "string exceeds maximum length");
    assert!(
      !getter_called.get(),
      "record conversion must convert key before invoking the property getter"
    );
  }

  // `resolve_overload` (overload resolution conversions).
  {
    let mut rt = VmJsRuntime::new();
    rt.set_webidl_limits(limits);

    let getter_called = Rc::new(Cell::new(false));
    let getter_called_for_fn = getter_called.clone();
    let getter = rt
      .alloc_function_value(move |_rt, _this, _args| {
        getter_called_for_fn.set(true);
        Ok(Value::Number(1.0))
      })
      .unwrap();

    let obj = rt.alloc_object_value().unwrap();
    let key_pk = rt.property_key_from_str(key).unwrap();
    rt.define_accessor_property(obj, key_pk, getter, Value::Undefined, true)
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
    assert_eq!(thrown_message(&mut rt, thrown), "string exceeds maximum length");
    assert!(
      !getter_called.get(),
      "overload resolution must convert record keys before invoking property getters"
    );
  }
}

