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
}
