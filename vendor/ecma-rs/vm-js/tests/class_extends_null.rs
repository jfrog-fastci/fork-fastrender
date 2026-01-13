use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn assert_value_is_utf8(rt: &JsRuntime, value: Value, expected: &str) {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  let actual = rt.heap().get_string(s).unwrap().to_utf8_lossy();
  assert_eq!(actual, expected);
}

#[test]
fn class_extends_null_prototype_wiring() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      class C extends null {}
      Object.getPrototypeOf(C) === Function.prototype &&
        Object.getPrototypeOf(C.prototype) === null
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn class_extends_null_default_constructor_throws_type_error() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      class C extends null {}
      try { new C(); 'no' } catch(e) { e.name }
    "#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "TypeError");
}

#[test]
fn class_extends_null_explicit_constructor_can_return_null_proto_object() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      class C extends null { constructor() { return Object.create(null); } }
      const o = new C();
      Object.getPrototypeOf(o) === null
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

