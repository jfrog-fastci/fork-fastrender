use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_number(value: Value) -> f64 {
  let Value::Number(n) = value else {
    panic!("expected number, got {value:?}");
  };
  n
}

#[test]
fn promise_finally_uses_species_constructor_for_then_and_promise_resolve_fulfilled() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let called = rt.exec_script(
    r#"
      var called = 0;
      function C(executor) { called++; return new Promise(executor); }
      var ctor = {};
      Object.defineProperty(ctor, Symbol.species, { value: C });

      var p = Promise.resolve(1);
      p.constructor = ctor;

      p.finally(() => 0);
      called;
    "#,
  )?;
  assert_eq!(value_to_number(called), 1.0);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let called = rt.exec_script("called")?;
  assert_eq!(value_to_number(called), 2.0);
  Ok(())
}

#[test]
fn promise_finally_uses_species_constructor_for_then_and_promise_resolve_rejected() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let called = rt.exec_script(
    r#"
      var called = 0;
      function C(executor) { called++; return new Promise(executor); }
      var ctor = {};
      Object.defineProperty(ctor, Symbol.species, { value: C });

      var p = Promise.reject(1);
      p.constructor = ctor;

      p.finally(() => 0);
      called;
    "#,
  )?;
  assert_eq!(value_to_number(called), 1.0);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let called = rt.exec_script("called")?;
  assert_eq!(value_to_number(called), 2.0);
  Ok(())
}

