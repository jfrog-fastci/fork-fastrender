use std::any::Any;

use vm_js::{
  GcObject, HeapLimits, Job, PropertyDescriptor, PropertyKey, PropertyKind, RealmId, Scope, Value,
  Vm, VmError, VmHost, VmHostHooks, VmOptions,
};
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
  getter_calls: usize,
}

impl VmHostHooks for CapturingHooks {
  fn host_enqueue_promise_job(&mut self, job: Job, realm: Option<RealmId>) {
    self.jobs.push((job, realm));
  }

  fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
    Some(self)
  }
}

fn handle_event_getter(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  if let Some(any) = hooks.as_any_mut() {
    if let Some(hooks) = any.downcast_mut::<CapturingHooks>() {
      hooks.getter_calls += 1;
    }
  }
  Ok(
    scope
      .heap()
      .get_function_native_slots(callee)?
      .get(0)
      .copied()
      .unwrap_or(Value::Undefined),
  )
}

fn register_callback_interface_with_accessor(
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

  let Value::Object(obj) = value else {
    return Err(VmError::TypeError("expected callback interface object"));
  };

  // Root the object + key across allocations while we rewrite `handleEvent`.
  scope.push_root(Value::Object(obj))?;
  let key_str = scope.alloc_string("handleEvent")?;
  scope.push_root(Value::String(key_str))?;
  let key = PropertyKey::from_string(key_str);

  // Read the existing method value before overwriting the property.
  let method = scope
    .heap()
    .object_get_own_data_property_value(obj, &key)?
    .unwrap_or(Value::Undefined);
  scope.push_root(method)?;

  // Root the callback handle (ensures `handleEvent` is callable).
  host.handle = CallbackHandle::from_callback_interface(vm, scope.heap_mut(), value, false)?;

  // Replace `handleEvent` with an accessor getter that returns the stored method.
  let getter_id = vm.register_native_call(handle_event_getter)?;
  let getter_name = scope.alloc_string("handleEvent getter")?;
  scope.push_root(Value::String(getter_name))?;
  let getter =
    scope.alloc_native_function_with_slots(getter_id, None, getter_name, 0, &[method])?;
  scope.push_root(Value::Object(getter))?;
  scope.define_property(
    obj,
    key,
    PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Accessor {
        get: Value::Object(getter),
        set: Value::Undefined,
      },
    },
  )?;

  Ok(Value::Undefined)
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
  rt.exec_script_with_host(
    &mut host,
    "registerCallback(function () { globalThis.called = 1; });",
  )?;

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
fn callback_interface_handle_invocation_calls_handle_event_getter_with_hooks() -> Result<(), VmError>
{
  let vm = Vm::new(VmOptions::default());
  let heap = vm_js::Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut rt = vm_js::JsRuntime::new(vm, heap)?;
  rt.register_global_native_function(
    "registerListenerAccessor",
    register_callback_interface_with_accessor,
    1,
  )?;

  let mut host = StoredHandle::default();
  rt.exec_script_with_host(
    &mut host,
    "const obj = { handleEvent(e) { globalThis.ok = (this === obj); globalThis.arg = e; } }; registerListenerAccessor(obj);",
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

  assert_eq!(hooks.getter_calls, 1);
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
