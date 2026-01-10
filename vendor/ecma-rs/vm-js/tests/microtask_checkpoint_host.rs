use vm_js::{
  GcObject, Heap, HeapLimits, JsRuntime, Scope, Value, Vm, VmError, VmHost, VmHostHooks, VmOptions,
};

#[derive(Default)]
struct Host {
  counter: u32,
}

fn inc_host_counter(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Some(host) = host.as_any_mut().downcast_mut::<Host>() else {
    return Err(VmError::Unimplemented("inc_host_counter expected Host"));
  };
  host.counter += 1;
  Ok(Value::Undefined)
}

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn microtask_checkpoint_with_host_threads_vmhost_into_promise_jobs() -> Result<(), VmError> {
  let mut rt = new_runtime();
  rt.register_global_native_function("inc", inc_host_counter, 0)?;

  let mut host = Host::default();

  // Scheduling a Promise reaction should enqueue a job into the VM-owned microtask queue.
  rt.exec_script_with_host(&mut host, "Promise.resolve().then(inc);")?;
  assert_eq!(host.counter, 0);

  // Drain the microtask queue with the same host: the Promise job should call `inc`, which should
  // have access to the embedder host context.
  rt
    .vm
    .perform_microtask_checkpoint_with_host(&mut host, &mut rt.heap)?;

  assert_eq!(host.counter, 1);
  Ok(())
}

#[test]
fn microtask_checkpoint_without_host_uses_dummy_host_context() -> Result<(), VmError> {
  let mut rt = new_runtime();
  rt.register_global_native_function("inc", inc_host_counter, 0)?;

  let mut host = Host::default();

  // Schedule a Promise job that will call into the native `inc` binding.
  rt.exec_script_with_host(&mut host, "Promise.resolve().then(inc);")?;
  assert_eq!(host.counter, 0);

  // The legacy checkpoint API uses a dummy host context (`()`), so `inc` should fail to downcast
  // and the microtask checkpoint should surface that error.
  let err = rt.vm.perform_microtask_checkpoint(&mut rt.heap).unwrap_err();
  assert!(matches!(
    err,
    VmError::Unimplemented("inc_host_counter expected Host")
  ));

  assert_eq!(host.counter, 0);
  Ok(())
}

