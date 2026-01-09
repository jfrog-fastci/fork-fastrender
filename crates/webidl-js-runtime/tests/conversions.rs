use std::cell::Cell;
use std::collections::BTreeMap;
use std::rc::Rc;
use vm_js::{Value, VmError};
use webidl_ir::{
  parse_default_value, DictionaryMemberSchema, DictionarySchema, IdlType, NamedType, NamedTypeKind,
  NumericType, StringType, TypeAnnotation, TypeContext,
};
use webidl_js_runtime::{
  convert_arguments, convert_to_idl, ArgumentSchema, ConvertedValue, JsRuntime, VmJsRuntime,
  WebIdlJsRuntime, WebIdlLimits,
};

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

#[test]
fn record_conversion_ignores_enumerable_symbol_keys() {
  let mut rt = VmJsRuntime::new();
  let ctx = TypeContext::default();

  let obj = rt.alloc_object_value().unwrap();
  let a_key = rt.property_key_from_str("a").unwrap();
  rt.define_data_property(obj, a_key, Value::Number(1.0), true)
    .unwrap();

  // Use a well-known Symbol as an enumerable key; record conversion should ignore it rather than
  // attempting to convert it to a string.
  let sym_key = rt.symbol_iterator().unwrap();
  rt.define_data_property(obj, sym_key, Value::Number(2.0), true)
    .unwrap();

  let ty = IdlType::Record(
    Box::new(IdlType::String(StringType::DomString)),
    Box::new(IdlType::Numeric(NumericType::Long)),
  );

  let converted = convert_to_idl(&mut rt, obj, &ty, &ctx).unwrap();
  let ConvertedValue::Record { entries, .. } = converted else {
    panic!("expected record, got {converted:?}");
  };

  let expected = BTreeMap::from([("a".to_string(), ConvertedValue::Long(1))]);
  assert_eq!(entries, expected);
}

#[test]
fn convert_arguments_treats_defaulted_params_as_optional() {
  let mut rt = VmJsRuntime::new();
  let ctx = TypeContext::default();

  let params = vec![
    ArgumentSchema {
      name: "a",
      ty: IdlType::Numeric(NumericType::Long),
      optional: false,
      default: None,
    },
    // This is not marked `optional`, but it has a default value; it should not contribute to the
    // required argument count.
    ArgumentSchema {
      name: "b",
      ty: IdlType::Numeric(NumericType::Long),
      optional: false,
      default: Some(parse_default_value("5").unwrap()),
    },
  ];

  let args = vec![Value::Number(1.0)];
  let converted = convert_arguments(&mut rt, &args, &params, &ctx).unwrap();
  assert_eq!(converted, vec![ConvertedValue::Long(1), ConvertedValue::Long(5)]);
}

#[test]
fn dictionary_inheritance_includes_base_members_and_defaults() {
  let mut rt = VmJsRuntime::new();

  let mut ctx = TypeContext::default();
  ctx.add_dictionary(DictionarySchema {
    name: "Base".to_string(),
    inherits: None,
    members: vec![DictionaryMemberSchema {
      name: "base".to_string(),
      required: false,
      ty: IdlType::Numeric(NumericType::Long),
      default: Some(parse_default_value("1").unwrap()),
    }],
  });
  ctx.add_dictionary(DictionarySchema {
    name: "Derived".to_string(),
    inherits: Some("Base".to_string()),
    members: vec![
      DictionaryMemberSchema {
        name: "req".to_string(),
        required: true,
        ty: IdlType::String(StringType::DomString),
        default: None,
      },
      DictionaryMemberSchema {
        name: "derived".to_string(),
        required: false,
        ty: IdlType::Numeric(NumericType::Long),
        default: Some(parse_default_value("2").unwrap()),
      },
    ],
  });

  let ty = IdlType::Named(NamedType {
    name: "Derived".to_string(),
    kind: NamedTypeKind::Unresolved,
  });

  // Missing required member throws (derived required member).
  let empty_obj = rt.alloc_object_value().unwrap();
  let err = convert_to_idl(&mut rt, empty_obj, &ty, &ctx).unwrap_err();
  assert!(error_to_string(&mut rt, err).starts_with("TypeError"));

  // Defaults are applied for both base and derived members.
  let obj = rt.alloc_object_value().unwrap();
  let req_key = rt.property_key_from_str("req").unwrap();
  let req_val = rt.alloc_string_value("hello").unwrap();
  rt.define_data_property(obj, req_key, req_val, true).unwrap();

  let converted = convert_to_idl(&mut rt, obj, &ty, &ctx).unwrap();
  let ConvertedValue::Dictionary { name, members } = converted else {
    panic!("expected dictionary, got {converted:?}");
  };
  assert_eq!(name, "Derived");
  let expected = BTreeMap::from([
    ("base".to_string(), ConvertedValue::Long(1)),
    ("derived".to_string(), ConvertedValue::Long(2)),
    ("req".to_string(), ConvertedValue::String("hello".to_string())),
  ]);
  assert_eq!(members, expected);
}

