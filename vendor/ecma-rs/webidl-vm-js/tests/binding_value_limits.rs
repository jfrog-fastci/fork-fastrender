use vm_js::{GcObject, Heap, HeapLimits, PropertyKey, Realm, Scope, Value, Vm, VmError, VmOptions};

use webidl_vm_js::bindings_runtime::{BindingValue, BindingsRuntime};

fn alloc_key(scope: &mut Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
}

fn assert_thrown_range_error(
  rt: &mut BindingsRuntime<'_>,
  realm: &Realm,
  err: VmError,
  expected_message: &str,
) -> Result<(), VmError> {
  let thrown = err.thrown_value().expect("expected a thrown exception value");
  let Value::Object(obj) = thrown else {
    return Err(VmError::TypeError("expected thrown error to be an object"));
  };

  let expected_proto: GcObject = realm.intrinsics().range_error_prototype();
  assert_eq!(rt.scope.object_get_prototype(obj)?, Some(expected_proto));

  // Root the error object while allocating keys / reading properties.
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

  assert_eq!(rt.scope.heap().get_string(name_s)?.to_utf8_lossy(), "RangeError");
  assert_eq!(
    rt.scope.heap().get_string(message_s)?.to_utf8_lossy(),
    expected_message
  );

  Ok(())
}

#[test]
fn binding_value_to_js_enforces_string_length_limit() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut rt = BindingsRuntime::new(&mut vm, &mut heap);
  rt.set_limits(webidl::WebIdlLimits {
    max_string_code_units: 1,
    ..Default::default()
  });

  let err = rt
    .binding_value_to_js(BindingValue::RustString("ab".to_string()))
    .expect_err("expected string length limit to throw");

  assert_thrown_range_error(&mut rt, &realm, err, "string exceeds maximum length")?;

  drop(rt);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn binding_value_to_js_enforces_sequence_length_limit() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut rt = BindingsRuntime::new(&mut vm, &mut heap);
  rt.set_limits(webidl::WebIdlLimits {
    max_sequence_length: 1,
    ..Default::default()
  });

  let err = rt
    .binding_value_to_js(BindingValue::Sequence(vec![
      BindingValue::Undefined,
      BindingValue::Undefined,
    ]))
    .expect_err("expected sequence length limit to throw");

  assert_thrown_range_error(&mut rt, &realm, err, "sequence exceeds maximum length")?;

  drop(rt);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn binding_value_to_js_enforces_object_entry_limit() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut rt = BindingsRuntime::new(&mut vm, &mut heap);
  rt.set_limits(webidl::WebIdlLimits {
    max_record_entries: 1,
    ..Default::default()
  });

  let mut map = std::collections::BTreeMap::new();
  map.insert("a".to_string(), BindingValue::Undefined);
  map.insert("b".to_string(), BindingValue::Undefined);

  let err = rt
    .binding_value_to_js(BindingValue::Dictionary(map))
    .expect_err("expected record entry limit to throw");

  assert_thrown_range_error(
    &mut rt,
    &realm,
    err,
    "record exceeds maximum entry count",
  )?;

  drop(rt);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn alloc_array_enforces_max_sequence_length() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut rt = BindingsRuntime::new(&mut vm, &mut heap);
  rt.set_limits(webidl::WebIdlLimits {
    max_sequence_length: 1,
    ..Default::default()
  });

  let err = rt
    .alloc_array(2)
    .expect_err("expected alloc_array to enforce max_sequence_length");

  assert_thrown_range_error(&mut rt, &realm, err, "sequence exceeds maximum length")?;

  drop(rt);
  realm.teardown(&mut heap);
  Ok(())
}

