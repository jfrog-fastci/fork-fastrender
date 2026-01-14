use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_yield_in_mul_assignment_rhs_captures_old_value() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var x = 2;
        function* g() { return x *= (yield 0); }
        var it = g();
        var r1 = it.next();
        x = 100; // mutate after the yield but before resuming
        var r2 = it.next(3);
        r1.value === 0 && r1.done === false &&
        r2.value === 6 && r2.done === true &&
        x === 6
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_in_mul_assignment_rhs_captures_property_reference_and_old_value() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var o1 = { a: 2 };
        var o2 = { a: 100 };
        var o = o1;
        function* g() { return o.a *= (yield 0); }
        var it = g();
        var r1 = it.next();
        // Mutate the original target and also rebind `o` after the yield but before resuming.
        o1.a = 4;
        o = o2;
        var r2 = it.next(3);
        r1.value === 0 && r1.done === false &&
        r2.value === 6 && r2.done === true &&
        o1.a === 6 && o2.a === 100
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_in_logical_or_assignment_rhs_captures_base_and_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var o1 = { a: 0, b: 1 };
        var o2 = { a: 0, b: 0 };
        var o = o1;
        var k = "a";

        function* g() {
          o[k] ||= (yield 0);
          return o1.a === 5 && o1.b === 1 && o2.a === 0 && o2.b === 0;
        }

        var it = g();
        var r1 = it.next();

        // Rebind both the base and the key after the yield.
        o = o2;
        k = "b";

        var r2 = it.next(5);

        r1.value === 0 && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_star_in_mul_assignment_rhs_captures_old_value() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var x = 2;
        function* rhs() {
          yield 0;
          yield 1;
          return 3;
        }
        function* g() { return x *= (yield* rhs()); }
        var it = g();
        var r1 = it.next();
        x = 100; // mutate after the first delegated yield
        var r2 = it.next();
        x = 200; // mutate again after the second delegated yield
        var r3 = it.next();
        r1.value === 0 && r1.done === false &&
        r2.value === 1 && r2.done === false &&
        r3.value === 6 && r3.done === true &&
        x === 6
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_star_in_mul_assignment_rhs_captures_property_reference_and_old_value() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var o1 = { a: 2 };
        var o2 = { a: 100 };
        var o = o1;

        function* rhs() {
          yield 0;
          yield 1;
          return 3;
        }

        function* g() { return o.a *= (yield* rhs()); }
        var it = g();
        var r1 = it.next();

        // Mutate the original target and also rebind `o` after the first delegated yield.
        o1.a = 4;
        o = o2;

        var r2 = it.next();

        // Mutate again after the second delegated yield.
        o1.a = 5;
        o = o2;

        var r3 = it.next();

        r1.value === 0 && r1.done === false &&
        r2.value === 1 && r2.done === false &&
        r3.value === 6 && r3.done === true &&
        o1.a === 6 && o2.a === 100
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
