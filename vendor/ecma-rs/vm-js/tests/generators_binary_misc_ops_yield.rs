use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
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
        r1.value === 1 && r2.value === 2 && r3.done === true && r3.value === 3
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
        it.next();
        var r = it.next(2);
        r.done === true && r.value === 4
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
        it.next();
        var r = it.next(0);
        r.done === true && r.value === 4294967295
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
        it.next();
        it.next({});
        var r = it.next(Object);
        r.done === true && r.value === true
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
        it.next();
        var r = it.next(1);
        r.done === true && r.value === true
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
        it.next();
        var r = it.next(1n);
        r.done === true && r.value === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

