use std::any::Any;

use vm_js::{
  Heap, HeapLimits, Job, PropertyKey, Realm, RealmId, Value, Vm, VmError, VmHostHooks, VmOptions,
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

#[test]
fn record_conversion_uses_to_object_and_errors_on_enumerable_symbol_keys() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut scope = heap.scope();
  let mut dummy_host = ();
  let mut hooks = DummyHooks;

  let mut rt = BindingsRuntime::from_scope(&mut vm, scope.reborrow());

  let intr = rt
    .vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;

  // ---- record conversion skips non-enumerable keys (including symbols) ----
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
    "record conversion should only include string keys"
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

  // ---- record conversion throws on enumerable symbol keys ----
  let input2 = rt.alloc_object()?;
  rt.define_data_property(
    input2,
    sym_key,
    Value::Number(2.0),
    DataPropertyAttributes::new(true, true, true),
  )?;
  let err = conversions::to_record(
    &mut rt,
    &mut dummy_host,
    &mut hooks,
    Value::Object(input2),
    "expected object for record",
    |_rt, _host, _hooks, v| Ok(v),
  )
  .expect_err("expected enumerable symbol key to throw");
  let thrown = err.thrown_value().expect("expected thrown exception value");
  let Value::Object(thrown_obj) = thrown else {
    return Err(VmError::TypeError("expected thrown error to be an object"));
  };
  assert_eq!(
    rt.scope.object_get_prototype(thrown_obj)?,
    Some(intr.type_error_prototype())
  );

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

  // ---- record conversion enforces max_record_entries ----
  rt.set_limits(webidl::WebIdlLimits {
    max_record_entries: 1,
    ..Default::default()
  });
  let input3 = rt.alloc_object()?;
  let a_key = rt.property_key("a")?;
  rt.define_data_property(
    input3,
    a_key,
    Value::Number(1.0),
    DataPropertyAttributes::new(true, true, true),
  )?;
  let b_key = rt.property_key("b")?;
  rt.define_data_property(
    input3,
    b_key,
    Value::Number(2.0),
    DataPropertyAttributes::new(true, true, true),
  )?;
  let err = conversions::to_record(
    &mut rt,
    &mut dummy_host,
    &mut hooks,
    Value::Object(input3),
    "expected object for record",
    |_rt, _host, _hooks, v| Ok(v),
  )
  .expect_err("expected record entry limit to fail");
  let thrown = err.thrown_value().expect("expected thrown error value");
  let Value::Object(thrown_obj) = thrown else {
    return Err(VmError::TypeError("expected thrown error to be an object"));
  };
  assert_eq!(
    rt.scope.object_get_prototype(thrown_obj)?,
    Some(intr.range_error_prototype())
  );

  drop(rt);
  drop(scope);
  realm.teardown(&mut heap);
  Ok(())
}