#[test]
fn union_disambiguates_dictionary_vs_boolean() {
  let mut rt = VmJsRuntime::new();

  let mut ctx = TypeContext::default();
  ctx.add_dictionary(DictionarySchema {
    name: "Options".to_string(),
    inherits: None,
    members: vec![],
  });

  let dict_ty = IdlType::Named(NamedType {
    name: "Options".to_string(),
    kind: NamedTypeKind::Unresolved,
  });
  let union_ty = IdlType::Union(vec![dict_ty.clone(), IdlType::Boolean]);

  // `{}` should match the dictionary member, not `boolean`.
  let obj = rt.alloc_object_value().unwrap();
  let converted = convert_to_idl(&mut rt, obj, &union_ty, &ctx).unwrap();
  let ConvertedValue::Union { member_ty, value } = converted else {
    panic!("expected union, got {converted:?}");
  };
  assert_eq!(*member_ty, dict_ty);
  assert_eq!(
    *value,
    ConvertedValue::Dictionary {
      name: "Options".to_string(),
      members: BTreeMap::new(),
    }
  );

  // `true` should match the boolean member.
  let converted = convert_to_idl(&mut rt, Value::Bool(true), &union_ty, &ctx).unwrap();
  let ConvertedValue::Union { member_ty, value } = converted else {
    panic!("expected union, got {converted:?}");
  };
  assert_eq!(*member_ty, IdlType::Boolean);
  assert_eq!(*value, ConvertedValue::Boolean(true));
}

#[test]
fn callback_interface_accepts_callable_or_handle_event_object() {
  let mut rt = VmJsRuntime::new();
  let ctx = TypeContext::default();

  let ty = IdlType::Named(NamedType {
    name: "EventListener".to_string(),
    kind: NamedTypeKind::CallbackInterface,
  });

  let func = rt
    .alloc_function_value(|_rt, _this, _args| Ok(Value::Undefined))
    .unwrap();
  let converted = convert_to_idl(&mut rt, func, &ty, &ctx).unwrap();
  assert_eq!(converted, ConvertedValue::Any(func));

  let obj = rt.alloc_object_value().unwrap();
  let handle_event = rt
    .alloc_function_value(|_rt, _this, _args| Ok(Value::Undefined))
    .unwrap();
  let handle_event_key = rt.property_key_from_str("handleEvent").unwrap();
  rt.define_data_property(obj, handle_event_key, handle_event, true)
    .unwrap();
  let converted = convert_to_idl(&mut rt, obj, &ty, &ctx).unwrap();
  assert_eq!(converted, ConvertedValue::Any(obj));

  let bad = rt.alloc_object_value().unwrap();
  let err = convert_to_idl(&mut rt, bad, &ty, &ctx).unwrap_err();
  assert!(error_to_string(&mut rt, err).starts_with("TypeError"));
}

#[test]
fn conversion_limits_are_enforced() {
  let mut rt = VmJsRuntime::new();
  rt.set_webidl_limits(WebIdlLimits {
    max_string_code_units: 1,
    max_sequence_length: 1,
    max_record_entries: 1,
  });
  let ctx = TypeContext::default();

  // String limits.
  let v = rt.alloc_string_value("ab").unwrap();
  let ty = IdlType::String(StringType::DomString);
  let err = convert_to_idl(&mut rt, v, &ty, &ctx).unwrap_err();
  assert!(error_to_string(&mut rt, err).starts_with("RangeError"));

  // Sequence limits.
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
  let err = convert_to_idl(&mut rt, iterable_obj, &ty, &ctx).unwrap_err();
  assert!(error_to_string(&mut rt, err).starts_with("RangeError"));

  // Record limits (symbol keys are ignored, string keys count).
  let record_ty = IdlType::Record(
    Box::new(IdlType::String(StringType::DomString)),
    Box::new(IdlType::Numeric(NumericType::Long)),
  );

  let obj = rt.alloc_object_value().unwrap();
  let a_key = rt.property_key_from_str("a").unwrap();
  let b_key = rt.property_key_from_str("b").unwrap();
  rt.define_data_property(obj, a_key, Value::Number(1.0), true)
    .unwrap();
  rt.define_data_property(obj, b_key, Value::Number(2.0), true)
    .unwrap();
  let err = convert_to_idl(&mut rt, obj, &record_ty, &ctx).unwrap_err();
  assert!(error_to_string(&mut rt, err).starts_with("RangeError"));

  let obj = rt.alloc_object_value().unwrap();
  let a_key = rt.property_key_from_str("a").unwrap();
  rt.define_data_property(obj, a_key, Value::Number(1.0), true)
    .unwrap();
  // Symbol keys should be ignored for record conversion (and should not count toward limits).
  rt.define_data_property(obj, iterator_sym, Value::Number(2.0), true)
    .unwrap();
  let converted = convert_to_idl(&mut rt, obj, &record_ty, &ctx).unwrap();
  let ConvertedValue::Record { entries, .. } = converted else {
    panic!("expected record, got {converted:?}");
  };
  assert_eq!(
    entries,
    BTreeMap::from([("a".to_string(), ConvertedValue::Long(1))])
  );
}
