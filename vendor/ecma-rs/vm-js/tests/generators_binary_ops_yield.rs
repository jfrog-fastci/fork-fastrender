use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_binary_add_yields_twice_and_uses_resume_values() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() { return (yield 1) + (yield 2); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(10);
      var r3 = it.next(20);
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.value === 30 && r3.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_mul_yields_once_and_uses_resume_value() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() { return (yield 1) * 2; }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(21);
      r1.value === 1 && r1.done === false &&
      r2.value === 42 && r2.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_strict_eq_yields_twice_and_compares_resume_values() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() { return (yield 1) === (yield 2); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(10);
      var r3 = it.next(10);
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.value === true && r3.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_relational_yields_twice_and_orders_operands_left_to_right() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() { return (yield 1) < (yield 2); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(10);
      var r3 = it.next(5);
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.value === false && r3.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

