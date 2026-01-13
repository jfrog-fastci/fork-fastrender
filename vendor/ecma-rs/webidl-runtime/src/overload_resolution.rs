//! WebIDL overload resolution and helpers.
//!
//! This module is a thin re-export of the runtime-agnostic algorithms in `webidl`.

pub use webidl::overload_resolution::*;

#[cfg(test)]
mod tests {
  use super::*;
  use crate::JsRuntime;
  use crate::VmJsRuntime;
  use crate::WebIdlJsRuntime;
  use vm_js::{PropertyKey, Value, VmError};
  use webidl::ir::{DefaultValue, IdlType, NamedType, NamedTypeKind, NumericType, StringType};

  fn as_utf8_lossy(rt: &VmJsRuntime, v: Value) -> String {
    let Value::String(s) = v else {
      panic!("expected string");
    };
    rt.heap().get_string(s).unwrap().to_utf8_lossy()
  }

  fn thrown_message(rt: &mut VmJsRuntime, err: VmError) -> String {
    let Some(v) = err.thrown_value() else {
      panic!("expected throw");
    };
    let Value::Object(obj) = v else {
      panic!("expected object");
    };
    let key_value = rt.alloc_string_value("message").unwrap();
    let Value::String(key) = key_value else {
      panic!("expected string value for key");
    };
    let msg = rt
      .get(Value::Object(obj), PropertyKey::String(key))
      .unwrap();
    let msg = rt.to_string(msg).unwrap();
    let Value::String(msg) = msg else {
      panic!("expected string message");
    };
    rt.heap().get_string(msg).unwrap().to_utf8_lossy()
  }

  #[test]
  fn overload_mismatch_error_message_includes_candidates() {
    let mut rt = VmJsRuntime::new();

    let err = throw_no_matching_overload(
      &mut rt,
      "doThing",
      2,
      &["doThing(DOMString)", "doThing()", "doThing(long, long)"],
    );

    let Some(thrown) = err.thrown_value() else {
      panic!("expected VmError::Throw, got {err:?}");
    };

    let s = rt.to_string(thrown).unwrap();
    let msg = as_utf8_lossy(&rt, s);

    assert!(
      msg.starts_with("TypeError:"),
      "expected TypeError, got {msg:?}"
    );
    assert!(msg.contains("doThing"));
    assert!(msg.contains("2"));
    assert!(msg.contains("Candidates:"));

    let idx_empty = msg.find("doThing()").expect("missing doThing() signature");
    let idx_dom = msg
      .find("doThing(DOMString)")
      .expect("missing doThing(DOMString) signature");
    let idx_ll = msg
      .find("doThing(long, long)")
      .expect("missing doThing(long, long) signature");

    assert!(
      idx_empty < idx_dom && idx_dom < idx_ll,
      "expected lexicographically sorted candidates, got {msg:?}"
    );
  }

