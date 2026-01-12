use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
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
