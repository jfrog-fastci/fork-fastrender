use vm_js::{
  GcObject, Heap, HeapLimits, PropertyKey, PropertyKind, Realm, Scope, Value, Vm, VmError, VmOptions,
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

fn get_accessor_getter(scope: &mut Scope<'_>, obj: GcObject, name: &str) -> Result<GcObject, VmError> {
  let key = PropertyKey::from_string(scope.alloc_string(name)?);
  let Some(desc) = scope.heap().get_property(obj, &key)? else {
    return Err(VmError::Unimplemented("missing accessor property"));
  };
  match desc.kind {
    PropertyKind::Accessor {
      get: Value::Object(get),
      ..
    } => Ok(get),
    _ => Err(VmError::Unimplemented("property is not an accessor")),
  }
}

fn call_size_getter(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  receiver: Value,
  proto: GcObject,
) -> Result<usize, VmError> {
  let getter = get_accessor_getter(scope, proto, "size")?;
  let out = vm.call_without_host(scope, Value::Object(getter), receiver, &[])?;
  let Value::Number(n) = out else {
    return Err(VmError::Unimplemented("size getter did not return number"));
  };
  Ok(n as usize)
}

fn iterator_next(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  iter: Value,
) -> Result<(Value, bool), VmError> {
  let Value::Object(iter_obj) = iter else {
    return Err(VmError::Unimplemented("iterator is not an object"));
  };
  let next = get_data_property(scope, iter_obj, "next")?.unwrap();
  let res = vm.call_without_host(scope, next, iter, &[])?;
  let Value::Object(res_obj) = res else {
    return Err(VmError::Unimplemented("iterator.next did not return object"));
  };
  let done = get_data_property(scope, res_obj, "done")?.unwrap();
  let value = get_data_property(scope, res_obj, "value")?.unwrap();
  let Value::Bool(done) = done else {
    return Err(VmError::Unimplemented("iterator result done is not boolean"));
  };
  Ok((value, done))
}

fn object_to_string(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  intr: vm_js::Intrinsics,
  value: Value,
) -> Result<String, VmError> {
  let to_string = get_data_property(scope, intr.object_prototype(), "toString")?.unwrap();
  let call = get_data_property(scope, intr.function_prototype(), "call")?.unwrap();
  let out = vm.call_without_host(scope, call, to_string, &[value])?;
  let Value::String(s) = out else {
    return Err(VmError::Unimplemented(
      "Object.prototype.toString.call did not return string",
    ));
  };
  Ok(scope.heap().get_string(s)?.to_utf8_lossy())
}

#[test]
fn map_same_value_zero_normalizes_negative_zero_and_nans() -> Result<(), VmError> {
  let mut rt = TestRt::new(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024))?;
  let intr = *rt.realm.intrinsics();
  let mut scope = rt.heap.scope();

  let map_ctor = Value::Object(intr.map());
  let map = rt.vm.construct_without_host(&mut scope, map_ctor, &[], map_ctor)?;
  let Value::Object(map_obj) = map else {
    return Err(VmError::Unimplemented("Map constructor did not return object"));
  };

  // Insert `-0` and ensure iteration observes `+0`.
  let set = get_data_property(&mut scope, map_obj, "set")?.unwrap();
  let neg_zero = Value::Number(-0.0);
  let neg_zero_val = Value::String(scope.alloc_string("negzero")?);
  rt.vm
    .call_without_host(&mut scope, set, map, &[neg_zero, neg_zero_val])?;

  let get = get_data_property(&mut scope, map_obj, "get")?.unwrap();
  let out = rt
    .vm
    .call_without_host(&mut scope, get, map, &[Value::Number(0.0)])?;
  assert_eq!(out, neg_zero_val);

  let keys = get_data_property(&mut scope, map_obj, "keys")?.unwrap();
  let iter = rt.vm.call_without_host(&mut scope, keys, map, &[])?;
  let (k, done) = iterator_next(&mut rt.vm, &mut scope, iter)?;
  assert!(!done);
  let Value::Number(n) = k else {
    return Err(VmError::Unimplemented("Map iterator did not yield number key"));
  };
  assert_eq!(n, 0.0);
  assert!(!n.is_sign_negative(), "Map should normalize -0 to +0");

  // NaN keys use SameValueZero and do not grow the map.
  let nan = Value::Number(f64::NAN);
  let x = Value::String(scope.alloc_string("x")?);
  let y = Value::String(scope.alloc_string("y")?);
  rt.vm.call_without_host(&mut scope, set, map, &[nan, x])?;
  rt.vm.call_without_host(&mut scope, set, map, &[nan, y])?;

  let size = call_size_getter(&mut rt.vm, &mut scope, map, intr.map_prototype())?;
  assert_eq!(size, 2);

  let out = rt.vm.call_without_host(&mut scope, get, map, &[nan])?;
  assert_eq!(out, y);

  Ok(())
}

