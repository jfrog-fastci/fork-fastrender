use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Async generators use Promises for `next()` results and can allocate more than simple sync tests.
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn async_generator_super_member_call_with_await_in_args() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = false;
      class B { m(x) { return this.v + x; } }
      class D extends B {
        constructor() { super(); this.v = 1; }
        async *gen() {
          yield super.m(await Promise.resolve(1));
        }
      }
      async function f() {
        const it = new D().gen();
        const r1 = await it.next();
        const r2 = await it.next();
        return r1.value === 2 && r1.done === false && r2.value === undefined && r2.done === true;
      }
      f().then(v => out = v);
    "#,
  )?;

  assert_eq!(rt.exec_script("out")?, Value::Bool(false));
  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;
  assert_eq!(rt.exec_script("out")?, Value::Bool(true));
  Ok(())
}

#[test]
fn async_generator_super_computed_member_call_with_await_in_key_and_args() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = false;
      class B { m(x) { return this.v + x; } }
      class D extends B {
        constructor() { super(); this.v = 1; }
        async *gen() {
          yield super[await Promise.resolve("m")](await Promise.resolve(1));
        }
      }
      async function f() {
        const it = new D().gen();
        const r1 = await it.next();
        const r2 = await it.next();
        return r1.value === 2 && r1.done === false && r2.value === undefined && r2.done === true;
      }
      f().then(v => out = v);
    "#,
  )?;

  assert_eq!(rt.exec_script("out")?, Value::Bool(false));
  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;
  assert_eq!(rt.exec_script("out")?, Value::Bool(true));
  Ok(())
}

#[test]
fn async_generator_super_property_assignment_and_update_with_await_in_key() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = false;
      class B {
        get x() { return this._x; }
        set x(v) { this._x = v; }
      }
      class D extends B {
        constructor() { super(); this._x = 1; }
        async *gen() {
          super.x = await Promise.resolve(2);
          yield this._x;
          const old = super[await Promise.resolve("x")]++;
          yield old;
          yield this._x;
        }
      }
      async function f() {
        const it = new D().gen();
        const r1 = await it.next();
        const r2 = await it.next();
        const r3 = await it.next();
        const r4 = await it.next();
        return (
          r1.value === 2 && r1.done === false &&
          r2.value === 2 && r2.done === false &&
          r3.value === 3 && r3.done === false &&
          r4.value === undefined && r4.done === true
        );
      }
      f().then(v => out = v);
    "#,
  )?;

  assert_eq!(rt.exec_script("out")?, Value::Bool(false));
  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;
  assert_eq!(rt.exec_script("out")?, Value::Bool(true));
  Ok(())
}

