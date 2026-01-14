use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn private_static_field_get_set() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      class C {
        static #x = 1;
        static getX() { return C.#x; }
        static setX(v) { C.#x = v; }
      }
      C.getX() === 1 && (C.setX(2), C.getX() === 2)
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn private_static_method_call() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      class C {
        static #m() { return 1; }
        static m() { return C.#m(); }
      }
      C.m() === 1
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn private_static_accessor_get_set() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      class C {
        static #v = 0;
        static get #x() { return this.#v; }
        static set #x(v) { this.#v = v; }
        static getX() { return C.#x; }
        static setX(v) { C.#x = v; }
      }
      C.getX() === 0 && (C.setX(3), C.getX() === 3)
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn private_static_elements_are_not_exposed_via_symbol_introspection() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      class C {
        static #x = 1;
        static getX() { return C.#x; }
      }
      Object.getOwnPropertySymbols(C).length === 0
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn private_static_field_initializer_super_property_resolves() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class B { static x = 2 }
        class C extends B {
          static #y = super.x;
          static getY() { return C.#y }
        }
        C.getY() === 2
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
