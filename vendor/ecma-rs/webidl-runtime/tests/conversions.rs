use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;
use vm_js::{JsBigInt, PropertyKey, Value, VmError};
use webidl::ir::{
  parse_default_value, DictionaryMemberSchema, DictionarySchema, IdlType, NamedType, NamedTypeKind,
  NumericType, StringType, TypeAnnotation, TypeContext,
};
use webidl_runtime::{
  convert_arguments, convert_to_idl, ArgumentSchema, ConvertedValue, JsRuntime, VmJsRuntime,
  WebIdlJsRuntime, WebIdlLimits,
};
use webidl_runtime::conversions::AsyncSequenceKind;

fn error_to_string(rt: &mut VmJsRuntime, err: VmError) -> String {
  let Some(thrown) = err.thrown_value() else {
    panic!("expected throw, got {err:?}");
  };
  // These tests sometimes set extremely small `WebIdlLimits::max_string_code_units` values to
  // validate that conversion limits are enforced. When that limit is tiny, the runtime's
  // `string_to_utf8_lossy` helper will intentionally throw on *any* non-trivial string to avoid
  // unbounded host allocations.
  //
  // For test diagnostics we still want to stringify thrown errors, so bypass `string_to_utf8_lossy`
  // and read the string directly from the VM heap (these error strings are short, fixed messages).
  let s = rt.to_string(thrown).unwrap();
  let Value::String(handle) = s else {
    panic!("expected ToString(thrown) to return a string");
  };
  rt.heap().get_string(handle).unwrap().to_utf8_lossy()
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
fn sequence_conversion_rejects_primitive_string_without_to_object_boxing() {
  let mut rt = VmJsRuntime::new();
  let ctx = TypeContext::default();

  // WebIDL `sequence<T>` conversion requires an Object input; primitives must be rejected even if
  // they are iterable when boxed (e.g. strings).
  let v = rt.alloc_string_value("abc").unwrap();
  let ty = IdlType::Sequence(Box::new(IdlType::Any));

  let err = convert_to_idl(&mut rt, v, &ty, &ctx).unwrap_err();
  assert_eq!(
    error_to_string(&mut rt, err),
    "TypeError: Value is not an object"
  );
}

#[test]
fn async_sequence_conversion_rejects_primitive_string_without_to_object_boxing() {
  let mut rt = VmJsRuntime::new();
  let ctx = TypeContext::default();

  // WebIDL `async sequence<T>` conversion requires an Object input; primitives must be rejected
  // rather than boxed and treated as sync iterables.
  let v = rt.alloc_string_value("abc").unwrap();
  let ty = IdlType::AsyncSequence(Box::new(IdlType::Any));

  let err = convert_to_idl(&mut rt, v, &ty, &ctx).unwrap_err();
  assert_eq!(
    error_to_string(&mut rt, err),
    "TypeError: Value is not an object"
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
  rt.define_data_property(obj, req_key, req_val, true)
    .unwrap();

  let converted = convert_to_idl(&mut rt, obj, &dict_ty, &ctx).unwrap();
  let ConvertedValue::Dictionary { name, members } = converted else {
    panic!("expected dictionary, got {converted:?}");
  };
  assert_eq!(name, "TestDict");

  let expected = BTreeMap::from([
    ("opt".to_string(), ConvertedValue::Long(5)),
    (
      "req".to_string(),
      ConvertedValue::String("hello".to_string()),
    ),
  ]);
  assert_eq!(members, expected);
}

#[test]
fn interface_named_type_converts_platform_object_to_object_value() {
  let mut rt = VmJsRuntime::new();
  let ctx = TypeContext::default();

  let _opaque = 0xfeed_u64;
  let obj = rt
    .alloc_platform_object_value("Node", &["EventTarget"], _opaque)
    .unwrap();

  let ty = IdlType::Named(NamedType {
    name: "Node".to_string(),
    kind: NamedTypeKind::Interface,
  });

  let converted = convert_to_idl(&mut rt, obj, &ty, &ctx).unwrap();
  let ConvertedValue::Object(v) = converted else {
    panic!("expected object, got {converted:?}");
  };
  assert_eq!(v, obj);
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
fn object_conversion_rejects_primitives_without_to_object_boxing() {
  let mut rt = VmJsRuntime::new();
  let ctx = TypeContext::default();

  let err = convert_to_idl(&mut rt, Value::Bool(true), &IdlType::Object, &ctx).unwrap_err();
  assert_eq!(
    error_to_string(&mut rt, err),
    "TypeError: Value is not an object"
  );
}

#[test]
fn object_conversion_accepts_objects_and_preserves_identity() {
  let mut rt = VmJsRuntime::new();
  let ctx = TypeContext::default();

  let obj = rt.alloc_object_value().unwrap();
  let converted = convert_to_idl(&mut rt, obj, &IdlType::Object, &ctx).unwrap();
  let ConvertedValue::Object(out) = converted else {
    panic!("expected object, got {converted:?}");
  };
  assert_eq!(out, obj);
}

#[test]
fn bigint_conversion_uses_to_bigint() {
  let mut rt = VmJsRuntime::new();
  let ctx = TypeContext::default();
  let ty = IdlType::BigInt;

  let bigint = {
    let mut scope = rt.heap_mut().scope();
    Value::BigInt(scope.alloc_bigint_from_u128(42).unwrap())
  };
  let converted = convert_to_idl(&mut rt, bigint, &ty, &ctx).unwrap();
  assert_eq!(converted, ConvertedValue::Any(bigint));

  // WebIDL bigint conversion uses ECMAScript `ToBigInt`, which rejects Numbers.
  let err = convert_to_idl(&mut rt, Value::Number(5.0), &ty, &ctx).unwrap_err();
  assert!(error_to_string(&mut rt, err).starts_with("TypeError"));
}

#[test]
fn bigint_conversion_parses_ecmascript_string_numeric_literals() {
  let mut rt = VmJsRuntime::new();
  let ctx = TypeContext::default();
  let ty = IdlType::BigInt;

  for (input, expected) in [
    ("0x10", JsBigInt::from_u128(16).unwrap()),
    ("0b101", JsBigInt::from_u128(5).unwrap()),
    ("0o10", JsBigInt::from_u128(8).unwrap()),
    ("\u{FEFF}123\u{FEFF}", JsBigInt::from_u128(123).unwrap()),
    ("-123", JsBigInt::from_u128(123).unwrap().negate()),
  ] {
    let v = rt.alloc_string_value(input).unwrap();
    let converted = convert_to_idl(&mut rt, v, &ty, &ctx).unwrap();
    let ConvertedValue::Any(Value::BigInt(actual)) = converted else {
      panic!("expected BigInt Any conversion, got {converted:?}");
    };
    assert_eq!(
      rt.heap().get_bigint(actual).unwrap(),
      &expected,
      "input={input:?}"
    );
  }
}

#[test]
fn bigint_conversion_rejects_signed_non_decimal_string_literals() {
  let mut rt = VmJsRuntime::new();
  let ctx = TypeContext::default();
  let ty = IdlType::BigInt;

  for input in ["-0x10", "+0b101", "-0o10"] {
    let v = rt.alloc_string_value(input).unwrap();
    let err = convert_to_idl(&mut rt, v, &ty, &ctx).unwrap_err();
    assert!(
      error_to_string(&mut rt, err).starts_with("SyntaxError"),
      "input={input:?}"
    );
  }
}

#[test]
fn symbol_conversion_requires_symbol() {
  let mut rt = VmJsRuntime::new();
  let ctx = TypeContext::default();

  let sym = {
    let mut scope = rt.heap_mut().scope();
    scope.alloc_symbol(Some("s")).unwrap()
  };
  let value = Value::Symbol(sym);
  let converted = convert_to_idl(&mut rt, value, &IdlType::Symbol, &ctx).unwrap();
  assert_eq!(converted, ConvertedValue::Any(value));

  let err = convert_to_idl(&mut rt, Value::Number(0.0), &IdlType::Symbol, &ctx).unwrap_err();
  assert!(error_to_string(&mut rt, err).starts_with("TypeError"));
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
fn record_conversion_throws_on_enumerable_symbol_keys() {
  let mut rt = VmJsRuntime::new();
  let ctx = TypeContext::default();

  let obj = rt.alloc_object_value().unwrap();
  let a_key = rt.property_key_from_str("a").unwrap();
  rt.define_data_property(obj, a_key, Value::Number(1.0), true)
    .unwrap();

  // Use a well-known Symbol as an enumerable key; record conversion must throw when attempting to
  // convert the Symbol key to a string.
  let sym_key = rt.symbol_iterator().unwrap();
  rt.define_data_property(obj, sym_key, Value::Number(2.0), true)
    .unwrap();

  let ty = IdlType::Record(
    Box::new(IdlType::String(StringType::DomString)),
    Box::new(IdlType::Numeric(NumericType::Long)),
  );

  let err = convert_to_idl(&mut rt, obj, &ty, &ctx).unwrap_err();
  assert!(error_to_string(&mut rt, err).starts_with("TypeError"));
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
      variadic: false,
      default: None,
    },
    // This is not marked `optional`, but it has a default value; it should not contribute to the
    // required argument count.
    ArgumentSchema {
      name: "b",
      ty: IdlType::Numeric(NumericType::Long),
      optional: false,
      variadic: false,
      default: Some(parse_default_value("5").unwrap()),
    },
  ];

  let args = vec![Value::Number(1.0)];
  let converted = convert_arguments(&mut rt, &args, &params, &ctx).unwrap();
  assert_eq!(
    converted,
    vec![ConvertedValue::Long(1), ConvertedValue::Long(5)]
  );
}

#[test]
fn record_conversion_rejects_non_objects() {
  let mut rt = VmJsRuntime::new();
  let ctx = TypeContext::default();

  let ty = IdlType::Record(
    Box::new(IdlType::String(StringType::DomString)),
    Box::new(IdlType::Numeric(NumericType::Long)),
  );

  let err = convert_to_idl(&mut rt, Value::Bool(true), &ty, &ctx).unwrap_err();
  assert!(error_to_string(&mut rt, err).starts_with("TypeError"));

  let err = convert_to_idl(&mut rt, Value::Number(1.0), &ty, &ctx).unwrap_err();
  assert!(error_to_string(&mut rt, err).starts_with("TypeError"));

  let s = rt.alloc_string_value("x").unwrap();
  let err = convert_to_idl(&mut rt, s, &ty, &ctx).unwrap_err();
  assert!(error_to_string(&mut rt, err).starts_with("TypeError"));
}

#[test]
fn union_conversion_prefers_matching_interface_member() {
  let mut rt = VmJsRuntime::new();
  let ctx = TypeContext::default();

  let _opaque = 123u64;
  let obj = rt
    .alloc_platform_object_value("Node", &[], _opaque)
    .unwrap();

  let interface_ty = IdlType::Named(NamedType {
    name: "Node".to_string(),
    kind: NamedTypeKind::Interface,
  });
  let ty = IdlType::Union(vec![
    interface_ty.clone(),
    IdlType::String(StringType::DomString),
  ]);

  let converted = convert_to_idl(&mut rt, obj, &ty, &ctx).unwrap();
  let ConvertedValue::Union { member_ty, value } = converted else {
    panic!("expected union, got {converted:?}");
  };
  assert_eq!(*member_ty, interface_ty);

  let ConvertedValue::Object(v) = *value else {
    panic!("expected object union value, got {value:?}");
  };
  assert_eq!(v, obj);
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
  rt.define_data_property(obj, req_key, req_val, true)
    .unwrap();

  let converted = convert_to_idl(&mut rt, obj, &ty, &ctx).unwrap();
  let ConvertedValue::Dictionary { name, members } = converted else {
    panic!("expected dictionary, got {converted:?}");
  };
  assert_eq!(name, "Derived");
  let expected = BTreeMap::from([
    ("base".to_string(), ConvertedValue::Long(1)),
    ("derived".to_string(), ConvertedValue::Long(2)),
    (
      "req".to_string(),
      ConvertedValue::String("hello".to_string()),
    ),
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
fn dictionary_member_access_order_is_lexicographic_and_inheritance_based() {
  let mut rt = VmJsRuntime::new();

  let mut ctx = TypeContext::default();
  // Declare members out of order; the conversion algorithm must access them in lexicographical
  // order within each dictionary.
  ctx.add_dictionary(DictionarySchema {
    name: "Base".to_string(),
    inherits: None,
    members: vec![
      DictionaryMemberSchema {
        name: "b".to_string(),
        required: false,
        ty: IdlType::Numeric(NumericType::Long),
        default: None,
      },
      DictionaryMemberSchema {
        name: "a".to_string(),
        required: false,
        ty: IdlType::Numeric(NumericType::Long),
        default: None,
      },
    ],
  });
  ctx.add_dictionary(DictionarySchema {
    name: "Derived".to_string(),
    inherits: Some("Base".to_string()),
    members: vec![
      DictionaryMemberSchema {
        name: "d".to_string(),
        required: false,
        ty: IdlType::Numeric(NumericType::Long),
        default: None,
      },
      DictionaryMemberSchema {
        name: "c".to_string(),
        required: false,
        ty: IdlType::Numeric(NumericType::Long),
        default: None,
      },
    ],
  });

  let calls: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));

  let obj = rt.alloc_object_value().unwrap();
  for (name, n) in [("a", 1.0), ("b", 2.0), ("c", 3.0), ("d", 4.0)] {
    let calls_for_get = calls.clone();
    let name_owned = name.to_string();
    let getter = rt
      .alloc_function_value(move |_rt, _this, _args| {
        calls_for_get.borrow_mut().push(name_owned.clone());
        Ok(Value::Number(n))
      })
      .unwrap();
    let key = rt.property_key_from_str(name).unwrap();
    rt.define_accessor_property(obj, key, getter, Value::Undefined, true)
      .unwrap();
  }

  let ty = IdlType::Named(NamedType {
    name: "Derived".to_string(),
    kind: NamedTypeKind::Unresolved,
  });

  let converted = convert_to_idl(&mut rt, obj, &ty, &ctx).unwrap();
  let ConvertedValue::Dictionary { name, members } = converted else {
    panic!("expected dictionary, got {converted:?}");
  };
  assert_eq!(name, "Derived");
  assert_eq!(
    calls.borrow().as_slice(),
    &["a", "b", "c", "d"],
    "dictionary conversion should access members in WebIDL order",
  );

  // Spot-check that values converted as expected.
  assert_eq!(members.get("a"), Some(&ConvertedValue::Long(1)));
  assert_eq!(members.get("d"), Some(&ConvertedValue::Long(4)));
}

#[test]
fn record_conversion_preserves_own_property_key_order() {
  let mut rt = VmJsRuntime::new();
  let ctx = TypeContext::default();

  // WebIDL record conversion iterates `[[OwnPropertyKeys]]` order; for string keys this is
  // insertion order.
  let obj = rt.alloc_object_value().unwrap();
  let key_b = rt.property_key_from_str("b").unwrap();
  let key_a = rt.property_key_from_str("a").unwrap();
  rt.define_data_property(obj, key_b, Value::Number(3.0), true)
    .unwrap();
  rt.define_data_property(obj, key_a, Value::Number(4.0), true)
    .unwrap();

  let ty = IdlType::Record(
    Box::new(IdlType::String(StringType::DomString)),
    Box::new(IdlType::Numeric(NumericType::Long)),
  );
  let converted = convert_to_idl(&mut rt, obj, &ty, &ctx).unwrap();
  let ConvertedValue::Record { entries, .. } = converted else {
    panic!("expected record, got {converted:?}");
  };

  assert_eq!(
    entries,
    vec![
      ("b".to_string(), ConvertedValue::Long(3)),
      ("a".to_string(), ConvertedValue::Long(4)),
    ]
  );
}

#[test]
fn record_conversion_overwrites_duplicate_keys_without_reordering() {
  let mut rt = VmJsRuntime::new();
  let ctx = TypeContext::default();

  // Create two distinct string property keys that both stringify to U+FFFD after UTF-16 lossy
  // conversion (unpaired surrogate code units).
  let js_key1 = rt.alloc_string_from_code_units(&[0xD800]).unwrap();
  let Value::String(handle1) = js_key1 else {
    panic!("expected string");
  };
  let js_key2 = rt.alloc_string_from_code_units(&[0xDC00]).unwrap();
  let Value::String(handle2) = js_key2 else {
    panic!("expected string");
  };

  let obj = rt.alloc_object_value().unwrap();
  rt.define_data_property(obj, PropertyKey::String(handle1), Value::Number(1.0), true)
    .unwrap();
  rt.define_data_property(obj, PropertyKey::String(handle2), Value::Number(2.0), true)
    .unwrap();

  let ty = IdlType::Record(
    Box::new(IdlType::String(StringType::DomString)),
    Box::new(IdlType::Numeric(NumericType::Long)),
  );
  let converted = convert_to_idl(&mut rt, obj, &ty, &ctx).unwrap();
  let ConvertedValue::Record { entries, .. } = converted else {
    panic!("expected record, got {converted:?}");
  };

  assert_eq!(
    entries,
    vec![("\u{FFFD}".to_string(), ConvertedValue::Long(2))]
  );
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

  // Record limits (string keys count).
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
  // Non-enumerable symbol keys should be ignored for record conversion.
  rt.define_data_property(obj, iterator_sym, Value::Number(2.0), false)
    .unwrap();
  let converted = convert_to_idl(&mut rt, obj, &record_ty, &ctx).unwrap();
  let ConvertedValue::Record { entries, .. } = converted else {
    panic!("expected record, got {converted:?}");
  };
  assert_eq!(entries, vec![("a".to_string(), ConvertedValue::Long(1))]);
}

#[test]
fn promise_any_conversion_returns_promise_object() {
  let mut rt = VmJsRuntime::new();
  let ctx = TypeContext::default();

  let ty = IdlType::Promise(Box::new(IdlType::Any));
  let converted = convert_to_idl(&mut rt, Value::Number(1.0), &ty, &ctx).unwrap();
  let ConvertedValue::Promise { inner_ty, promise } = converted else {
    panic!("expected Promise conversion, got {converted:?}");
  };
  assert_eq!(*inner_ty, IdlType::Any);

  let Value::Object(obj) = promise else {
    panic!("expected Promise conversion to return an object");
  };
  assert!(rt.heap().is_promise_object(obj));
}

#[test]
fn async_sequence_conversion_prefers_async_iterator_then_falls_back_to_sync_iterator() {
  let mut rt = VmJsRuntime::new();
  let ctx = TypeContext::default();

  let elem_ty = IdlType::String(StringType::DomString);
  let ty = IdlType::AsyncSequence(Box::new(elem_ty.clone()));

  // ---- prefers @@asyncIterator ----
  let obj = rt.alloc_object_value().unwrap();
  let async_method = rt
    .alloc_function_value(|_rt, _this, _args| Ok(Value::Undefined))
    .unwrap();
  let async_key = rt.symbol_async_iterator().unwrap();
  rt.define_data_property(obj, async_key, async_method, true)
    .unwrap();

  let converted = convert_to_idl(&mut rt, obj, &ty, &ctx).unwrap();
  let ConvertedValue::AsyncSequence {
    object,
    kind,
    elem_ty: out_elem_ty,
  } = converted
  else {
    panic!("expected async sequence, got {converted:?}");
  };
  assert_eq!(*out_elem_ty, elem_ty);
  assert_eq!(object, obj);
  assert_eq!(kind, AsyncSequenceKind::Async);

  // ---- falls back to @@iterator ----
  let obj2 = rt.alloc_object_value().unwrap();
  let iter_method = rt
    .alloc_function_value(|_rt, _this, _args| Ok(Value::Undefined))
    .unwrap();
  let iter_key = rt.symbol_iterator().unwrap();
  rt.define_data_property(obj2, iter_key, iter_method, true)
    .unwrap();

  let converted = convert_to_idl(&mut rt, obj2, &ty, &ctx).unwrap();
  let ConvertedValue::AsyncSequence { object, kind, .. } = converted else {
    panic!("expected async sequence, got {converted:?}");
  };
  assert_eq!(object, obj2);
  assert_eq!(kind, AsyncSequenceKind::Sync);
}

#[test]
fn union_async_sequence_string_object_special_case_does_not_probe_iterators() {
  let mut rt = VmJsRuntime::new();
  let ctx = TypeContext::default();

  let union_ty = IdlType::Union(vec![
    IdlType::AsyncSequence(Box::new(IdlType::String(StringType::DomString))),
    IdlType::String(StringType::DomString),
  ]);

  // Create a String object wrapper.
  let s = rt.alloc_string_value("hello").unwrap();
  let string_obj = rt.to_object(s).unwrap();

  // If the union conversion tried to probe iterator methods, it would trigger this getter and
  // throw. The special-case (d) must skip probing for String objects when a string member is
  // present.
  let throwing_getter = rt
    .alloc_function_value(|rt, _this, _args| Err(rt.throw_type_error("getter must not run")))
    .unwrap();
  let async_key = rt.symbol_async_iterator().unwrap();
  rt.define_accessor_property(string_obj, async_key, throwing_getter, Value::Undefined, true)
    .unwrap();

  let iter_key = rt.symbol_iterator().unwrap();
  rt.define_accessor_property(string_obj, iter_key, throwing_getter, Value::Undefined, true)
    .unwrap();

  let converted = convert_to_idl(&mut rt, string_obj, &union_ty, &ctx).unwrap();
  let ConvertedValue::Union { member_ty, value } = converted else {
    panic!("expected union, got {converted:?}");
  };
  assert_eq!(*member_ty, IdlType::String(StringType::DomString));
  assert_eq!(*value, ConvertedValue::String("hello".to_string()));
}

#[test]
fn union_sequence_string_object_special_case_does_not_probe_iterator() {
  let mut rt = VmJsRuntime::new();
  let ctx = TypeContext::default();

  let union_ty = IdlType::Union(vec![
    IdlType::Sequence(Box::new(IdlType::Any)),
    IdlType::String(StringType::DomString),
  ]);

  // Create a String object wrapper.
  let s = rt.alloc_string_value("hello").unwrap();
  let string_obj = rt.to_object(s).unwrap();

  // If the union conversion tried to probe @@iterator (sequence/FrozenArray), it would trigger this
  // getter and throw. The special-case (d) must treat String objects as strings when a string
  // member is present.
  let throwing_getter = rt
    .alloc_function_value(|rt, _this, _args| Err(rt.throw_type_error("getter must not run")))
    .unwrap();
  let iter_key = rt.symbol_iterator().unwrap();
  rt.define_accessor_property(string_obj, iter_key, throwing_getter, Value::Undefined, true)
    .unwrap();

  let converted = convert_to_idl(&mut rt, string_obj, &union_ty, &ctx).unwrap();
  let ConvertedValue::Union { member_ty, value } = converted else {
    panic!("expected union, got {converted:?}");
  };
  assert_eq!(*member_ty, IdlType::String(StringType::DomString));
  assert_eq!(*value, ConvertedValue::String("hello".to_string()));
}

#[test]
fn union_record_string_object_special_case_does_not_probe_properties() {
  let mut rt = VmJsRuntime::new();
  let ctx = TypeContext::default();

  let union_ty = IdlType::Union(vec![
    IdlType::Record(
      Box::new(IdlType::String(StringType::DomString)),
      Box::new(IdlType::Any),
    ),
    IdlType::String(StringType::DomString),
  ]);

  // Create a String object wrapper.
  let s = rt.alloc_string_value("hello").unwrap();
  let string_obj = rt.to_object(s).unwrap();

  // If the union conversion tried to treat the value as a record/object and enumerate properties,
  // it would access this enumerable accessor and throw. The special-case (d) must treat String
  // objects as strings when a string member is present.
  let throwing_getter = rt
    .alloc_function_value(|rt, _this, _args| Err(rt.throw_type_error("getter must not run")))
    .unwrap();
  let key_value = rt.alloc_string_value("x").unwrap();
  let Value::String(key) = key_value else {
    panic!("expected string key");
  };
  rt.define_accessor_property(
    string_obj,
    PropertyKey::String(key),
    throwing_getter,
    Value::Undefined,
    true,
  )
  .unwrap();

  let converted = convert_to_idl(&mut rt, string_obj, &union_ty, &ctx).unwrap();
  let ConvertedValue::Union { member_ty, value } = converted else {
    panic!("expected union, got {converted:?}");
  };
  assert_eq!(*member_ty, IdlType::String(StringType::DomString));
  assert_eq!(*value, ConvertedValue::String("hello".to_string()));
}

#[test]
fn union_dictionary_string_object_special_case_does_not_probe_members() {
  let mut rt = VmJsRuntime::new();

  let mut ctx = TypeContext::default();
  ctx.add_dictionary(DictionarySchema {
    name: "TestDictForUnion".to_string(),
    inherits: None,
    members: vec![DictionaryMemberSchema {
      name: "req".to_string(),
      required: true,
      ty: IdlType::String(StringType::DomString),
      default: None,
    }],
  });

  let dict_ty = IdlType::Named(NamedType {
    name: "TestDictForUnion".to_string(),
    kind: NamedTypeKind::Unresolved,
  });

  let union_ty = IdlType::Union(vec![dict_ty, IdlType::String(StringType::DomString)]);

  // Create a String object wrapper.
  let s = rt.alloc_string_value("hello").unwrap();
  let string_obj = rt.to_object(s).unwrap();

  // If the union conversion tried to select the dictionary member, it would attempt to read the
  // required `req` member and trigger this getter.
  let throwing_getter = rt
    .alloc_function_value(|rt, _this, _args| Err(rt.throw_type_error("getter must not run")))
    .unwrap();
  let req_key = rt.property_key_from_str("req").unwrap();
  rt.define_accessor_property(
    string_obj,
    req_key,
    throwing_getter,
    Value::Undefined,
    true,
  )
  .unwrap();

  let converted = convert_to_idl(&mut rt, string_obj, &union_ty, &ctx).unwrap();
  let ConvertedValue::Union { member_ty, value } = converted else {
    panic!("expected union, got {converted:?}");
  };
  assert_eq!(*member_ty, IdlType::String(StringType::DomString));
  assert_eq!(*value, ConvertedValue::String("hello".to_string()));
}

#[test]
fn union_object_string_object_special_case_prefers_string_over_object() {
  let mut rt = VmJsRuntime::new();
  let ctx = TypeContext::default();

  // Without the special-case (d), the union algorithm would pick the `object` branch for a String
  // object before reaching the string fallback.
  let union_ty = IdlType::Union(vec![
    IdlType::Object,
    IdlType::String(StringType::DomString),
  ]);

  let s = rt.alloc_string_value("hello").unwrap();
  let string_obj = rt.to_object(s).unwrap();

  let converted = convert_to_idl(&mut rt, string_obj, &union_ty, &ctx).unwrap();
  let ConvertedValue::Union { member_ty, value } = converted else {
    panic!("expected union, got {converted:?}");
  };
  assert_eq!(*member_ty, IdlType::String(StringType::DomString));
  assert_eq!(*value, ConvertedValue::String("hello".to_string()));
}