  #[test]
  fn spec_overload_set_example_selects_correct_overload() {
    let mut rt = VmJsRuntime::new();

    let overloads = vec![
      // f1: f(DOMString a)
      OverloadSig {
        args: vec![OverloadArg {
          ty: IdlType::String(StringType::DomString),
          optionality: Optionality::Required,
          default: None,
        }],
        decl_index: 0,
        distinguishing_arg_index_by_arg_count: None,
      },
      // f2: f(Node a, DOMString b, double... c)
      OverloadSig {
        args: vec![
          OverloadArg {
            ty: IdlType::Named(NamedType {
              name: "Node".into(),
              kind: NamedTypeKind::Interface,
            }),
            optionality: Optionality::Required,
            default: None,
          },
          OverloadArg {
            ty: IdlType::String(StringType::DomString),
            optionality: Optionality::Required,
            default: None,
          },
          OverloadArg {
            ty: IdlType::Numeric(NumericType::Double),
            optionality: Optionality::Variadic,
            default: None,
          },
        ],
        decl_index: 1,
        distinguishing_arg_index_by_arg_count: None,
      },
      // f3: f()
      OverloadSig {
        args: vec![],
        decl_index: 2,
        distinguishing_arg_index_by_arg_count: None,
      },
      // f4: f(Event a, DOMString b, optional DOMString c, double... d)
      OverloadSig {
        args: vec![
          OverloadArg {
            ty: IdlType::Named(NamedType {
              name: "Event".into(),
              kind: NamedTypeKind::Interface,
            }),
            optionality: Optionality::Required,
            default: None,
          },
          OverloadArg {
            ty: IdlType::String(StringType::DomString),
            optionality: Optionality::Required,
            default: None,
          },
          OverloadArg {
            ty: IdlType::String(StringType::DomString),
            optionality: Optionality::Optional,
            default: None,
          },
          OverloadArg {
            ty: IdlType::Numeric(NumericType::Double),
            optionality: Optionality::Variadic,
            default: None,
          },
        ],
        decl_index: 3,
        distinguishing_arg_index_by_arg_count: None,
      },
    ];

    // f() selects f3.
    let out = resolve_overload(&mut rt, &overloads, &[]).unwrap();
    assert_eq!(out.overload_index, 2);
    assert!(out.values.is_empty());

    // f("x") selects f1.
    let x = rt.alloc_string_value("x").unwrap();
    let out = resolve_overload(&mut rt, &overloads, &[x]).unwrap();
    assert_eq!(out.overload_index, 0);
    assert_eq!(out.values.len(), 1);

    // f(Node, "b") selects f2.
    let node = rt.alloc_platform_object_value("Node", &[], 1).unwrap();
    let b = rt.alloc_string_value("b").unwrap();
    let out = resolve_overload(&mut rt, &overloads, &[node, b]).unwrap();
    assert_eq!(out.overload_index, 1);

    // f(Event, "b", undefined) selects f4 and marks optional c as missing.
    let event = rt.alloc_platform_object_value("Event", &[], 2).unwrap();
    let out = resolve_overload(&mut rt, &overloads, &[event, b, Value::Undefined]).unwrap();
    assert_eq!(out.overload_index, 3);
    assert_eq!(
      out.values,
      vec![
        ConvertedArgument::Value(WebIdlValue::JsValue(event)),
        ConvertedArgument::Value(WebIdlValue::String(b)),
        ConvertedArgument::Missing,
      ]
    );
  }

  #[test]
  fn url_constructor_like_overloads_select_by_argument_count() {
    let mut rt = VmJsRuntime::new();

    // Real-world-ish: URL(url) vs URL(url, base)
    let overloads = vec![
      OverloadSig {
        args: vec![OverloadArg {
          ty: IdlType::String(StringType::UsvString),
          optionality: Optionality::Required,
          default: None,
        }],
        decl_index: 0,
        distinguishing_arg_index_by_arg_count: None,
      },
      OverloadSig {
        args: vec![
          OverloadArg {
            ty: IdlType::String(StringType::UsvString),
            optionality: Optionality::Required,
            default: None,
          },
          OverloadArg {
            ty: IdlType::String(StringType::UsvString),
            optionality: Optionality::Required,
            default: None,
          },
        ],
        decl_index: 1,
        distinguishing_arg_index_by_arg_count: None,
      },
    ];

    let url = rt.alloc_string_value("https://example.com/").unwrap();
    let base = rt.alloc_string_value("https://base.example/").unwrap();

    let out = resolve_overload(&mut rt, &overloads, &[url]).unwrap();
    assert_eq!(out.overload_index, 0);

    let out = resolve_overload(&mut rt, &overloads, &[url, base]).unwrap();
    assert_eq!(out.overload_index, 1);
  }

  #[test]
  fn overload_resolution_no_match_throws_type_error() {
    let mut rt = VmJsRuntime::new();
    let overloads = vec![OverloadSig {
      args: vec![OverloadArg {
        ty: IdlType::Boolean,
        optionality: Optionality::Required,
        default: None,
      }],
      decl_index: 0,
      distinguishing_arg_index_by_arg_count: None,
    }];

    let err = resolve_overload(&mut rt, &overloads, &[]).unwrap_err();
    let msg = thrown_message(&mut rt, err);
    assert!(msg.contains("No matching overload"));
  }

  #[test]
  fn overload_resolution_ambiguous_overload_set_throws_type_error() {
    let mut rt = VmJsRuntime::new();
    let overloads = vec![
      OverloadSig {
        args: vec![OverloadArg {
          ty: IdlType::String(StringType::DomString),
          optionality: Optionality::Required,
          default: None,
        }],
        decl_index: 0,
        distinguishing_arg_index_by_arg_count: None,
      },
      OverloadSig {
        args: vec![OverloadArg {
          ty: IdlType::String(StringType::UsvString),
          optionality: Optionality::Required,
          default: None,
        }],
        decl_index: 1,
        distinguishing_arg_index_by_arg_count: None,
      },
    ];

    let x = rt.alloc_string_value("x").unwrap();
    let err = resolve_overload(&mut rt, &overloads, &[x]).unwrap_err();
    let msg = thrown_message(&mut rt, err);
    assert!(msg.contains("Ambiguous"));
  }

