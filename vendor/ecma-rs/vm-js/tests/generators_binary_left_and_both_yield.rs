use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_binary_addition_yield_on_both_sides() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield 1) + (yield 2); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(10);  // left = 10, now yields RHS prompt 2
      var r3 = it.next(20);  // right = 20
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.done === true && r3.value === 30
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_multiplication_yield_on_lhs_only() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield 1) * 2; }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(3);
      r1.value === 1 && r1.done === false &&
      r2.done === true && r2.value === 6
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_subtraction_yield_on_both_sides() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield 1) - (yield 2); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(10);
      var r3 = it.next(3);
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.done === true && r3.value === 7
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_division_yield_on_both_sides() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield 1) / (yield 2); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(20);
      var r3 = it.next(4);
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.done === true && r3.value === 5
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_remainder_yield_on_both_sides() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield 1) % (yield 2); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(20);
      var r3 = it.next(6);
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.done === true && r3.value === 2
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_exponentiation_yield_on_both_sides() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield 1) ** (yield 2); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(2);
      var r3 = it.next(3);
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.done === true && r3.value === 8
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_strict_equality_yield_on_both_sides() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield 1) === (yield 2); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(5);
      var r3 = it.next(5);
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.done === true && r3.value === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_relational_comparison_yield_on_both_sides() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield 1) < (yield 2); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(1);
      var r3 = it.next(2);
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.done === true && r3.value === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
