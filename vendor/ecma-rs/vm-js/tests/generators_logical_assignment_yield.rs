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
fn generator_logical_or_assignment_captures_binding_and_decision_across_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var x = 0;

        function* g() {
          const r = (x ||= (yield 0));
          return r === 5 && x === 5;
        }

        const it = g();
        const r1 = it.next();

        // Mutate after the yield but before resuming. The decision to assign was made before the
        // yield (because x was falsy), so the assignment must still happen.
        x = 1;

        const r2 = it.next(5);

        r1.value === 0 && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_logical_and_assignment_captures_binding_and_decision_across_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var x = 1;

        function* g() {
          const r = (x &&= (yield 0));
          return r === 5 && x === 5;
        }

        const it = g();
        const r1 = it.next();

        // Mutate after the yield but before resuming. The decision to assign was made before the
        // yield (because x was truthy), so the assignment must still happen.
        x = 0;

        const r2 = it.next(5);

        r1.value === 0 && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_nullish_coalescing_assignment_captures_binding_and_decision_across_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var x = null;

        function* g() {
          const r = (x ??= (yield 0));
          return r === 5 && x === 5;
        }

        const it = g();
        const r1 = it.next();

        // Mutate after the yield but before resuming. The decision to assign was made before the
        // yield (because x was nullish), so the assignment must still happen.
        x = 0;

        const r2 = it.next(5);

        r1.value === 0 && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_logical_or_assignment_captures_base_and_decision_across_yield_for_member_expr() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var o1 = { a: 0 };
        var o2 = { a: 100 };
        var o = o1;

        function* g() {
          const r = (o.a ||= (yield 0));
          return r === 5 && o1.a === 5 && o2.a === 100;
        }

        const it = g();
        const r1 = it.next();

        // Mutate the LHS value and rebind the base after the yield but before resuming.
        // The assignment must still occur (decision was made before yielding) and target the
        // original base.
        o1.a = 1; // truthy now, but should not cancel the pending assignment
        o = o2;

        const r2 = it.next(5);

        r1.value === 0 && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_logical_and_assignment_captures_base_and_decision_across_yield_for_member_expr() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var o1 = { a: 1 };
        var o2 = { a: 100 };
        var o = o1;

        function* g() {
          const r = (o.a &&= (yield 0));
          return r === 5 && o1.a === 5 && o2.a === 100;
        }

        const it = g();
        const r1 = it.next();

        // Mutate the LHS value and rebind the base after the yield but before resuming.
        // The assignment must still occur (decision was made before yielding) and target the
        // original base.
        o1.a = 0; // falsy now, but should not cancel the pending assignment
        o = o2;

        const r2 = it.next(5);

        r1.value === 0 && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_nullish_coalescing_assignment_captures_base_and_decision_across_yield_for_member_expr() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var o1 = { a: null };
        var o2 = { a: 100 };
        var o = o1;

        function* g() {
          const r = (o.a ??= (yield 0));
          return r === 5 && o1.a === 5 && o2.a === 100;
        }

        const it = g();
        const r1 = it.next();

        // Mutate the LHS value and rebind the base after the yield but before resuming.
        // The assignment must still occur (decision was made before yielding) and target the
        // original base.
        o1.a = 0; // non-nullish now, but should not cancel the pending assignment
        o = o2;

        const r2 = it.next(5);

        r1.value === 0 && r1.done === false &&
        r2.value === true && r2.done === true
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
fn generator_logical_or_assignment_captures_base_key_and_decision_across_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var o1 = { a: 0, b: 1 };
        var o2 = { a: 100, b: 100 };
        var o = o1;
        var k = "a";

        function* g() {
          const r = (o[k] ||= (yield 0));
          return r === 5 && o1.a === 5 && o1.b === 1 && o2.a === 100 && o2.b === 100;
        }

        const it = g();
        const r1 = it.next();

        // Mutate the LHS value and rebind the base/key after the yield but before resuming.
        // The assignment must still occur (decision was made before yielding) and target the
        // original base/key pair.
        o1.a = 1; // truthy now, but should not cancel the pending assignment
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
fn generator_logical_or_assignment_captures_binding_and_decision_across_yield_star() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var x = 0;

        function* rhs() {
          yield 1;
          yield 2;
          return 5;
        }

        function* g() {
          const r = (x ||= (yield* rhs()));
          return r === 5 && x === 5;
        }

        const it = g();
        const r1 = it.next();

        // Mutate after yielding but before resuming; must not affect the already-made decision.
        x = 1;

        const r2 = it.next();

        const r3 = it.next();

        r1.value === 1 && r1.done === false &&
        r2.value === 2 && r2.done === false &&
        r3.value === true && r3.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_logical_or_assignment_captures_base_and_decision_across_yield_star_for_member_expr() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var o1 = { a: 0 };
        var o2 = { a: 100 };
        var o = o1;

        function* rhs() {
          yield "rhs1";
          yield "rhs2";
          return 5;
        }

        function* g() {
          const r = (o.a ||= (yield* rhs()));
          return r === 5 && o1.a === 5 && o2.a === 100;
        }

        const it = g();
        const r1 = it.next();

        // Mutate the LHS value and rebind the base after yielding but before resuming.
        // The pending assignment must still target the original base and proceed through the
        // remaining yields.
        o1.a = 1;
        o = o2;

        const r2 = it.next();

        const r3 = it.next();

        r1.value === "rhs1" && r1.done === false &&
        r2.value === "rhs2" && r2.done === false &&
        r3.value === true && r3.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_logical_and_assignment_captures_base_key_and_decision_across_yield_star() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var o1 = { a: 1, b: 1 };
        var o2 = { a: 100, b: 100 };
        var o = o1;
        var k = "a";

        function* rhs() {
          yield "rhs1";
          yield "rhs2";
          return 5;
        }

        function* g() {
          const r = (o[k] &&= (yield* rhs()));
          return r === 5 && o1.a === 5 && o1.b === 1 && o2.a === 100 && o2.b === 100;
        }

        const it = g();
        const r1 = it.next();

        // Mutate base/key/LHS value after yielding but before resuming. The assignment target and
        // decision must still be the one determined before evaluating the RHS.
        o1.a = 0;
        o = o2;
        k = "b";

        const r2 = it.next();

        const r3 = it.next();

        r1.value === "rhs1" && r1.done === false &&
        r2.value === "rhs2" && r2.done === false &&
        r3.value === true && r3.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_nullish_coalescing_assignment_captures_base_key_and_decision_across_yield_star() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var o1 = { a: null, b: 1 };
        var o2 = { a: null, b: null };
        var o = o1;
        var k = "a";

        function* rhs() {
          yield "rhs1";
          yield "rhs2";
          return 5;
        }

        function* g() {
          const r = (o[k] ??= (yield* rhs()));
          return r === 5 && o1.a === 5 && o1.b === 1 && o2.a === null && o2.b === null;
        }

        const it = g();
        const r1 = it.next();

        // Mutate base/key/LHS value after yielding but before resuming. The assignment target and
        // decision must still be the one determined before evaluating the RHS.
        o1.a = 0;
        o = o2;
        k = "b";

        const r2 = it.next();

        const r3 = it.next();

        r1.value === "rhs1" && r1.done === false &&
        r2.value === "rhs2" && r2.done === false &&
        r3.value === true && r3.done === true
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