  #[test]
  fn overload_resolution_getter_throw_propagates() {
    let mut rt = VmJsRuntime::new();

    // Overloads: f(sequence<any>) vs f(DOMString)
    let overloads = vec![
      OverloadSig {
        args: vec![OverloadArg {
          ty: IdlType::Sequence(Box::new(IdlType::Any)),
          optionality: Optionality::Required,
          default: None,
        }],
        decl_index: 0,
        distinguishing_arg_index_by_arg_count: None,
      },
      OverloadSig {
        args: vec![OverloadArg {
          ty: IdlType::String(StringType::DomString),
          optionality: Optionality::Required,
          default: None,
        }],
        decl_index: 1,
        distinguishing_arg_index_by_arg_count: None,
      },
    ];

    let getter = rt
      .alloc_function_value(|rt, _this, _args| Err(rt.throw_type_error("boom")))
      .unwrap();
    let obj = rt.alloc_object_value().unwrap();

    let iter_key = rt.symbol_iterator().unwrap();
    rt.define_accessor_property(obj, iter_key, getter, Value::Undefined, true)
      .unwrap();

    let err = resolve_overload(&mut rt, &overloads, &[obj]).unwrap_err();
    let msg = thrown_message(&mut rt, err);
    assert!(msg.contains("boom"));
  }

  #[test]
  fn optional_argument_default_is_used_when_undefined() {
    let mut rt = VmJsRuntime::new();
    let overloads = vec![OverloadSig {
      args: vec![OverloadArg {
        ty: IdlType::String(StringType::DomString),
        optionality: Optionality::Optional,
        default: Some(DefaultValue::String("foo".to_string())),
      }],
      decl_index: 0,
      distinguishing_arg_index_by_arg_count: None,
    }];

    let out = resolve_overload(&mut rt, &overloads, &[Value::Undefined]).unwrap();
    assert_eq!(out.overload_index, 0);
    let [ConvertedArgument::Value(WebIdlValue::String(v))] = out.values.as_slice() else {
      panic!("expected exactly one converted string argument");
    };
    let Value::String(handle) = *v else {
      panic!("expected JS string value");
    };
    assert_eq!(rt.heap().get_string(handle).unwrap().to_utf8_lossy(), "foo");
  }

  #[test]
  fn promise_argument_conversion_wraps_value_in_promise() {
    let mut rt = VmJsRuntime::new();

    let overloads = vec![OverloadSig {
      args: vec![OverloadArg {
        ty: IdlType::Promise(Box::new(IdlType::Any)),
        optionality: Optionality::Required,
        default: None,
      }],
      decl_index: 0,
      distinguishing_arg_index_by_arg_count: None,
    }];

    // Non-Promise input should be coerced via `PromiseResolve(%Promise%, V)`.
    let out = resolve_overload(&mut rt, &overloads, &[Value::Number(1.0)]).unwrap();
    assert_eq!(out.overload_index, 0);

    let [ConvertedArgument::Value(WebIdlValue::JsValue(promise))] = out.values.as_slice() else {
      panic!("expected exactly one converted Promise argument");
    };

    let Value::Object(obj) = *promise else {
      panic!("expected Promise conversion to return an object");
    };
    assert!(rt.heap().is_promise_object(obj));
  }

  fn alloc_iterable_from_values(
    rt: &mut VmJsRuntime,
    values: Vec<Value>,
  ) -> Result<Value, VmError> {
    let next_key = rt.property_key_from_str("next")?;
    let done_key = rt.property_key_from_str("done")?;
    let value_key = rt.property_key_from_str("value")?;

    let iterator_obj = rt.alloc_object_value()?;

    let idx = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let values = std::rc::Rc::new(values);
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

    let iterator_method = rt.alloc_function_value(move |_rt, _this, _args| Ok(iterator_obj))?;

    let iterable_obj = rt.alloc_object_value()?;
    let iterator_sym = rt.symbol_iterator()?;
    rt.define_data_property(iterable_obj, iterator_sym, iterator_method, true)?;

    Ok(iterable_obj)
  }