#[test]
fn set_same_value_zero_normalizes_negative_zero_and_nans() -> Result<(), VmError> {
  let mut rt = TestRt::new(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024))?;
  let intr = *rt.realm.intrinsics();
  let mut scope = rt.heap.scope();

  // Set.prototype.keys === Set.prototype.values and @@iterator === values.
  let set_proto = intr.set_prototype();
  let keys = get_data_property(&mut scope, set_proto, "keys")?.unwrap();
  let values = get_data_property(&mut scope, set_proto, "values")?.unwrap();
  assert_eq!(keys, values);

  let iter_key = PropertyKey::Symbol(intr.well_known_symbols().iterator);
  let Some(iter_desc) = scope.heap().get_property(set_proto, &iter_key)? else {
    return Err(VmError::Unimplemented("Set.prototype[@@iterator] missing"));
  };
  let PropertyKind::Data { value: iter_val, .. } = iter_desc.kind else {
    return Err(VmError::Unimplemented("Set.prototype[@@iterator] is not data"));
  };
  assert_eq!(iter_val, values);

  let set_ctor = Value::Object(intr.set());
  let set_obj_v = rt.vm.construct_without_host(&mut scope, set_ctor, &[], set_ctor)?;
  let Value::Object(set_obj) = set_obj_v else {
    return Err(VmError::Unimplemented("Set constructor did not return object"));
  };

  let add = get_data_property(&mut scope, set_obj, "add")?.unwrap();
  rt.vm.call_without_host(
    &mut scope,
    add,
    set_obj_v,
    &[Value::Number(-0.0)],
  )?;
  rt.vm.call_without_host(
    &mut scope,
    add,
    set_obj_v,
    &[Value::Number(0.0)],
  )?;

  let nan = Value::Number(f64::NAN);
  rt.vm.call_without_host(&mut scope, add, set_obj_v, &[nan])?;
  rt.vm.call_without_host(&mut scope, add, set_obj_v, &[nan])?;

  let size = call_size_getter(&mut rt.vm, &mut scope, set_obj_v, set_proto)?;
  assert_eq!(size, 2);

  // The values iterator yields +0 (not -0).
  let values_fn = values;
  let iter = rt
    .vm
    .call_without_host(&mut scope, values_fn, set_obj_v, &[])?;
  let (v, done) = iterator_next(&mut rt.vm, &mut scope, iter)?;
  assert!(!done);
  let Value::Number(n) = v else {
    return Err(VmError::Unimplemented("Set iterator did not yield number value"));
  };
  assert_eq!(n, 0.0);
  assert!(!n.is_sign_negative(), "Set should normalize -0 to +0");

  Ok(())
}