#[test]
fn generator_logical_or_assignment_with_yield_in_computed_key_and_rhs_captures_key_across_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var o = { a: 0, b: 0 };
        var k = "a";

        function* g() {
          const r = (o[(yield "key", k)] ||= (yield 0));
          return r === 5 && o.a === 0 && o.b === 5;
        }

        const it = g();
        const r1 = it.next();

        // Key is evaluated *after* resuming from the key-yield.
        k = "b";
        const r2 = it.next();

        // Mutate the key and the LHS value after the RHS yield but before resuming.
        // The assignment must still happen (decision was made before yielding) and target the
        // original key.
        o.b = 1;
        k = "a";

        const r3 = it.next(5);

        r1.value === "key" && r1.done === false &&
        r2.value === 0 && r2.done === false &&
        r3.value === true && r3.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_nullish_coalescing_assignment_with_yield_in_computed_key_and_rhs_captures_key_across_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var o = { a: 0, b: null };
        var k = "a";

        function* g() {
          const r = (o[(yield "key", k)] ??= (yield 0));
          return r === 5 && o.a === 0 && o.b === 5;
        }

        const it = g();
        const r1 = it.next();

        // Key is evaluated *after* resuming from the key-yield.
        k = "b";
        const r2 = it.next();

        // Mutate the key and the LHS value after the RHS yield but before resuming.
        // The assignment must still happen (decision was made before yielding) and target the
        // original key.
        o.b = 0;
        k = "a";

        const r3 = it.next(5);

        r1.value === "key" && r1.done === false &&
        r2.value === 0 && r2.done === false &&
        r3.value === true && r3.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_logical_and_assignment_with_yield_in_computed_key_and_rhs_captures_base_key_and_decision_across_yield(
) {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var o1 = { a: 0, b: 1 };
        var o2 = { a: 0, b: 100 };
        var o = o1;
        var k = "a";

        function* g() {
          const r = (o[(yield "key", k)] &&= (yield 0));
          return r === 5 && o1.a === 0 && o1.b === 5 && o2.a === 0 && o2.b === 100;
        }

        const it = g();
        const r1 = it.next();

        // Key is evaluated after resuming from the key-yield.
        k = "b";
        // Base is evaluated before the key-yield; rebinding it must not affect the target.
        o = o2;
        const r2 = it.next();

        // Mutate base/key/LHS value after the RHS yield but before resuming.
        // The assignment must still happen and target the original base+key.
        o1.b = 0;
        k = "a";
        o = o2;

        const r3 = it.next(5);

        r1.value === "key" && r1.done === false &&
        r2.value === 0 && r2.done === false &&
        r3.value === true && r3.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_logical_or_assignment_with_yield_in_base_and_computed_key_and_rhs_captures_base_key_and_decision_across_yield(
) {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var o1 = { a: 0, b: 0 };
        var o2 = { a: 0, b: 0 };
        var o = o1;
        var k = "a";

        function* g() {
          const r = ((yield "base", o)[(yield "key", k)] ||= (yield 0));
          return r === 5 && o1.a === 0 && o1.b === 0 && o2.a === 0 && o2.b === 5;
        }

        const it = g();
        const r1 = it.next();

        // Base is evaluated *after* resuming from the base-yield.
        o = o2;
        const r2 = it.next();

        // Key is evaluated *after* resuming from the key-yield.
        k = "b";
        // Mutating the base binding here must not affect the already-chosen base object.
        o = o1;
        const r3 = it.next();

        // Mutate after the RHS yield but before resuming. The decision to assign was made before
        // yielding, so the assignment must still happen to the original base + key.
        o2.b = 1;
        k = "a";
        o = o1;

        const r4 = it.next(5);

        r1.value === "base" && r1.done === false &&
        r2.value === "key" && r2.done === false &&
        r3.value === 0 && r3.done === false &&
        r4.value === true && r4.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
