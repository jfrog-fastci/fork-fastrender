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
fn dropping_runtime_after_termination_discards_pending_promise_jobs() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut runtime = JsRuntime::new(vm, heap)?;

  // Terminate after enqueuing at least one Promise job.
  runtime.vm.set_budget(Budget {
    // Needs to be large enough to run `Promise.resolve().then(..)` before we hit the infinite loop.
    fuel: Some(50),
    deadline: None,
    check_time_every: 1,
  });

  let err = runtime
    .exec_script("Promise.resolve().then(() => 0); while (true) {}")
    .unwrap_err();
  match err {
    VmError::Termination(term) => assert_eq!(term.reason, TerminationReason::OutOfFuel),
    other => panic!("expected termination, got {other:?}"),
  }

  assert!(
    !runtime.vm.microtask_queue().is_empty(),
    "expected Promise.resolve().then(..) to enqueue at least one Promise job before termination"
  );

  // Regression test: dropping after termination must also discard pending jobs.
  drop(runtime);
  Ok(())
}
