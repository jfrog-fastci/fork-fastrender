use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_and_short_circuit_prevents_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return false && (yield 1); }
      var it = g();
      var r = it.next();
      r.done === true && r.value === false
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_and_evaluates_rhs_when_truthy_and_can_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return true && (yield 1); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(42);
      r1.done === false && r1.value === 1 && r2.done === true && r2.value === 42
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_and_short_circuiting_after_yield_in_lhs_skips_rhs() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield false) && (yield 1); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(0);
      r1.done === false && r1.value === false &&
      r2.done === true && r2.value === 0
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_and_rhs_is_evaluated_after_yield_in_lhs_when_truthy() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield true) && (yield 1); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(1);
      var r3 = it.next(42);
      r1.done === false && r1.value === true &&
      r2.done === false && r2.value === 1 &&
      r3.done === true && r3.value === 42
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_or_short_circuit_prevents_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return true || (yield 1); }
      var it = g();
      var r = it.next();
      r.done === true && r.value === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_or_evaluates_rhs_when_falsy_and_can_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return false || (yield 1); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(42);
      r1.done === false && r1.value === 1 && r2.done === true && r2.value === 42
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_or_short_circuiting_after_yield_in_lhs_skips_rhs() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield true) || (yield 1); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(1);
      r1.done === false && r1.value === true &&
      r2.done === true && r2.value === 1
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_or_rhs_is_evaluated_after_yield_in_lhs_when_falsy() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield false) || (yield 1); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(0);
      var r3 = it.next(42);
      r1.done === false && r1.value === false &&
      r2.done === false && r2.value === 1 &&
      r3.done === true && r3.value === 42
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_nullish_coalescing_short_circuit_prevents_yield_when_not_nullish() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return 0 ?? (yield 1); }
      var it = g();
      var r = it.next();
      r.done === true && r.value === 0
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_nullish_coalescing_evaluates_rhs_when_nullish_and_can_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return null ?? (yield 1); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(42);
      r1.done === false && r1.value === 1 && r2.done === true && r2.value === 42
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_nullish_coalescing_short_circuiting_after_yield_in_lhs_skips_rhs() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield null) ?? (yield 1); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(0);
      r1.done === false && r1.value === null &&
      r2.done === true && r2.value === 0
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_nullish_coalescing_rhs_is_evaluated_after_yield_in_lhs_when_nullish() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield undefined) ?? (yield 1); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(undefined);
      var r3 = it.next(42);
      r1.done === false && r1.value === undefined &&
      r2.done === false && r2.value === 1 &&
      r3.done === true && r3.value === 42
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_comma_operator_with_yield_on_lhs() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield 1, 2); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(999);
      r1.done === false && r1.value === 1 && r2.done === true && r2.value === 2
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_comma_operator_with_yield_on_rhs() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (0, yield 1); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(7);
      r1.done === false && r1.value === 1 && r2.done === true && r2.value === 7
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_comma_operator_with_yield_on_both_sides() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield 1, yield 2); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(10);
      var r3 = it.next(20);
      r1.done === false && r1.value === 1 &&
      r2.done === false && r2.value === 2 &&
      r3.done === true && r3.value === 20
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
