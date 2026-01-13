use std::any::Any;
use std::cell::Cell;

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

fn assert_thrown_error(
  rt: &mut BindingsRuntime<'_>,
  realm: &Realm,
  err: VmError,
  expected_proto: GcObject,
  expected_name: &str,
  expected_message: &str,
) -> Result<(), VmError> {
  let thrown = err.thrown_value().expect("expected thrown error value");
  let Value::Object(thrown_obj) = thrown else {
    return Err(VmError::TypeError("expected thrown error to be an object"));
  };
  assert_eq!(
    rt.scope.object_get_prototype(thrown_obj)?,
    Some(expected_proto)
  );

  // Root error object across property lookups / key allocations.
  rt.scope.push_root(thrown)?;
  let name_key = rt.property_key("name")?;
  let message_key = rt.property_key("message")?;
  let name_val = rt.scope.heap().get(thrown_obj, &name_key)?;
  let message_val = rt.scope.heap().get(thrown_obj, &message_key)?;

  let Value::String(name_s) = name_val else {
    return Err(VmError::TypeError("expected error.name to be a string"));
  };
  let Value::String(message_s) = message_val else {
    return Err(VmError::TypeError("expected error.message to be a string"));
  };

  assert_eq!(rt.scope.heap().get_string(name_s)?.to_utf8_lossy(), expected_name);
  assert_eq!(
    rt.scope.heap().get_string(message_s)?.to_utf8_lossy(),
    expected_message
  );

  // Keep `realm` live (it owns the intrinsics prototypes).
  let _ = realm;
  Ok(())
}

thread_local! {
  static OWN_KEYS_CALLS: Cell<u32> = const { Cell::new(0) };
  static OWN_KEYS_EXPECTED_THIS: Cell<Option<GcObject>> = const { Cell::new(None) };

  static GET_OWN_PROPERTY_DESCRIPTOR_CALLS: Cell<u32> = const { Cell::new(0) };
  static GET_OWN_PROPERTY_DESCRIPTOR_EXPECTED_THIS: Cell<Option<GcObject>> = const { Cell::new(None) };
}

fn own_keys_trap(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  OWN_KEYS_CALLS.with(|c| c.set(c.get() + 1));

  let expected = OWN_KEYS_EXPECTED_THIS
    .with(|t| t.get())
    .expect("OWN_KEYS_EXPECTED_THIS should be set");
  assert_eq!(this, Value::Object(expected));

  // Stress rooting: `own_property_keys_with_host_and_hooks` must keep any values alive across
  // allocations/trap invocation.
  scope.heap_mut().collect_garbage();

  let out = scope.alloc_array(1)?;
  scope.push_root(Value::Object(out))?;

  let idx0_s = scope.alloc_string("0")?;
  scope.push_root(Value::String(idx0_s))?;
  let idx0 = PropertyKey::from_string(idx0_s);

  let a_s = scope.alloc_string("a")?;
  scope.push_root(Value::String(a_s))?;
  scope.create_data_property_or_throw(out, idx0, Value::String(a_s))?;

  Ok(Value::Object(out))
}

fn get_own_property_descriptor_trap(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  GET_OWN_PROPERTY_DESCRIPTOR_CALLS.with(|c| c.set(c.get() + 1));

  let expected = GET_OWN_PROPERTY_DESCRIPTOR_EXPECTED_THIS
    .with(|t| t.get())
    .expect("GET_OWN_PROPERTY_DESCRIPTOR_EXPECTED_THIS should be set");
  assert_eq!(this, Value::Object(expected));

  scope.heap_mut().collect_garbage();

  // Return a data property descriptor object compatible with the target's descriptor:
  // { value: 1, writable: true, enumerable: true, configurable: true }
  let desc = scope.alloc_object()?;
  scope.push_root(Value::Object(desc))?;

  let key_value_s = scope.alloc_string("value")?;
  scope.push_root(Value::String(key_value_s))?;
  scope.create_data_property_or_throw(
    desc,
    PropertyKey::from_string(key_value_s),
    Value::Number(1.0),
  )?;

  let key_writable_s = scope.alloc_string("writable")?;
  scope.push_root(Value::String(key_writable_s))?;
  scope.create_data_property_or_throw(
    desc,
    PropertyKey::from_string(key_writable_s),
    Value::Bool(true),
  )?;

  let key_enumerable_s = scope.alloc_string("enumerable")?;
  scope.push_root(Value::String(key_enumerable_s))?;
  scope.create_data_property_or_throw(
    desc,
    PropertyKey::from_string(key_enumerable_s),
    Value::Bool(true),
  )?;

  let key_configurable_s = scope.alloc_string("configurable")?;
  scope.push_root(Value::String(key_configurable_s))?;
  scope.create_data_property_or_throw(
    desc,
    PropertyKey::from_string(key_configurable_s),
    Value::Bool(true),
  )?;

  Ok(Value::Object(desc))
}

