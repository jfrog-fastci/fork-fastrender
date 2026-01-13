use std::any::Any;

use vm_js::{
  GcObject, Heap, HeapLimits, Job, PropertyKey, Realm, RealmId, Value, Vm, VmError, VmHostHooks,
  VmOptions,
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
