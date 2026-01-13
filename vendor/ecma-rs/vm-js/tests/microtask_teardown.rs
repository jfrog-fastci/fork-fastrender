use vm_js::{Budget, TerminationReason};
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

#[test]
fn dropping_runtime_discards_pending_promise_jobs() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut runtime = JsRuntime::new(vm, heap)?;

  runtime.exec_script("Promise.resolve().then(() => 0);")?;
  assert!(
    !runtime.vm.microtask_queue().is_empty(),
    "expected Promise.resolve().then(..) to enqueue at least one Promise job"
  );

  // Regression test: dropping a runtime with pending jobs must not trigger `Job`'s debug-assert
  // about leaked persistent roots.
  drop(runtime);
  Ok(())
}

#[test]
fn termination_tears_down_pending_microtasks_and_async_continuations() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  // Async/await allocates more internal state than a simple Promise.then(); give it some headroom.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  let mut runtime = JsRuntime::new(vm, heap)?;

  let baseline_roots = runtime.heap.persistent_root_count();
  assert_eq!(runtime.vm.async_continuation_count(), 0);

  // Terminate after enqueuing at least one Promise job.
  runtime.vm.set_budget(Budget {
    // Needs to be large enough to create an async continuation and enqueue a Promise job before we
    // hit the infinite loop.
    fuel: Some(2_000),
    deadline: None,
    check_time_every: 1,
  });

  let err = runtime
    .exec_script("async function f() { await 0; } f(); while (true) {}")
    .unwrap_err();
  match err {
    VmError::Termination(term) => assert_eq!(term.reason, TerminationReason::OutOfFuel),
    other => panic!("expected termination, got {other:?}"),
  }

  assert!(
    runtime.vm.microtask_queue().is_empty(),
    "hard-stop termination should discard queued Promise jobs"
  );
  assert_eq!(
    runtime.vm.async_continuation_count(),
    0,
    "hard-stop termination should tear down suspended async continuations"
  );
  assert_eq!(
    runtime.heap.persistent_root_count(),
    baseline_roots,
    "hard-stop termination should not leak persistent roots"
  );

  // Dropping the runtime should not panic from debug assertions about leaked Job roots.
  drop(runtime);
  Ok(())
}

#[test]
fn teardown_microtasks_aborts_async_continuations() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  // Async/await allocates more internal state than a simple Promise.then(); give it some headroom.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  let mut runtime = JsRuntime::new(vm, heap)?;

  let baseline_roots = runtime.heap.persistent_root_count();
  assert_eq!(runtime.vm.async_continuation_count(), 0);

  // Create an async continuation that will be resumed by Promise jobs (microtasks).
  runtime.exec_script("async function f() { await 0; } f();")?;
  assert!(
    runtime.vm.async_continuation_count() > 0,
    "expected async function to suspend and store an async continuation"
  );
  assert!(
    runtime.heap.persistent_root_count() > baseline_roots,
    "expected async continuation to allocate persistent roots"
  );

  runtime.teardown_microtasks();

  assert_eq!(
    runtime.vm.async_continuation_count(),
    0,
    "teardown_microtasks should tear down in-progress async continuations"
  );
  assert_eq!(
    runtime.heap.persistent_root_count(),
    baseline_roots,
    "expected teardown_microtasks to restore baseline persistent root count"
  );

  Ok(())
}
