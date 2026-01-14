use vm_js::{Budget, Heap, HeapLimits, JsRuntime, TerminationReason, Vm, VmError, VmOptions};

mod _async_generator_support;

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // This test queues many async generator requests (creating Promise capabilities) before triggering
  // a resume job; use a larger heap to avoid spurious OOMs as vm-js evolves.
  let heap = Heap::new(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn draining_large_async_generator_request_queue_consumes_fuel() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !_async_generator_support::supports_async_generators(&mut rt)? {
    return Ok(());
  }

  // Create an async generator that suspends on a pending Promise so we can enqueue many `next()`
  // requests while the generator is still executing.
  let n = 200;
  rt.exec_script(&format!(
    r#"
      var __ag_resolve;
      async function* g() {{
        await new Promise((r) => {{ __ag_resolve = r; }});
      }}
      var it = g();
      it.next();
      for (let i = 0; i < {n}; i++) {{
        it.next();
      }}
    "#,
  ))?;

  // Resolving the awaited Promise should enqueue a single resume job into the microtask queue.
  rt.exec_script("__ag_resolve(0);")?;
  assert!(
    !rt.vm.microtask_queue().is_empty(),
    "expected resolving the awaited promise to enqueue a microtask"
  );

  // The resume job will complete the generator and then synchronously drain the queued request
  // list, resolving many Promises in a tight loop. Ensure that loop is fuel-budgeted.
  rt.vm.set_budget(Budget {
    fuel: Some(350),
    deadline: None,
    check_time_every: 1,
  });

  let err = rt.vm.perform_microtask_checkpoint(&mut rt.heap).unwrap_err();
  match err {
    VmError::Termination(term) => assert_eq!(term.reason, TerminationReason::OutOfFuel),
    other => panic!("expected termination, got {other:?}"),
  }

  // Termination is a hard stop: the checkpoint should discard remaining jobs so we don't leak
  // persistent roots.
  assert!(
    rt.vm.microtask_queue().is_empty(),
    "expected microtask queue to be empty after termination"
  );

  Ok(())
}

