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

#[test]
fn generator_yield_star_in_mul_assignment_rhs_captures_base_key_and_old_value_for_computed_member() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var o1 = { a: 2, b: 10 };
        var o2 = { a: 100, b: 1000 };
        var o = o1;
        var k = "a";

        function* rhs() {
          yield "rhs1";
          yield "rhs2";
          return 3;
        }

        function* g() { return o[k] *= (yield* rhs()); }
        var it = g();
        var r1 = it.next();

        // Mutate the original target and also rebind the base/key after the first delegated yield.
        o1.a = 4;
        o = o2;
        k = "b";

        var r2 = it.next();

        // Mutate again after the second delegated yield.
        o1.a = 5;
        o = o2;
        k = "b";

        var r3 = it.next();

        r1.value === "rhs1" && r1.done === false &&
        r2.value === "rhs2" && r2.done === false &&
        r3.value === 6 && r3.done === true &&
        // Must still target the original base/key pair and use the pre-yield old value (2).
        o1.a === 6 && o1.b === 10 &&
        o2.a === 100 && o2.b === 1000
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_star_in_mul_assignment_rhs_captures_super_base_and_old_value() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        const log = [];

        function* rhs() {
          yield "rhs1";
          yield "rhs2";
          return 3;
        }

        class B1 {
          get x(){ log.push("get1"); return this._x; }
          set x(v){ log.push("set1:" + v); this._x = v; }
        }
        class B2 {
          get x(){ log.push("get2"); return this._x; }
          set x(v){ log.push("set2:" + v); this._x = v; }
        }

        class D extends B1 {
          constructor(){ super(); this._x = 2; }
          *gen() {
            const r = (super.x *= (yield* rhs()));
            return r === 6 && this._x === 6 && log.join(",") === "get1,set1:6";
          }
        }

        const d = new D();
        const it = d.gen();
        const r1 = it.next();

        // Mutate the old value and super base after the first delegated yield but before resuming.
        d._x = 100;
        Object.setPrototypeOf(D.prototype, B2.prototype);

        const r2 = it.next();

        // Mutate again after the second delegated yield.
        d._x = 200;
        Object.setPrototypeOf(D.prototype, B2.prototype);

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
fn generator_yield_star_in_mul_assignment_rhs_captures_super_base_key_and_old_value() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        const log = [];

        function* rhs() {
          yield "rhs1";
          yield "rhs2";
          return 3;
        }

        var k = "x";

        class B1 {
          get x(){ log.push("get1x"); return this._x; }
          set x(v){ log.push("set1x:" + v); this._x = v; }
          get y(){ log.push("get1y"); return this._y; }
          set y(v){ log.push("set1y:" + v); this._y = v; }
        }
        class B2 {
          get x(){ log.push("get2x"); return this._x; }
          set x(v){ log.push("set2x:" + v); this._x = v; }
          get y(){ log.push("get2y"); return this._y; }
          set y(v){ log.push("set2y:" + v); this._y = v; }
        }

        class D extends B1 {
          constructor(){ super(); this._x = 2; this._y = 10; }
          *gen() {
            const r = (super[k] *= (yield* rhs()));
            return r === 6 && this._x === 6 && this._y === 10 && log.join(",") === "get1x,set1x:6";
          }
        }

        const d = new D();
        const it = d.gen();
        const r1 = it.next();

        // Mutate the old value, key, and super base after the first delegated yield.
        d._x = 100;
        k = "y";
        Object.setPrototypeOf(D.prototype, B2.prototype);

        const r2 = it.next();

        // Mutate again after the second delegated yield.
        d._x = 200;

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
fn generator_yield_star_in_mul_assignment_rhs_captures_private_field_old_value() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* rhs() {
          yield "rhs1";
          yield "rhs2";
          return 3;
        }

        class C {
          static #x = 2;
          static getX(){ return this.#x; }
          static setX(v){ this.#x = v; }
          static *g(){
            const r = (this.#x *= (yield* rhs()));
            return r === 6 && this.#x === 6;
          }
        }

        const it = C.g();
        const r1 = it.next();

        // Mutate after the first delegated yield.
        C.setX(100);

        const r2 = it.next();

        // Mutate again after the second delegated yield.
        C.setX(200);

        const r3 = it.next();

        r1.done === false && r1.value === "rhs1" &&
        r2.done === false && r2.value === "rhs2" &&
        r3.done === true && r3.value === true &&
        C.getX() === 6
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_star_in_add_assignment_rhs_captures_old_value() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var x = 5;
        function* rhs() {
          yield "rhs1";
          yield "rhs2";
          return 7;
        }
        function* g() { return x += (yield* rhs()); }
        var it = g();
        var r1 = it.next();
        x = 100; // mutate after first delegated yield
        var r2 = it.next();
        x = 200; // mutate after second delegated yield
        var r3 = it.next();
        r1.value === "rhs1" && r1.done === false &&
        r2.value === "rhs2" && r2.done === false &&
        r3.value === 12 && r3.done === true &&
        x === 12
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_star_in_add_assignment_rhs_captures_property_reference_and_old_value_with_strings() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var o1 = { a: "hi" };
        var o2 = { a: "no" };
        var o = o1;

        function* rhs() {
          yield "rhs1";
          yield "rhs2";
          return "!";
        }

        function* g() { return o.a += (yield* rhs()); }
        var it = g();
        var r1 = it.next();

        // Mutate the original target and also rebind `o` after the first delegated yield.
        o1.a = "bye";
        o = o2;

        var r2 = it.next();

        // Mutate again after the second delegated yield.
        o1.a = "ciao";
        o = o2;

        var r3 = it.next();

        r1.value === "rhs1" && r1.done === false &&
        r2.value === "rhs2" && r2.done === false &&
        r3.value === "hi!" && r3.done === true &&
        o1.a === "hi!" && o2.a === "no"
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_star_in_add_assignment_rhs_captures_base_key_and_old_value_for_computed_member() {
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
          return 7;
        }

        function* g() { return o[k] += (yield* rhs()); }
        var it = g();
        var r1 = it.next();

        // Mutate the original target and also rebind the base/key after the first delegated yield.
        o1.a = 50;
        o = o2;
        k = "b";

        var r2 = it.next();

        // Mutate again after the second delegated yield.
        o1.a = 500;
        o = o2;
        k = "b";

        var r3 = it.next();

        r1.value === "rhs1" && r1.done === false &&
        r2.value === "rhs2" && r2.done === false &&
        r3.value === 12 && r3.done === true &&
        // Must still target the original base/key pair and use the pre-yield old value (5).
        o1.a === 12 && o1.b === 10 &&
        o2.a === 100 && o2.b === 1000
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_star_in_add_assignment_rhs_captures_super_base_and_old_value() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        const log = [];

        function* rhs() {
          yield "rhs1";
          yield "rhs2";
          return 7;
        }

        class B1 {
          get x(){ log.push("get1"); return this._x; }
          set x(v){ log.push("set1:" + v); this._x = v; }
        }
        class B2 {
          get x(){ log.push("get2"); return this._x; }
          set x(v){ log.push("set2:" + v); this._x = v; }
        }

        class D extends B1 {
          constructor(){ super(); this._x = 5; }
          *gen() {
            const r = (super.x += (yield* rhs()));
            return r === 12 && this._x === 12 && log.join(",") === "get1,set1:12";
          }
        }

        const d = new D();
        const it = d.gen();
        const r1 = it.next();

        // Mutate the old value and super base after the first delegated yield but before resuming.
        d._x = 50;
        Object.setPrototypeOf(D.prototype, B2.prototype);

        const r2 = it.next();

        // Mutate again after the second delegated yield.
        d._x = 500;
        Object.setPrototypeOf(D.prototype, B2.prototype);

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
fn generator_yield_star_in_add_assignment_rhs_captures_super_base_key_and_old_value() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        const log = [];

        function* rhs() {
          yield "rhs1";
          yield "rhs2";
          return 7;
        }

        var k = "x";

        class B1 {
          get x(){ log.push("get1x"); return this._x; }
          set x(v){ log.push("set1x:" + v); this._x = v; }
          get y(){ log.push("get1y"); return this._y; }
          set y(v){ log.push("set1y:" + v); this._y = v; }
        }
        class B2 {
          get x(){ log.push("get2x"); return this._x; }
          set x(v){ log.push("set2x:" + v); this._x = v; }
          get y(){ log.push("get2y"); return this._y; }
          set y(v){ log.push("set2y:" + v); this._y = v; }
        }

        class D extends B1 {
          constructor(){ super(); this._x = 5; this._y = 10; }
          *gen() {
            const r = (super[k] += (yield* rhs()));
            return r === 12 && this._x === 12 && this._y === 10 && log.join(",") === "get1x,set1x:12";
          }
        }

        const d = new D();
        const it = d.gen();
        const r1 = it.next();

        // Mutate the old value, key, and super base after the first delegated yield.
        d._x = 50;
        k = "y";
        Object.setPrototypeOf(D.prototype, B2.prototype);

        const r2 = it.next();

        // Mutate again after the second delegated yield.
        d._x = 500;

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
fn generator_yield_star_in_add_assignment_rhs_captures_private_field_old_value() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* rhs() {
          yield "rhs1";
          yield "rhs2";
          return 7;
        }

        class C {
          static #x = 5;
          static getX(){ return this.#x; }
          static setX(v){ this.#x = v; }
          static *g(){
            const r = (this.#x += (yield* rhs()));
            return r === 12 && this.#x === 12;
          }
        }

        const it = C.g();
        const r1 = it.next();

        // Mutate after the first delegated yield.
        C.setX(50);

        const r2 = it.next();

        // Mutate again after the second delegated yield.
        C.setX(500);

        const r3 = it.next();

        r1.done === false && r1.value === "rhs1" &&
        r2.done === false && r2.value === "rhs2" &&
        r3.done === true && r3.value === true &&
        C.getX() === 12
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_star_in_mul_assignment_rhs_uses_pre_yield_old_value_bigint() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var x = 2n;

        function* rhs() {
          yield "rhs1";
          yield "rhs2";
          return 3n;
        }

        function* g() { return x *= (yield* rhs()); }
        var it = g();
        var r1 = it.next();
        x = 100n; // mutate after first delegated yield
        var r2 = it.next();
        x = 200n; // mutate after second delegated yield
        var r3 = it.next();

        r1.value === "rhs1" && r1.done === false &&
        r2.value === "rhs2" && r2.done === false &&
        r3.value === 6n && r3.done === true &&
        x === 6n
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_star_in_add_assignment_rhs_captures_base_key_and_old_value_for_computed_member_bigint(
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
          return 7n;
        }

        function* g() { return o[k] += (yield* rhs()); }
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
        r3.value === 12n && r3.done === true &&
        // Must still target the original base/key pair and use the pre-yield old value (5n).
        o1.a === 12n && o1.b === 10n &&
        o2.a === 100n && o2.b === 1000n
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_star_in_div_assignment_rhs_uses_pre_yield_old_value_bigint() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var x = 5n;

        function* rhs() {
          yield "rhs1";
          yield "rhs2";
          return 2n;
        }

        function* g() { return x /= (yield* rhs()); }
        var it = g();
        var r1 = it.next();
        x = 100n; // mutate after first delegated yield
        var r2 = it.next();
        x = 200n; // mutate after second delegated yield
        var r3 = it.next();

        r1.value === "rhs1" && r1.done === false &&
        r2.value === "rhs2" && r2.done === false &&
        r3.value === 2n && r3.done === true &&
        // Must still use the pre-yield old value (5n).
        x === 2n
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_star_in_mul_assignment_cannot_mix_bigint_and_number() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var x = 5n;

        function* rhs() {
          yield "rhs1";
          yield "rhs2";
          return 2;
        }

        function* g() {
          try {
            x *= (yield* rhs());
            return false;
          } catch (e) {
            return (e && e.name === "TypeError") && x === 5n;
          }
        }

        var it = g();
        var r1 = it.next();
        var r2 = it.next();
        var r3 = it.next();

        r1.value === "rhs1" && r1.done === false &&
        r2.value === "rhs2" && r2.done === false &&
        r3.value === true && r3.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
