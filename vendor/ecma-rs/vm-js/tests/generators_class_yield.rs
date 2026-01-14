use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_yield_in_class_expr_extends() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        return class extends (yield 1) {};
      }
      class Base {}
      var it = g();
      var r1 = it.next();
      var r2 = it.next(Base);
      var C = r2.value;
      r1.value === 1 && r1.done === false &&
      r2.done === true &&
      Object.getPrototypeOf(C) === Base &&
      Object.getPrototypeOf(C.prototype) === Base.prototype
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_in_class_decl_extends() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        class D extends (yield 1) {}
        return D;
      }
      class Base {}
      var it = g();
      var r1 = it.next();
      var r2 = it.next(Base);
      var D = r2.value;
      r1.value === 1 && r1.done === false &&
      r2.done === true &&
      D.name === "D" &&
      Object.getPrototypeOf(D) === Base &&
      Object.getPrototypeOf(D.prototype) === Base.prototype
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_in_class_computed_method_name() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        class C { [yield 1]() { return 2; } }
        return new C().m();
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next("m");
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

