use std::any::Any;

use vm_js::{
  GcObject, Heap, HeapLimits, Job, PropertyKey, Realm, RealmId, Scope, Value, Vm, VmError, VmHost,
  VmHostHooks, VmOptions,
};

use webidl_vm_js::bindings_runtime::{BindingsRuntime, DataPropertyAttributes};
use webidl_vm_js::conversions;

struct DummyHooks;

impl VmHostHooks for DummyHooks {
  fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}

  fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
    None
  }
}

fn alloc_key(scope: &mut Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
}

fn assert_thrown_type_error(
  rt: &mut BindingsRuntime<'_>,
  realm: &Realm,
  err: VmError,
  expected_message: &str,
) -> Result<(), VmError> {
  let thrown = err.thrown_value().expect("expected a thrown exception value");
  let Value::Object(thrown_obj) = thrown else {
    return Err(VmError::TypeError("expected thrown error to be an object"));
  };
  assert_eq!(
    rt.scope.object_get_prototype(thrown_obj)?,
    Some(realm.intrinsics().type_error_prototype())
  );

  rt.scope.push_root(thrown)?;
  let name_key = alloc_key(&mut rt.scope, "name")?;
  let message_key = alloc_key(&mut rt.scope, "message")?;
  let name_val = rt.scope.heap().get(thrown_obj, &name_key)?;
  let message_val = rt.scope.heap().get(thrown_obj, &message_key)?;

  let Value::String(name_s) = name_val else {
    return Err(VmError::TypeError("expected error.name to be a string"));
  };
  let Value::String(message_s) = message_val else {
    return Err(VmError::TypeError("expected error.message to be a string"));
  };
  assert_eq!(rt.scope.heap().get_string(name_s)?.to_utf8_lossy(), "TypeError");
  assert_eq!(
    rt.scope.heap().get_string(message_s)?.to_utf8_lossy(),
    expected_message
  );

  Ok(())
}

fn noop_native_call(
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

#[test]
fn callback_interface_conversion_accepts_callable_or_handle_event_object() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut scope = heap.scope();
  let mut dummy_host = ();
  let mut hooks = DummyHooks;
  let mut rt = BindingsRuntime::from_scope(&mut vm, scope.reborrow());

  // Native callback function (callable).
  let cb = rt.alloc_native_function(noop_native_call, None, "cb", 0)?;
  let cb_val = Value::Object(cb);
  rt.scope.push_root(cb_val)?;

  let converted = conversions::to_callback_interface(&mut rt, &mut dummy_host, &mut hooks, cb_val)?;
  assert_eq!(converted, cb_val);

  // Object with callable handleEvent method.
  let listener = rt.alloc_object()?;
  rt.scope.push_root(Value::Object(listener))?;
  rt.define_data_property_str(
    listener,
    "handleEvent",
    cb_val,
    DataPropertyAttributes::new(true, true, true),
  )?;
  let listener_val = Value::Object(listener);
  let converted =
    conversions::to_callback_interface(&mut rt, &mut dummy_host, &mut hooks, listener_val)?;
  assert_eq!(converted, listener_val);

  // Object without handleEvent -> TypeError.
  let no_handle = rt.alloc_object()?;
  rt.scope.push_root(Value::Object(no_handle))?;
  let err =
    conversions::to_callback_interface(&mut rt, &mut dummy_host, &mut hooks, Value::Object(no_handle))
      .expect_err("expected missing handleEvent to fail");
  assert_thrown_type_error(
    &mut rt,
    &realm,
    err,
    "Callback interface object is missing a callable handleEvent method",
  )?;

  // Object with non-callable handleEvent -> TypeError.
  let bad_handle = rt.alloc_object()?;
  rt.scope.push_root(Value::Object(bad_handle))?;
  rt.define_data_property_str(
    bad_handle,
    "handleEvent",
    Value::Number(1.0),
    DataPropertyAttributes::new(true, true, true),
  )?;
  let err = conversions::to_callback_interface(
    &mut rt,
    &mut dummy_host,
    &mut hooks,
    Value::Object(bad_handle),
  )
  .expect_err("expected non-callable handleEvent to fail");
  assert_thrown_type_error(&mut rt, &realm, err, "GetMethod: target is not callable")?;

  // Non-object primitive -> TypeError.
  let err = conversions::to_callback_interface(&mut rt, &mut dummy_host, &mut hooks, Value::Number(1.0))
    .expect_err("expected primitive to fail callback interface conversion");
  assert_thrown_type_error(
    &mut rt,
    &realm,
    err,
    "Value is not a callable callback interface",
  )?;

  drop(rt);
  drop(scope);
  realm.teardown(&mut heap);
  Ok(())
}
