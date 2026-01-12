use std::collections::VecDeque;

use vm_js::{
  GcObject, Heap, HeapLimits, Job, PropertyKey, Realm, RealmId, RootId, Scope, Value, Vm, VmError,
  VmHost, VmHostHooks, VmJobContext, VmOptions,
};

fn noop(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Ok(Value::Undefined)
}

fn get_own_data_property(
  heap: &mut Heap,
  obj: GcObject,
  name: &str,
) -> Result<Option<Value>, VmError> {
  let mut scope = heap.scope();
  scope.push_root(Value::Object(obj))?;
  let key = PropertyKey::from_string(scope.alloc_string(name)?);
  scope
    .heap()
    .object_get_own_data_property_value(obj, &key)
}

fn get_own_data_function(heap: &mut Heap, obj: GcObject, name: &str) -> Result<GcObject, VmError> {
  let Some(Value::Object(func)) = get_own_data_property(heap, obj, name)? else {
    return Err(VmError::Unimplemented("missing intrinsic function"));
  };
  Ok(func)
}

#[derive(Default)]
struct RecordingHost {
  jobs: VecDeque<(Option<RealmId>, Job)>,
}

impl VmHostHooks for RecordingHost {
  fn host_enqueue_promise_job(&mut self, job: Job, realm: Option<RealmId>) {
    self.jobs.push_back((realm, job));
  }
}

struct RootingCtx<'a> {
  heap: &'a mut Heap,
}

impl VmJobContext for RootingCtx<'_> {
  fn call(
    &mut self,
    _hooks: &mut dyn VmHostHooks,
    _callee: Value,
    _this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    Err(VmError::Unimplemented("RootingCtx::call"))
  }

  fn construct(
    &mut self,
    _hooks: &mut dyn VmHostHooks,
    _callee: Value,
    _args: &[Value],
    _new_target: Value,
  ) -> Result<Value, VmError> {
    Err(VmError::Unimplemented("RootingCtx::construct"))
  }

  fn add_root(&mut self, value: Value) -> Result<RootId, VmError> {
    self.heap.add_root(value)
  }

  fn remove_root(&mut self, id: RootId) {
    self.heap.remove_root(id)
  }
}

#[test]
fn new_function_works_in_fresh_realm() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;
  let intr = *realm.intrinsics();

  let value = {
    let mut scope = heap.scope();
    let body_s = scope.alloc_string("return 1")?;
    scope.push_root(Value::String(body_s))?;

    let ctor = Value::Object(intr.function_constructor());
    let func = vm.construct_without_host(&mut scope, ctor, &[Value::String(body_s)], ctor)?;
    scope.push_root(func)?;

    vm.call_without_host(&mut scope, func, Value::Undefined, &[])?
  };

  assert_eq!(value, Value::Number(1.0));

  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn promise_jobs_are_tagged_with_realm_when_called_without_execution_context() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;
  let intr = *realm.intrinsics();

  let then = get_own_data_function(&mut heap, intr.promise_prototype(), "then")?;

  let mut host = RecordingHost::default();
  let promise = {
    let mut scope = heap.scope();
    let promise = scope.alloc_promise_with_prototype(Some(intr.promise_prototype()))?;
    scope.push_root(Value::Object(promise))?;
    scope
      .heap_mut()
      .promise_fulfill(promise, Value::Number(1.0))?;

    let call_id = vm.register_native_call(noop)?;
    let name = scope.alloc_string("onFulfilled")?;
    let on_fulfilled = scope.alloc_native_function(call_id, None, name, 1)?;

    let _ = vm.call_with_host(
      &mut scope,
      &mut host,
      Value::Object(then),
      Value::Object(promise),
      &[Value::Object(on_fulfilled)],
    )?;
    promise
  };

  // The job must be tagged with the realm even though the host call did not establish an execution
  // context explicitly.
  let (realm_tag, job) = host
    .jobs
    .pop_front()
    .expect("Promise.prototype.then should enqueue a job for an already-fulfilled promise");
  assert_eq!(realm_tag, Some(realm.id()));

  // Clean up the job's persistent roots to avoid leaking roots (and tripping debug assertions in
  // `Drop`).
  let mut ctx = RootingCtx { heap: &mut heap };
  job.discard(&mut ctx);

  // Avoid unused-variable warnings on `promise` in case future changes stop using it.
  let _ = promise;

  realm.teardown(&mut heap);
  Ok(())
}

