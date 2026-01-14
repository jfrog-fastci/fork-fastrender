use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_super_member_call_yield_in_args() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class B { m(x){ return this.v + x; } }
        class D extends B {
          constructor(){ super(); this.v = 1; }
          *gen(){ return super.m(yield 1); }
        }
        const it = new D().gen();
        const r1 = it.next();
        const r2 = it.next(41);
        r1.value === 1 && r1.done === false && r2.value === 42 && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_super_computed_member_call_yield_in_key_and_args() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class B { m(x){ return this.v + x; } }
        class D extends B {
          constructor(){ super(); this.v = 1; }
          *gen(){ return super[yield "m"](yield 1); }
        }
        const it = new D().gen();
        const r1 = it.next();
        const r2 = it.next("m");
        const r3 = it.next(41);
        r1.value === "m" && r1.done === false
          && r2.value === 1 && r2.done === false
          && r3.value === 42 && r3.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_super_computed_member_access_yield_in_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class B { get x(){ return this.v; } }
        class D extends B {
          constructor(){ super(); this.v = 42; }
          *gen(){ return super[yield "x"]; }
        }
        const it = new D().gen();
        const r1 = it.next();
        const r2 = it.next("x");
        r1.value === "x" && r1.done === false && r2.value === 42 && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_super_computed_member_assignment_yield_in_rhs() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class B { set x(v){ this._x = v; } }
        class D extends B {
          *gen(){
            super["x"] = yield 1;
            return this._x;
          }
        }
        const it = new D().gen();
        const r1 = it.next();
        const r2 = it.next(42);
        r1.value === 1 && r1.done === false && r2.value === 42 && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_super_computed_member_assignment_yield_in_key_and_rhs() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class B { set x(v){ this._x = v; } }
        class D extends B {
          *gen(){
            super[yield "x"] = yield 1;
            return this._x;
          }
        }
        const it = new D().gen();
        const r1 = it.next();
        const r2 = it.next("x");
        const r3 = it.next(42);
        r1.value === "x" && r1.done === false
          && r2.value === 1 && r2.done === false
          && r3.value === 42 && r3.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_super_computed_member_add_assignment_yield_in_key_and_rhs() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class B {
          get x(){ return this._x; }
          set x(v){ this._x = v; }
        }
        class D extends B {
          constructor(){ super(); this._x = 10; }
          *gen(){
            super[yield "x"] += yield 1;
            return this._x;
          }
        }
        const it = new D().gen();
        const r1 = it.next();
        const r2 = it.next("x");
        const r3 = it.next(32);
        r1.value === "x" && r1.done === false
          && r2.value === 1 && r2.done === false
          && r3.value === 42 && r3.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_super_computed_member_update_yield_in_key() {
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
          *gen(){
            let a = super[yield "x"]++;
            let b = ++super[yield "x"];
            return String(a) + "," + String(b) + "," + String(this._x);
          }
        }
        const it = new D().gen();
        const r1 = it.next();
        const r2 = it.next("x");
        const r3 = it.next("x");
        r1.value === "x" && r1.done === false
          && r2.value === "x" && r2.done === false
          && r3.value === "1,3,3" && r3.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_super_computed_member_decrement_yield_in_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class B {
          get x(){ return this._x; }
          set x(v){ this._x = v; }
        }
        class D extends B {
          constructor(){ super(); this._x = 3; }
          *gen(){
            let a = super[yield "x"]--;
            let b = --super[yield "x"];
            return String(a) + "," + String(b) + "," + String(this._x);
          }
        }
        const it = new D().gen();
        const r1 = it.next();
        const r2 = it.next("x");
        const r3 = it.next("x");
        r1.value === "x" && r1.done === false
          && r2.value === "x" && r2.done === false
          && r3.value === "3,1,1" && r3.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
