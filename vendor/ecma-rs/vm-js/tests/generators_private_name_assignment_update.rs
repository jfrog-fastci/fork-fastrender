use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_private_field_assignment_yield_in_rhs() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      class C {
        static #x = 0;
        static *g() {
          this.#x = (yield 1);
          return this.#x;
        }
      }
      var it = C.g();
      var r1 = it.next();
      var r2 = it.next(42);
      r1.done === false && r1.value === 1 &&
      r2.done === true && r2.value === 42
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_private_field_add_assignment_yield_in_rhs() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      class C {
        static #x = 1;
        static getX() { return this.#x; }
        static *g() {
          this.#x += (yield 1);
          return this.#x;
        }
      }
      var it = C.g();
      var r1 = it.next();
      var r2 = it.next(41);
      r1.done === false && r1.value === 1 &&
      r2.done === true && r2.value === 42 && C.getX() === 42
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_private_field_update_yield_in_base() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      class C {
        static #x = 1;
        static getX() { return this.#x; }
        static *g() { return (yield this).#x++; }
      }
      var it = C.g();
      var r1 = it.next();
      var r2 = it.next(r1.value);
      r2.done === true && r2.value === 1 && C.getX() === 2
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_private_field_prefix_update_yield_in_base() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      class C {
        static #x = 1;
        static getX() { return this.#x; }
        static *g() { return ++(yield this).#x; }
      }
      var it = C.g();
      var r1 = it.next();
      var r2 = it.next(r1.value);
      r2.done === true && r2.value === 2 && C.getX() === 2
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
