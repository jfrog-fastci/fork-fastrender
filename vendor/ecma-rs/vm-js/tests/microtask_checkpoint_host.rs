use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use vm_js::{
  Budget, GcObject, Heap, HeapLimits, Job, JobKind, JsRuntime, PromiseState, PropertyKey, Scope,
  TerminationReason, Value, Vm, VmError, VmHost, VmHostHooks, VmOptions,
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

fn tick_and_terminate(
  vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  // `Vm::call`/`Vm::call_with_host_and_hooks` already charge one tick on entry. With a fuel budget
  // of 1, that entry tick consumes the last fuel, so this extra tick triggers
  // `TerminationReason::OutOfFuel`.
  vm.tick()?;
  Ok(Value::Undefined)
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

  // Schedule a Promise job that will call into the native `inc` binding and record whether the
  // handler ran successfully.
  rt.exec_script_with_host(
    &mut host,
    r#"
      var ok = false;
      var failed = false;
      Promise.resolve().then(inc).then(
        () => { ok = true; },
        () => { failed = true; }
      );
    "#,
  )?;
  assert_eq!(host.counter, 0);

  // The legacy checkpoint API uses a dummy host context (`()`), so `inc` should fail to downcast,
  // causing the Promise chain to reject (and `failed` to become true).
  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("failed && !ok")?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn microtask_checkpoint_terminates_and_discards_remaining_jobs() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions {
    check_time_every: 1,
    ..VmOptions::default()
  });
  vm.set_budget(Budget {
    fuel: Some(1),
    deadline: None,
    check_time_every: 1,
  });
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

  let call_id = vm.register_native_call(tick_and_terminate)?;

  let tick_fn = {
    let mut scope = heap.scope();
    let name = scope.alloc_string("tickAndTerminate")?;
    scope.alloc_native_function(call_id, None, name, 0)?
  };
  let tick_fn_root = heap.add_root(Value::Object(tick_fn))?;
  let tick_fn_value = Value::Object(tick_fn);

  // First job triggers a termination error via a VM call.
  let mut job1 = Job::new(JobKind::Promise, move |ctx, hooks| {
    ctx.call(hooks, tick_fn_value, Value::Undefined, &[])?;
    Ok(())
  })?;
  if let Err(e) = job1.try_push_root(tick_fn_root) {
    heap.remove_root(tick_fn_root);
    return Err(e);
  }

  // Second job has a Rust-side side effect but does not call into the VM (so it won't tick).
  let counter = Arc::new(AtomicUsize::new(0));
  let counter_for_job2 = counter.clone();
  let job2 = Job::new(JobKind::Promise, move |_ctx, _hooks| {
    counter_for_job2.fetch_add(1, Ordering::SeqCst);
    Ok(())
  })?;

  vm.microtask_queue_mut().enqueue_promise_job(job1, None);
  vm.microtask_queue_mut().enqueue_promise_job(job2, None);

  let mut host = ();
  let err = vm
    .perform_microtask_checkpoint_with_host(&mut host, &mut heap)
    .unwrap_err();
  match err {
    VmError::Termination(term) => assert_eq!(term.reason, TerminationReason::OutOfFuel),
    other => panic!("expected termination, got {other:?}"),
  }

  // The checkpoint must not execute further jobs after termination.
  assert_eq!(counter.load(Ordering::SeqCst), 0);
  Ok(())
}
