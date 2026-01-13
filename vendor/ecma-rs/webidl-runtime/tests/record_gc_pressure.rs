use vm_js::{HeapLimits, PropertyKey, Value, VmError};
use webidl::ir::{IdlType, StringType, TypeContext};
use webidl_runtime::{
  convert_to_idl, resolve_overload, ConvertedArgument, ConvertedValue, JsRuntime as _, Optionality,
  OverloadArg, OverloadSig, VmJsRuntime, WebIdlValue,
};

#[test]
fn record_bigint_conversion_is_gc_safe_under_extreme_gc_pressure() -> Result<(), VmError> {
  // Trigger a GC cycle before every allocation.
  let mut rt = VmJsRuntime::with_limits(HeapLimits::new(1024 * 1024, 0));
  let ctx = TypeContext::default();

  let obj = rt.alloc_object_value()?;

  // Root `obj` for the duration of setup + conversions.
  rt.with_stack_roots(&[obj], |rt| {
    // Define `{a: true, b: false}` with explicit rooting of the property key strings. Under extreme
    // GC pressure, allocating the key string and then defining the property can trigger a GC cycle
    // before the key becomes reachable from the object.
    for (name, value) in [("a", Value::Bool(true)), ("b", Value::Bool(false))] {
      let key = rt.property_key_from_str(name)?;
      let PropertyKey::String(key_str) = key else {
        unreachable!("property_key_from_str should return a string key");
      };
      rt.with_stack_roots(&[Value::String(key_str)], |rt| {
        rt.define_data_property(obj, key, value, true)
      })?;
    }

    let record_ty = IdlType::Record(
      Box::new(IdlType::String(StringType::DomString)),
      Box::new(IdlType::BigInt),
    );

    // Bindings conversions path.
    let converted = convert_to_idl(rt, obj, &record_ty, &ctx)?;
    let ConvertedValue::Record { entries, .. } = converted else {
      panic!("expected record conversion, got {converted:?}");
    };
    for (_k, v) in entries {
      let ConvertedValue::Any(Value::BigInt(b)) = v else {
        panic!("expected record values to convert to BigInt, got {v:?}");
      };
      assert!(rt.heap().is_valid_bigint(b));
    }

    // Overload resolution conversions path.
    let overloads = vec![OverloadSig {
      args: vec![OverloadArg {
        ty: record_ty,
        optionality: Optionality::Required,
        default: None,
      }],
      decl_index: 0,
      distinguishing_arg_index_by_arg_count: None,
    }];
    let out = resolve_overload(rt, &overloads, &[obj])?;
    let [ConvertedArgument::Value(WebIdlValue::Record { entries, .. })] = out.values.as_slice()
    else {
      panic!("expected overload resolution to convert record argument");
    };
    for (_k, v) in entries {
      let WebIdlValue::JsValue(Value::BigInt(b)) = v else {
        panic!("expected record values to convert to BigInt, got {v:?}");
      };
      assert!(rt.heap().is_valid_bigint(*b));
    }

    Ok(())
  })
}

