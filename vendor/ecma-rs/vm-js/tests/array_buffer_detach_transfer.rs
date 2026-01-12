use vm_js::{Heap, HeapLimits, PropertyKey, PropertyKind, Realm, Value, Vm, VmError, VmOptions};

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

fn get_number(v: Value) -> Result<f64, VmError> {
  match v {
    Value::Number(n) => Ok(n),
    _ => Err(VmError::Unimplemented("expected number")),
  }
}

#[test]
fn transfer_detaches_array_buffer_and_typed_array_views_become_oob() -> Result<(), VmError> {
  let mut rt = TestRt::new(HeapLimits::new(1024 * 1024, 1024 * 1024))?;
  let intr = *rt.realm.intrinsics();

  let mut scope = rt.heap.scope();

  let ab = scope.alloc_array_buffer_from_u8_vec(vec![1, 2, 3, 4])?;
  scope
    .heap_mut()
    .object_set_prototype(ab, Some(intr.array_buffer_prototype()))?;
  scope.push_root(Value::Object(ab))?;

  let view = scope.alloc_uint8_array(ab, 0, 4)?;
  scope
    .heap_mut()
    .object_set_prototype(view, Some(intr.uint8_array_prototype()))?;
  scope.push_root(Value::Object(view))?;

  // Sanity-check integer indexed exotic behaviour before detachment.
  let key0 = PropertyKey::from_string(scope.alloc_string("0")?);
  let Some(desc) = scope.heap().object_get_own_property(view, &key0)? else {
    return Err(VmError::Unimplemented(
      "Uint8Array[0] missing before detachment",
    ));
  };
  let PropertyKind::Data { value, .. } = desc.kind else {
    return Err(VmError::Unimplemented("Uint8Array[0] not a data descriptor"));
  };
  assert_eq!(value, Value::Number(1.0));

  let external_before = scope.heap().external_bytes();

  let transferred = scope.heap_mut().transfer_array_buffer(ab)?;
  scope
    .heap_mut()
    .object_set_prototype(transferred, Some(intr.array_buffer_prototype()))?;
  scope.push_root(Value::Object(transferred))?;

  // Transferring should not double-count external bytes.
  assert_eq!(scope.heap().external_bytes(), external_before);

  // `ab` becomes detached.
  assert!(scope.heap().is_detached_array_buffer(ab)?);

  // The returned ArrayBuffer should contain the original bytes.
  assert_eq!(scope.heap().array_buffer_data(transferred)?, &[1, 2, 3, 4]);

  // JS-visible getters should reflect the detached state.
  let byte_length_key = PropertyKey::from_string(scope.alloc_string("byteLength")?);
  let old_len = scope.heap_mut().ordinary_get(
    &mut rt.vm,
    ab,
    byte_length_key,
    Value::Object(ab),
  )?;
  assert_eq!(get_number(old_len)?, 0.0);

  let new_len = scope.heap_mut().ordinary_get(
    &mut rt.vm,
    transferred,
    byte_length_key,
    Value::Object(transferred),
  )?;
  assert_eq!(get_number(new_len)?, 4.0);

  let length_key = PropertyKey::from_string(scope.alloc_string("length")?);
  let view_len = scope.heap_mut().ordinary_get(
    &mut rt.vm,
    view,
    length_key,
    Value::Object(view),
  )?;
  assert_eq!(get_number(view_len)?, 0.0);

  let view_byte_len = scope.heap_mut().ordinary_get(
    &mut rt.vm,
    view,
    byte_length_key,
    Value::Object(view),
  )?;
  assert_eq!(get_number(view_byte_len)?, 0.0);

  let byte_offset_key = PropertyKey::from_string(scope.alloc_string("byteOffset")?);
  let view_byte_offset = scope.heap_mut().ordinary_get(
    &mut rt.vm,
    view,
    byte_offset_key,
    Value::Object(view),
  )?;
  assert_eq!(get_number(view_byte_offset)?, 0.0);

  // Integer-indexed properties should behave out-of-bounds (no own property).
  assert!(scope.heap().object_get_own_property(view, &key0)?.is_none());

  Ok(())
}
