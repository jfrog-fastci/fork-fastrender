use vm_js::{
  GcObject, Heap, HeapLimits, JsRuntime, PropertyKey, Realm, Value, Vm, VmError, VmOptions,
};

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

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn get_global(rt: &mut JsRuntime, name: &str) -> Result<Option<Value>, VmError> {
  let global = rt.realm().global_object();
  let mut scope = rt.heap_mut().scope();
  let key = PropertyKey::from_string(scope.alloc_string(name)?);
  scope.heap().object_get_own_data_property_value(global, &key)
}

fn require_global_object(rt: &mut JsRuntime, name: &str) -> Result<GcObject, VmError> {
  let Some(value) = get_global(rt, name)? else {
    return Err(VmError::PropertyNotFound);
  };
  let Value::Object(obj) = value else {
    return Err(VmError::TypeError("expected object"));
  };
  Ok(obj)
}

#[test]
fn array_buffer_prototype_detached_accessor_observes_detachment() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script("globalThis.ab = new ArrayBuffer(1); ab.detached")?;
  assert_eq!(value, Value::Bool(false));

  let ab = require_global_object(&mut rt, "ab")?;
  rt.heap_mut().detach_array_buffer(ab)?;

  let value = rt.exec_script("ab.detached")?;
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script("ab.byteLength")?;
  assert_eq!(value, Value::Number(0.0));

  let value = rt.exec_script(
    r#"
      try {
        ArrayBuffer.prototype.detached.call({});
        false
      } catch (e) {
        e instanceof TypeError
      }
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));

  Ok(())
}

