use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Async generators + template object caching can allocate; keep the heap slightly larger to avoid
  // spurious OOMs.
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn async_generator_super_tagged_template_member_yield_in_substitution() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = false;

      class Base {
        tag(strings, x) { return this.prefix + strings[0] + x + strings[1]; }
      }
      class Derived extends Base {
        constructor() { super(); this.prefix = "P"; }
        async *gen() { return super.tag`a${yield 1}b`; }
      }

      async function f() {
        const it = (new Derived()).gen();
        const r0 = await it.next();
        const r1 = await it.next("Z");
        return (
          r0.value === 1 && r0.done === false &&
          r1.value === "PaZb" && r1.done === true
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
fn async_generator_super_tagged_template_computed_key_yield_star() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = false;

      async function* keyIter() { yield "key"; return "tag"; }

      class Base { tag(strings) { return this.prefix + strings[0]; } }
      class Derived extends Base {
        constructor() { super(); this.prefix = "P"; }
        async *gen() { return super[yield* keyIter()]`x`; }
      }

      async function f() {
        const it = (new Derived()).gen();
        const r0 = await it.next();
        const r1 = await it.next();
        return (
          r0.value === "key" && r0.done === false &&
          r1.value === "Px" && r1.done === true
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
fn async_generator_super_tagged_template_member_await_in_substitution() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = false;

      class Base {
        tag(strings, x) { return this.prefix + strings[0] + x + strings[1]; }
      }
      class Derived extends Base {
        constructor() { super(); this.prefix = "P"; }
        async *gen() { return super.tag`a${await Promise.resolve("Z")}b`; }
      }

      async function f() {
        const it = (new Derived()).gen();
        const r0 = await it.next();
        return r0.value === "PaZb" && r0.done === true;
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
fn async_generator_super_tagged_template_computed_key_can_await() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = false;

      class Base { tag(strings) { return this.prefix + strings[0]; } }
      class Derived extends Base {
        constructor() { super(); this.prefix = "P"; }
        async *gen() { return super[await Promise.resolve("tag")]`x`; }
      }

      async function f() {
        const it = (new Derived()).gen();
        const r0 = await it.next();
        return r0.value === "Px" && r0.done === true;
      }

      f().then(v => out = v);
    "#,
  )?;

  assert_eq!(rt.exec_script("out")?, Value::Bool(false));
  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;
  assert_eq!(rt.exec_script("out")?, Value::Bool(true));
  Ok(())
}

