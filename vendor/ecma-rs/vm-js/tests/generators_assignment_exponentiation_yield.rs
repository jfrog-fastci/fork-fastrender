use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn exponentiation_assignment_on_binding_uses_pre_yield_old_value() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let v = rt.exec_script(
    r#"
      var x = 2;
      function* g(){ return x **= (yield 0); }
      var it = g();
      var r0 = it.next();
      x = 10; // mutate after yield; should not affect captured old value
      var r = it.next(3);
      r0.value === 0 && r0.done === false &&
      r.done === true && r.value === 8 && x === 8
    "#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn exponentiation_assignment_on_property_captures_base_and_key_before_yield() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let v = rt.exec_script(
    r#"
      var o1 = { a: 2 };
      var o2 = { a: 10 };
      var o = o1;
      function* g(){ return o.a **= (yield 0); }
      var it = g();
      var r0 = it.next();
      o = o2;
      var r = it.next(3);
      r0.value === 0 && r0.done === false &&
      r.done === true && r.value === 8 && o1.a === 8 && o2.a === 10
    "#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn exponentiation_assignment_on_property_uses_pre_yield_old_value() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let v = rt.exec_script(
    r#"
      var o = { a: 2 };
      function* g(){ return o.a **= (yield 0); }
      var it = g();
      var r0 = it.next();
      o.a = 10; // mutate after yield; should not affect captured old value
      var r = it.next(3);
      r0.value === 0 && r0.done === false &&
      r.done === true && r.value === 8 && o.a === 8
    "#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn exponentiation_assignment_on_computed_property_captures_base_and_key_before_yield() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let v = rt.exec_script(
    r#"
      var o1 = { a: 2, b: 3 };
      var o2 = { a: 10, b: 100 };
      var o = o1;
      var k = "a";
      function* g(){ return o[k] **= (yield 0); }
      var it = g();
      var r0 = it.next();
      o = o2;
      k = "b";
      var r = it.next(3);
      r0.value === 0 && r0.done === false &&
      r.done === true && r.value === 8 &&
      o1.a === 8 && o1.b === 3 &&
      o2.a === 10 && o2.b === 100
    "#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn exponentiation_assignment_on_binding_uses_pre_yield_old_value_across_yield_star() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let v = rt.exec_script(
    r#"
      var x = 2;
      function* rhs() {
        yield 0;
        yield 1;
        return 3;
      }
      function* g(){ return x **= (yield* rhs()); }
      var it = g();
      var r1 = it.next();
      x = 10; // mutate after first delegated yield
      var r2 = it.next();
      x = 100; // mutate after second delegated yield
      var r3 = it.next();
      r1.value === 0 && r1.done === false &&
      r2.value === 1 && r2.done === false &&
      r3.done === true && r3.value === 8 && x === 8
    "#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn exponentiation_assignment_on_property_captures_reference_and_old_value_across_yield_star() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let v = rt.exec_script(
    r#"
      var o1 = { a: 2 };
      var o2 = { a: 10 };
      var o = o1;

      function* rhs() {
        yield 0;
        yield 1;
        return 3;
      }

      function* g(){ return o.a **= (yield* rhs()); }
      var it = g();
      var r1 = it.next();

      // Mutate the original target and also rebind the base after the first delegated yield.
      o1.a = 4;
      o = o2;

      var r2 = it.next();

      // Mutate again after the second delegated yield.
      o1.a = 5;
      o = o2;

      var r3 = it.next();

      r1.value === 0 && r1.done === false &&
      r2.value === 1 && r2.done === false &&
      r3.done === true && r3.value === 8 &&
      // Must still target the original base and use the pre-yield old value (2).
      o1.a === 8 && o2.a === 10
    "#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn exponentiation_assignment_on_computed_property_captures_base_key_and_old_value_across_yield_star() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let v = rt.exec_script(
    r#"
      var o1 = { a: 2, b: 3 };
      var o2 = { a: 10, b: 100 };
      var o = o1;
      var k = "a";

      function* rhs() {
        yield 0;
        yield 1;
        return 3;
      }

      function* g(){ return o[k] **= (yield* rhs()); }
      var it = g();
      var r1 = it.next();

      // Mutate and rebind base/key after the first delegated yield.
      o1.a = 4;
      o = o2;
      k = "b";

      var r2 = it.next();

      // Mutate again after the second delegated yield.
      o1.a = 5;
      o = o2;
      k = "b";

      var r3 = it.next();

      r1.value === 0 && r1.done === false &&
      r2.value === 1 && r2.done === false &&
      r3.done === true && r3.value === 8 &&
      // Must still target the original base/key pair and use the pre-yield old value (2).
      o1.a === 8 && o1.b === 3 &&
      o2.a === 10 && o2.b === 100
    "#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn exponentiation_assignment_on_super_property_captures_base_and_old_value_across_yield_star(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let v = rt.exec_script(
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
          const r = (super.x **= (yield* rhs()));
          return r === 8 && this._x === 8 && log.join(",") === "get1,set1:8";
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
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn exponentiation_assignment_on_super_computed_property_captures_base_key_and_old_value_across_yield_star(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let v = rt.exec_script(
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
          const r = (super[k] **= (yield* rhs()));
          return r === 8 &&
            this._x === 8 &&
            this._y === 10 &&
            log.join(",") === "get1x,set1x:8";
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
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn exponentiation_assignment_on_private_field_captures_old_value_across_yield_star() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let v = rt.exec_script(
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
          const r = (this.#x **= (yield* rhs()));
          return r === 8 && this.#x === 8;
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
      C.getX() === 8
    "#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}
