use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn async_generator_object_literal_method_can_call_super_with_await_in_args() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = false;

      var proto = {
        m(x) { return this.v + x; }
      };

      var obj = {
        __proto__: proto,
        v: 1,
        async *gen() {
          yield super.m(await Promise.resolve(1));
        }
      };

      async function f() {
        const it = obj.gen();
        const r1 = await it.next();
        const r2 = await it.next();
        return (
          r1.value === 2 && r1.done === false &&
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
fn async_generator_object_literal_method_can_call_super_computed_with_yield_star_in_key(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = false;

      async function* keyIter() { yield "key"; return "m"; }

      var proto = {
        m(x) { return this.v + x; }
      };

      var obj = {
        __proto__: proto,
        v: 1,
        async *gen() {
          return super[yield* keyIter()](41);
        }
      };

      async function f() {
        const it = obj.gen();
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
fn async_generator_object_literal_arrow_captures_lexical_super_and_observes_dynamic_prototype_across_yield(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = false;

      var proto1 = { x: "p1" };
      var proto2 = { x: "p2" };

      var obj = {
        __proto__: proto1,
        async *gen() {
          const f = () => super.x;
          // Suspend so user code can mutate the home object's prototype.
          yield 0;
          return f();
        }
      };

      async function f() {
        const it = obj.gen();
        const r0 = await it.next();
        Object.setPrototypeOf(obj, proto2);
        const r1 = await it.next();
        return (
          r0.value === 0 && r0.done === false &&
          r1.value === "p2" && r1.done === true
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
fn async_generator_object_literal_arrow_captures_lexical_super_and_observes_dynamic_prototype_across_yield_star(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = false;

      async function* inner() { yield "yielded"; return 0; }

      var proto1 = { x: "p1" };
      var proto2 = { x: "p2" };

      var obj = {
        __proto__: proto1,
        async *gen() {
          const f = () => super.x;
          // Suspend via yield* so we cover YieldIteratorResult suspensions.
          yield* inner();
          return f();
        }
      };

      async function f() {
        const it = obj.gen();
        const r0 = await it.next();
        Object.setPrototypeOf(obj, proto2);
        const r1 = await it.next();
        return (
          r0.value === "yielded" && r0.done === false &&
          r1.value === "p2" && r1.done === true
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
fn async_generator_object_literal_arrow_captures_lexical_super_and_observes_dynamic_prototype_across_await(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = false;

      var proto1 = { x: "p1" };
      var proto2 = { x: "p2" };

      var obj = {
        __proto__: proto1,
        async *gen() {
          const f = () => super.x;
          // Suspend via await before calling `f`.
          await Promise.resolve(0);
          return f();
        }
      };

      async function f() {
        const it = obj.gen();
        const p = it.next();
        Object.setPrototypeOf(obj, proto2);
        const r0 = await p;
        return r0.value === "p2" && r0.done === true;
      }

      f().then(v => out = v);
    "#,
  )?;

  assert_eq!(rt.exec_script("out")?, Value::Bool(false));
  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;
  assert_eq!(rt.exec_script("out")?, Value::Bool(true));
  Ok(())
}