#[test]
fn record_conversion_uses_to_object_and_ignores_non_enumerable_keys() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut scope = heap.scope();
  let mut dummy_host = ();
  let mut hooks = DummyHooks;
  let mut rt = BindingsRuntime::from_scope(&mut vm, scope.reborrow());
  let intr = realm.intrinsics();

  // ---- record conversion ignores non-enumerable keys ----
  let input = rt.alloc_object()?;

  let a_key = rt.property_key("a")?;
  rt.define_data_property(
    input,
    a_key,
    Value::Number(1.0),
    DataPropertyAttributes::new(true, true, true),
  )?;

  let hidden_key = rt.property_key("hidden")?;
  rt.define_data_property(
    input,
    hidden_key,
    Value::Number(3.0),
    DataPropertyAttributes::new(true, false, true),
  )?;

  let sym = intr.well_known_symbols().iterator;
  rt.scope.push_root(Value::Symbol(sym))?;
  let sym_key = PropertyKey::from_symbol(sym);
  rt.define_data_property(
    input,
    sym_key,
    Value::Number(2.0),
    DataPropertyAttributes::new(true, false, true),
  )?;

  let out = conversions::to_record(
    &mut rt,
    &mut dummy_host,
    &mut hooks,
    Value::Object(input),
    "expected object for record",
    |_rt, _host, _hooks, v| Ok(v),
  )?;

  let Value::Object(out_obj) = out else {
    return Err(VmError::TypeError("expected object from record conversion"));
  };
  rt.scope.push_root(Value::Object(out_obj))?;

  let keys = rt.scope.ordinary_own_property_keys(out_obj)?;
  assert_eq!(
    keys.len(),
    1,
    "record conversion should only include enumerable string keys"
  );
  let PropertyKey::String(key_s) = keys[0] else {
    return Err(VmError::TypeError(
      "expected string key in record conversion output",
    ));
  };
  assert_eq!(rt.scope.heap().get_string(key_s)?.to_utf8_lossy(), "a");

  let v = rt
    .scope
    .heap()
    .object_get_own_data_property_value(out_obj, &keys[0])?
    .unwrap_or(Value::Undefined);
  assert_eq!(v, Value::Number(1.0));

  // ---- record conversion uses `ToObject` (primitives are accepted) ----
  let out2 = conversions::to_record(
    &mut rt,
    &mut dummy_host,
    &mut hooks,
    Value::Bool(true),
    "expected object for record",
    |_rt, _host, _hooks, v| Ok(v),
  )?;
  let Value::Object(out2_obj) = out2 else {
    return Err(VmError::TypeError("expected object from record conversion"));
  };
  rt.scope.push_root(Value::Object(out2_obj))?;
  let keys = rt.scope.ordinary_own_property_keys(out2_obj)?;
  assert!(
    keys.is_empty(),
    "boxed primitive should convert to an empty record"
  );

  drop(rt);
  drop(scope);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn record_conversion_throws_on_enumerable_symbol_keys() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut scope = heap.scope();
  let mut dummy_host = ();
  let mut hooks = DummyHooks;
  let mut rt = BindingsRuntime::from_scope(&mut vm, scope.reborrow());
  let intr = realm.intrinsics();

  let input = rt.alloc_object()?;

  let a_key = rt.property_key("a")?;
  rt.define_data_property(
    input,
    a_key,
    Value::Number(1.0),
    DataPropertyAttributes::new(true, true, true),
  )?;

  let sym = intr.well_known_symbols().iterator;
  rt.scope.push_root(Value::Symbol(sym))?;
  let sym_key = PropertyKey::from_symbol(sym);
  rt.define_data_property(
    input,
    sym_key,
    Value::Number(2.0),
    DataPropertyAttributes::new(true, true, true),
  )?;

  let err = conversions::to_record(
    &mut rt,
    &mut dummy_host,
    &mut hooks,
    Value::Object(input),
    "expected object for record",
    |_rt, _host, _hooks, v| Ok(v),
  )
  .expect_err("expected enumerable symbol key to fail record conversion");

  assert_thrown_error(
    &mut rt,
    &realm,
    err,
    intr.type_error_prototype(),
    "TypeError",
    "Cannot convert a Symbol value to a string",
  )?;

  drop(rt);
  drop(scope);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn record_conversion_enforces_max_record_entries() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut scope = heap.scope();
  let mut dummy_host = ();
  let mut hooks = DummyHooks;
  let mut rt = BindingsRuntime::from_scope(&mut vm, scope.reborrow());
  let intr = realm.intrinsics();

  rt.set_limits(webidl::WebIdlLimits {
    max_record_entries: 1,
    ..Default::default()
  });

  let input = rt.alloc_object()?;

  let a_key = rt.property_key("a")?;
  rt.define_data_property(
    input,
    a_key,
    Value::Number(1.0),
    DataPropertyAttributes::new(true, true, true),
  )?;
  let b_key = rt.property_key("b")?;
  rt.define_data_property(
    input,
    b_key,
    Value::Number(2.0),
    DataPropertyAttributes::new(true, true, true),
  )?;

  let err = conversions::to_record(
    &mut rt,
    &mut dummy_host,
    &mut hooks,
    Value::Object(input),
    "expected object for record",
    |_rt, _host, _hooks, v| Ok(v),
  )
  .expect_err("expected record entry limit to fail");

  assert_thrown_error(
    &mut rt,
    &realm,
    err,
    intr.range_error_prototype(),
    "RangeError",
    "record exceeds maximum entry count",
  )?;

  drop(rt);
  drop(scope);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn record_conversion_enforces_max_string_code_units_for_keys() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut scope = heap.scope();
  let mut dummy_host = ();
  let mut hooks = DummyHooks;
  let mut rt = BindingsRuntime::from_scope(&mut vm, scope.reborrow());
  let intr = realm.intrinsics();

  rt.set_limits(webidl::WebIdlLimits {
    max_string_code_units: 1,
    ..Default::default()
  });

  let input = rt.alloc_object()?;
  let long_key = rt.property_key("ab")?;
  rt.define_data_property(
    input,
    long_key,
    Value::Number(1.0),
    DataPropertyAttributes::new(true, true, true),
  )?;

  let err = conversions::to_record(
    &mut rt,
    &mut dummy_host,
    &mut hooks,
    Value::Object(input),
    "expected object for record",
    |_rt, _host, _hooks, v| Ok(v),
  )
  .expect_err("expected string length limit to fail");

  assert_thrown_error(
    &mut rt,
    &realm,
    err,
    intr.range_error_prototype(),
    "RangeError",
    "string exceeds maximum length",
  )?;

  drop(rt);
  drop(scope);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn record_conversion_supports_proxy_forwarding() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut scope = heap.scope();
  let mut dummy_host = ();
  let mut hooks = DummyHooks;
  let mut rt = BindingsRuntime::from_scope(&mut vm, scope.reborrow());

  let target = rt.alloc_object()?;
  let a_key = rt.property_key("a")?;
  rt.define_data_property(
    target,
    a_key,
    Value::Number(1.0),
    DataPropertyAttributes::new(true, true, true),
  )?;

  let handler = rt.alloc_object()?;
  let proxy = rt.scope.alloc_proxy(Some(target), Some(handler))?;

  let out = conversions::to_record(
    &mut rt,
    &mut dummy_host,
    &mut hooks,
    Value::Object(proxy),
    "expected object for record",
    |_rt, _host, _hooks, v| Ok(v),
  )?;
  let Value::Object(out_obj) = out else {
    return Err(VmError::TypeError("expected object from record conversion"));
  };
  rt.scope.push_root(Value::Object(out_obj))?;

  let keys = rt.scope.ordinary_own_property_keys(out_obj)?;
  assert_eq!(keys.len(), 1);
  let PropertyKey::String(key_s) = keys[0] else {
    return Err(VmError::TypeError("expected string key in record conversion output"));
  };
  assert_eq!(rt.scope.heap().get_string(key_s)?.to_utf8_lossy(), "a");

  let v = rt
    .scope
    .heap()
    .object_get_own_data_property_value(out_obj, &keys[0])?
    .unwrap_or(Value::Undefined);
  assert_eq!(v, Value::Number(1.0));

  drop(rt);
  drop(scope);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn record_conversion_invokes_proxy_own_keys_trap() -> Result<(), VmError> {
  OWN_KEYS_CALLS.with(|c| c.set(0));
  OWN_KEYS_EXPECTED_THIS.with(|t| t.set(None));

  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut scope = heap.scope();
  let mut dummy_host = ();
  let mut hooks = DummyHooks;
  let mut rt = BindingsRuntime::from_scope(&mut vm, scope.reborrow());

  let target = rt.alloc_object()?;
  let a_key = rt.property_key("a")?;
  rt.define_data_property(
    target,
    a_key,
    Value::Number(1.0),
    DataPropertyAttributes::new(true, true, true),
  )?;

  let own_keys_id = rt.vm.register_native_call(own_keys_trap)?;
  let own_keys_name = rt.scope.alloc_string("ownKeys")?;
  rt.scope.push_root(Value::String(own_keys_name))?;
  let own_keys_fn = rt
    .scope
    .alloc_native_function(own_keys_id, None, own_keys_name, 0)?;
  rt.scope.push_root(Value::Object(own_keys_fn))?;

  let handler = rt.alloc_object()?;
  OWN_KEYS_EXPECTED_THIS.with(|t| t.set(Some(handler)));

  let own_keys_key = rt.property_key("ownKeys")?;
  rt.define_data_property(
    handler,
    own_keys_key,
    Value::Object(own_keys_fn),
    DataPropertyAttributes::new(true, true, true),
  )?;

  let proxy = rt.scope.alloc_proxy(Some(target), Some(handler))?;

  let out = conversions::to_record(
    &mut rt,
    &mut dummy_host,
    &mut hooks,
    Value::Object(proxy),
    "expected object for record",
    |_rt, _host, _hooks, v| Ok(v),
  )?;

  assert_eq!(OWN_KEYS_CALLS.with(|c| c.get()), 1);

  let Value::Object(out_obj) = out else {
    return Err(VmError::TypeError("expected object from record conversion"));
  };
  rt.scope.push_root(Value::Object(out_obj))?;
  let keys = rt.scope.ordinary_own_property_keys(out_obj)?;
  assert_eq!(keys.len(), 1);

  drop(rt);
  drop(scope);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn record_conversion_invokes_proxy_get_own_property_trap() -> Result<(), VmError> {
  GET_OWN_PROPERTY_DESCRIPTOR_CALLS.with(|c| c.set(0));
  GET_OWN_PROPERTY_DESCRIPTOR_EXPECTED_THIS.with(|t| t.set(None));

  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut scope = heap.scope();
  let mut dummy_host = ();
  let mut hooks = DummyHooks;
  let mut rt = BindingsRuntime::from_scope(&mut vm, scope.reborrow());

  let target = rt.alloc_object()?;
  let a_key = rt.property_key("a")?;
  rt.define_data_property(
    target,
    a_key,
    Value::Number(1.0),
    DataPropertyAttributes::new(true, true, true),
  )?;

  let gopd_id = rt.vm.register_native_call(get_own_property_descriptor_trap)?;
  let gopd_name = rt.scope.alloc_string("getOwnPropertyDescriptor")?;
  rt.scope.push_root(Value::String(gopd_name))?;
  let gopd_fn = rt
    .scope
    .alloc_native_function(gopd_id, None, gopd_name, 2)?;
  rt.scope.push_root(Value::Object(gopd_fn))?;

  let handler = rt.alloc_object()?;
  GET_OWN_PROPERTY_DESCRIPTOR_EXPECTED_THIS.with(|t| t.set(Some(handler)));

  let gopd_key = rt.property_key("getOwnPropertyDescriptor")?;
  rt.define_data_property(
    handler,
    gopd_key,
    Value::Object(gopd_fn),
    DataPropertyAttributes::new(true, true, true),
  )?;

  let proxy = rt.scope.alloc_proxy(Some(target), Some(handler))?;

  let out = conversions::to_record(
    &mut rt,
    &mut dummy_host,
    &mut hooks,
    Value::Object(proxy),
    "expected object for record",
    |_rt, _host, _hooks, v| Ok(v),
  )?;

  assert_eq!(GET_OWN_PROPERTY_DESCRIPTOR_CALLS.with(|c| c.get()), 1);

  let Value::Object(out_obj) = out else {
    return Err(VmError::TypeError("expected object from record conversion"));
  };
  rt.scope.push_root(Value::Object(out_obj))?;
  let keys = rt.scope.ordinary_own_property_keys(out_obj)?;
  assert_eq!(keys.len(), 1);

  drop(rt);
  drop(scope);
  realm.teardown(&mut heap);
  Ok(())
}
