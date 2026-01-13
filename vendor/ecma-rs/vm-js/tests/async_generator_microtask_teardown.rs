use vm_js::{Budget, Heap, HeapLimits, JsRuntime, TerminationReason, Vm, VmError, VmOptions};

mod _async_generator_support;

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Async generator tests allocate Promise/job machinery; use a slightly larger heap than the
  // minimal 1MiB used by some unit tests to avoid spurious OOMs as builtin surface area grows.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap).expect("create runtime")
}

#[test]
fn teardown_microtasks_with_pending_async_generator_resume_jobs_does_not_leak_roots(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !_async_generator_support::supports_async_generators(&mut rt)? {
    return Ok(());
  }

  let baseline_roots = rt.heap.persistent_root_count();

  rt.exec_script(
    r#"
      async function* g() { yield 1; }
      const it = g();
      // Enqueue the resume job but don't run a microtask checkpoint.
      it.next();
    "#,
  )?;

  assert!(
    !rt.vm.microtask_queue().is_empty(),
    "expected async generator .next() to enqueue at least one Promise job"
  );

  rt.teardown_microtasks();
  assert!(
    rt.vm.microtask_queue().is_empty(),
    "expected teardown_microtasks to discard queued Promise jobs"
  );
  assert_eq!(
    rt.heap.persistent_root_count(),
    baseline_roots,
    "expected teardown_microtasks to restore baseline persistent root count"
  );

  Ok(())
}

#[test]
fn termination_tears_down_pending_async_generator_jobs() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !_async_generator_support::supports_async_generators(&mut rt)? {
    return Ok(());
  }

  let baseline_roots = rt.heap.persistent_root_count();

  // Terminate after enqueuing at least one async generator Promise job.
  rt.vm.set_budget(Budget {
    // Needs to be large enough to run `g().next()` before we hit the infinite loop.
    fuel: Some(1000),
    deadline: None,
    check_time_every: 1,
  });

  let err = rt
    .exec_script(
      r#"
        async function* g() { yield 1; }
        g().next();
        while (true) {}
      "#,
    )
    .unwrap_err();
  match err {
    VmError::Termination(term) => assert_eq!(term.reason, TerminationReason::OutOfFuel),
    other => panic!("expected termination, got {other:?}"),
  }

  // Hard-stop termination should tear down any queued jobs so we don't leak persistent roots.
  assert!(
    rt.vm.microtask_queue().is_empty(),
    "hard-stop termination should discard queued Promise jobs"
  );
  assert_eq!(
    rt.heap.persistent_root_count(),
    baseline_roots,
    "hard-stop termination should not leak persistent roots"
  );

  Ok(())
}
