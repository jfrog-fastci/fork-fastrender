use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_yield_star_in_bitwise_or_assignment_rhs_captures_base_key_and_old_value_for_computed_member_bigint(
) {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var o1 = { a: 5n, b: 10n };
        var o2 = { a: 100n, b: 1000n };
        var o = o1;
        var k = "a";

        function* rhs() {
          yield "rhs1";
          yield "rhs2";
          return 3n;
        }

        function* g() { return o[k] |= (yield* rhs()); }

        var it = g();
        var r1 = it.next();

        // Mutate and rebind after the first delegated yield.
        o1.a = 50n;
        o = o2;
        k = "b";

        var r2 = it.next();

        // Mutate and rebind again after the second delegated yield.
        o1.a = 500n;
        o = o2;
        k = "b";

        var r3 = it.next();

        r1.value === "rhs1" && r1.done === false &&
        r2.value === "rhs2" && r2.done === false &&
        r3.value === 7n && r3.done === true &&
        // Must still target the original base/key pair and use the pre-yield old value (5n).
        o1.a === 7n && o1.b === 10n &&
        o2.a === 100n && o2.b === 1000n
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_star_in_unsigned_right_shift_assignment_rhs_captures_property_reference_and_old_value(
) {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var o1 = { a: -5 };
        var o2 = { a: 0 };
        var o = o1;

        function* rhs() {
          yield "rhs1";
          yield "rhs2";
          return 1;
        }

        function* g() { return o.a >>>= (yield* rhs()); }

        var it = g();
        var r1 = it.next();

        // Mutate the original target and also rebind `o` after the first delegated yield.
        o1.a = -10;
        o = o2;

        var r2 = it.next();

        // Mutate again after the second delegated yield.
        o1.a = -20;
        o = o2;

        var r3 = it.next();

        r1.value === "rhs1" && r1.done === false &&
        r2.value === "rhs2" && r2.done === false &&
        r3.value === 2147483645 && r3.done === true &&
        // Must still target the original base and use the pre-yield old value (-5).
        o1.a === 2147483645 && o2.a === 0
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_star_in_left_shift_assignment_rhs_uses_pre_yield_old_value_with_negative_bigint_shift(
) {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var x = 8n;

        function* rhs() {
          yield "rhs1";
          yield "rhs2";
          return -1n;
        }

        function* g() { return x <<= (yield* rhs()); }

        var it = g();
        var r1 = it.next();
        x = 100n; // mutate after first delegated yield
        var r2 = it.next();
        x = 200n; // mutate after second delegated yield
        var r3 = it.next();

        r1.value === "rhs1" && r1.done === false &&
        r2.value === "rhs2" && r2.done === false &&
        r3.value === 4n && r3.done === true &&
        // Must still use the pre-yield old value (8n).
        x === 4n
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_star_in_bitwise_and_assignment_rhs_uses_pre_yield_old_value_for_binding_bigint() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var x = 5n;

        function* rhs() {
          yield "rhs1";
          yield "rhs2";
          return 3n;
        }

        function* g() { return x &= (yield* rhs()); }

        var it = g();
        var r1 = it.next();
        x = 100n; // mutate after first delegated yield
        var r2 = it.next();
        x = 200n; // mutate after second delegated yield
        var r3 = it.next();

        r1.value === "rhs1" && r1.done === false &&
        r2.value === "rhs2" && r2.done === false &&
        r3.value === 1n && r3.done === true &&
        // Must still use the pre-yield old value (5n).
        x === 1n
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_star_in_bitwise_xor_assignment_rhs_captures_base_key_and_old_value_for_computed_member(
) {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var o1 = { a: 5, b: 10 };
        var o2 = { a: 100, b: 1000 };
        var o = o1;
        var k = "a";

        function* rhs() {
          yield "rhs1";
          yield "rhs2";
          return 3;
        }

        function* g() { return o[k] ^= (yield* rhs()); }

        var it = g();
        var r1 = it.next();

        // Mutate and rebind after the first delegated yield.
        o1.a = 50;
        o = o2;
        k = "b";

        var r2 = it.next();

        // Mutate and rebind again after the second delegated yield.
        o1.a = 500;
        o = o2;
        k = "b";

        var r3 = it.next();

        r1.value === "rhs1" && r1.done === false &&
        r2.value === "rhs2" && r2.done === false &&
        r3.value === 6 && r3.done === true &&
        // Must still target the original base/key pair and use the pre-yield old value (5).
        o1.a === 6 && o1.b === 10 &&
        o2.a === 100 && o2.b === 1000
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_star_in_signed_right_shift_assignment_rhs_captures_property_reference_and_old_value(
) {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var o1 = { a: -8 };
        var o2 = { a: 0 };
        var o = o1;

        function* rhs() {
          yield "rhs1";
          yield "rhs2";
          return 1;
        }

        function* g() { return o.a >>= (yield* rhs()); }

        var it = g();
        var r1 = it.next();

        // Mutate the original target and also rebind `o` after the first delegated yield.
        o1.a = -16;
        o = o2;

        var r2 = it.next();

        // Mutate again after the second delegated yield.
        o1.a = -32;
        o = o2;

        var r3 = it.next();

        r1.value === "rhs1" && r1.done === false &&
        r2.value === "rhs2" && r2.done === false &&
        r3.value === -4 && r3.done === true &&
        // Must still target the original base and use the pre-yield old value (-8).
        o1.a === -4 && o2.a === 0
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_star_in_right_shift_assignment_rhs_uses_pre_yield_old_value_with_negative_bigint_shift(
) {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var x = 8n;

        function* rhs() {
          yield "rhs1";
          yield "rhs2";
          return -1n;
        }

        function* g() { return x >>= (yield* rhs()); }

        var it = g();
        var r1 = it.next();
        x = 100n; // mutate after first delegated yield
        var r2 = it.next();
        x = 200n; // mutate after second delegated yield
        var r3 = it.next();

        r1.value === "rhs1" && r1.done === false &&
        r2.value === "rhs2" && r2.done === false &&
        r3.value === 16n && r3.done === true &&
        // Must still use the pre-yield old value (8n).
        x === 16n
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
