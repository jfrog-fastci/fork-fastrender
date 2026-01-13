use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generators_yield_in_lhs_of_and_and_rhs_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield 1) && (yield 2); }

      // Falsy path: should short-circuit and never evaluate `(yield 2)`.
      var it = g();
      var a1 = it.next();
      var a2 = it.next(0);
      var a3 = it.next();
      var ok_falsy =
        a1.value === 1 && a1.done === false &&
        a2.value === 0 && a2.done === true &&
        a3.value === undefined && a3.done === true;

      // Truthy path: should evaluate RHS and yield 2.
      it = g();
      var b1 = it.next();
      var b2 = it.next(10);
      var b3 = it.next(20);
      var b4 = it.next();
      var ok_truthy =
        b1.value === 1 && b1.done === false &&
        b2.value === 2 && b2.done === false &&
        b3.value === 20 && b3.done === true &&
        b4.value === undefined && b4.done === true;

      ok_falsy && ok_truthy
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_lhs_of_or_or_rhs_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield 1) || (yield 2); }

      // Truthy path: should short-circuit and never evaluate `(yield 2)`.
      var it = g();
      var a1 = it.next();
      var a2 = it.next(10);
      var a3 = it.next();
      var ok_truthy =
        a1.value === 1 && a1.done === false &&
        a2.value === 10 && a2.done === true &&
        a3.value === undefined && a3.done === true;

      // Falsy path: should evaluate RHS and yield 2.
      it = g();
      var b1 = it.next();
      var b2 = it.next(0);
      var b3 = it.next(20);
      var b4 = it.next();
      var ok_falsy =
        b1.value === 1 && b1.done === false &&
        b2.value === 2 && b2.done === false &&
        b3.value === 20 && b3.done === true &&
        b4.value === undefined && b4.done === true;

      ok_truthy && ok_falsy
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_lhs_of_nullish_coalescing_and_rhs_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g(){ return (yield 1) ?? (yield 2); }

      // Non-nullish path: should short-circuit even if value is falsy (e.g. 0).
      var it = g();
      var a1 = it.next();
      var a2 = it.next(0);
      var a3 = it.next();
      var ok_non_nullish =
        a1.value === 1 && a1.done === false &&
        a2.value === 0 && a2.done === true &&
        a3.value === undefined && a3.done === true;

      // Nullish path: should evaluate RHS and yield 2 when resumed with undefined.
      it = g();
      var b1 = it.next();
      var b2 = it.next(); // undefined
      var b3 = it.next(20);
      var b4 = it.next();
      var ok_nullish =
        b1.value === 1 && b1.done === false &&
        b2.value === 2 && b2.done === false &&
        b3.value === 20 && b3.done === true &&
        b4.value === undefined && b4.done === true;

      ok_non_nullish && ok_nullish
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

