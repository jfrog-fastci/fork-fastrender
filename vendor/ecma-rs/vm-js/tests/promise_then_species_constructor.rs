use vm_js::{Heap, HeapLimits, JsRuntime, MicrotaskQueue, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn then_uses_constructor_species_and_invokes_it() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let mut host = ();
  let mut hooks = MicrotaskQueue::new();

  let value = rt.exec_script_with_host_and_hooks(
    &mut host,
    &mut hooks,
    r#"
      var called = 0;
      function C(executor) {
        called++;
        return new Promise(executor);
      }
      var ctor = {};
      Object.defineProperty(ctor, Symbol.species, { value: C });
      var p = Promise.resolve(1);
      p.constructor = ctor;
      p.then(() => {});
      called;
    "#,
  )?;

  // NewPromiseCapability calls the species constructor synchronously.
  assert_eq!(value, Value::Number(1.0));

  // Discard any queued Promise jobs so their persistent roots don't leak into the next test.
  hooks.cancel_all(&mut rt);
  Ok(())
}

#[test]
fn then_throws_if_constructor_is_not_object() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let mut host = ();
  let mut hooks = MicrotaskQueue::new();

  let value = rt.exec_script_with_host_and_hooks(
    &mut host,
    &mut hooks,
    r#"
      var p = Promise.resolve(1);
      p.constructor = 1;
      var threw = false;
      try {
        p.then(() => {});
      } catch (e) {
        threw = e && e.name === "TypeError";
      }
      threw;
    "#,
  )?;

  assert_eq!(value, Value::Bool(true));
  hooks.cancel_all(&mut rt);
  Ok(())
}

#[test]
fn then_throws_if_species_is_not_constructor() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let mut host = ();
  let mut hooks = MicrotaskQueue::new();

  let value = rt.exec_script_with_host_and_hooks(
    &mut host,
    &mut hooks,
    r#"
      var ctor = {};
      Object.defineProperty(ctor, Symbol.species, { value: 1 });
      var p = Promise.resolve(1);
      p.constructor = ctor;
      var threw = false;
      try {
        p.then(() => {});
      } catch (e) {
        threw = e && e.name === "TypeError";
      }
      threw;
    "#,
  )?;

  assert_eq!(value, Value::Bool(true));
  hooks.cancel_all(&mut rt);
  Ok(())
}

#[test]
fn then_falls_back_to_default_constructor_if_species_is_null() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let mut host = ();
  let mut hooks = MicrotaskQueue::new();

  let value = rt.exec_script_with_host_and_hooks(
    &mut host,
    &mut hooks,
    r#"
      var called = 0;
      function C(executor) {
        called++;
        return new Promise(executor);
      }
      var ctor = {};
      Object.defineProperty(ctor, Symbol.species, { value: null });
      var p = Promise.resolve(1);
      p.constructor = ctor;
      p.then(() => {});
      called;
    "#,
  )?;

  assert_eq!(value, Value::Number(0.0));
  hooks.cancel_all(&mut rt);
  Ok(())
}

