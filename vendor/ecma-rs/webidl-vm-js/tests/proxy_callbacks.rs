use std::cell::Cell;

use vm_js::{
  GcObject, Heap, HeapLimits, MicrotaskQueue, Scope, Value, Vm, VmError, VmHost, VmHostHooks,
  VmOptions,
};
use webidl_vm_js::{invoke_callback_interface, CallbackHandle};

thread_local! {
  static EXPECTED_PROXY: Cell<Option<GcObject>> = const { Cell::new(None) };
  static EXPECTED_TARGET: Cell<Option<GcObject>> = const { Cell::new(None) };
  static GET_TRAP_CALLS: Cell<u32> = const { Cell::new(0) };
  static HANDLE_EVENT_CALLS: Cell<u32> = const { Cell::new(0) };
  static HANDLE_EVENT_FN: Cell<Option<GcObject>> = const { Cell::new(None) };
}

fn proxy_get_trap(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  GET_TRAP_CALLS.with(|c| c.set(c.get() + 1));

  assert_eq!(args.len(), 3, "Proxy get trap must receive (target, key, receiver)");

  let expected_target = EXPECTED_TARGET.with(|c| c.get()).expect("EXPECTED_TARGET should be set");
  assert_eq!(args[0], Value::Object(expected_target));

  let Value::String(key_s) = args[1] else {
    return Err(VmError::TypeError("expected string key in Proxy get trap"));
  };
  assert_eq!(
    scope.heap().get_string(key_s)?.to_utf8_lossy(),
    "handleEvent".to_string()
  );

  let expected_proxy = EXPECTED_PROXY.with(|c| c.get()).expect("EXPECTED_PROXY should be set");
  assert_eq!(args[2], Value::Object(expected_proxy));

  // Force a GC while the trap is running to stress rooting.
  scope.heap_mut().collect_garbage();

  let method = HANDLE_EVENT_FN
    .with(|m| m.get())
    .expect("HANDLE_EVENT_FN should be set");
  Ok(Value::Object(method))
}

fn handle_event(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  HANDLE_EVENT_CALLS.with(|c| c.set(c.get() + 1));

  let expected_proxy = EXPECTED_PROXY.with(|c| c.get()).expect("EXPECTED_PROXY should be set");
  assert_eq!(this, Value::Object(expected_proxy));

  assert_eq!(args, &[Value::Number(123.0)]);

  // Stress rooting: ensure `this` survives a GC during callback execution.
  scope.heap_mut().collect_garbage();

  Ok(Value::Number(9.0))
}

fn alloc_proxy_callback_interface(
  vm: &mut Vm,
  heap: &mut Heap,
) -> Result<GcObject, VmError> {
  let mut scope = heap.scope();

  let handle_event_id = vm.register_native_call(handle_event)?;
  let handle_event_name = scope.alloc_string("handleEvent")?;
  scope.push_root(Value::String(handle_event_name))?;
  let handle_event_fn = scope.alloc_native_function(handle_event_id, None, handle_event_name, 1)?;
  scope.push_root(Value::Object(handle_event_fn))?;
  HANDLE_EVENT_FN.with(|m| m.set(Some(handle_event_fn)));

  let get_id = vm.register_native_call(proxy_get_trap)?;
  let get_name = scope.alloc_string("get")?;
  scope.push_root(Value::String(get_name))?;
  let get_fn = scope.alloc_native_function(get_id, None, get_name, 3)?;
  scope.push_root(Value::Object(get_fn))?;

  // Target object is empty; the get trap supplies the method.
  let target = scope.alloc_object()?;
  scope.push_root(Value::Object(target))?;
  EXPECTED_TARGET.with(|c| c.set(Some(target)));

  let handler = scope.alloc_object()?;
  scope.push_root(Value::Object(handler))?;

  // handler.get = <native get trap>
  let get_key = vm_js::PropertyKey::from_string(get_name);
  scope.create_data_property_or_throw(handler, get_key, Value::Object(get_fn))?;

  // Keep the method function alive across GCs by storing it on the handler object, which is
  // reachable from the Proxy.
  let method_key_s = scope.alloc_string("handleEventFn")?;
  scope.push_root(Value::String(method_key_s))?;
  let method_key = vm_js::PropertyKey::from_string(method_key_s);
  scope.create_data_property_or_throw(handler, method_key, Value::Object(handle_event_fn))?;

  let proxy = scope.alloc_proxy(Some(target), Some(handler))?;
  scope.push_root(Value::Object(proxy))?;
  EXPECTED_PROXY.with(|c| c.set(Some(proxy)));

  Ok(proxy)
}

#[test]
fn invoke_callback_interface_respects_proxy_get_trap() -> Result<(), VmError> {
  GET_TRAP_CALLS.with(|c| c.set(0));
  HANDLE_EVENT_CALLS.with(|c| c.set(0));

  let mut vm = Vm::new(VmOptions::default());
  // Stress rooting: force a GC before each allocation.
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 0));
  let proxy = alloc_proxy_callback_interface(&mut vm, &mut heap)?;

  let mut scope = heap.scope();
  let mut hooks = MicrotaskQueue::new();

  let out = invoke_callback_interface(
    &mut vm,
    &mut scope,
    &mut hooks,
    Value::Object(proxy),
    Value::Undefined,
    &[Value::Number(123.0)],
  )?;
  assert_eq!(out, Value::Number(9.0));

  assert_eq!(GET_TRAP_CALLS.with(|c| c.get()), 1);
  assert_eq!(HANDLE_EVENT_CALLS.with(|c| c.get()), 1);
  Ok(())
}

#[test]
fn callback_handle_invocation_respects_proxy_get_trap() -> Result<(), VmError> {
  GET_TRAP_CALLS.with(|c| c.set(0));
  HANDLE_EVENT_CALLS.with(|c| c.set(0));

  let mut vm = Vm::new(VmOptions::default());
  // Stress rooting: force a GC before each allocation.
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 0));
  let proxy = alloc_proxy_callback_interface(&mut vm, &mut heap)?;

  // Creating the handle performs a `GetMethod` check; ignore those trap calls for this test.
  let handle = CallbackHandle::from_callback_interface(&mut vm, &mut heap, Value::Object(proxy), false)?
    .expect("expected callback interface handle");
  GET_TRAP_CALLS.with(|c| c.set(0));
  HANDLE_EVENT_CALLS.with(|c| c.set(0));

  let mut host = ();
  let mut hooks = MicrotaskQueue::new();
  let out = handle.invoke_with_this(
    &mut vm,
    &mut heap,
    &mut host,
    &mut hooks,
    Value::Undefined,
    &[Value::Number(123.0)],
  )?;
  handle.unroot(&mut heap);

  assert_eq!(out, Value::Number(9.0));
  assert_eq!(GET_TRAP_CALLS.with(|c| c.get()), 1);
  assert_eq!(HANDLE_EVENT_CALLS.with(|c| c.get()), 1);
  Ok(())
}
