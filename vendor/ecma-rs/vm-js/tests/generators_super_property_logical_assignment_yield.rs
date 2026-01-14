use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_super_logical_and_assignment_short_circuits_without_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class B {
          get x(){ return this._x; }
          set x(v){ this._x = v; }
        }
        class D extends B {
          constructor(){ super(); this._x = 0; }
          *gen() {
            // RHS contains a yield, but must not be evaluated because `super.x` is falsy.
            const r = (super.x &&= (yield 1));
            return r === 0 && this._x === 0;
          }
        }
        const it = new D().gen();
        const r1 = it.next();
        r1.done === true && r1.value === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_super_logical_or_assignment_short_circuits_without_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class B {
          get x(){ return this._x; }
          set x(v){ this._x = v; }
        }
        class D extends B {
          constructor(){ super(); this._x = 1; }
          *gen() {
            // RHS contains a yield, but must not be evaluated because `super.x` is truthy.
            const r = (super.x ||= (yield 1));
            return r === 1 && this._x === 1;
          }
        }
        const it = new D().gen();
        const r1 = it.next();
        r1.done === true && r1.value === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_super_nullish_coalescing_assignment_short_circuits_without_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class B {
          get x(){ return this._x; }
          set x(v){ this._x = v; }
        }
        class D extends B {
          constructor(){ super(); this._x = 0; }
          *gen() {
            // RHS contains a yield, but must not be evaluated because `super.x` is non-nullish.
            const r = (super.x ??= (yield 1));
            return r === 0 && this._x === 0;
          }
        }
        const it = new D().gen();
        const r1 = it.next();
        r1.done === true && r1.value === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_super_logical_or_assignment_captures_super_base_and_decision_across_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        const log = [];
        class B1 {
          get x(){ log.push("get1"); return this._x; }
          set x(v){ log.push("set1:" + v); this._x = v; }
        }
        class B2 {
          get x(){ log.push("get2"); return this._x; }
          set x(v){ log.push("set2:" + v); this._x = v; }
        }
        class D extends B1 {
          constructor(){ super(); this._x = 0; }
          *gen() {
            const r = (super.x ||= (yield 0));
            // If the engine incorrectly recomputes the super base or the should-assign decision
            // after resuming, the setter call (and/or the write itself) will differ.
            return r === 5 && this._x === 5 && log.join(",") === "get1,set1:5";
          }
        }

        const d = new D();
        const it = d.gen();
        const r1 = it.next();

        // Mutate the LHS value and super base after the yield but before resuming.
        // The assignment must still occur (decision was made before yielding) and target the
        // original super base.
        d._x = 1; // truthy now, but should not cancel the pending assignment
        Object.setPrototypeOf(D.prototype, B2.prototype);

        const r2 = it.next(5);

        r1.value === 0 && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_super_logical_and_assignment_captures_super_base_and_decision_across_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        const log = [];
        class B1 {
          get x(){ log.push("get1"); return this._x; }
          set x(v){ log.push("set1:" + v); this._x = v; }
        }
        class B2 {
          get x(){ log.push("get2"); return this._x; }
          set x(v){ log.push("set2:" + v); this._x = v; }
        }
        class D extends B1 {
          constructor(){ super(); this._x = 1; }
          *gen() {
            const r = (super.x &&= (yield 0));
            return r === 5 && this._x === 5 && log.join(",") === "get1,set1:5";
          }
        }

        const d = new D();
        const it = d.gen();
        const r1 = it.next();

        // Mutate the LHS value and super base after the yield but before resuming.
        // The assignment must still occur (decision was made before yielding) and target the
        // original super base.
        d._x = 0; // falsy now, but should not cancel the pending assignment
        Object.setPrototypeOf(D.prototype, B2.prototype);

        const r2 = it.next(5);

        r1.value === 0 && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_super_nullish_coalescing_assignment_captures_super_base_and_decision_across_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        const log = [];
        class B1 {
          get x(){ log.push("get1"); return this._x; }
          set x(v){ log.push("set1:" + v); this._x = v; }
        }
        class B2 {
          get x(){ log.push("get2"); return this._x; }
          set x(v){ log.push("set2:" + v); this._x = v; }
        }
        class D extends B1 {
          constructor(){ super(); this._x = null; }
          *gen() {
            const r = (super.x ??= (yield 0));
            return r === 5 && this._x === 5 && log.join(",") === "get1,set1:5";
          }
        }

        const d = new D();
        const it = d.gen();
        const r1 = it.next();

        // Mutate the LHS value and super base after the yield but before resuming.
        // The assignment must still occur (decision was made before yielding) and target the
        // original super base.
        d._x = 0; // non-nullish now, but should not cancel the pending assignment
        Object.setPrototypeOf(D.prototype, B2.prototype);

        const r2 = it.next(5);

        r1.value === 0 && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_super_logical_or_assignment_with_yield_in_computed_key_uses_updated_super_base_after_yield(
) {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        const log = [];
        class B1 {
          get x(){ log.push("get1"); return this._x; }
          set x(v){ log.push("set1:" + v); this._x = v; }
        }
        class B2 {
          get x(){ log.push("get2"); return this._x; }
          set x(v){ log.push("set2:" + v); this._x = v; }
        }
        class D extends B1 {
          constructor(){ super(); this._x = 0; }
          *gen() {
            const r = (super[(yield "k")] ||= 5);
            return r === 5 && this._x === 5 && log.join(",") === "get2,set2:5";
          }
        }

        const d = new D();
        const it = d.gen();
        const r1 = it.next();

        // `GetSuperBase` must run *after* the key-yield; changing the prototype chain here must
        // affect which getter/setter are used.
        Object.setPrototypeOf(D.prototype, B2.prototype);
        const r2 = it.next("x");

        r1.value === "k" && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_super_logical_or_assignment_with_yield_in_computed_key_short_circuits_rhs_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class B {
          get x(){ return this._x; }
          set x(v){ this._x = v; }
        }
        class D extends B {
          constructor(){ super(); this._x = 1; }
          *gen() {
            // Yield in the computed key expression happens first.
            // Because `super.x` is truthy, `||=` must short-circuit and never evaluate the RHS yield.
            super[(yield "k")] ||= (yield 0);
            return this._x;
          }
        }
        const it = new D().gen();
        const r1 = it.next();
        const r2 = it.next("x");
        r1.value === "k" && r1.done === false &&
        r2.value === 1 && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_super_logical_and_assignment_with_yield_in_computed_key_short_circuits_rhs_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class B {
          get x(){ return this._x; }
          set x(v){ this._x = v; }
        }
        class D extends B {
          constructor(){ super(); this._x = 0; }
          *gen() {
            // Yield in the computed key expression happens first.
            // Because `super.x` is falsy, `&&=` must short-circuit and never evaluate the RHS yield.
            super[(yield "k")] &&= (yield 0);
            return this._x;
          }
        }
        const it = new D().gen();
        const r1 = it.next();
        const r2 = it.next("x");
        r1.value === "k" && r1.done === false &&
        r2.value === 0 && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_super_nullish_coalescing_assignment_with_yield_in_computed_key_short_circuits_rhs_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class B {
          get x(){ return this._x; }
          set x(v){ this._x = v; }
        }
        class D extends B {
          constructor(){ super(); this._x = 0; }
          *gen() {
            // Yield in the computed key expression happens first.
            // Because `super.x` is non-nullish, `??=` must short-circuit and never evaluate the RHS yield.
            super[(yield "k")] ??= (yield 0);
            return this._x;
          }
        }
        const it = new D().gen();
        const r1 = it.next();
        const r2 = it.next("x");
        r1.value === "k" && r1.done === false &&
        r2.value === 0 && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_super_logical_or_assignment_with_yield_in_computed_key_and_rhs_captures_key_across_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        const log = [];
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
          constructor(){ super(); this._x = 0; this._y = 0; }
          *gen() {
            const r = (super[(yield "key", k)] ||= (yield 0));
            return r === 5 && this._x === 0 && this._y === 5 && log.join(",") === "get1y,set1y:5";
          }
        }

        const d = new D();
        const it = d.gen();
        const r1 = it.next();

        // Key is evaluated after resuming from the key-yield.
        k = "y";
        const r2 = it.next();

        // Mutate the super base, key, and LHS value after the RHS yield but before resuming.
        // The assignment must still happen (decision was made before yielding) and target the
        // original super base + key.
        d._y = 1;
        k = "x";
        Object.setPrototypeOf(D.prototype, B2.prototype);

        const r3 = it.next(5);

        r1.value === "key" && r1.done === false &&
        r2.value === 0 && r2.done === false &&
        r3.value === true && r3.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
