use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use vm_js::{
  ExecutionContext, GcObject, Heap, HeapLimits, Job, JobKind, JsRuntime, MicrotaskQueue, RealmId,
  RootId, Scope, SourceText, Value, Vm, VmError, VmHost, VmHostHooks, VmJobContext, VmOptions,
};

#[derive(Debug, Default)]
struct Host {
  counter: u32,
}

#[derive(Debug, Default)]
struct NoopHooks;

impl VmHostHooks for NoopHooks {
  fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {
    // Not needed for these tests.
  }
}

#[derive(Debug, Default)]
struct RecordingHooks {
  jobs: Vec<(Option<RealmId>, Job)>,
}

impl VmHostHooks for RecordingHooks {
  fn host_enqueue_promise_job(&mut self, job: Job, realm: Option<RealmId>) {
    self.jobs.push((realm, job));
  }
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
  let host = host
    .as_any_mut()
    .downcast_mut::<Host>()
    .ok_or(VmError::Unimplemented("host context has unexpected type"))?;
  host.counter += 1;
  Ok(Value::Undefined)
}

fn enqueue_vm_microtask_job(
  vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let counter = vm
    .user_data::<Arc<AtomicUsize>>()
    .cloned()
    .ok_or(VmError::Unimplemented("missing Arc<AtomicUsize> user_data"))?;

  // Intentionally enqueue onto the VM-owned microtask queue (instead of the supplied host hooks).
  // The script execution entry points should drain this queue into the provided hooks as a safety
  // net.
  let job = Job::new(JobKind::Promise, move |_ctx, _host| {
    counter.fetch_add(1, Ordering::SeqCst);
    Ok(())
  })?;
  vm.microtask_queue_mut().host_enqueue_promise_job(job, None);
  Ok(Value::Undefined)
}

#[test]
fn native_handlers_can_downcast_and_mutate_embedder_host_context() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

  let mut scope = heap.scope();
  let call_id = vm.register_native_call(inc_host_counter)?;
  let name = scope.alloc_string("inc")?;
  let func = scope.alloc_native_function(call_id, None, name, 0)?;

  let mut host = Host::default();
  assert_eq!(host.counter, 0);
  vm.call(&mut host, &mut scope, Value::Object(func), Value::Undefined, &[])?;
  assert_eq!(host.counter, 1);
  Ok(())
}

#[test]
fn call_with_host_passes_dummy_vmhost_context() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

  let func = {
    let mut scope = heap.scope();
    let call_id = vm.register_native_call(inc_host_counter)?;
    let name = scope.alloc_string("inc")?;
    scope.alloc_native_function(call_id, None, name, 0)?
  };

  let mut hooks = NoopHooks::default();

  // `call_with_host` always passes a dummy `VmHost` (`()`), so native handlers that require an
  // embedding-specific host type cannot downcast it.
  let err = {
    let mut scope = heap.scope();
    vm.call_with_host(&mut scope, &mut hooks, Value::Object(func), Value::Undefined, &[])
      .expect_err("call_with_host should not provide embedder host context")
  };
  assert!(matches!(err, VmError::Unimplemented(_)));

  // `call_with_host_and_hooks` allows embeddings to provide both host state and host hooks.
  let mut host = Host::default();
  {
    let mut scope = heap.scope();
    vm.call_with_host_and_hooks(
      &mut host,
      &mut scope,
      &mut hooks,
      Value::Object(func),
      Value::Undefined,
      &[],
    )?;
  }
  assert_eq!(host.counter, 1);

  Ok(())
}

#[test]
fn exec_script_source_with_host_and_hooks_threads_host_context_into_native_calls() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;
  rt.register_global_native_function("inc", inc_host_counter, 0)?;

  let mut host = Host::default();
  let mut hooks = MicrotaskQueue::new();

  let source = SourceText::new_charged_arc(&mut rt.heap, "<inline>", "inc();")?;
  rt.exec_script_source_with_host_and_hooks(
    &mut host,
    &mut hooks,
    source,
  )?;

  assert_eq!(host.counter, 1);
  Ok(())
}

#[test]
fn exec_script_with_host_and_hooks_threads_host_context_and_records_promise_jobs() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;
  rt.register_global_native_function("inc", inc_host_counter, 0)?;

  let mut host = Host::default();
  let mut hooks = RecordingHooks::default();

  rt.exec_script_with_host_and_hooks(
    &mut host,
    &mut hooks,
    r#"
      inc();
      Promise.resolve().then(() => {});
    "#,
  )?;

  assert_eq!(host.counter, 1);
  assert!(
    !hooks.jobs.is_empty(),
    "Promise.resolve().then(..) should enqueue at least one Promise job via VmHostHooks"
  );

  // Ensure any persistent roots owned by queued jobs are cleaned up before dropping the runtime.
  for (_realm, job) in hooks.jobs.drain(..) {
    job.discard(&mut rt);
  }

  Ok(())
}

