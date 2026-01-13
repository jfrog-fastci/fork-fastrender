use vm_js::{PropertyKey, Value};
use webidl::ir::{IdlType, StringType, TypeContext};
use webidl_runtime::{
  convert_arguments, resolve_overload, ArgumentSchema, Optionality, OverloadArg, OverloadSig,
  JsRuntime as _, VmJsRuntime, WebIdlLimits,
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
fn max_string_code_units_is_enforced_in_both_conversion_paths() {
  let limits = WebIdlLimits {
    max_string_code_units: 3,
    max_sequence_length: 1024,
    max_record_entries: 1024,
  };

  // `convert_arguments` (bindings conversions).
  {
    let mut rt = VmJsRuntime::new();
    rt.set_webidl_limits(limits);
    let long = rt.alloc_string_value("abcd").unwrap();

    let ctx = TypeContext::default();
    let params = [ArgumentSchema {
      name: "x",
      ty: IdlType::String(StringType::DomString),
      optional: false,
      variadic: false,
      default: None,
    }];

    let err = convert_arguments(&mut rt, &[long], &params, &ctx).unwrap_err();
    let Some(thrown) = err.thrown_value() else {
      panic!("expected throw");
    };
    assert_eq!(thrown_message(&mut rt, thrown), "string exceeds maximum length");
    assert!(
      thrown_to_string(&mut rt, thrown).starts_with("RangeError"),
      "expected RangeError"
    );
  }

  // `resolve_overload` (overload resolution conversions).
  {
    let mut rt = VmJsRuntime::new();
    rt.set_webidl_limits(limits);
    let long = rt.alloc_string_value("abcd").unwrap();

    let overloads = vec![OverloadSig {
      args: vec![OverloadArg {
        ty: IdlType::String(StringType::DomString),
        optionality: Optionality::Required,
        default: None,
      }],
      decl_index: 0,
      distinguishing_arg_index_by_arg_count: None,
    }];

    let err = resolve_overload(&mut rt, &overloads, &[long]).unwrap_err();
    let Some(thrown) = err.thrown_value() else {
      panic!("expected throw");
    };
    assert_eq!(thrown_message(&mut rt, thrown), "string exceeds maximum length");
    assert!(
      thrown_to_string(&mut rt, thrown).starts_with("RangeError"),
      "expected RangeError"
    );
  }
}
