use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn symbol_for_returns_same_symbol_for_same_key() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"Symbol.for("x") === Symbol.for("x")"#).unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn symbol_key_for_returns_key_for_registered_symbols() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"Symbol.keyFor(Symbol.for("x")) === "x""#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn symbol_key_for_returns_undefined_for_unregistered_symbols() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"Symbol.keyFor(Symbol("x")) === undefined"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn new_symbol_throws_type_error() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var ok = false;
        try { new Symbol(); } catch (e) { ok = e && e.name === "TypeError"; }
        ok
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn symbol_description_accessor_behaves_per_spec() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var a = Symbol().description === undefined;
        var b = Symbol("x").description === "x";
        var c = Object(Symbol("y")).description === "y";
        a && b && c
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn symbol_to_string_tag_is_installed() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"Symbol.prototype[Symbol.toStringTag] === "Symbol""#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn symbol_length_is_zero() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"Symbol.length === 0"#).unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn symbol_to_primitive_is_non_writable() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var desc = Reflect.getOwnPropertyDescriptor(Symbol.prototype, Symbol.toPrimitive);
        desc &&
          desc.writable === false &&
          desc.enumerable === false &&
          desc.configurable === true &&
          typeof desc.value === "function" &&
          desc.value.name === "[Symbol.toPrimitive]" &&
          desc.value.length === 1
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