  #[test]
  fn frozen_array_overload_wins_for_iterable_object() -> Result<(), VmError> {
    let mut rt = VmJsRuntime::new();

    // Overloads: f(FrozenArray<any>) vs f(DOMString)
    let overloads = vec![
      OverloadSig {
        args: vec![OverloadArg {
          ty: IdlType::FrozenArray(Box::new(IdlType::Any)),
          optionality: Optionality::Required,
          default: None,
        }],
        decl_index: 0,
        distinguishing_arg_index_by_arg_count: None,
      },
      OverloadSig {
        args: vec![OverloadArg {
          ty: IdlType::String(StringType::DomString),
          optionality: Optionality::Required,
          default: None,
        }],
        decl_index: 1,
        distinguishing_arg_index_by_arg_count: None,
      },
    ];

    let iterable = alloc_iterable_from_values(&mut rt, vec![Value::Number(1.0)])?;
    let out = resolve_overload(&mut rt, &overloads, &[iterable])?;
    assert_eq!(out.overload_index, 0);
    Ok(())
  }

  #[test]
  fn primitive_string_prefers_domstring_over_sequence_overload() {
    let mut rt = VmJsRuntime::new();

    // Overloads: f(sequence<DOMString>) vs f(DOMString)
    let overloads = vec![
      OverloadSig {
        args: vec![OverloadArg {
          ty: IdlType::Sequence(Box::new(IdlType::String(StringType::DomString))),
          optionality: Optionality::Required,
          default: None,
        }],
        decl_index: 0,
        distinguishing_arg_index_by_arg_count: None,
      },
      OverloadSig {
        args: vec![OverloadArg {
          ty: IdlType::String(StringType::DomString),
          optionality: Optionality::Required,
          default: None,
        }],
        decl_index: 1,
        distinguishing_arg_index_by_arg_count: None,
      },
    ];

    // Primitive strings are not objects; `sequence<T>` conversion must fail, so overload resolution
    // should prefer the DOMString overload.
    let s = rt.alloc_string_value("abc").unwrap();
    let out = resolve_overload(&mut rt, &overloads, &[s]).unwrap();
    assert_eq!(out.overload_index, 1);
  }

  #[test]
  fn string_object_prefers_domstring_over_sequence_overload_without_probing_iterator() {
    let mut rt = VmJsRuntime::new();

    // Overloads: f(sequence<any>) vs f(DOMString)
    let overloads = vec![
      OverloadSig {
        args: vec![OverloadArg {
          ty: IdlType::Sequence(Box::new(IdlType::Any)),
          optionality: Optionality::Required,
          default: None,
        }],
        decl_index: 0,
        distinguishing_arg_index_by_arg_count: None,
      },
      OverloadSig {
        args: vec![OverloadArg {
          ty: IdlType::String(StringType::DomString),
          optionality: Optionality::Required,
          default: None,
        }],
        decl_index: 1,
        distinguishing_arg_index_by_arg_count: None,
      },
    ];

    // Create a String object wrapper.
    let s = rt.alloc_string_value("hello").unwrap();
    let string_obj = rt.to_object(s).unwrap();

    // If overload resolution tried to probe @@iterator for sequence matching, it would trigger this
    // getter and throw. The special-case (d) must treat String objects as strings when a string
    // overload is present.
    let throwing_getter = rt
      .alloc_function_value(|rt, _this, _args| Err(rt.throw_type_error("getter must not run")))
      .unwrap();
    let iter_key = rt.symbol_iterator().unwrap();
    rt.define_accessor_property(string_obj, iter_key, throwing_getter, Value::Undefined, true)
      .unwrap();

    let out = resolve_overload(&mut rt, &overloads, &[string_obj]).unwrap();
    assert_eq!(out.overload_index, 1);

    let [ConvertedArgument::Value(WebIdlValue::String(v))] = out.values.as_slice() else {
      panic!("expected exactly one converted DOMString argument");
    };
    let Value::String(handle) = *v else {
      panic!("expected JS string value");
    };
    assert_eq!(
      rt.heap().get_string(handle).unwrap().to_utf8_lossy(),
      "hello"
    );
  }

  #[test]
  fn sequence_conversion_rejects_non_object_primitives_in_overload_resolution() {
    let mut rt = VmJsRuntime::new();

    let overloads = vec![OverloadSig {
      args: vec![OverloadArg {
        ty: IdlType::Sequence(Box::new(IdlType::Any)),
        optionality: Optionality::Required,
        default: None,
      }],
      decl_index: 0,
      distinguishing_arg_index_by_arg_count: None,
    }];

    let err = resolve_overload(&mut rt, &overloads, &[Value::Number(1.0)]).unwrap_err();
    assert_eq!(thrown_message(&mut rt, err), "Value is not an object");
  }

