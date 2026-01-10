use std::sync::Arc;

use vm_js::{
  GcObject, Heap, HeapLimits, Job, JsRuntime, MicrotaskQueue, RealmId, RootId, Scope, SourceText,
  Value, Vm, VmError, VmHost, VmHostHooks, VmJobContext, VmOptions,
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

  rt.exec_script_source_with_host_and_hooks(
    &mut host,
    &mut hooks,
    Arc::new(SourceText::new("<inline>", "inc();")),
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

  rt.exec_script_source_with_host_and_hooks(
    &mut host,
    &mut hooks,
    Arc::new(SourceText::new("<inline>", "Promise.resolve().then(inc);")),
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
