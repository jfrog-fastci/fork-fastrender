use vm_js::{Heap, HeapLimits, JsRuntime, PropertyKey, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn assert_exec_throws_type_error(rt: &mut JsRuntime, script: &str) {
  let err = rt.exec_script(script).unwrap_err();
  let VmError::Throw(thrown) = err else {
    panic!("expected a thrown JS exception for {script:?}, got {err:?}");
  };
  let Value::Object(obj) = thrown else {
    panic!("expected a thrown object for {script:?}, got {thrown:?}");
  };

  // Root the thrown value across any heap allocations needed to probe it.
  let root = rt.heap_mut().add_root(thrown).unwrap();

  let name_key = {
    let mut scope = rt.heap_mut().scope();
    PropertyKey::from_string(scope.alloc_string("name").unwrap())
  };
  let name_value = rt
    .heap()
    .object_get_own_data_property_value(obj, &name_key)
    .unwrap()
    .unwrap();
  let Value::String(name) = name_value else {
    panic!("expected TypeError.name to be a string, got {name_value:?}");
  };
  let name = rt.heap().get_string(name).unwrap().to_utf8_lossy();
  assert_eq!(name, "TypeError");

  rt.heap_mut().remove_root(root);
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

  // Signed hex/binary/octal forms are *not* valid `StringToNumber` inputs.
  // (e.g. `Number("-0x10")` is `NaN`; use `parseInt` for signed radix parsing).
  let value = rt.exec_script(r#"+'+0x10' !== +'+0x10'"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script(r#"+'-0x10' !== +'-0x10'"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script(r#"+'0b10' === 2"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script(r#"+'+0b10' !== +'+0b10'"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script(r#"+'-0b10' !== +'-0b10'"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script(r#"+'0o10' === 8"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script(r#"+'+0o10' !== +'+0o10'"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script(r#"+'-0o10' !== +'-0o10'"#).unwrap();
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

  assert_exec_throws_type_error(&mut rt, r#"+Symbol('x')"#);

  // String concatenation uses `ToString`, which throws for Symbols.
  assert_exec_throws_type_error(&mut rt, r#"'' + Symbol('x')"#);

  assert_exec_throws_type_error(&mut rt, r#"Symbol('x') + ''"#);

  // But equality just returns false (no coercion to string/number).
  let value = rt.exec_script(r#"Symbol('x') == 'x'"#).unwrap();
  assert_eq!(value, Value::Bool(false));
}
