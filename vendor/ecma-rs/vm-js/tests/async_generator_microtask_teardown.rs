use vm_js::{Budget, Heap, HeapLimits, JsRuntime, PropertyKey, TerminationReason, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Async generator tests allocate Promise/job machinery; use a slightly larger heap than the
  // minimal 1MiB used by some unit tests to avoid spurious OOMs as builtin surface area grows.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap).expect("create runtime")
}

fn is_unimplemented_async_generator_error(rt: &mut JsRuntime, err: &VmError) -> Result<bool, VmError> {
  match err {
    VmError::Unimplemented(msg) if msg.contains("async generator functions") => return Ok(true),
    _ => {}
  }

  let Some(thrown) = err.thrown_value() else {
    return Ok(false);
  };
  let Value::Object(err_obj) = thrown else {
    return Ok(false);
  };

  // vm-js historically surfaced async generator support as a throwable SyntaxError (feature-detectable
  // via try/catch) so tests can land before full semantics are implemented.
  let syntax_error_proto = rt.realm().intrinsics().syntax_error_prototype();
  if rt.heap().object_prototype(err_obj)? != Some(syntax_error_proto) {
    return Ok(false);
  }

  let mut scope = rt.heap_mut().scope();
  scope.push_root(Value::Object(err_obj))?;

  let message_key = PropertyKey::from_string(scope.alloc_string("message")?);
  let Some(Value::String(message_s)) =
    scope.heap().object_get_own_data_property_value(err_obj, &message_key)?
  else {
    return Ok(false);
  };

  let message = scope.heap().get_string(message_s)?.to_utf8_lossy();
  Ok(message == "async generator functions")
}

fn feature_detect_async_generators(rt: &mut JsRuntime) -> Result<bool, VmError> {
  // Parse-level support for `async function*` isn't sufficient: vm-js can accept the syntax and
  // still surface `VmError::Unimplemented` once the generator is actually executed. Probe a minimal
  // `.next()` call so tests only activate when core async generator machinery exists.
  match rt.exec_script(
    r#"
      async function* __ag_support() { yield 1; }
      __ag_support().next();
    "#,
  ) {
    Ok(_) => {
      // Avoid leaking Promise jobs into subsequent assertions.
      rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;
      Ok(true)
    }
    Err(err) if is_unimplemented_async_generator_error(rt, &err)? => Ok(false),
    Err(err) => Err(err),
  }
}

#[test]
fn teardown_microtasks_with_pending_async_generator_resume_jobs_does_not_leak_roots() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !feature_detect_async_generators(&mut rt)? {
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
  if !feature_detect_async_generators(&mut rt)? {
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
