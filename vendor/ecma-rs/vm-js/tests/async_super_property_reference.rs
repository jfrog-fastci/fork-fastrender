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

#[test]
fn async_super_computed_member_update_expressions_across_await() -> Result<(), VmError> {
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
        async m() {
          let a = super[await Promise.resolve("x")]++;
          let b = ++super[await Promise.resolve("x")];
          return a + "," + b + "," + this._x;
        }
      }
      async function f() {
        return await new D().m();
      }
      f().then(v => out = String(v));
    "#,
  )?;

  // No microtasks run yet.
  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "1,3,3");

  Ok(())
}

#[test]
fn async_super_computed_member_assignment_in_derived_ctor_arrow_uses_get_this_binding() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = "";
      async function f() {
        let log1 = "";
        class B {
          constructor() { this._x = 0; }
          set x(v) { this._x = v; }
          get x() { return this._x; }
        }

        // Before `super()`: must throw a ReferenceError *before* evaluating the computed key
        // expression (so the await/thenable must not run).
        class D1 extends B {
          constructor() {
            let thenable = { get then() { log1 += "T"; return (resolve) => resolve("x"); } };
            let p = (async () => { super[await thenable] = 1; return "ok"; })();
            super();
            this.p = p;
          }
        }

        let d1 = new D1();
        let r1;
        try { r1 = await d1.p; } catch (e) { r1 = e.name; }

        // After `super()`: should work across await even when `this` is represented via a
        // DerivedConstructorState cell.
        let log2 = "";
        class D2 extends B {
          constructor() {
            super();
            let thenable = { get then() { log2 += "T"; return (resolve) => resolve("x"); } };
            this._x = 0;
            this.p = (async () => { super[await thenable] = 1; return this._x; })();
          }
        }
        let d2 = new D2();
        let r2 = await d2.p;

        return r1 + ":" + log1 + "," + r2 + ":" + log2;
      }
      f().then(v => out = String(v));
    "#,
  )?;

  // No microtasks run yet.
  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "ReferenceError:,1:T");

  Ok(())
}

#[test]
fn async_super_computed_member_update_in_derived_ctor_arrow_uses_get_this_binding() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = "";
      async function f() {
        let log1 = "";
        class B {
          constructor() { this._x = 1; }
          set x(v) { this._x = v; }
          get x() { return this._x; }
        }

        // Before `super()`: must throw a ReferenceError *before* evaluating the computed key
        // expression (so the await/thenable must not run).
        class D1 extends B {
          constructor() {
            let thenable = { get then() { log1 += "T"; return (resolve) => resolve("x"); } };
            let p = (async () => { super[await thenable]++; return "ok"; })();
            super();
            this.p = p;
          }
        }

        let d1 = new D1();
        let r1;
        try { r1 = await d1.p; } catch (e) { r1 = e.name; }

        // After `super()`: update should operate on the actual receiver across await.
        let log2 = "";
        class D2 extends B {
          constructor() {
            super();
            let thenable = { get then() { log2 += "T"; return (resolve) => resolve("x"); } };
            this.p = (async () => {
              let a = super[await thenable]++;
              return String(a) + ":" + String(this._x);
            })();
          }
        }
        let d2 = new D2();
        let r2 = await d2.p;

        return r1 + ":" + log1 + "," + r2 + ":" + log2;
      }
      f().then(v => out = String(v));
    "#,
  )?;

  // No microtasks run yet.
  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "ReferenceError:,1:2:T");

  Ok(())
}
