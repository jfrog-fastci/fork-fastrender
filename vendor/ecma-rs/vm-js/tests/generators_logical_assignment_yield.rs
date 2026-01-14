use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_logical_and_assignment_short_circuits_without_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          let x = 0;
          // RHS contains a yield, but must not be evaluated because x is falsy.
          const r = (x &&= (yield 1));
          return r === 0 && x === 0;
        }
        const it = g();
        const r1 = it.next();
        r1.done === true && r1.value === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_logical_or_assignment_short_circuits_without_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          let x = 1;
          // RHS contains a yield, but must not be evaluated because x is truthy.
          const r = (x ||= (yield 1));
          return r === 1 && x === 1;
        }
        const it = g();
        const r1 = it.next();
        r1.done === true && r1.value === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_nullish_coalescing_assignment_short_circuits_without_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          let x = 0;
          // RHS contains a yield, but must not be evaluated because x is non-nullish.
          const r = (x ??= (yield 1));
          return r === 0 && x === 0;
        }
        const it = g();
        const r1 = it.next();
        r1.done === true && r1.value === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_logical_and_assignment_captures_base_key_and_decision_across_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var o1 = { a: 1, b: 1 };
        var o2 = { a: 100, b: 100 };
        var o = o1;
        var k = "a";

        function* g() {
          const r = (o[k] &&= (yield 0));
          return r === 5 && o1.a === 5 && o1.b === 1 && o2.a === 100 && o2.b === 100;
        }

        const it = g();
        const r1 = it.next();

        // Mutate the LHS value and rebind the base/key after the yield but before resuming.
        // The assignment must still occur (decision was made before yielding) and target the
        // original base/key pair.
        o1.a = 0; // falsy now, but should not cancel the pending assignment
        o = o2;
        k = "b";

        const r2 = it.next(5);

        r1.value === 0 && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_nullish_coalescing_assignment_captures_base_key_and_decision_across_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var o1 = { a: null, b: 1 };
        var o2 = { a: null, b: null };
        var o = o1;
        var k = "a";

        function* g() {
          const r = (o[k] ??= (yield 0));
          return r === 5 && o1.a === 5 && o1.b === 1 && o2.a === null && o2.b === null;
        }

        const it = g();
        const r1 = it.next();

        // Mutate the LHS value and rebind the base/key after the yield but before resuming.
        // The assignment must still occur (decision was made before yielding) and target the
        // original base/key pair.
        o1.a = 0; // non-nullish now, but should not cancel the pending assignment
        o = o2;
        k = "b";

        const r2 = it.next(5);

        r1.value === 0 && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_logical_or_assignment_with_yield_in_computed_key_short_circuits_rhs_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          const o = { a: 1 };
          // Yield in the computed key expression happens first.
          // Because o.a is truthy, `||=` must short-circuit and never evaluate the RHS yield.
          o[(yield "k")] ||= (yield 0);
          return o.a;
        }
        const it = g();
        const r1 = it.next();
        const r2 = it.next("a");
        r1.value === "k" && r1.done === false &&
        r2.value === 1 && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_logical_and_assignment_with_yield_in_computed_key_short_circuits_rhs_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          const o = { a: 0 };
          // Yield in the computed key expression happens first.
          // Because o.a is falsy, `&&=` must short-circuit and never evaluate the RHS yield.
          o[(yield "k")] &&= (yield 0);
          return o.a;
        }
        const it = g();
        const r1 = it.next();
        const r2 = it.next("a");
        r1.value === "k" && r1.done === false &&
        r2.value === 0 && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_nullish_coalescing_assignment_with_yield_in_computed_key_short_circuits_rhs_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          const o = { a: 0 };
          // Yield in the computed key expression happens first.
          // Because o.a is non-nullish, `??=` must short-circuit and never evaluate the RHS yield.
          o[(yield "k")] ??= (yield 0);
          return o.a;
        }
        const it = g();
        const r1 = it.next();
        const r2 = it.next("a");
        r1.value === "k" && r1.done === false &&
        r2.value === 0 && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
