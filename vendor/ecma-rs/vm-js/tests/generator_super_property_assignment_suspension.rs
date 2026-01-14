use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_super_property_assignment_survives_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class B { set x(v){ this._x = v; } }
        class D extends B {
          *gen(){
            super.x = yield 1;
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
fn generator_super_property_add_assignment_survives_yield() {
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
            super.x += yield 1;
            return this._x;
          }
        }
        const it = new D().gen();
        const r1 = it.next();
        const r2 = it.next(32);
        r1.value === 1 && r1.done === false && r2.value === 42 && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_super_computed_property_assignment_survives_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class B { set x(v){ this._x = v; } }
        class D extends B {
          *gen(){
            super['x'] = yield 1;
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
fn generator_super_computed_property_add_assignment_survives_yield() {
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
            super['x'] += yield 1;
            return this._x;
          }
        }
        const it = new D().gen();
        const r1 = it.next();
        const r2 = it.next(32);
        r1.value === 1 && r1.done === false && r2.value === 42 && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_super_computed_property_key_yield_survives_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class B { set x(v){ this._x = v; } }
        class D extends B {
          *gen(){
            super[yield 'key'] = 42;
            return this._x;
          }
        }
        const it = new D().gen();
        const r1 = it.next();
        const r2 = it.next('x');
        r1.value === 'key' && r1.done === false && r2.value === 42 && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
