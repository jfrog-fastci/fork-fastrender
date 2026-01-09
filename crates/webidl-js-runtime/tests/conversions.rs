use std::cell::Cell;
use std::collections::BTreeMap;
use std::rc::Rc;
use vm_js::{Value, VmError};
use webidl_ir::{
  parse_default_value, DictionaryMemberSchema, DictionarySchema, IdlType, NamedType, NamedTypeKind,
  NumericType, StringType, TypeAnnotation, TypeContext,
};
use webidl_js_runtime::{convert_to_idl, ConvertedValue, JsRuntime, VmJsRuntime, WebIdlJsRuntime};

fn error_to_string(rt: &mut VmJsRuntime, err: VmError) -> String {
  let VmError::Throw(thrown) = err else {
    panic!("expected throw, got {err:?}");
  };
  let s = rt.to_string(thrown).unwrap();
  rt.string_to_utf8_lossy(s).unwrap()
}

#[test]
fn byte_string_rejects_code_points_above_ff() {
  let mut rt = VmJsRuntime::new();
  let ctx = TypeContext::default();

  // U+0100 (256) is outside ByteString's allowed range.
  let v = rt.alloc_string_value("\u{0100}").unwrap();
  let ty = IdlType::String(StringType::ByteString);

  let err = convert_to_idl(&mut rt, v, &ty, &ctx).unwrap_err();
  assert!(error_to_string(&mut rt, err).starts_with("TypeError"));
}

#[test]
fn enforce_range_throws_range_error() {
  let mut rt = VmJsRuntime::new();
  let ctx = TypeContext::default();

  let ty = IdlType::Annotated {
    annotations: vec![TypeAnnotation::EnforceRange],
    inner: Box::new(IdlType::Numeric(NumericType::Octet)),
  };
  let err = convert_to_idl(&mut rt, Value::Number(256.0), &ty, &ctx).unwrap_err();
  assert!(error_to_string(&mut rt, err).starts_with("RangeError"));
}

#[test]
fn clamp_rounds_ties_to_even() {
  let mut rt = VmJsRuntime::new();
  let ctx = TypeContext::default();

  let ty = IdlType::Annotated {
    annotations: vec![TypeAnnotation::Clamp],
    inner: Box::new(IdlType::Numeric(NumericType::Long)),
  };

  let converted = convert_to_idl(&mut rt, Value::Number(2.5), &ty, &ctx).unwrap();
  assert_eq!(converted, ConvertedValue::Long(2));
}

#[test]
fn sequence_conversion_from_custom_iterable() {
  let mut rt = VmJsRuntime::new();
  let ctx = TypeContext::default();

  let next_key = rt.property_key_from_str("next").unwrap();
  let done_key = rt.property_key_from_str("done").unwrap();
  let value_key = rt.property_key_from_str("value").unwrap();

  let iterator_obj = rt.alloc_object_value().unwrap();

  let idx = Rc::new(Cell::new(0usize));
  let values = Rc::new(vec![Value::Number(1.0), Value::Number(2.0)]);
  let idx_for_next = idx.clone();
  let values_for_next = values.clone();

  let next_fn = rt
    .alloc_function_value(move |rt, _this, _args| {
      let i = idx_for_next.get();
      let result_obj = rt.alloc_object_value()?;
      if i >= values_for_next.len() {
        rt.define_data_property(result_obj, done_key, Value::Bool(true), true)?;
        rt.define_data_property(result_obj, value_key, Value::Undefined, true)?;
      } else {
        rt.define_data_property(result_obj, done_key, Value::Bool(false), true)?;
        rt.define_data_property(result_obj, value_key, values_for_next[i], true)?;
        idx_for_next.set(i + 1);
      }
      Ok(result_obj)
    })
    .unwrap();

  rt.define_data_property(iterator_obj, next_key, next_fn, true)
    .unwrap();

  let iterator_getter = rt
    .alloc_function_value(move |_rt, _this, _args| Ok(iterator_obj))
    .unwrap();

  let iterable_obj = rt.alloc_object_value().unwrap();
  let iterator_sym = rt.symbol_iterator().unwrap();
  rt.define_data_property(iterable_obj, iterator_sym, iterator_getter, true)
    .unwrap();

  let ty = IdlType::Sequence(Box::new(IdlType::Numeric(NumericType::Long)));
  let converted = convert_to_idl(&mut rt, iterable_obj, &ty, &ctx).unwrap();

  let ConvertedValue::Sequence { values, .. } = converted else {
    panic!("expected sequence, got {converted:?}");
  };
  assert_eq!(
    values,
    vec![ConvertedValue::Long(1), ConvertedValue::Long(2)]
  );
}

