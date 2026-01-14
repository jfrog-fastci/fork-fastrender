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

#[test]
fn async_generator_super_member_call_yield_in_args() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = false;
      class B { m(x) { return this.v + x; } }
      class D extends B {
        constructor() { super(); this.v = 1; }
        async *gen() {
          return super.m(yield 1);
        }
      }
      async function f() {
        const it = new D().gen();
        const r1 = await it.next();
        const r2 = await it.next(41);
        return (
          r1.value === 1 && r1.done === false &&
          r2.value === 42 && r2.done === true
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

#[test]
fn async_generator_super_computed_member_call_yield_in_key_and_args() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = false;
      class B { m(x) { return this.v + x; } }
      class D extends B {
        constructor() { super(); this.v = 1; }
        async *gen() {
          return super[yield "m"](yield 1);
        }
      }
      async function f() {
        const it = new D().gen();
        const r1 = await it.next();
        const r2 = await it.next("m");
        const r3 = await it.next(41);
        return (
          r1.value === "m" && r1.done === false &&
          r2.value === 1 && r2.done === false &&
          r3.value === 42 && r3.done === true
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

#[test]
fn async_generator_super_computed_member_assignment_yield_in_key_and_rhs() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = false;
      class B { set x(v) { this._x = v; } }
      class D extends B {
        async *gen() {
          super[yield "x"] = yield 1;
          return this._x;
        }
      }
      async function f() {
        const it = new D().gen();
        const r1 = await it.next();
        const r2 = await it.next("x");
        const r3 = await it.next(42);
        return (
          r1.value === "x" && r1.done === false &&
          r2.value === 1 && r2.done === false &&
          r3.value === 42 && r3.done === true
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

#[test]
fn async_generator_super_computed_member_update_yield_in_key() -> Result<(), VmError> {
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
          let a = super[yield "x"]++;
          let b = ++super[yield "x"];
          return String(a) + "," + String(b) + "," + String(this._x);
        }
      }
      async function f() {
        const it = new D().gen();
        const r1 = await it.next();
        const r2 = await it.next("x");
        const r3 = await it.next("x");
        return (
          r1.value === "x" && r1.done === false &&
          r2.value === "x" && r2.done === false &&
          r3.value === "1,3,3" && r3.done === true
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

#[test]
fn async_generator_super_computed_member_access_yield_star_in_key() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = false;

      async function* keyIter() {
        // Force a `yield*` suspension before producing the final key.
        yield 0;
        return "x";
      }

      class B { get x() { return this.v; } }
      class D extends B {
        constructor() { super(); this.v = 42; }
        async *gen() {
          // `yield*` in the computed key should suspend and resume correctly, and the resulting key
          // should still be applied as a Super Reference.
          yield super[yield* keyIter()];
        }
      }

      async function f() {
        const it = new D().gen();
        const r0 = await it.next();
        const r1 = await it.next();
        const r2 = await it.next();
        return (
          r0.value === 0 && r0.done === false &&
          r1.value === 42 && r1.done === false &&
          r2.value === undefined && r2.done === true
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

#[test]
fn async_generator_super_computed_member_call_yield_star_in_key() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = false;

      async function* keyIter() {
        yield "key";
        return "m";
      }

      class B { m(x) { return this.v + x; } }
      class D extends B {
        constructor() { super(); this.v = 1; }
        async *gen() {
          return super[yield* keyIter()](41);
        }
      }

      async function f() {
        const it = new D().gen();
        const r0 = await it.next();
        const r1 = await it.next();
        return (
          r0.value === "key" && r0.done === false &&
          r1.value === 42 && r1.done === true
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

#[test]
fn async_generator_super_computed_member_assignment_yield_star_in_key_and_yield_in_rhs(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = false;

      async function* keyIter() {
        yield "key";
        return "x";
      }

      class B { set x(v) { this._x = v; } }
      class D extends B {
        async *gen() {
          super[yield* keyIter()] = yield 1;
          return this._x;
        }
      }

      async function f() {
        const it = new D().gen();
        const r0 = await it.next();
        const r1 = await it.next();
        const r2 = await it.next(42);
        return (
          r0.value === "key" && r0.done === false &&
          r1.value === 1 && r1.done === false &&
          r2.value === 42 && r2.done === true
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

#[test]
fn async_generator_optional_super_property_call_binds_this_across_yield_star_in_arg(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = false;

      async function* inner() { yield 0; return 1; }

      let inst;
      class B { m(x) { return this === inst && x === 1; } }
      class D extends B {
        async *gen() {
          return super.m?.(yield* inner());
        }
      }

      async function f() {
        inst = new D();
        const it = inst.gen();
        const r0 = await it.next();
        const r1 = await it.next();
        return (
          r0.value === 0 && r0.done === false &&
          r1.value === true && r1.done === true
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

#[test]
fn async_generator_optional_super_property_call_short_circuits_and_skips_yield_star_in_arg(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = false;
      var side = 0;

      async function* inner() { side++; yield "should-not-yield"; return 0; }

      class B {}
      class D extends B {
        async *gen() {
          return super.missing?.(yield* inner());
        }
      }

      async function f() {
        const it = (new D()).gen();
        const r = await it.next();
        return r.value === undefined && r.done === true && side === 0;
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
fn async_generator_optional_super_computed_member_call_short_circuits_and_skips_yield_star_in_arg(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = false;

      async function* key() { yield "key"; return "missing"; }
      async function* arg() { yield "should-not-yield"; return 0; }

      class B {}
      class D extends B {
        async *gen() {
          return super[yield* key()]?.(yield* arg());
        }
      }

      async function f() {
        const it = (new D()).gen();
        const r0 = await it.next();
        const r1 = await it.next();
        return (
          r0.value === "key" && r0.done === false &&
          r1.value === undefined && r1.done === true
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

#[test]
fn async_generator_optional_chain_on_super_property_skips_yield_star_in_computed_key(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = false;
      var side = 0;

      async function* innerGen() { yield "should-not-yield"; return "k"; }
      function inner() { side++; return innerGen(); }

      class B {}
      class D extends B {
        async *gen() {
          return super.missing?.[(yield* inner())];
        }
      }

      async function f() {
        const it = (new D()).gen();
        const r = await it.next();
        return r.value === undefined && r.done === true && side === 0;
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
fn async_generator_optional_chain_after_super_property_skips_yield_star_in_computed_key(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = false;
      var side = 0;

      async function* innerGen() { yield "should-not-yield"; return "k"; }
      function inner() { side++; return innerGen(); }

      class B {}
      class D extends B {
        async *gen() {
          // If `super.missing` is nullish, the entire optional chain should short-circuit without
          // evaluating any following member operations (including `yield*` in computed keys).
          return super.missing?.a[(yield* inner())];
        }
      }

      async function f() {
        const it = (new D()).gen();
        const r = await it.next();
        return r.value === undefined && r.done === true && side === 0;
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
fn async_generator_delete_super_computed_member_evaluates_key_and_to_property_key_before_reference_error(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = false;
      var side = 0;

      class C {
        async *del() {
          try {
            delete super[(yield (side = 1, "yielded"))];
            return "no";
          } catch (e) {
            return String(side) + ":" + e.name;
          }
        }
      }

      async function f() {
        const it = (new C()).del();
        const r1 = await it.next();
        const side1 = side;
        const key = { toString() { side = side + 1; return "m"; } };
        const r2 = await it.next(key);
        return (
          r1.value === "yielded" && r1.done === false &&
          side1 === 1 &&
          side === 2 &&
          r2.value === "2:ReferenceError" && r2.done === true
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
