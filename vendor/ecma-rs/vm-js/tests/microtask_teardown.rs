use vm_js::{Heap, HeapLimits, JsRuntime, Vm, VmError, VmOptions};

#[test]
fn teardown_microtasks_discards_pending_promise_jobs() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut runtime = JsRuntime::new(vm, heap)?;

  runtime.exec_script("Promise.resolve().then(() => 0);")?;
  assert!(
    !runtime.vm.microtask_queue().is_empty(),
    "expected Promise.resolve().then(..) to enqueue at least one Promise job"
  );

  runtime.teardown_microtasks();
  assert!(runtime.vm.microtask_queue().is_empty());

  // Dropping the runtime should not panic from debug assertions about leaked Job roots.
  drop(runtime);
  Ok(())
}

