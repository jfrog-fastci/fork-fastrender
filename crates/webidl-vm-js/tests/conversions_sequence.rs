use std::any::Any;

use vm_js::{GcObject, Heap, HeapLimits, Job, PropertyKey, Realm, RealmId, Value, Vm, VmError, VmHostHooks, VmOptions};

use webidl_vm_js::bindings_runtime::BindingsRuntime;
use webidl_vm_js::conversions;

struct DummyHooks;

impl VmHostHooks for DummyHooks {
  fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}

  fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
    None
  }
}

fn alloc_key(scope: &mut vm_js::Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
}

fn assert_thrown_error(
  rt: &mut BindingsRuntime<'_>,
  realm: &Realm,
  err: VmError,
  expected_proto: GcObject,
  expected_name: &str,
  expected_message: &str,
) -> Result<(), VmError> {
  let thrown = err.thrown_value().expect("expected a thrown exception value");
  let Value::Object(obj) = thrown else {
    return Err(VmError::TypeError("expected thrown error to be an object"));
  };
  assert_eq!(rt.scope.object_get_prototype(obj)?, Some(expected_proto));

  rt.scope.push_root(thrown)?;
  let name_key = alloc_key(&mut rt.scope, "name")?;
  let message_key = alloc_key(&mut rt.scope, "message")?;
  let name_val = rt.scope.heap().get(obj, &name_key)?;
  let message_val = rt.scope.heap().get(obj, &message_key)?;

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
fn sequence_conversion_materializes_array_and_enforces_limits() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut scope = heap.scope();
  let mut dummy_host = ();
  let mut hooks = DummyHooks;
  let mut rt = BindingsRuntime::from_scope(&mut vm, scope.reborrow());

  // ---- basic conversion ----
  let input = rt.alloc_array(3)?;
  rt.scope.push_root(Value::Object(input))?;
  for (idx, n) in [1.0, 2.0, 3.0].into_iter().enumerate() {
    let key = alloc_key(&mut rt.scope, &idx.to_string())?;
    rt.scope.create_data_property_or_throw(input, key, Value::Number(n))?;
  }

  let out = conversions::to_iterable_list(
    &mut rt,
    &mut dummy_host,
    &mut hooks,
    Value::Object(input),
    "expected object for sequence",
    |_rt, _host, _hooks, v| match v {
      Value::Number(n) => Ok(Value::Number(n * 2.0)),
      other => Ok(other),
    },
  )?;

  let Value::Object(out_obj) = out else {
    return Err(VmError::TypeError("expected object result from sequence conversion"));
  };
  rt.scope.push_root(Value::Object(out_obj))?;

  // Output should be an Array exotic object.
  let intr = realm.intrinsics();
  assert_eq!(
    rt.scope.object_get_prototype(out_obj)?,
    Some(intr.array_prototype())
  );

  let len_key = alloc_key(&mut rt.scope, "length")?;
  let len = rt
    .scope
    .heap()
    .object_get_own_data_property_value(out_obj, &len_key)?
    .unwrap_or(Value::Undefined);
  assert_eq!(len, Value::Number(3.0));

  for (idx, expected) in [2.0, 4.0, 6.0].into_iter().enumerate() {
    let key = alloc_key(&mut rt.scope, &idx.to_string())?;
    let v = rt
      .scope
      .heap()
      .object_get_own_data_property_value(out_obj, &key)?
      .unwrap_or(Value::Undefined);
    assert_eq!(v, Value::Number(expected));
  }

  // ---- non-object inputs throw TypeError ----
  let err = conversions::to_iterable_list(
    &mut rt,
    &mut dummy_host,
    &mut hooks,
    Value::Number(1.0),
    "expected object for sequence",
    |_rt, _host, _hooks, v| Ok(v),
  )
  .expect_err("expected non-object to fail sequence conversion");

  assert_thrown_error(
    &mut rt,
    &realm,
    err,
    intr.type_error_prototype(),
    "TypeError",
    "expected object for sequence",
  )?;

  // ---- length limit throws RangeError ----
  rt.set_limits(webidl::WebIdlLimits {
    max_sequence_length: 1,
    ..Default::default()
  });

  let input2 = rt.alloc_array(2)?;
  rt.scope.push_root(Value::Object(input2))?;
  for (idx, n) in [1.0, 2.0].into_iter().enumerate() {
    let key = alloc_key(&mut rt.scope, &idx.to_string())?;
    rt.scope.create_data_property_or_throw(input2, key, Value::Number(n))?;
  }

  let err = conversions::to_iterable_list(
    &mut rt,
    &mut dummy_host,
    &mut hooks,
    Value::Object(input2),
    "expected object for sequence",
    |_rt, _host, _hooks, v| Ok(v),
  )
  .expect_err("expected sequence length limit to fail");

  assert_thrown_error(
    &mut rt,
    &realm,
    err,
    intr.range_error_prototype(),
    "RangeError",
    "sequence exceeds maximum length",
  )?;

  drop(rt);
  drop(scope);
  realm.teardown(&mut heap);
  Ok(())
}
