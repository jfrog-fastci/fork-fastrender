use std::any::Any;

use vm_js::{GcObject, Heap, HeapLimits, Job, Realm, Scope, Value, Vm, VmError, VmHost, VmHostHooks, VmOptions};

use webidl::WebIdlLimits;
use webidl_vm_js::bindings_runtime::{BindingsRuntime, DataPropertyAttributes};
use webidl_vm_js::conversions;
use webidl_vm_js::VmJsHostHooksPayload;

struct HooksWithPayload {
  payload: VmJsHostHooksPayload,
}

impl VmHostHooks for HooksWithPayload {
  fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<vm_js::RealmId>) {}

  fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
    Some(&mut self.payload)
  }
}

fn native_get_max_string_code_units(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  // Mirrors vm-js generated WebIDL bindings behaviour: construct a fresh `BindingsRuntime` inside
  // the native call handler and read its limits without calling `set_limits`.
  let rt = BindingsRuntime::from_scope(vm, scope.reborrow());
  Ok(Value::Number(rt.limits().max_string_code_units as f64))
}

fn native_to_string_enforces_payload_limits(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let mut rt = BindingsRuntime::from_scope(vm, scope.reborrow());
  let value = args.get(0).copied().unwrap_or(Value::Undefined);
  // Root the input across `ToString` since it may allocate/GC and `value` may contain GC handles.
  rt.scope.push_root(value)?;
  let _ = rt
    .scope
    .to_string(&mut *rt.vm, host, hooks, value)
    .map(|_| ())?;
  Ok(Value::Undefined)
}

fn native_sequence_conversion_enforces_payload_limits(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let mut rt = BindingsRuntime::from_scope(vm, scope.reborrow());

  // Build a small iterable (Array) with 2 elements.
  let arr = rt.alloc_array(2)?;
  rt.scope.push_root(Value::Object(arr))?;
  let key0 = rt.property_key("0")?;
  rt
    .scope
    .create_data_property_or_throw(arr, key0, Value::Number(1.0))?;
  let key1 = rt.property_key("1")?;
  rt
    .scope
    .create_data_property_or_throw(arr, key1, Value::Number(2.0))?;

  // Convert to a sequence representation. This should enforce `max_sequence_length`.
  conversions::to_iterable_list(
    &mut rt,
    host,
    hooks,
    Value::Object(arr),
    "expected object for sequence",
    |_rt, _host, _hooks, v| Ok(v),
  )?;
  Ok(Value::Undefined)
}

fn native_record_conversion_enforces_payload_limits(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let mut rt = BindingsRuntime::from_scope(vm, scope.reborrow());

  let obj = rt.alloc_object()?;
  rt.define_data_property_str(
    obj,
    "a",
    Value::Number(1.0),
    DataPropertyAttributes::new(true, true, true),
  )?;
  rt.define_data_property_str(
    obj,
    "b",
    Value::Number(2.0),
    DataPropertyAttributes::new(true, true, true),
  )?;

  conversions::to_record(
    &mut rt,
    host,
    hooks,
    Value::Object(obj),
    "expected object for record",
    |_rt, _host, _hooks, v| Ok(v),
  )?;
  Ok(Value::Undefined)
}

