use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn async_super_member_and_computed_member_calls() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = '';
      class B {
        m(x) { return this.v + x; }
      }
      class D extends B {
        constructor() { super(); this.v = 1; }
        async m() {
          // `super.prop(...)` with `await` in the argument list.
          return super.m(await Promise.resolve(1));
        }
        async n() {
          // `super[expr](...)` with `await` in the computed key expression.
          return super[await Promise.resolve("m")](await Promise.resolve(1));
        }
      }
      async function f() {
        let a = await new D().m();
        let b = await new D().n();
        return a + "," + b;
      }
      f().then(v => out = String(v));
    "#,
  )?;

  // No microtasks run yet.
  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "2,2");

  Ok(())
}

#[test]
fn async_super_property_assignments_across_await() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = '';
      class B {
        get x() { return this._x; }
        set x(v) { this._x = v; }
      }
      class D extends B {
        constructor() { super(); this._x = 1; }
        async assign() {
          super.x = await Promise.resolve(2);
          return this._x;
        }
        async add() {
          super.x += await Promise.resolve(3);
          return this._x;
        }
        async computed() {
          super[await Promise.resolve("x")] = await Promise.resolve(7);
          return this._x;
        }
      }
      async function f() {
        let d = new D();
        let a = await d.assign();
        let b = await d.add();
        let c = await d.computed();
        return a + "," + b + "," + c;
      }
      f().then(v => out = String(v));
    "#,
  )?;

  // No microtasks run yet.
  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "2,5,7");

  Ok(())
}
