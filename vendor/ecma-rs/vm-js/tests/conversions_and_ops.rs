use vm_js::{
  GcObject, Heap, HeapLimits, JsRuntime, PropertyDescriptor, PropertyKey, PropertyKind, Scope,
  Value, Vm, VmError, VmOptions,
};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn global_var_desc(value: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: true,
    configurable: true,
    kind: PropertyKind::Data {
      value,
      writable: true,
    },
  }
}

fn define_global(
  scope: &mut Scope<'_>,
  global: GcObject,
  name: &str,
  value: Value,
) -> Result<(), VmError> {
  scope.push_root(Value::Object(global))?;
  scope.push_root(value)?;
  let key_s = scope.alloc_string(name)?;
  scope.push_root(Value::String(key_s))?;
  let key = PropertyKey::from_string(key_s);
  scope.define_property(global, key, global_var_desc(value))
}

#[test]
fn plus_operator_concatenates_when_either_side_is_string() {
  let mut rt = new_runtime();

  let value = rt.exec_script(r#""1" + 2 === "12""#).unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script(r#"1 + "2" === "12""#).unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn numeric_operators_use_tonumber_for_strings() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#""5" - 2 === 3"#).unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn abstract_equality_matches_ecmascript_primitives() {
  let mut rt = new_runtime();

  let value = rt.exec_script(r#"null == undefined"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script(r#"false == 0"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script(r#"true == 1"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script(r#""0" == 0"#).unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn abstract_equality_coerces_objects_via_to_primitive() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      "(() => {\n\
        const o = { valueOf() { return 1; } };\n\
        return o == 1;\n\
      })()",
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn abstract_equality_uses_symbol_to_primitive_and_typeerrors_are_catchable_with_stack() {
  let mut rt = new_runtime();

  // `==` should observe `@@toPrimitive` and throw if it is non-callable.
  let ok = rt
    .exec_script(
      "(() => {\n\
        try {\n\
          return ({ [Symbol.toPrimitive]: 123 }) == 1;\n\
        } catch (e) {\n\
          return e && e.name === 'TypeError';\n\
        }\n\
      })()",
    )
    .unwrap();
  assert_eq!(ok, Value::Bool(true));

  // Uncaught conversion TypeErrors should be surfaced as ThrowWithStack.
  let err = rt
    .exec_script(
      "let o = { [Symbol.toPrimitive]: 123 };\n\
       \n\
       o == 1;",
    )
    .unwrap_err();
  match err {
    VmError::ThrowWithStack { stack, .. } => {
      assert!(!stack.is_empty(), "expected ThrowWithStack frames");
      assert_eq!(stack[0].line, 3);
    }
    other => panic!("expected ThrowWithStack, got {other:?}"),
  }
}

#[test]
fn plus_operator_uses_symbol_to_primitive_via_proxy_get_trap() -> Result<(), VmError> {
  let mut rt = new_runtime();

  // `vm-js` does not currently expose a JS `Proxy` constructor. Allocate a Proxy object from Rust
  // and use a JS-level handler `get` trap to validate that interpreter coercions dispatch `Get`
  // through Proxy internal methods.
  rt.exec_script(
    r#"
      var log = [];
      var target = {};
      var handler = {
        get: function (t, k, r) {
          log.push(String(k));
          if (k === Symbol.toPrimitive) {
            return function () { return "x"; };
          }
        },
      };
    "#,
  )?;

  let target = match rt.exec_script("target")? {
    Value::Object(o) => o,
    other => panic!("expected target object, got {other:?}"),
  };
  let handler = match rt.exec_script("handler")? {
    Value::Object(o) => o,
    other => panic!("expected handler object, got {other:?}"),
  };

  let global = rt.realm().global_object();
  {
    let (_vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(Value::Object(target))?;
    scope.push_root(Value::Object(handler))?;
    let proxy = scope.alloc_proxy(Some(target), Some(handler))?;
    define_global(&mut scope, global, "p", Value::Object(proxy))?;
  }

  let result = rt.exec_script(
    r#"
      (() => {
        var out = p + "";
        return out === "x" && log.join(",").includes("toPrimitive");
      })()
    "#,
  )?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn plus_operator_falls_back_to_valueof_via_proxy_get_trap() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var log = [];
      var target = {};
      var handler = {
        get: function (t, k, r) {
          log.push(String(k));
          if (k === "valueOf") {
            return function () { return 41; };
          }
        },
      };
    "#,
  )?;

  let target = match rt.exec_script("target")? {
    Value::Object(o) => o,
    other => panic!("expected target object, got {other:?}"),
  };
  let handler = match rt.exec_script("handler")? {
    Value::Object(o) => o,
    other => panic!("expected handler object, got {other:?}"),
  };

  let global = rt.realm().global_object();
  {
    let (_vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(Value::Object(target))?;
    scope.push_root(Value::Object(handler))?;
    let proxy = scope.alloc_proxy(Some(target), Some(handler))?;
    define_global(&mut scope, global, "p", Value::Object(proxy))?;
  }

  let result = rt.exec_script(
    r#"
      (() => {
        var out = p + 1;
        return out === 42 && log.join(",").includes("valueOf");
      })()
    "#,
  )?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn relational_comparison_uses_string_comparison_when_both_operands_are_strings() {
  let mut rt = new_runtime();

  let value = rt.exec_script(r#""2" < "10""#).unwrap();
  assert_eq!(value, Value::Bool(false));

  let value = rt.exec_script(r#""2" > "10""#).unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn relational_comparison_coerces_objects_to_primitives_and_compares_strings_lexicographically() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      "(() => {\n\
        const o = {\n\
          valueOf() { return {}; },\n\
          toString() { return 'b'; },\n\
        };\n\
        return o < 'c';\n\
      })()",
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn tonumber_rejects_signed_radix_prefixes() {
  let mut rt = new_runtime();

  // ECMA-262 `StringToNumber` does not accept signed 0x/0b/0o prefixes.
  // Assert NaN via self-inequality to avoid depending on NaN bit patterns.
  let value = rt.exec_script(r#"+'+0x10' !== +'+0x10'"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script(r#"+'-0x10' !== +'-0x10'"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script(r#"+'+0b10' !== +'+0b10'"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script(r#"+'-0b10' !== +'-0b10'"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script(r#"+'+0o10' !== +'+0o10'"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script(r#"+'-0o10' !== +'-0o10'"#).unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn plus_operator_concatenates_objects_when_toprimitive_returns_string() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      "(() => {\n\
        const o = {\n\
          valueOf() { return {}; },\n\
          toString() { return 'x'; },\n\
        };\n\
        return o + 1 === 'x1';\n\
      })()",
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_tostring_uses_join_for_common_coercions() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"(() => new Array(2, 4, 8) + "" === "2,4,8")()"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn bigint_and_string_relational_comparisons_use_stringtobigint() {
  let mut rt = new_runtime();

  // Empty string (after trimming) parses as 0n.
  let value = rt.exec_script(r#"0n == """#).unwrap();
  assert_eq!(value, Value::Bool(true));

  // String comparisons must not round through Number for BigInt relational operations.
  let value = rt
    .exec_script(r#""9007199254740993" > 9007199254740992n"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));

  // Invalid BigInt parses yield "undefined" from Abstract Relational Comparison => false.
  let value = rt.exec_script(r#""0." < 1n"#).unwrap();
  assert_eq!(value, Value::Bool(false));
}

#[test]
fn symbol_to_primitive_computed_method_is_parsed_and_receives_default_hint() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      "(() => {\n\
        const o = { [Symbol.toPrimitive](hint) { return hint; } };\n\
        return o == 'default';\n\
      })()",
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