  #[test]
  fn record_conversion_collects_enumerable_string_keys() -> Result<(), VmError> {
    let mut rt = VmJsRuntime::new();

    let overloads = vec![OverloadSig {
      args: vec![OverloadArg {
        ty: IdlType::Record(
          Box::new(IdlType::String(StringType::DomString)),
          Box::new(IdlType::Numeric(NumericType::Long)),
        ),
        optionality: Optionality::Required,
        default: None,
      }],
      decl_index: 0,
      distinguishing_arg_index_by_arg_count: None,
    }];

    let obj = rt.alloc_object_value()?;
    let a = rt.property_key_from_str("a")?;
    let b = rt.property_key_from_str("b")?;
    rt.define_data_property(obj, a, Value::Number(1.0), true)?;
    rt.define_data_property(obj, b, Value::Number(2.0), true)?;

    let out = resolve_overload(&mut rt, &overloads, &[obj])?;
    let [ConvertedArgument::Value(WebIdlValue::Record { entries, .. })] = out.values.as_slice()
    else {
      panic!("expected record conversion");
    };

    assert_eq!(
      entries,
      &[
        ("a".to_string(), WebIdlValue::Long(1)),
        ("b".to_string(), WebIdlValue::Long(2)),
      ]
    );
    Ok(())
  }

  #[test]
  fn record_conversion_enumerable_symbol_key_throws_type_error() -> Result<(), VmError> {
    let mut rt = VmJsRuntime::new();

    let overloads = vec![OverloadSig {
      args: vec![OverloadArg {
        ty: IdlType::Record(
          Box::new(IdlType::String(StringType::DomString)),
          Box::new(IdlType::Any),
        ),
        optionality: Optionality::Required,
        default: None,
      }],
      decl_index: 0,
      distinguishing_arg_index_by_arg_count: None,
    }];

    let obj = rt.alloc_object_value()?;
    let sym = {
      let mut scope = rt.heap_mut().scope();
      scope.alloc_symbol(Some("s"))?
    };
    rt.define_data_property(obj, PropertyKey::Symbol(sym), Value::Number(1.0), true)?;

    let err = resolve_overload(&mut rt, &overloads, &[obj]).unwrap_err();
    let Some(thrown) = err.thrown_value() else {
      panic!("expected a throw");
    };
    let s = rt.to_string(thrown)?;
    let msg = as_utf8_lossy(&rt, s);
    assert!(
      msg.starts_with("TypeError:"),
      "expected TypeError, got {msg:?}"
    );
    Ok(())
  }

  #[test]
  fn sequence_conversion_enforces_max_sequence_length() -> Result<(), VmError> {
    let mut rt = VmJsRuntime::new();
    rt.set_webidl_limits(webidl::WebIdlLimits {
      max_sequence_length: 2,
      ..webidl::WebIdlLimits::default()
    });

    let overloads = vec![OverloadSig {
      args: vec![OverloadArg {
        ty: IdlType::Sequence(Box::new(IdlType::Any)),
        optionality: Optionality::Required,
        default: None,
      }],
      decl_index: 0,
      distinguishing_arg_index_by_arg_count: None,
    }];

    let iterable =
      alloc_iterable_from_values(&mut rt, vec![Value::Number(1.0), Value::Number(2.0), Value::Number(3.0)])?;
    let err = resolve_overload(&mut rt, &overloads, &[iterable]).unwrap_err();
    assert_eq!(thrown_message(&mut rt, err), "sequence exceeds maximum length");
    Ok(())
  }

  #[test]
  fn string_conversion_enforces_max_string_code_units() -> Result<(), VmError> {
    let mut rt = VmJsRuntime::new();
    rt.set_webidl_limits(webidl::WebIdlLimits {
      max_string_code_units: 4,
      ..webidl::WebIdlLimits::default()
    });

    let overloads = vec![OverloadSig {
      args: vec![OverloadArg {
        ty: IdlType::String(StringType::DomString),
        optionality: Optionality::Required,
        default: None,
      }],
      decl_index: 0,
      distinguishing_arg_index_by_arg_count: None,
    }];

    let s = rt.alloc_string_value("12345")?; // 5 code units > 4 limit.
    let err = resolve_overload(&mut rt, &overloads, &[s]).unwrap_err();
    assert_eq!(thrown_message(&mut rt, err), "string exceeds maximum length");
    Ok(())
  }
}