#[test]
fn bindings_runtime_reads_webidl_limits_from_vmjs_host_hooks_payload() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  // Allocate a native function object.
  let func = {
    let call_id = vm.register_native_call(native_get_max_string_code_units)?;
    let mut scope = heap.scope();
    let name = scope.alloc_string("getLimit")?;
    scope.push_root(Value::String(name))?;
    scope.alloc_native_function(call_id, None, name, 0)?
  };
  let _func_root = heap.add_root(Value::Object(func))?;

  // Install a host hooks payload with a very small WebIDL max string limit.
  let mut payload = VmJsHostHooksPayload::default();
  let mut limits = WebIdlLimits::default();
  limits.max_string_code_units = 1;
  payload.set_webidl_limits(limits);

  let mut hooks = HooksWithPayload { payload };

  // Call the native function under a hooks override so `BindingsRuntime::from_scope` can recover
  // the configured limits from `vm.active_host_hooks_ptr()`.
  let out = {
    let mut scope = heap.scope();
    scope.push_root(Value::Object(func))?;
    vm.call_with_host(&mut scope, &mut hooks, Value::Object(func), Value::Undefined, &[])?
  };
  assert_eq!(out, Value::Number(1.0));

  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn bindings_scope_to_string_uses_webidl_limits_from_vmjs_host_hooks_payload() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let func = {
    let call_id = vm.register_native_call(native_to_string_enforces_payload_limits)?;
    let mut scope = heap.scope();
    let name = scope.alloc_string("toStringLimited")?;
    scope.push_root(Value::String(name))?;
    scope.alloc_native_function(call_id, None, name, 1)?
  };
  let _func_root = heap.add_root(Value::Object(func))?;

  let mut payload = VmJsHostHooksPayload::default();
  let mut limits = WebIdlLimits::default();
  limits.max_string_code_units = 1;
  payload.set_webidl_limits(limits);
  let mut hooks = HooksWithPayload { payload };

  let err = {
    let mut scope = heap.scope();
    scope.push_root(Value::Object(func))?;
    let s = scope.alloc_string("ab")?;
    scope.push_root(Value::String(s))?;
    vm.call_with_host(
      &mut scope,
      &mut hooks,
      Value::Object(func),
      Value::Undefined,
      &[Value::String(s)],
    )
    .expect_err("expected to_string to throw RangeError due to max_string_code_units")
  };

  let thrown = match err {
    VmError::Throw(v) => v,
    VmError::ThrowWithStack { value, .. } => value,
    other => panic!("expected thrown RangeError object, got {other:?}"),
  };
  let Value::Object(err_obj) = thrown else {
    panic!("expected thrown value to be an object, got {thrown:?}");
  };

  let mut scope = heap.scope();
  // Root the thrown value across string allocations for property key creation.
  scope.push_root(thrown)?;
  assert_eq!(
    scope.object_get_prototype(err_obj)?,
    Some(realm.intrinsics().range_error_prototype())
  );

  let msg_key_s = scope.alloc_string("message")?;
  scope.push_root(Value::String(msg_key_s))?;
  let msg_key = vm_js::PropertyKey::from_string(msg_key_s);
  let message = scope.heap().get(err_obj, &msg_key)?;
  let Value::String(message_s) = message else {
    panic!("expected error.message to be a string, got {message:?}");
  };
  assert_eq!(
    scope.heap().get_string(message_s)?.to_utf8_lossy(),
    "string exceeds maximum length"
  );

  drop(scope);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn sequence_conversion_uses_max_sequence_length_from_vmjs_host_hooks_payload() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let func = {
    let call_id = vm.register_native_call(native_sequence_conversion_enforces_payload_limits)?;
    let mut scope = heap.scope();
    let name = scope.alloc_string("seqLimited")?;
    scope.push_root(Value::String(name))?;
    scope.alloc_native_function(call_id, None, name, 0)?
  };
  let _func_root = heap.add_root(Value::Object(func))?;

  let mut payload = VmJsHostHooksPayload::default();
  let mut limits = WebIdlLimits::default();
  limits.max_sequence_length = 1;
  payload.set_webidl_limits(limits);
  let mut hooks = HooksWithPayload { payload };

  let err = {
    let mut scope = heap.scope();
    scope.push_root(Value::Object(func))?;
    vm.call_with_host(&mut scope, &mut hooks, Value::Object(func), Value::Undefined, &[])
      .expect_err("expected sequence conversion to throw RangeError due to max_sequence_length")
  };

  let thrown = err.thrown_value().expect("expected thrown exception value");
  let Value::Object(err_obj) = thrown else {
    return Err(VmError::TypeError("expected thrown value to be an object"));
  };

  let mut scope = heap.scope();
  scope.push_root(thrown)?;
  assert_eq!(
    scope.object_get_prototype(err_obj)?,
    Some(realm.intrinsics().range_error_prototype())
  );

  let msg_key_s = scope.alloc_string("message")?;
  scope.push_root(Value::String(msg_key_s))?;
  let msg_key = vm_js::PropertyKey::from_string(msg_key_s);
  let message = scope.heap().get(err_obj, &msg_key)?;
  let Value::String(message_s) = message else {
    return Err(VmError::TypeError("expected error.message to be a string"));
  };
  assert_eq!(
    scope.heap().get_string(message_s)?.to_utf8_lossy(),
    "sequence exceeds maximum length"
  );
  drop(scope);

  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn record_conversion_uses_max_record_entries_from_vmjs_host_hooks_payload() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let func = {
    let call_id = vm.register_native_call(native_record_conversion_enforces_payload_limits)?;
    let mut scope = heap.scope();
    let name = scope.alloc_string("recordLimited")?;
    scope.push_root(Value::String(name))?;
    scope.alloc_native_function(call_id, None, name, 0)?
  };
  let _func_root = heap.add_root(Value::Object(func))?;

  let mut payload = VmJsHostHooksPayload::default();
  let mut limits = WebIdlLimits::default();
  limits.max_record_entries = 1;
  payload.set_webidl_limits(limits);
  let mut hooks = HooksWithPayload { payload };

  let err = {
    let mut scope = heap.scope();
    scope.push_root(Value::Object(func))?;
    vm.call_with_host(&mut scope, &mut hooks, Value::Object(func), Value::Undefined, &[])
      .expect_err("expected record conversion to throw RangeError due to max_record_entries")
  };

  let thrown = err.thrown_value().expect("expected thrown exception value");
  let Value::Object(err_obj) = thrown else {
    return Err(VmError::TypeError("expected thrown value to be an object"));
  };

  let mut scope = heap.scope();
  scope.push_root(thrown)?;
  assert_eq!(
    scope.object_get_prototype(err_obj)?,
    Some(realm.intrinsics().range_error_prototype())
  );

  let msg_key_s = scope.alloc_string("message")?;
  scope.push_root(Value::String(msg_key_s))?;
  let msg_key = vm_js::PropertyKey::from_string(msg_key_s);
  let message = scope.heap().get(err_obj, &msg_key)?;
  let Value::String(message_s) = message else {
    return Err(VmError::TypeError("expected error.message to be a string"));
  };
  assert_eq!(
    scope.heap().get_string(message_s)?.to_utf8_lossy(),
    "record exceeds maximum entry count"
  );
  drop(scope);

  realm.teardown(&mut heap);
  Ok(())
}