#[test]
fn map_and_set_iteration_order_and_to_string_tag() -> Result<(), VmError> {
  let mut rt = TestRt::new(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024))?;
  let intr = *rt.realm.intrinsics();
  let mut scope = rt.heap.scope();

  // --- Map iteration stability and tagging ---
  let map_ctor = Value::Object(intr.map());
  let map = rt.vm.construct_without_host(&mut scope, map_ctor, &[], map_ctor)?;
  let Value::Object(map_obj) = map else {
    return Err(VmError::Unimplemented("Map constructor did not return object"));
  };

  let map_set = get_data_property(&mut scope, map_obj, "set")?.unwrap();
  let map_delete = get_data_property(&mut scope, map_obj, "delete")?.unwrap();

  let a = Value::String(scope.alloc_string("a")?);
  let b = Value::String(scope.alloc_string("b")?);
  let c = Value::String(scope.alloc_string("c")?);
  let d = Value::String(scope.alloc_string("d")?);

  rt.vm
    .call_without_host(&mut scope, map_set, map, &[a, Value::Number(1.0)])?;
  rt.vm
    .call_without_host(&mut scope, map_set, map, &[b, Value::Number(2.0)])?;
  rt.vm
    .call_without_host(&mut scope, map_set, map, &[c, Value::Number(3.0)])?;

  let keys = get_data_property(&mut scope, map_obj, "keys")?.unwrap();
  let iter = rt.vm.call_without_host(&mut scope, keys, map, &[])?;

  let (v1, done) = iterator_next(&mut rt.vm, &mut scope, iter)?;
  assert!(!done);
  assert_eq!(v1, a);

  // Delete an entry that has not yet been visited and insert a new entry. The iterator should skip
  // the deleted entry and still visit the new one.
  rt.vm.call_without_host(&mut scope, map_delete, map, &[b])?;
  rt.vm
    .call_without_host(&mut scope, map_set, map, &[d, Value::Number(4.0)])?;

  let (v2, done) = iterator_next(&mut rt.vm, &mut scope, iter)?;
  assert!(!done);
  assert_eq!(v2, c);

  let (v3, done) = iterator_next(&mut rt.vm, &mut scope, iter)?;
  assert!(!done);
  assert_eq!(v3, d);

  let (_v4, done) = iterator_next(&mut rt.vm, &mut scope, iter)?;
  assert!(done);

  assert_eq!(
    object_to_string(&mut rt.vm, &mut scope, intr, map)?,
    "[object Map]"
  );
  assert_eq!(
    object_to_string(&mut rt.vm, &mut scope, intr, iter)?,
    "[object Map Iterator]"
  );

  // --- Set iteration stability and tagging ---
  let set_ctor = Value::Object(intr.set());
  let set = rt.vm.construct_without_host(&mut scope, set_ctor, &[], set_ctor)?;
  let Value::Object(set_obj) = set else {
    return Err(VmError::Unimplemented("Set constructor did not return object"));
  };

  let set_add = get_data_property(&mut scope, set_obj, "add")?.unwrap();
  let set_delete = get_data_property(&mut scope, set_obj, "delete")?.unwrap();
  let values = get_data_property(&mut scope, set_obj, "values")?.unwrap();

  let sa = Value::String(scope.alloc_string("a")?);
  let sb = Value::String(scope.alloc_string("b")?);
  let sc = Value::String(scope.alloc_string("c")?);
  let sd = Value::String(scope.alloc_string("d")?);

  rt.vm.call_without_host(&mut scope, set_add, set, &[sa])?;
  rt.vm.call_without_host(&mut scope, set_add, set, &[sb])?;
  rt.vm.call_without_host(&mut scope, set_add, set, &[sc])?;

  let iter = rt.vm.call_without_host(&mut scope, values, set, &[])?;
  let (v1, done) = iterator_next(&mut rt.vm, &mut scope, iter)?;
  assert!(!done);
  assert_eq!(v1, sa);

  rt.vm.call_without_host(&mut scope, set_delete, set, &[sb])?;
  rt.vm.call_without_host(&mut scope, set_add, set, &[sd])?;

  let (v2, done) = iterator_next(&mut rt.vm, &mut scope, iter)?;
  assert!(!done);
  assert_eq!(v2, sc);

  let (v3, done) = iterator_next(&mut rt.vm, &mut scope, iter)?;
  assert!(!done);
  assert_eq!(v3, sd);

  let (_v4, done) = iterator_next(&mut rt.vm, &mut scope, iter)?;
  assert!(done);

  assert_eq!(
    object_to_string(&mut rt.vm, &mut scope, intr, set)?,
    "[object Set]"
  );
  assert_eq!(
    object_to_string(&mut rt.vm, &mut scope, intr, iter)?,
    "[object Set Iterator]"
  );

  Ok(())
}
