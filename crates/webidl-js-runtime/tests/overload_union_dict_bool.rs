use std::cell::Cell;
use std::rc::Rc;

use vm_js::{Value, VmError};
use webidl_ir::{
  DefaultValue, DictionaryMemberSchema, DictionarySchema, IdlType, NamedType, NamedTypeKind,
  StringType, TypeContext,
};
use webidl_js_runtime::{
  convert_arguments, resolve_overload, ArgumentSchema, ConvertedValue, JsRuntime as _, Optionality,
  OverloadArg, OverloadSig, VmJsRuntime, WebIdlJsRuntime as _,
};

#[test]
fn overload_resolution_union_dict_boolean_and_conversions_work() -> Result<(), VmError> {
  let mut rt = VmJsRuntime::new();

  // Type context: dictionary with a defaulted member + a sequence member.
  let mut ctx = TypeContext::default();
  ctx.add_dictionary(DictionarySchema {
    name: "MyOptions".to_string(),
    inherits: None,
    members: vec![
      DictionaryMemberSchema {
        name: "flag".to_string(),
        required: false,
        ty: IdlType::Boolean,
        default: Some(DefaultValue::Boolean(true)),
      },
      DictionaryMemberSchema {
        name: "items".to_string(),
        required: false,
        ty: IdlType::Sequence(Box::new(IdlType::String(StringType::DomString))),
        default: None,
      },
    ],
  });

  let dict_ty = IdlType::Named(NamedType {
    name: "MyOptions".to_string(),
    kind: NamedTypeKind::Dictionary,
  });
  // Use a union that is *distinguishable* from the `boolean` overload: if the union itself
  // contained `boolean`, the overload set would be ambiguous (no distinguishing argument index).
  let union_ty = IdlType::Union(vec![
    dict_ty.clone(),
    IdlType::String(StringType::DomString),
  ]);

  // Overloads:
  //  - f((MyOptions or DOMString) options)
  //  - f(boolean capture)
  let overloads = vec![
    OverloadSig {
      args: vec![OverloadArg {
        ty: union_ty.clone(),
        optionality: Optionality::Required,
        default: None,
      }],
      decl_index: 0,
      distinguishing_arg_index_by_arg_count: None,
    },
    OverloadSig {
      args: vec![OverloadArg {
        ty: IdlType::Boolean,
        optionality: Optionality::Required,
        default: None,
      }],
      decl_index: 1,
      distinguishing_arg_index_by_arg_count: None,
    },
  ];

  // Build a custom iterable for the `items` member.
  let next_key = rt.property_key_from_str("next")?;
  let done_key = rt.property_key_from_str("done")?;
  let value_key = rt.property_key_from_str("value")?;

  let iterator_obj = rt.alloc_object_value()?;

  let idx = Rc::new(Cell::new(0usize));
  let values = Rc::new(vec![
    rt.alloc_string_value("a")?,
    rt.alloc_string_value("b")?,
  ]);
  let idx_for_next = idx.clone();
  let values_for_next = values.clone();

  let next_fn = rt.alloc_function_value(move |rt, _this, _args| {
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
  })?;

  rt.define_data_property(iterator_obj, next_key, next_fn, true)?;

  let iterator_getter = rt.alloc_function_value(move |_rt, _this, _args| Ok(iterator_obj))?;

  let iterable_obj = rt.alloc_object_value()?;
  let iterator_sym = rt.symbol_iterator()?;
  rt.define_data_property(iterable_obj, iterator_sym, iterator_getter, true)?;

  // options = { items: iterable }
  let options_obj = rt.alloc_object_value()?;
  let items_prop = rt.property_key_from_str("items")?;
  rt.define_data_property(options_obj, items_prop, iterable_obj, true)?;

  // Overload resolution should pick:
  // - the union overload for `{}`-like objects (dictionary member wins inside the union)
  // - the boolean overload for `true`
  let resolved_obj = resolve_overload(&mut rt, &overloads, &[options_obj])?;
  assert_eq!(resolved_obj.overload_index, 0);

  let resolved_bool = resolve_overload(&mut rt, &overloads, &[Value::Bool(true)])?;
  assert_eq!(resolved_bool.overload_index, 1);

  // Now run full argument conversion for the union overload and validate:
  // - union selects the dictionary member (not boolean),
  // - dictionary default is applied for `flag`,
  // - sequence conversion reads from the custom iterable.
  let params = [ArgumentSchema {
    name: "options",
    ty: union_ty,
    optional: false,
    variadic: false,
    default: None,
  }];
  let converted = convert_arguments(&mut rt, &[options_obj], &params, &ctx)?;
  assert_eq!(converted.len(), 1);

  let ConvertedValue::Union { member_ty, value } = &converted[0] else {
    panic!("expected union conversion, got {:?}", converted[0]);
  };
  assert_eq!(member_ty.as_ref(), &dict_ty);

  let ConvertedValue::Dictionary { name, members } = value.as_ref() else {
    panic!("expected dictionary conversion, got {:?}", value);
  };
  assert_eq!(name, "MyOptions");

  assert_eq!(members.get("flag"), Some(&ConvertedValue::Boolean(true)));

  let Some(items) = members.get("items") else {
    panic!("missing items member");
  };
  let ConvertedValue::Sequence { values, .. } = items else {
    panic!("expected sequence for items, got {items:?}");
  };
  assert_eq!(
    values,
    &[
      ConvertedValue::String("a".to_string()),
      ConvertedValue::String("b".to_string())
    ]
  );

  Ok(())
}
