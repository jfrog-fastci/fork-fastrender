use std::any::Any;

use vm_js::{GcObject, HeapLimits, Job, RealmId, Scope, Value, Vm, VmError, VmHost, VmHostHooks, VmOptions};
use webidl_vm_js::CallbackHandle;

#[derive(Default)]
struct StoredHandle {
  handle: Option<CallbackHandle>,
}

fn register_callback_function(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let host = host
    .as_any_mut()
    .downcast_mut::<StoredHandle>()
    .ok_or(VmError::Unimplemented("host downcast failed"))?;
  let value = args.get(0).copied().unwrap_or(Value::Undefined);
  host.handle = CallbackHandle::from_callback_function(vm, scope.heap_mut(), value, false)?;
  Ok(Value::Undefined)
}

fn register_callback_interface(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let host = host
    .as_any_mut()
    .downcast_mut::<StoredHandle>()
    .ok_or(VmError::Unimplemented("host downcast failed"))?;
  let value = args.get(0).copied().unwrap_or(Value::Undefined);
  host.handle = CallbackHandle::from_callback_interface(vm, scope.heap_mut(), value, false)?;
  Ok(Value::Undefined)
}

#[derive(Default)]
struct CapturingHooks {
  jobs: Vec<(Job, Option<RealmId>)>,
}

impl VmHostHooks for CapturingHooks {
  fn host_enqueue_promise_job(&mut self, job: Job, realm: Option<RealmId>) {
    self.jobs.push((job, realm));
  }

  fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
    Some(self)
  }
}

fn get_global_number(rt: &mut vm_js::JsRuntime, name: &str) -> Result<Option<f64>, VmError> {
  let global = rt.realm().global_object();
  let mut scope = rt.heap.scope();
  scope.push_root(Value::Object(global))?;
  let key_str = scope.alloc_string(name)?;
  scope.push_root(Value::String(key_str))?;
  let key = vm_js::PropertyKey::from_string(key_str);
  let value = rt.vm.get(&mut scope, global, key)?;
  Ok(match value {
    Value::Undefined => None,
    Value::Number(n) => Some(n),
    _ => None,
  })
}

fn get_global_bool(rt: &mut vm_js::JsRuntime, name: &str) -> Result<Option<bool>, VmError> {
  let global = rt.realm().global_object();
  let mut scope = rt.heap.scope();
  scope.push_root(Value::Object(global))?;
  let key_str = scope.alloc_string(name)?;
  scope.push_root(Value::String(key_str))?;
  let key = vm_js::PropertyKey::from_string(key_str);
  let value = rt.vm.get(&mut scope, global, key)?;
  Ok(match value {
    Value::Undefined => None,
    Value::Bool(b) => Some(b),
    _ => None,
  })
}

fn assert_type_error(rt: &mut vm_js::JsRuntime, err: VmError) -> Result<(), VmError> {
  let VmError::ThrowWithStack { value, .. } = err else {
    return Err(VmError::Unimplemented("expected thrown exception"));
  };
  let Value::Object(obj) = value else {
    return Err(VmError::Unimplemented("expected thrown object"));
  };
  let proto = rt.heap.object_prototype(obj)?;
  assert_eq!(proto, Some(rt.realm().intrinsics().type_error_prototype()));
  Ok(())
}

#[test]
fn callback_function_handle_roots_and_invokes_later() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = vm_js::Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut rt = vm_js::JsRuntime::new(vm, heap)?;
  rt.register_global_native_function("registerCallback", register_callback_function, 1)?;

  let mut host = StoredHandle::default();
  rt.exec_script_with_host(&mut host, "registerCallback(function () { globalThis.called = 1; });")?;

  let handle = host.handle.take().expect("expected callback handle");
  let mut hooks = CapturingHooks::default();
  let mut host_ctx = ();
  handle.invoke(&mut rt.vm, &mut rt.heap, &mut host_ctx, &mut hooks, &[])?;
  handle.unroot(&mut rt.heap);

  // Drain any Promise jobs as a safety net (should be empty for this script).
  for (job, _realm) in hooks.jobs.drain(..) {
    job.discard(&mut rt);
  }

  assert_eq!(get_global_number(&mut rt, "called")?, Some(1.0));
  Ok(())
}

#[test]
fn callback_interface_handle_calls_handle_event_with_object_this() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = vm_js::Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut rt = vm_js::JsRuntime::new(vm, heap)?;
  rt.register_global_native_function("registerListener", register_callback_interface, 1)?;

  let mut host = StoredHandle::default();
  rt.exec_script_with_host(
    &mut host,
    "const obj = { handleEvent(e) { globalThis.ok = (this === obj); globalThis.arg = e; } }; registerListener(obj);",
  )?;

  let handle = host.handle.take().expect("expected callback handle");
  let mut hooks = CapturingHooks::default();
  let mut host_ctx = ();
  handle.invoke_with_this(
    &mut rt.vm,
    &mut rt.heap,
    &mut host_ctx,
    &mut hooks,
    Value::Undefined,
    &[Value::Number(123.0)],
  )?;
  handle.unroot(&mut rt.heap);

  for (job, _realm) in hooks.jobs.drain(..) {
    job.discard(&mut rt);
  }

  assert_eq!(get_global_bool(&mut rt, "ok")?, Some(true));
  assert_eq!(get_global_number(&mut rt, "arg")?, Some(123.0));
  Ok(())
}

#[test]
fn invalid_callback_types_throw_type_error() -> Result<(), VmError> {
  // callback function
  {
    let vm = Vm::new(VmOptions::default());
    let heap = vm_js::Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
    let mut rt = vm_js::JsRuntime::new(vm, heap)?;
    rt.register_global_native_function("registerCallback", register_callback_function, 1)?;

    let mut host = StoredHandle::default();
    let err = rt
      .exec_script_with_host(&mut host, "registerCallback(1);")
      .unwrap_err();
    assert_type_error(&mut rt, err)?;
  }

  // callback interface
  {
    let vm = Vm::new(VmOptions::default());
    let heap = vm_js::Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
    let mut rt = vm_js::JsRuntime::new(vm, heap)?;
    rt.register_global_native_function("registerListener", register_callback_interface, 1)?;

    let mut host = StoredHandle::default();
    let err = rt
      .exec_script_with_host(&mut host, "registerListener({});")
      .unwrap_err();
    assert_type_error(&mut rt, err)?;
  }

  Ok(())
}

