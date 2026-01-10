use vm_js::{
  GcObject, Heap, HeapLimits, Job, RealmId, Scope, Value, Vm, VmError, VmHost, VmHostHooks,
  VmOptions,
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
