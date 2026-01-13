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
fn generator_short_circuit_uses_comma_result_after_yield_in_operand() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g_and(){ return (yield 1, false) && (yield 2); }
      var it_and = g_and();
      var a1 = it_and.next();
      var a2 = it_and.next(123);

      function* g_or(){ return (yield 1, true) || (yield 2); }
      var it_or = g_or();
      var b1 = it_or.next();
      var b2 = it_or.next(123);

      function* g_nullish_skip(){ return (yield 1, 0) ?? (yield 2); }
      var it_ns = g_nullish_skip();
      var c1 = it_ns.next();
      var c2 = it_ns.next(123);

      function* g_nullish_eval(){ return (yield 1, null) ?? (yield 2); }
      var it_ne = g_nullish_eval();
      var d1 = it_ne.next();
      var d2 = it_ne.next(123);
      var d3 = it_ne.next(456);

      a1.value === 1 && a1.done === false &&
      a2.value === false && a2.done === true &&

      b1.value === 1 && b1.done === false &&
      b2.value === true && b2.done === true &&

      c1.value === 1 && c1.done === false &&
      c2.value === 0 && c2.done === true &&

      d1.value === 1 && d1.done === false &&
      d2.value === 2 && d2.done === false &&
      d3.value === 456 && d3.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_short_circuit_skips_rhs_yield_star() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g_and(){ return false && (yield* [1, 2]); }
      var it_and = g_and();
      var r_and = it_and.next();

      function* g_or(){ return true || (yield* [1, 2]); }
      var it_or = g_or();
      var r_or = it_or.next();

      function* g_nullish(){ return 0 ?? (yield* [1, 2]); }
      var it_nullish = g_nullish();
      var r_nullish = it_nullish.next();

      r_and.done === true && r_and.value === false &&
      r_or.done === true && r_or.value === true &&
      r_nullish.done === true && r_nullish.value === 0
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_short_circuit_evaluates_rhs_yield_star_when_needed() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g_and(){ return true && (yield* [1, 2]); }
      var it_and = g_and();
      var a1 = it_and.next();
      var a2 = it_and.next();
      var a3 = it_and.next();

      function* g_or(){ return false || (yield* [1, 2]); }
      var it_or = g_or();
      var b1 = it_or.next();
      var b2 = it_or.next();
      var b3 = it_or.next();

      function* g_nullish(){ return null ?? (yield* [1, 2]); }
      var it_nullish = g_nullish();
      var c1 = it_nullish.next();
      var c2 = it_nullish.next();
      var c3 = it_nullish.next();

      a1.value === 1 && a1.done === false &&
      a2.value === 2 && a2.done === false &&
      a3.value === undefined && a3.done === true &&

      b1.value === 1 && b1.done === false &&
      b2.value === 2 && b2.done === false &&
      b3.value === undefined && b3.done === true &&

      c1.value === 1 && c1.done === false &&
      c2.value === 2 && c2.done === false &&
      c3.value === undefined && c3.done === true
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
