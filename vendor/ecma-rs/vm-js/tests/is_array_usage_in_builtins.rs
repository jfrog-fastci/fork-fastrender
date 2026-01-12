use vm_js::{
  GcObject, Heap, HeapLimits, PropertyDescriptor, PropertyKey, PropertyKind, Realm, Scope, Value, Vm, VmError,
  VmOptions,
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

fn data_desc(value: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: true,
    configurable: true,
    kind: PropertyKind::Data {
      value,
      writable: true,
    },
  }
}

fn get_data_property(
  scope: &mut Scope<'_>,
  obj: GcObject,
  name: &str,
) -> Result<Option<Value>, VmError> {
  let key = PropertyKey::from_string(scope.alloc_string(name)?);
  let Some(desc) = scope.heap().get_property(obj, &key)? else {
    return Ok(None);
  };
  match desc.kind {
    PropertyKind::Data { value, .. } => Ok(Some(value)),
    PropertyKind::Accessor { .. } => Err(VmError::PropertyNotData),
  }
}

#[test]
fn array_concat_spreads_proxy_to_array() -> Result<(), VmError> {
  let mut rt = TestRt::new(HeapLimits::new(1024 * 1024, 1024 * 1024))?;
  let intr = *rt.realm.intrinsics();

  let mut scope = rt.heap.scope();

  let array_ctor = Value::Object(intr.array_constructor());

  // [].
  let empty = rt
    .vm
    .construct_without_host(&mut scope, array_ctor, &[], array_ctor)?;
  let Value::Object(empty_obj) = empty else {
    return Err(VmError::Unimplemented("Array constructor did not return object"));
  };

  // Target: [1, 2]
  let target = rt.vm.construct_without_host(
    &mut scope,
    array_ctor,
    &[Value::Number(1.0), Value::Number(2.0)],
    array_ctor,
  )?;
  let Value::Object(target_obj) = target else {
    return Err(VmError::Unimplemented("Array constructor did not return object"));
  };

  // Handler: {}
  let handler = scope.alloc_object()?;
  scope
    .heap_mut()
    .object_set_prototype(handler, Some(intr.object_prototype()))?;

  // Proxy -> target array
  let proxy = scope.alloc_proxy(target_obj, handler)?;

  // [].concat(proxy)
  let concat = get_data_property(&mut scope, empty_obj, "concat")?.unwrap();
  let out = rt
    .vm
    .call_without_host(&mut scope, concat, empty, &[Value::Object(proxy)])?;
  let Value::Object(out_obj) = out else {
    return Err(VmError::Unimplemented("Array.prototype.concat did not return object"));
  };

  assert_eq!(
    get_data_property(&mut scope, out_obj, "length")?,
    Some(Value::Number(2.0))
  );
  assert_eq!(
    get_data_property(&mut scope, out_obj, "0")?,
    Some(Value::Number(1.0))
  );
  assert_eq!(
    get_data_property(&mut scope, out_obj, "1")?,
    Some(Value::Number(2.0))
  );
  Ok(())
}

#[test]
fn array_concat_spreads_symbol_is_concat_spreadable_object() -> Result<(), VmError> {
  let mut rt = TestRt::new(HeapLimits::new(1024 * 1024, 1024 * 1024))?;
  let intr = *rt.realm.intrinsics();

  let mut scope = rt.heap.scope();

  let array_ctor = Value::Object(intr.array_constructor());

  // [].
  let empty = rt
    .vm
    .construct_without_host(&mut scope, array_ctor, &[], array_ctor)?;
  let Value::Object(empty_obj) = empty else {
    return Err(VmError::Unimplemented("Array constructor did not return object"));
  };

  // { [Symbol.isConcatSpreadable]: true, length: 1, 0: "x" }
  let spreadable_obj = scope.alloc_object()?;
  scope
    .heap_mut()
    .object_set_prototype(spreadable_obj, Some(intr.object_prototype()))?;

  let sym = intr.well_known_symbols().is_concat_spreadable;
  scope.define_property(
    spreadable_obj,
    PropertyKey::from_symbol(sym),
    data_desc(Value::Bool(true)),
  )?;

  let length_key = PropertyKey::from_string(scope.alloc_string("length")?);
  scope.define_property(spreadable_obj, length_key, data_desc(Value::Number(1.0)))?;

  // Root key/value strings across allocation so GC can't collect them before definition.
  let x = scope.alloc_string("x")?;
  scope.push_root(Value::String(x))?;
  let idx_s = scope.alloc_string("0")?;
  scope.push_root(Value::String(idx_s))?;
  let idx_key = PropertyKey::from_string(idx_s);
  scope.define_property(spreadable_obj, idx_key, data_desc(Value::String(x)))?;

  // [].concat(spreadable_obj)
  let concat = get_data_property(&mut scope, empty_obj, "concat")?.unwrap();
  let out = rt.vm.call_without_host(
    &mut scope,
    concat,
    empty,
    &[Value::Object(spreadable_obj)],
  )?;
  let Value::Object(out_obj) = out else {
    return Err(VmError::Unimplemented("Array.prototype.concat did not return object"));
  };

  assert_eq!(
    get_data_property(&mut scope, out_obj, "length")?,
    Some(Value::Number(1.0))
  );
  let Value::String(s) = get_data_property(&mut scope, out_obj, "0")?.unwrap() else {
    return Err(VmError::Unimplemented("concat element was not a string"));
  };
  assert_eq!(scope.heap().get_string(s)?.to_utf8_lossy(), "x");
  Ok(())
}

#[test]
fn array_concat_throws_on_revoked_proxy() -> Result<(), VmError> {
  let mut rt = TestRt::new(HeapLimits::new(1024 * 1024, 1024 * 1024))?;
  let intr = *rt.realm.intrinsics();

  let mut scope = rt.heap.scope();

  let array_ctor = Value::Object(intr.array_constructor());

  // [].
  let empty = rt
    .vm
    .construct_without_host(&mut scope, array_ctor, &[], array_ctor)?;
  let Value::Object(empty_obj) = empty else {
    return Err(VmError::Unimplemented("Array constructor did not return object"));
  };

  // Target: [1]
  let target = rt
    .vm
    .construct_without_host(&mut scope, array_ctor, &[Value::Number(1.0)], array_ctor)?;
  let Value::Object(target_obj) = target else {
    return Err(VmError::Unimplemented("Array constructor did not return object"));
  };

  // Handler: {}
  let handler = scope.alloc_object()?;
  scope
    .heap_mut()
    .object_set_prototype(handler, Some(intr.object_prototype()))?;

  let proxy = scope.alloc_proxy(target_obj, handler)?;
  scope.heap_mut().proxy_revoke(proxy)?;

  let concat = get_data_property(&mut scope, empty_obj, "concat")?.unwrap();
  let err = rt
    .vm
    .call_without_host(&mut scope, concat, empty, &[Value::Object(proxy)])
    .unwrap_err();
  assert!(
    matches!(err, VmError::Throw(_) | VmError::ThrowWithStack { .. }),
    "expected a thrown TypeError, got {err:?}"
  );
  Ok(())
}
