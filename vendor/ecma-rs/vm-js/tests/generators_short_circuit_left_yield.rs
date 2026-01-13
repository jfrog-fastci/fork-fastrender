use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generators_and_left_yield_short_circuit_falsy() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g(){ return (yield 1) && (yield 2); }
        var it = g();
        var r1 = it.next();          // yields 1
        var r2 = it.next(false);     // left becomes false => short-circuit, no second yield
        r1.done === false && r1.value === 1 && r2.done === true && r2.value === false
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_and_left_yield_evaluates_rhs_truthy() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g(){ return (yield 1) && (yield 2); }
        var it = g();
        var r1 = it.next();          // yields 1
        var r2 = it.next(true);      // triggers RHS => yields 2
        var r3 = it.next(99);        // RHS yield value becomes 99; overall result 99
        r1.done === false && r1.value === 1 &&
        r2.done === false && r2.value === 2 &&
        r3.done === true && r3.value === 99
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_or_left_yield_short_circuit_truthy() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g(){ return (yield 1) || (yield 2); }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(true);      // short-circuit
        r1.done === false && r1.value === 1 && r2.done === true && r2.value === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_nullish_coalescing_left_yield_evaluates_rhs_when_nullish() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g(){ return (yield 1) ?? (yield 2); }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(null);      // nullish => evaluate RHS => yields 2
        var r3 = it.next(7);
        r1.done === false && r1.value === 1 &&
        r2.done === false && r2.value === 2 &&
        r3.done === true && r3.value === 7
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