#[test]
fn dictionary_defaults_and_required_members() {
  let mut rt = VmJsRuntime::new();

  let mut ctx = TypeContext::default();
  ctx.add_dictionary(DictionarySchema {
    name: "TestDict".to_string(),
    inherits: None,
    members: vec![
      DictionaryMemberSchema {
        name: "opt".to_string(),
        required: false,
        ty: IdlType::Numeric(NumericType::Long),
        default: Some(parse_default_value("5").unwrap()),
      },
      DictionaryMemberSchema {
        name: "req".to_string(),
        required: true,
        ty: IdlType::String(StringType::DomString),
        default: None,
      },
    ],
  });

  let dict_ty = IdlType::Named(NamedType {
    name: "TestDict".to_string(),
    kind: NamedTypeKind::Unresolved,
  });

  // Missing required member throws.
  let empty_obj = rt.alloc_object_value().unwrap();
  let err = convert_to_idl(&mut rt, empty_obj, &dict_ty, &ctx).unwrap_err();
  assert!(error_to_string(&mut rt, err).starts_with("TypeError"));

  // Defaults are applied when missing.
  let obj = rt.alloc_object_value().unwrap();
  let req_key = rt.property_key_from_str("req").unwrap();
  let req_val = rt.alloc_string_value("hello").unwrap();
  rt.define_data_property(obj, req_key, req_val, true).unwrap();

  let converted = convert_to_idl(&mut rt, obj, &dict_ty, &ctx).unwrap();
  let ConvertedValue::Dictionary { name, members } = converted else {
    panic!("expected dictionary, got {converted:?}");
  };
  assert_eq!(name, "TestDict");

  let expected = BTreeMap::from([
    ("opt".to_string(), ConvertedValue::Long(5)),
    ("req".to_string(), ConvertedValue::String("hello".to_string())),
  ]);
  assert_eq!(members, expected);
}

#[test]
fn interface_named_type_converts_platform_object_to_opaque_id() {
  let mut rt = VmJsRuntime::new();
  let ctx = TypeContext::default();

  let opaque = 0xfeed_u64;
  let obj = rt
    .alloc_platform_object_value("Node", &["EventTarget"], opaque)
    .unwrap();

  let ty = IdlType::Named(NamedType {
    name: "Node".to_string(),
    kind: NamedTypeKind::Interface,
  });

  let converted = convert_to_idl(&mut rt, obj, &ty, &ctx).unwrap();
  let ConvertedValue::PlatformObject(host) = converted else {
    panic!("expected platform object, got {converted:?}");
  };
  assert_eq!(host.downcast_ref::<u64>().copied(), Some(opaque));
}

#[test]
fn interface_named_type_rejects_wrong_interface() {
  let mut rt = VmJsRuntime::new();
  let ctx = TypeContext::default();

  let obj = rt
    .alloc_platform_object_value("Node", &["EventTarget"], 1)
    .unwrap();

  let ty = IdlType::Named(NamedType {
    name: "Document".to_string(),
    kind: NamedTypeKind::Interface,
  });

  assert!(convert_to_idl(&mut rt, obj, &ty, &ctx).is_err());
}

#[test]
fn object_conversion_uses_to_object() {
  let mut rt = VmJsRuntime::new();
  let ctx = TypeContext::default();

  let converted = convert_to_idl(&mut rt, Value::Bool(true), &IdlType::Object, &ctx).unwrap();
  let ConvertedValue::Object(obj) = converted else {
    panic!("expected object, got {converted:?}");
  };
  assert!(rt.is_object(obj));
}

#[test]
fn frozen_array_conversion_from_custom_iterable() {
  let mut rt = VmJsRuntime::new();
  let ctx = TypeContext::default();

  let next_key = rt.property_key_from_str("next").unwrap();
  let done_key = rt.property_key_from_str("done").unwrap();
  let value_key = rt.property_key_from_str("value").unwrap();

  let iterator_obj = rt.alloc_object_value().unwrap();

  let idx = Rc::new(Cell::new(0usize));
  let values = Rc::new(vec![Value::Number(1.0), Value::Number(2.0)]);
  let idx_for_next = idx.clone();
  let values_for_next = values.clone();

  let next_fn = rt
    .alloc_function_value(move |rt, _this, _args| {
      let i = idx_for_next.get();
      let result_obj = rt.alloc_object_value()?;
      if i >= values_for_next.len() {
        rt.define_data_property(result_obj, done_key, Value::Bool(true), true)?;
        rt.define_data_property(result_obj, value_key, Value::Undefined, true)?;
      } else {
        rt.define_data_property(result_obj, done_key, Value::Bool(false), true)?;
        rt.define_data_property(result_obj, value_key, values_for_next[i], true)?;
        idx_for_next.set(i + 1);
      }
      Ok(result_obj)
    })
    .unwrap();

  rt.define_data_property(iterator_obj, next_key, next_fn, true)
    .unwrap();

  let iterator_getter = rt
    .alloc_function_value(move |_rt, _this, _args| Ok(iterator_obj))
    .unwrap();

  let iterable_obj = rt.alloc_object_value().unwrap();
  let iterator_sym = rt.symbol_iterator().unwrap();
  rt.define_data_property(iterable_obj, iterator_sym, iterator_getter, true)
    .unwrap();

  let ty = IdlType::FrozenArray(Box::new(IdlType::Numeric(NumericType::Long)));
  let converted = convert_to_idl(&mut rt, iterable_obj, &ty, &ctx).unwrap();

  let ConvertedValue::Sequence { values, .. } = converted else {
    panic!("expected frozen array to convert as sequence, got {converted:?}");
  };
  assert_eq!(
    values,
    vec![ConvertedValue::Long(1), ConvertedValue::Long(2)]
  );
}
