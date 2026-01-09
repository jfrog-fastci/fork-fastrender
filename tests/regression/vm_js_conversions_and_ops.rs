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

  // Extra primitive coverage.
  let value = rt.exec_script(r#""0" == false"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script(r#"'' == 0"#).unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn tonumber_parses_whitespace_radixes_and_infinity() {
  let mut rt = new_runtime();

  let value = rt.exec_script(r#"+'  1  ' === 1"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script(r#"+'0x10' === 16"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script(r#"+'0b10' === 2"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script(r#"+'0o10' === 8"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  // Infinity parsing is case-sensitive in ECMAScript.
  let value = rt.exec_script(r#"+'Infinity' === 1e999"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script(r#"+'-Infinity' === -1e999"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  // Ensure we don't accept Rust's "inf"/"infinity" shorthands.
  let value = rt.exec_script(r#"+'inf' !== +'inf'"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script(r#"+'' === 0"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script(r#"+'   ' === 0"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  // Empty radix prefixes parse to NaN.
  let value = rt.exec_script(r#"+'0x' !== +'0x'"#).unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn objects_use_toprimitive_for_addition_and_equality() {
  let mut rt = new_runtime();

  // Ordinary objects stringify to "[object Object]" when coerced.
  let value = rt.exec_script(r#"({}) + 'x' === '[object Object]x'"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script(r#"({}) == '[object Object]'"#).unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn abstract_equality_null_is_not_equal_to_zero() {
  let mut rt = new_runtime();

  let value = rt.exec_script(r#"null == 0"#).unwrap();
  assert_eq!(value, Value::Bool(false));

  let value = rt.exec_script(r#"undefined == 0"#).unwrap();
  assert_eq!(value, Value::Bool(false));
}

#[test]
fn symbol_coercions_throw_typeerror() {
  let mut rt = new_runtime();

  let err = rt.exec_script(r#"+Symbol('x')"#).unwrap_err();
  assert!(matches!(err, VmError::TypeError(_)), "err={err:?}");

  // String concatenation uses `ToString`, which throws for Symbols.
  let err = rt.exec_script(r#"'' + Symbol('x')"#).unwrap_err();
  assert!(matches!(err, VmError::TypeError(_)), "err={err:?}");

  let err = rt.exec_script(r#"Symbol('x') + ''"#).unwrap_err();
  assert!(matches!(err, VmError::TypeError(_)), "err={err:?}");

  // But equality just returns false (no coercion to string/number).
  let value = rt.exec_script(r#"Symbol('x') == 'x'"#).unwrap();
  assert_eq!(value, Value::Bool(false));
}
