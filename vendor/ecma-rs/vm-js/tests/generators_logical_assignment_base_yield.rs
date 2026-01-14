use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_logical_or_assignment_member_base_yield_short_circuits_rhs_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var yielded = { a: 100 };
        var resumed = { a: 1 };
        function* g() {
          // Yield in the member base expression happens first.
          // After resuming, the operator must short-circuit and never evaluate the RHS yield.
          return (yield yielded).a ||= (yield 0);
        }
        const it = g();
        const r1 = it.next();
        const r2 = it.next(resumed);
        r1.value === yielded && r1.done === false &&
        r2.value === 1 && r2.done === true &&
        resumed.a === 1 && yielded.a === 100
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_logical_or_assignment_member_base_yield_short_circuits_rhs_yield_star() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var yielded = { a: 100 };
        var resumed = { a: 1 };
        var called = false;
        function rhs() {
          called = true;
          return (function*() { yield 0; })();
        }
        function* g() {
          // Yield in the member base expression happens first.
          // After resuming, the operator must short-circuit and never evaluate the RHS yield*.
          return (yield yielded).a ||= (yield* rhs());
        }
        const it = g();
        const r1 = it.next();
        const r2 = it.next(resumed);
        r1.value === yielded && r1.done === false &&
        r2.value === 1 && r2.done === true &&
        called === false &&
        resumed.a === 1 && yielded.a === 100
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_logical_or_assignment_member_base_yield_then_rhs_yield_assigns_to_resumed_base() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var yielded = { a: 100 };
        var resumed = { a: 0 };
        function* g() {
          // Base yields first; after resuming, LHS is falsy so `||=` evaluates the RHS `yield`
          // and assigns to the resumed base object.
          const r = ((yield yielded).a ||= (yield 1));
          return r;
        }
        const it = g();
        const r1 = it.next();
        const r2 = it.next(resumed);
        const r3 = it.next(5);
        r1.value === yielded && r1.done === false &&
        r2.value === 1 && r2.done === false &&
        r3.value === 5 && r3.done === true &&
        resumed.a === 5 && yielded.a === 100
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_logical_or_assignment_computed_member_base_yield_evaluates_key_after_resume() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var yielded = { a: 100, b: 1000 };
        var resumed = { a: 0, b: 1 };
        var k = "a";
        function* g() {
          // Yield in the base expression happens before evaluating the computed key.
          // If `k` were evaluated before the yield, the operator would select "a", see a falsy LHS,
          // and start evaluating the RHS yield. The correct behavior is to evaluate `k` after
          // resuming, so updating it here affects which property is accessed.
          return (yield yielded)[k] ||= (yield 0);
        }
        const it = g();
        const r1 = it.next();
        k = "b";
        const r2 = it.next(resumed);
        r1.value === yielded && r1.done === false &&
        r2.value === 1 && r2.done === true &&
        resumed.a === 0 && resumed.b === 1 &&
        yielded.a === 100 && yielded.b === 1000
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