#[test]
fn exec_script_with_hooks_passes_dummy_vmhost_context_for_native_calls() -> Result<(), VmError> {
  // Regression guard: `exec_script_with_hooks`/`exec_script_source_with_hooks` preserve the historic
  // behavior of running native calls with a dummy host context (`()`), while still routing Promise
  // jobs through the supplied hooks.
  //
  // Embeddings that need real host state for native calls should use the new
  // `exec_script*_with_host_and_hooks` entrypoints instead.
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;
  rt.register_global_native_function("inc", inc_host_counter, 0)?;

  let mut hooks = NoopHooks::default();
  let err = rt
    .exec_script_with_hooks(&mut hooks, "inc();")
    .expect_err("expected dummy host context to fail Host downcast");
  assert!(matches!(err, VmError::Throw(_) | VmError::ThrowWithStack { .. }));

  let source = SourceText::new_charged_arc(&mut rt.heap, "<inline>", "inc();")?;
  let err = rt
    .exec_script_source_with_hooks(&mut hooks, source)
    .expect_err("expected dummy host context to fail Host downcast");
  assert!(matches!(err, VmError::Throw(_) | VmError::ThrowWithStack { .. }));
  Ok(())
}

#[test]
fn exec_script_source_with_host_and_hooks_drains_vm_microtask_queue_into_host_hooks() -> Result<(), VmError>
{
  // Regression guard: even when a native handler enqueues work onto the VM-owned microtask queue,
  // the script execution entry point should drain that queue into the provided host hooks as a
  // safety net.
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let counter = Arc::new(AtomicUsize::new(0));
  rt.vm.set_user_data(counter.clone());

  rt.register_global_native_function("enqueue", enqueue_vm_microtask_job, 0)?;

  let mut host = ();
  let mut hooks = RecordingHooks::default();

  let source = SourceText::new_charged_arc(&mut rt.heap, "<inline>", "enqueue();")?;
  rt.exec_script_source_with_host_and_hooks(
    &mut host,
    &mut hooks,
    source,
  )?;

  assert_eq!(hooks.jobs.len(), 1, "expected VM microtask to be forwarded to hooks");

  let mut noop_hooks = NoopHooks::default();
  let (_realm, job) = hooks.jobs.pop().unwrap();
  job.run(&mut rt, &mut noop_hooks)?;

  assert_eq!(counter.load(Ordering::SeqCst), 1);
  Ok(())
}

#[test]
fn exec_script_source_with_host_and_hooks_drains_vm_microtask_queue_even_on_error() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let counter = Arc::new(AtomicUsize::new(0));
  rt.vm.set_user_data(counter.clone());
  rt.register_global_native_function("enqueue", enqueue_vm_microtask_job, 0)?;

  let mut host = ();
  let mut hooks = RecordingHooks::default();

  let source = SourceText::new_charged_arc(&mut rt.heap, "<inline>", "enqueue(); throw 1;")?;
  let err = rt
    .exec_script_source_with_host_and_hooks(
      &mut host,
      &mut hooks,
      source,
    )
    .expect_err("script should throw");
  assert!(matches!(err, VmError::ThrowWithStack { .. }));

  assert_eq!(hooks.jobs.len(), 1, "expected VM microtask to be forwarded to hooks");

  let mut noop_hooks = NoopHooks::default();
  let (_realm, job) = hooks.jobs.pop().unwrap();
  job.run(&mut rt, &mut noop_hooks)?;

  assert_eq!(counter.load(Ordering::SeqCst), 1);
  Ok(())
}

#[test]
fn call_without_host_respects_host_hooks_override() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let schedule = rt.exec_script(
    r#"
      function schedule() {
        Promise.resolve().then(function () {});
      }
      schedule
    "#,
  )?;

  assert_eq!(
    rt.vm.microtask_queue().len(),
    0,
    "VM microtask queue should start empty"
  );

  let exec_ctx = ExecutionContext {
    realm: rt.realm().id(),
    script_or_module: None,
  };

  let mut host_queue = MicrotaskQueue::new();

  {
    // Split borrows so we can hold a `Scope` and a `&mut Vm` at the same time.
    let vm = &mut rt.vm;
    let heap = &mut rt.heap;

    let mut vm_ctx = vm.execution_context_guard(exec_ctx)?;
    let mut scope = heap.scope();

    vm_ctx.with_host_hooks_override(&mut host_queue, |vm| {
      vm.call_without_host(&mut scope, schedule, Value::Undefined, &[])
        .expect("schedule() call failed");
    });
  }

  assert!(
    host_queue.len() > 0,
    "Promise.resolve().then(..) should enqueue at least one job onto the active host hooks override"
  );
  assert_eq!(
    rt.vm.microtask_queue().len(),
    0,
    "call_without_host should not enqueue onto the VM-owned microtask queue when an override is active"
  );

  // Ensure any persistent roots held by queued jobs are cleaned up before dropping the runtime.
  let errors = host_queue.perform_microtask_checkpoint(&mut rt);
  assert!(errors.is_empty());

  Ok(())
}

