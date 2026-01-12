use vm_js::{Heap, HeapLimits, PropertyKey, Realm, Value, Vm, VmError, VmOptions};

struct TestRt {
  vm: Vm,
  heap: Heap,
  realm: Realm,
}

impl TestRt {
  fn new(limits: HeapLimits) -> Result<Self, VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(limits);
    let realm = Realm::new(&mut vm, &mut heap)?;
    Ok(Self { vm, heap, realm })
  }
}

impl Drop for TestRt {
  fn drop(&mut self) {
    self.realm.teardown(&mut self.heap);
  }
}

#[test]
fn array_buffer_prototype_detached_distinguishes_detached_from_zero_length() -> Result<(), VmError> {
  let mut rt = TestRt::new(HeapLimits::new(1024 * 1024, 1024 * 1024))?;
  let intr = *rt.realm.intrinsics();

  let mut scope = rt.heap.scope();

  // Ensure the property exists.
  let detached_s = scope.alloc_string("detached")?;
  scope.push_root(Value::String(detached_s))?;
  let detached_key = PropertyKey::from_string(detached_s);
  assert!(scope
    .heap()
    .has_property(intr.array_buffer_prototype(), &detached_key)?);

  // `new ArrayBuffer(0)` is not detached.
  let ab_ctor = Value::Object(intr.array_buffer());
  let ab = rt
    .vm
    .construct_without_host(&mut scope, ab_ctor, &[Value::Number(0.0)], ab_ctor)?;
  let Value::Object(ab_obj) = ab else {
    return Err(VmError::InvariantViolation(
      "ArrayBuffer constructor did not return an object",
    ));
  };
  scope.push_root(Value::Object(ab_obj))?;
  assert_eq!(
    scope.ordinary_get(&mut rt.vm, ab_obj, detached_key, Value::Object(ab_obj))?,
    Value::Bool(false)
  );

  // Detaching a non-zero-length buffer should zero its `byteLength` but `detached` becomes true.
  let ab2 = rt
    .vm
    .construct_without_host(&mut scope, ab_ctor, &[Value::Number(4.0)], ab_ctor)?;
  let Value::Object(ab2_obj) = ab2 else {
    return Err(VmError::InvariantViolation(
      "ArrayBuffer constructor did not return an object",
    ));
  };
  scope.push_root(Value::Object(ab2_obj))?;

  // Detach via host API.
  let _ = scope.heap_mut().detach_array_buffer_take_data(ab2_obj)?;

  let byte_length_s = scope.alloc_string("byteLength")?;
  scope.push_root(Value::String(byte_length_s))?;
  let byte_length_key = PropertyKey::from_string(byte_length_s);
  assert_eq!(
    scope.ordinary_get(&mut rt.vm, ab2_obj, byte_length_key, Value::Object(ab2_obj))?,
    Value::Number(0.0)
  );
  assert_eq!(
    scope.ordinary_get(&mut rt.vm, ab2_obj, detached_key, Value::Object(ab2_obj))?,
    Value::Bool(true)
  );

  Ok(())
}

