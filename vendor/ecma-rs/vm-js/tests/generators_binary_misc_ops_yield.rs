use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Keep a bit of headroom over the minimal 1MiB heaps used by some unit tests so these generator
  // tests don't become flaky as builtin surface area grows.
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
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
        var r2 = it.next(1);
        var r3 = it.next(2);
        r1.value === 1 && r1.done === false &&
        r2.value === 2 && r2.done === false &&
        r3.done === true && r3.value === 3
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
        r1.value === 1 && r1.done === false &&
        r2.value === 2 && r2.done === false &&
        r3.done === true && r3.value === 2
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
        var r3 = it.next(1);
        r1.value === 1 && r1.done === false &&
        r2.value === 2 && r2.done === false &&
        r3.done === true && r3.value === 4
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn left_shift_with_yield_in_rhs() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g(){ return 1 << (yield 1); }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(2);
        r1.value === 1 && r1.done === false &&
        r2.done === true && r2.value === 4
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn unsigned_right_shift_with_yield_in_rhs() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g(){ return (-1) >>> (yield 0); }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(0);
        r1.value === 0 && r1.done === false &&
        r2.done === true && r2.value === 4294967295
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn signed_right_shift_with_yield_in_rhs() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g(){ return 8 >> (yield 1); }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(2);
        r1.value === 1 && r1.done === false &&
        r2.done === true && r2.value === 2
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn left_shift_with_yield_in_both_operands() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g(){ return (yield 1) << (yield 2); }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(1);
        var r3 = it.next(2);
        r1.value === 1 && r1.done === false &&
        r2.value === 2 && r2.done === false &&
        r3.done === true && r3.value === 4
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn signed_right_shift_with_yield_in_both_operands() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g(){ return (yield 1) >> (yield 2); }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(8);
        var r3 = it.next(2);
        r1.value === 1 && r1.done === false &&
        r2.value === 2 && r2.done === false &&
        r3.done === true && r3.value === 2
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn unsigned_right_shift_with_yield_in_both_operands() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g(){ return (yield 1) >>> (yield 2); }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(-1);
        var r3 = it.next(0);
        r1.value === 1 && r1.done === false &&
        r2.value === 2 && r2.done === false &&
        r3.done === true && r3.value === 4294967295
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
        var r2 = it.next('a');
        var r3 = it.next({a:1});
        r1.value === 1 && r1.done === false &&
        r2.value === 2 && r2.done === false &&
        r3.done === true && r3.value === true
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
        r1.value === 1 && r1.done === false &&
        r2.value === 2 && r2.done === false &&
        r3.done === true && r3.value === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn in_operator_rhs_non_object_throws_type_error_under_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          try { return "a" in (yield 0); }
          catch (e) { return e && e.name === "TypeError"; }
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(null);
        r1.value === 0 && r1.done === false &&
        r2.done === true && r2.value === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn instanceof_operator_rhs_non_object_throws_type_error_under_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          try { return ({}) instanceof (yield 0); }
          catch (e) { return e && e.name === "TypeError"; }
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(null);
        r1.value === 0 && r1.done === false &&
        r2.done === true && r2.value === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn bigint_mixing_error_is_catchable_under_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g(){
          try { return 1n + (yield 0); }
          catch (e) { return e && e.name === 'TypeError'; }
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(1);
        r1.value === 0 && r1.done === false &&
        r2.done === true && r2.value === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn bigint_unsigned_right_shift_throws_type_error_under_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g(){
          try { return 1n >>> (yield 0); }
          catch (e) { return e && e.name === 'TypeError'; }
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(1n);
        r1.value === 0 && r1.done === false &&
        r2.done === true && r2.value === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
