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
fn generator_binary_bigint_addition_yield_on_both_sides() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield 1n) + (yield 2n); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(10n);
      var r3 = it.next(20n);
      r1.value === 1n && r1.done === false &&
      r2.value === 2n && r2.done === false &&
      r3.done === true && r3.value === 30n
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_bigint_division_yield_on_both_sides() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield 1n) / (yield 2n); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(20n);
      var r3 = it.next(6n);
      r1.value === 1n && r1.done === false &&
      r2.value === 2n && r2.done === false &&
      r3.done === true && r3.value === 3n
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

#[test]
fn generator_binary_abstract_equality_yield_on_both_sides() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield 1) == (yield 2); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next("5");
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
fn generator_binary_strict_inequality_yield_on_both_sides() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield 1) !== (yield 2); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(0);
      var r3 = it.next(0);
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.done === true && r3.value === false
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_abstract_inequality_yield_on_both_sides() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield 1) != (yield 2); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next("5");
      var r3 = it.next(5);
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.done === true && r3.value === false
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_relational_greater_yield_on_both_sides() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield 1) > (yield 2); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(2);
      var r3 = it.next(1);
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.done === true && r3.value === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_relational_less_than_or_equal_yield_on_both_sides() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield 1) <= (yield 2); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(3);
      var r3 = it.next(2);
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.done === true && r3.value === false
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_relational_greater_than_or_equal_yield_on_both_sides() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield 1) >= (yield 2); }
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
fn generator_binary_nested_expression_preserves_outer_left_across_inner_yields() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield 1) + ((yield 2) * (yield 3)); }
      var it = g();
      var r1 = it.next();      // yield outer LHS prompt 1
      var r2 = it.next(10);    // outer left = 10, now evaluating RHS -> yields prompt 2
      var r3 = it.next(5);     // mul left = 5, now yields prompt 3
      var r4 = it.next(2);     // mul right = 2
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.value === 3 && r3.done === false &&
      r4.done === true && r4.value === 20
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_operator_precedence_with_multiple_yields() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      // `*` has higher precedence than `+`: a + (b * c)
      function* g(){ return (yield 1) + (yield 2) * (yield 3); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(10);
      var r3 = it.next(5);
      var r4 = it.next(2);
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.value === 3 && r3.done === false &&
      r4.done === true && r4.value === 20
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_exponentiation_is_right_associative_under_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      // `**` is right-associative: a ** (b ** c)
      function* g(){ return (yield 1) ** (yield 2) ** (yield 3); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(2);
      var r3 = it.next(3);
      var r4 = it.next(2);
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.value === 3 && r3.done === false &&
      r4.done === true && r4.value === 512
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_bigint_exponentiation_is_right_associative_under_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      // `**` is right-associative for BigInt too: a ** (b ** c)
      function* g(){ return (yield 1n) ** (yield 2n) ** (yield 3n); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(2n);
      var r3 = it.next(3n);
      var r4 = it.next(2n);
      r1.value === 1n && r1.done === false &&
      r2.value === 2n && r2.done === false &&
      r3.value === 3n && r3.done === false &&
      r4.done === true && r4.value === 512n
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_binary_left_associativity_with_multiple_yields() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      // `-` is left-associative: ((a - b) - c)
      function* g(){ return (yield 1) - (yield 2) - (yield 3); }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(10);
      var r3 = it.next(3);
      var r4 = it.next(4);
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.value === 3 && r3.done === false &&
      r4.done === true && r4.value === 3
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
