use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
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