#[test]
fn vm_call_preserves_jobs_enqueued_directly_on_vm_microtask_queue() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

  let counter = Arc::new(AtomicUsize::new(0));
  vm.set_user_data(counter.clone());

  let func = {
    let mut scope = heap.scope();
    let call_id = vm.register_native_call(enqueue_vm_microtask_job)?;
    let name = scope.alloc_string("enqueue")?;
    scope.alloc_native_function(call_id, None, name, 0)?
  };

  {
    let mut scope = heap.scope();
    let mut host = ();
    vm.call(&mut host, &mut scope, Value::Object(func), Value::Undefined, &[])?;
  }

  assert_eq!(counter.load(Ordering::SeqCst), 0);

  vm.perform_microtask_checkpoint(&mut heap)?;
  assert_eq!(counter.load(Ordering::SeqCst), 1);
  Ok(())
}

#[test]
fn construct_without_host_respects_host_hooks_override() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let ctor = rt.exec_script(
    r#"
      function C() {
        Promise.resolve().then(function () {});
      }
      C
    "#,
  )?;

  assert_eq!(rt.vm.microtask_queue().len(), 0);

  let exec_ctx = ExecutionContext {
    realm: rt.realm().id(),
    script_or_module: None,
  };

  let mut host_queue = MicrotaskQueue::new();

  {
    let vm = &mut rt.vm;
    let heap = &mut rt.heap;
    let mut vm_ctx = vm.execution_context_guard(exec_ctx)?;
    let mut scope = heap.scope();

    vm_ctx.with_host_hooks_override(&mut host_queue, |vm| {
      let _obj = vm
        .construct_without_host(&mut scope, ctor, &[], ctor)
        .expect("new C() failed");
    });
  }

  assert!(
    host_queue.len() > 0,
    "new C() should enqueue at least one Promise job onto the active host hooks override"
  );
  assert_eq!(
    rt.vm.microtask_queue().len(),
    0,
    "construct_without_host should not enqueue onto the VM-owned microtask queue when an override is active"
  );

  let errors = host_queue.perform_microtask_checkpoint(&mut rt);
  assert!(errors.is_empty());
  Ok(())
}

struct HostJobContext<'a> {
  rt: &'a mut JsRuntime,
  host: &'a mut Host,
}

impl VmJobContext for HostJobContext<'_> {
  fn call(
    &mut self,
    hooks: &mut dyn VmHostHooks,
    callee: Value,
    this: Value,
    args: &[Value],
  ) -> Result<Value, VmError> {
    let vm = &mut self.rt.vm;
    let heap = &mut self.rt.heap;
    let mut scope = heap.scope();
    vm.call_with_host_and_hooks(self.host, &mut scope, hooks, callee, this, args)
  }

  fn construct(
    &mut self,
    hooks: &mut dyn VmHostHooks,
    callee: Value,
    args: &[Value],
    new_target: Value,
  ) -> Result<Value, VmError> {
    let vm = &mut self.rt.vm;
    let heap = &mut self.rt.heap;
    let mut scope = heap.scope();
    vm.construct_with_host_and_hooks(self.host, &mut scope, hooks, callee, args, new_target)
  }

  fn add_root(&mut self, value: Value) -> Result<RootId, VmError> {
    self.rt.heap.add_root(value)
  }

  fn remove_root(&mut self, id: RootId) {
    self.rt.heap.remove_root(id);
  }
}

#[test]
fn promise_jobs_can_access_host_context_when_job_context_calls_with_host_and_hooks() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;
  rt.register_global_native_function("inc", inc_host_counter, 0)?;

  let mut host = Host::default();
  let mut hooks = MicrotaskQueue::new();

  let source =
    SourceText::new_charged_arc(&mut rt.heap, "<inline>", "Promise.resolve().then(inc);")?;
  rt.exec_script_source_with_host_and_hooks(
    &mut host,
    &mut hooks,
    source,
  )?;
  assert_eq!(host.counter, 0);

  let errors = {
    let mut ctx = HostJobContext { rt: &mut rt, host: &mut host };
    hooks.perform_microtask_checkpoint(&mut ctx)
  };
  assert!(errors.is_empty());
  assert_eq!(host.counter, 1);
  Ok(())
}
