use vm_js::create_promise_resolve_thenable_job;
use vm_js::GcObject;
use vm_js::Heap;
use vm_js::HeapLimits;
use vm_js::Job;
use vm_js::JobCallback;
use vm_js::RootId;
use vm_js::Scope;
use vm_js::Value;
use vm_js::Vm;
use vm_js::VmError;
use vm_js::VmHostHooks;
use vm_js::VmJobContext;
use vm_js::VmOptions;
use vm_js::WeakGcObject;

fn noop(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Ok(Value::Undefined)
}

struct RootingContext<'a> {
  heap: &'a mut Heap,
}

impl VmJobContext for RootingContext<'_> {
  fn call(
    &mut self,
    _host: &mut dyn VmHostHooks,
    _callee: Value,
    _this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    Err(VmError::Unimplemented("RootingContext::call"))
  }

  fn construct(
    &mut self,
    _host: &mut dyn VmHostHooks,
    _callee: Value,
    _args: &[Value],
    _new_target: Value,
  ) -> Result<Value, VmError> {
    Err(VmError::Unimplemented("RootingContext::construct"))
  }

  fn add_root(&mut self, value: Value) -> Result<RootId, VmError> {
    self.heap.add_root(value)
  }

  fn remove_root(&mut self, id: RootId) {
    self.heap.remove_root(id)
  }
}

#[derive(Clone)]
struct TestHost {
  call_result: Result<Value, VmError>,
}

impl VmHostHooks for TestHost {
  fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<vm_js::RealmId>) {
    // Not used by these tests; we run jobs directly.
  }

  fn host_make_job_callback(&mut self, callback: GcObject) -> JobCallback {
    JobCallback::new(callback)
  }

  fn host_call_job_callback(
    &mut self,
    _ctx: &mut dyn VmJobContext,
    _callback: &JobCallback,
    _this_argument: Value,
    _arguments: &[Value],
  ) -> Result<Value, VmError> {
    self.call_result.clone()
  }
}

#[test]
fn promise_thenable_job_discard_releases_roots() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut vm = Vm::new(VmOptions::default());

  let call_id = vm.register_native_call(noop)?;

  let then_action;
  let thenable;
  let resolve;
  let reject;
  let job;
  {
    let mut scope = heap.scope();

    let name = scope.alloc_string("then")?;
    then_action = scope.alloc_native_function(call_id, None, name, 2)?;
    thenable = scope.alloc_object()?;
    resolve = scope.alloc_object()?;
    reject = scope.alloc_object()?;

    let mut host = TestHost {
      call_result: Ok(Value::Undefined),
    };
    job = create_promise_resolve_thenable_job(
      &mut host,
      scope.heap_mut(),
      Value::Object(thenable),
      Value::Object(then_action),
      Value::Object(resolve),
      Value::Object(reject),
    )?
    .expect("then_action is callable")
    .0;
  }

  let weak_then_action = WeakGcObject::from(then_action);
  let weak_thenable = WeakGcObject::from(thenable);
  let weak_resolve = WeakGcObject::from(resolve);
  let weak_reject = WeakGcObject::from(reject);

  // The job should keep all captured values alive until it runs or is discarded.
  heap.collect_garbage();
  assert!(weak_then_action.upgrade(&heap).is_some());
  assert!(weak_thenable.upgrade(&heap).is_some());
  assert!(weak_resolve.upgrade(&heap).is_some());
  assert!(weak_reject.upgrade(&heap).is_some());

  let mut ctx = RootingContext { heap: &mut heap };
  job.discard(&mut ctx);

  ctx.heap.collect_garbage();
  assert!(weak_then_action.upgrade(&*ctx.heap).is_none());
  assert!(weak_thenable.upgrade(&*ctx.heap).is_none());
  assert!(weak_resolve.upgrade(&*ctx.heap).is_none());
  assert!(weak_reject.upgrade(&*ctx.heap).is_none());

  Ok(())
}

#[test]
fn promise_thenable_job_error_still_releases_roots() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut vm = Vm::new(VmOptions::default());

  let call_id = vm.register_native_call(noop)?;

  let then_action;
  let thenable;
  let resolve;
  let reject;
  let job;
  let mut host = TestHost {
    call_result: Err(VmError::Unimplemented("host_call_job_callback failed")),
  };

  {
    let mut scope = heap.scope();

    let name = scope.alloc_string("then")?;
    then_action = scope.alloc_native_function(call_id, None, name, 2)?;
    thenable = scope.alloc_object()?;
    resolve = scope.alloc_object()?;
    reject = scope.alloc_object()?;

    job = create_promise_resolve_thenable_job(
      &mut host,
      scope.heap_mut(),
      Value::Object(thenable),
      Value::Object(then_action),
      Value::Object(resolve),
      Value::Object(reject),
    )?
    .expect("then_action is callable")
    .0;
  }

  let weak_then_action = WeakGcObject::from(then_action);
  let weak_thenable = WeakGcObject::from(thenable);
  let weak_resolve = WeakGcObject::from(resolve);
  let weak_reject = WeakGcObject::from(reject);

  heap.collect_garbage();
  assert!(weak_then_action.upgrade(&heap).is_some());
  assert!(weak_thenable.upgrade(&heap).is_some());
  assert!(weak_resolve.upgrade(&heap).is_some());
  assert!(weak_reject.upgrade(&heap).is_some());

  let mut ctx = RootingContext { heap: &mut heap };

  let err = job.run(&mut ctx, &mut host).expect_err("host should return error");
  assert!(matches!(
    err,
    VmError::Unimplemented("host_call_job_callback failed")
  ));

  ctx.heap.collect_garbage();
  assert!(weak_then_action.upgrade(&*ctx.heap).is_none());
  assert!(weak_thenable.upgrade(&*ctx.heap).is_none());
  assert!(weak_resolve.upgrade(&*ctx.heap).is_none());
  assert!(weak_reject.upgrade(&*ctx.heap).is_none());

  Ok(())
}
