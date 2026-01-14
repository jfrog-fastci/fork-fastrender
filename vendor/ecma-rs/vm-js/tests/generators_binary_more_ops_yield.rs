use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn new_runtime_with_frequent_gc() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(16 * 1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn subtraction_with_yield_in_both_operands() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g(){ return (yield 1) - (yield 2); }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(10);
        var r3 = it.next(3);
        r1.value === 1 && r2.value === 2 && r3.done === true && r3.value === 7
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn division_with_yield_in_both_operands() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g(){ return (yield 1) / (yield 2); }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(8);
        var r3 = it.next(2);
        r1.value === 1 && r2.value === 2 && r3.done === true && r3.value === 4
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn remainder_with_yield_in_both_operands() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g(){ return (yield 1) % (yield 2); }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(5);
        var r3 = it.next(2);
        r1.value === 1 && r2.value === 2 && r3.done === true && r3.value === 1
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn strict_inequality_with_yield_in_both_operands() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g(){ return (yield 1) !== (yield 2); }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(7);
        var r3 = it.next(8);
        r1.value === 1 && r2.value === 2 && r3.done === true && r3.value === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn abstract_equality_with_yield_in_both_operands() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g(){ return (yield 1) == (yield 2); }
        var it = g();
        var r1 = it.next();
        var r2 = it.next("5");
        var r3 = it.next(5);
        r1.value === 1 && r2.value === 2 && r3.done === true && r3.value === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn relational_greater_than_or_equal_with_yield_in_both_operands() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g(){ return (yield 1) >= (yield 2); }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(10);
        var r3 = it.next(10);
        r1.value === 1 && r2.value === 2 && r3.done === true && r3.value === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn exponentiation_with_yield_in_both_operands() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g(){ return (yield 1) ** (yield 2); }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(2);
        var r3 = it.next(3);
        r1.value === 1 && r2.value === 2 && r3.done === true && r3.value === 8
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn bitwise_or_with_yield_in_both_operands() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g(){ return (yield 1) | (yield 2); }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(5);
        var r3 = it.next(2);
        r1.value === 1 && r2.value === 2 && r3.done === true && r3.value === 7
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn bitwise_and_with_yield_in_both_operands() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g(){ return (yield 1) & (yield 2); }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(6);
        var r3 = it.next(3);
        r1.value === 1 && r2.value === 2 && r3.done === true && r3.value === 2
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn bitwise_xor_with_yield_in_both_operands() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g(){ return (yield 1) ^ (yield 2); }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(5);
        var r3 = it.next(3);
        r1.value === 1 && r2.value === 2 && r3.done === true && r3.value === 6
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn shift_left_with_yield_in_both_operands() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g(){ return (yield 1) << (yield 2); }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(5);
        var r3 = it.next(1);
        r1.value === 1 && r2.value === 2 && r3.done === true && r3.value === 10
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn shift_right_with_yield_in_both_operands() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g(){ return (yield 1) >> (yield 2); }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(10);
        var r3 = it.next(1);
        r1.value === 1 && r2.value === 2 && r3.done === true && r3.value === 5
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn unsigned_shift_right_with_yield_in_both_operands() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g(){ return (yield 1) >>> (yield 2); }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(10);
        var r3 = it.next(1);
        r1.value === 1 && r2.value === 2 && r3.done === true && r3.value === 5
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn in_operator_with_yield_in_both_operands() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g(){ return (yield 1) in (yield 2); }
        var it = g();
        var r1 = it.next();
        var r2 = it.next("a");
        var r3 = it.next({a: 1});
        r1.value === 1 && r2.value === 2 && r3.done === true && r3.value === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn instanceof_operator_with_yield_in_both_operands() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g(){ return (yield 1) instanceof (yield 2); }
        var it = g();
        var r1 = it.next();
        var r2 = it.next({});
        var r3 = it.next(Object);
        r1.value === 1 && r2.value === 2 && r3.done === true && r3.value === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn bigint_exponentiation_with_yield_in_both_operands() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g(){ return (yield 1n) ** (yield 2n); }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(2n);
        var r3 = it.next(3n);
        r1.value === 1n && r2.value === 2n && r3.done === true && r3.value === 8n
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn bigint_bitwise_or_with_yield_in_both_operands() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g(){ return (yield 1n) | (yield 2n); }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(5n);
        var r3 = it.next(2n);
        r1.value === 1n && r2.value === 2n && r3.done === true && r3.value === 7n
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn bigint_shift_left_with_yield_in_both_operands() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g(){ return (yield 1n) << (yield 2n); }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(5n);
        var r3 = it.next(1n);
        r1.value === 1n && r2.value === 2n && r3.done === true && r3.value === 10n
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn instanceof_yield_in_both_operands_with_gc_between_yields() {
  let mut rt = new_runtime_with_frequent_gc();
  rt
    .exec_script(
      r#"
        function churn() {
          // Allocate enough to exceed the low GC threshold and force a collection.
          const buf = new Uint8Array(2 * 1024 * 1024);
          return buf.length;
        }

        function* g(){ return (yield 1) instanceof (yield 2); }
        var it = g();
        var r1 = it.next();
      "#,
    )
    .unwrap();

  // Resume the first yield with an object that is only reachable through the generator's
  // continuation frame while we suspend on the RHS yield.
  rt.exec_script(r#"var r2 = it.next({});"#).unwrap();

  let gc_before = rt.heap.gc_runs();
  rt.exec_script(r#"churn();"#).unwrap();
  let gc_after = rt.heap.gc_runs();
  assert!(
    gc_after > gc_before,
    "expected at least one GC cycle while generator is suspended in binary operator"
  );

  let value = rt
    .exec_script(
      r#"
        var r3 = it.next(Object);
        r1.value === 1 && r1.done === false &&
        r2.value === 2 && r2.done === false &&
        r3.value === true && r3.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
