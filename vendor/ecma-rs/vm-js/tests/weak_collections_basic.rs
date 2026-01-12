use vm_js::{
  GcObject, Heap, HeapLimits, JsRuntime, PropertyKey, PropertyKind, Realm, Scope, Value, Vm, VmError,
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

fn string_key(scope: &mut Scope<'_>, s: &str) -> Result<PropertyKey, VmError> {
  let key_s = scope.alloc_string(s)?;
  scope.push_root(Value::String(key_s))?;
  Ok(PropertyKey::from_string(key_s))
}

#[test]
fn weak_map_and_weak_set_construct() -> Result<(), VmError> {
  let mut rt = TestRt::new(HeapLimits::new(1024 * 1024, 1024 * 1024))?;
  let intr = *rt.realm.intrinsics();

  let mut scope = rt.heap.scope();

  let weak_map_ctor = Value::Object(intr.weak_map());
  let weak_map = rt
    .vm
    .construct_without_host(&mut scope, weak_map_ctor, &[], weak_map_ctor)?;
  assert!(matches!(weak_map, Value::Object(_)));

  let weak_set_ctor = Value::Object(intr.weak_set());
  let weak_set = rt
    .vm
    .construct_without_host(&mut scope, weak_set_ctor, &[], weak_set_ctor)?;
  assert!(matches!(weak_set, Value::Object(_)));

  Ok(())
}

#[test]
fn weak_collection_primitive_key_semantics() -> Result<(), VmError> {
  let mut rt = TestRt::new(HeapLimits::new(1024 * 1024, 1024 * 1024))?;
  let intr = *rt.realm.intrinsics();

  let mut scope = rt.heap.scope();

  // WeakMap
  let weak_map_ctor = Value::Object(intr.weak_map());
  let weak_map = rt
    .vm
    .construct_without_host(&mut scope, weak_map_ctor, &[], weak_map_ctor)?;
  let Value::Object(weak_map_obj) = weak_map else {
    return Err(VmError::Unimplemented("WeakMap constructor did not return object"));
  };

  let set = get_data_property(&mut scope, weak_map_obj, "set")?.unwrap();
  let err = rt
    .vm
    .call_without_host(&mut scope, set, weak_map, &[Value::Number(1.0), Value::Number(2.0)])
    .unwrap_err();
  let thrown = err.thrown_value().ok_or(VmError::Unimplemented(
    "WeakMap.prototype.set did not throw a JS value",
  ))?;
  let Value::Object(thrown_obj) = thrown else {
    return Err(VmError::Unimplemented(
      "WeakMap.prototype.set did not throw an object",
    ));
  };
  assert_eq!(
    scope.heap().object_prototype(thrown_obj)?,
    Some(intr.type_error_prototype())
  );

  let get = get_data_property(&mut scope, weak_map_obj, "get")?.unwrap();
  let out = rt
    .vm
    .call_without_host(&mut scope, get, weak_map, &[Value::Number(1.0)])?;
  assert_eq!(out, Value::Undefined);

  let has = get_data_property(&mut scope, weak_map_obj, "has")?.unwrap();
  let out = rt
    .vm
    .call_without_host(&mut scope, has, weak_map, &[Value::Number(1.0)])?;
  assert_eq!(out, Value::Bool(false));

  let delete = get_data_property(&mut scope, weak_map_obj, "delete")?.unwrap();
  let out = rt
    .vm
    .call_without_host(&mut scope, delete, weak_map, &[Value::Number(1.0)])?;
  assert_eq!(out, Value::Bool(false));

  // WeakSet
  let weak_set_ctor = Value::Object(intr.weak_set());
  let weak_set = rt
    .vm
    .construct_without_host(&mut scope, weak_set_ctor, &[], weak_set_ctor)?;
  let Value::Object(weak_set_obj) = weak_set else {
    return Err(VmError::Unimplemented("WeakSet constructor did not return object"));
  };

  let add = get_data_property(&mut scope, weak_set_obj, "add")?.unwrap();
  let err = rt
    .vm
    .call_without_host(&mut scope, add, weak_set, &[Value::Number(1.0)])
    .unwrap_err();
  let thrown = err.thrown_value().ok_or(VmError::Unimplemented(
    "WeakSet.prototype.add did not throw a JS value",
  ))?;
  let Value::Object(thrown_obj) = thrown else {
    return Err(VmError::Unimplemented(
      "WeakSet.prototype.add did not throw an object",
    ));
  };
  assert_eq!(
    scope.heap().object_prototype(thrown_obj)?,
    Some(intr.type_error_prototype())
  );

  let has = get_data_property(&mut scope, weak_set_obj, "has")?.unwrap();
  let out = rt
    .vm
    .call_without_host(&mut scope, has, weak_set, &[Value::Number(1.0)])?;
  assert_eq!(out, Value::Bool(false));

  let delete = get_data_property(&mut scope, weak_set_obj, "delete")?.unwrap();
  let out = rt
    .vm
    .call_without_host(&mut scope, delete, weak_set, &[Value::Number(1.0)])?;
  assert_eq!(out, Value::Bool(false));

  Ok(())
}

#[test]
fn weak_map_iterable_constructor_with_array() -> Result<(), VmError> {
  let mut rt = TestRt::new(HeapLimits::new(1024 * 1024, 1024 * 1024))?;
  let intr = *rt.realm.intrinsics();

  let mut scope = rt.heap.scope();

  // key = {}
  let key = scope.alloc_object()?;
  scope.push_root(Value::Object(key))?;
  scope
    .heap_mut()
    .object_set_prototype(key, Some(intr.object_prototype()))?;

  // entry = [key, 123]
  let entry = scope.alloc_array(2)?;
  scope.push_root(Value::Object(entry))?;
  scope
    .heap_mut()
    .object_set_prototype(entry, Some(intr.array_prototype()))?;
  let entry_0 = string_key(&mut scope, "0")?;
  let entry_1 = string_key(&mut scope, "1")?;
  scope.create_data_property_or_throw(entry, entry_0, Value::Object(key))?;
  scope.create_data_property_or_throw(entry, entry_1, Value::Number(123.0))?;

  // iterable = [entry]
  let iterable = scope.alloc_array(1)?;
  scope.push_root(Value::Object(iterable))?;
  scope
    .heap_mut()
    .object_set_prototype(iterable, Some(intr.array_prototype()))?;
  let iterable_0 = string_key(&mut scope, "0")?;
  scope.create_data_property_or_throw(iterable, iterable_0, Value::Object(entry))?;

  // new WeakMap([[key, 123]]).get(key) === 123
  let weak_map_ctor = Value::Object(intr.weak_map());
  let weak_map = rt.vm.construct_without_host(
    &mut scope,
    weak_map_ctor,
    &[Value::Object(iterable)],
    weak_map_ctor,
  )?;
  let Value::Object(weak_map_obj) = weak_map else {
    return Err(VmError::Unimplemented("WeakMap constructor did not return object"));
  };

  let get = get_data_property(&mut scope, weak_map_obj, "get")?.unwrap();
  let out = rt
    .vm
    .call_without_host(&mut scope, get, weak_map, &[Value::Object(key)])?;
  assert_eq!(out, Value::Number(123.0));

  Ok(())
}

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn weak_map_and_weak_set_prototype_methods_have_brand_checks() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function isTypeError(thunk) {
        try { thunk(); return false; } catch (e) { return e instanceof TypeError; }
      }
      var ok =
        isTypeError(function () { WeakMap.prototype.get.call({}, {}); }) &&
        isTypeError(function () { WeakMap.prototype.set.call({}, {}, 1); }) &&
        isTypeError(function () { WeakMap.prototype.has.call({}, {}); }) &&
        isTypeError(function () { WeakMap.prototype.delete.call({}, {}); }) &&
        isTypeError(function () { WeakSet.prototype.add.call({}, {}); }) &&
        isTypeError(function () { WeakSet.prototype.has.call({}, {}); }) &&
        isTypeError(function () { WeakSet.prototype.delete.call({}, {}); });
      ok;
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn weak_map_and_weak_set_objects_support_ordinary_object_operations() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function check(obj, proto) {
        obj.x = 1;
        Object.defineProperty(obj, "y", { value: 2, enumerable: true, configurable: true, writable: true });
        var ok = obj.x === 1 && obj.y === 2;
        ok = ok && Object.keys(obj).join(",") === "x,y";
        ok = ok && Object.getPrototypeOf(obj) === proto;
        Object.setPrototypeOf(obj, null);
        ok = ok && Object.getPrototypeOf(obj) === null;
        Object.setPrototypeOf(obj, proto);
        ok = ok && Object.getPrototypeOf(obj) === proto;
        obj.z = 3;
        ok = ok && obj.z === 3;
        delete obj.y;
        ok = ok && obj.y === undefined && Object.keys(obj).join(",") === "x,z";
        return ok;
      }

      var ok = check(new WeakMap(), WeakMap.prototype) && check(new WeakSet(), WeakSet.prototype);
      ok;
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn weak_map_entry_growth_is_accounted_and_respects_heap_limits() -> Result<(), VmError> {
  const N: usize = 1024;

  // First, measure the bytes needed to allocate keys and then add N WeakMap entries.
  let (bytes_after_keys, bytes_after_inserts) = {
    let mut heap = Heap::new(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
    let mut scope = heap.scope();

    let wm = scope.alloc_weak_map()?;
    scope.push_root(Value::Object(wm))?;

    let mut keys: Vec<Value> = Vec::new();
    keys.try_reserve_exact(N).map_err(|_| VmError::OutOfMemory)?;
    for _ in 0..N {
      let key = scope.alloc_object()?;
      keys.push(Value::Object(key));
    }
    scope.push_roots(&keys)?;

    let bytes_after_keys = scope.heap().estimated_total_bytes();

    for value in &keys {
      let Value::Object(key) = *value else {
        unreachable!();
      };
      scope
        .heap_mut()
        .weak_map_set_with_tick(wm, key, Value::Number(0.0), || Ok(()))?;
    }

    let bytes_after_inserts = scope.heap().estimated_total_bytes();
    (bytes_after_keys, bytes_after_inserts)
  };

  assert!(
    bytes_after_inserts > bytes_after_keys,
    "expected WeakMap entry storage to increase heap bytes (after_keys={bytes_after_keys}, after_inserts={bytes_after_inserts})"
  );

  // Set a heap limit between the two values so we can allocate the keys, but cannot grow the WeakMap
  // all the way to N entries.
  let growth = bytes_after_inserts.saturating_sub(bytes_after_keys);
  let max_bytes = bytes_after_keys
    .saturating_add(growth / 2)
    .saturating_add(4096);

  let mut heap = Heap::new(HeapLimits::new(max_bytes, max_bytes));
  let mut scope = heap.scope();

  let wm = scope.alloc_weak_map()?;
  scope.push_root(Value::Object(wm))?;

  let mut keys: Vec<Value> = Vec::new();
  keys.try_reserve_exact(N).map_err(|_| VmError::OutOfMemory)?;
  for _ in 0..N {
    let key = scope.alloc_object()?;
    keys.push(Value::Object(key));
  }
  scope.push_roots(&keys)?;

  let mut inserted = 0usize;
  let mut saw_oom = false;
  for value in keys {
    let Value::Object(key) = value else {
      unreachable!();
    };
    match scope
      .heap_mut()
      .weak_map_set_with_tick(wm, key, Value::Number(0.0), || Ok(()))
    {
      Ok(()) => inserted += 1,
      Err(VmError::OutOfMemory) => {
        saw_oom = true;
        break;
      }
      Err(e) => return Err(e),
    }
  }

  assert!(
    saw_oom,
    "expected WeakMap growth to eventually hit VmError::OutOfMemory (inserted={inserted}, max_bytes={max_bytes}, estimated_total_bytes={})",
    scope.heap().estimated_total_bytes()
  );
  assert!(inserted > 0, "expected to insert at least one entry before OOM");
  assert!(inserted < N, "expected OOM before inserting all entries");

  // Allow a small epsilon since `estimated_total_bytes` includes vector capacities/rounding and the
  // limit check is based on an estimate.
  let epsilon = 4096;
  assert!(
    scope.heap().estimated_total_bytes() <= max_bytes + epsilon,
    "heap.estimated_total_bytes should be bounded by max_bytes (max_bytes={max_bytes}, estimated_total_bytes={}, epsilon={epsilon})",
    scope.heap().estimated_total_bytes(),
  );
  Ok(())
}

#[test]
fn weak_set_entry_growth_is_accounted_and_respects_heap_limits() -> Result<(), VmError> {
  const N: usize = 1024;

  // First, measure the bytes needed to allocate keys and then add N WeakSet entries.
  let (bytes_after_keys, bytes_after_inserts) = {
    let mut heap = Heap::new(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
    let mut scope = heap.scope();

    let ws = scope.alloc_weak_set()?;
    scope.push_root(Value::Object(ws))?;

    let mut keys: Vec<Value> = Vec::new();
    keys.try_reserve_exact(N).map_err(|_| VmError::OutOfMemory)?;
    for _ in 0..N {
      let key = scope.alloc_object()?;
      keys.push(Value::Object(key));
    }
    scope.push_roots(&keys)?;

    let bytes_after_keys = scope.heap().estimated_total_bytes();

    for value in &keys {
      let Value::Object(key) = *value else {
        unreachable!();
      };
      scope.heap_mut().weak_set_add_with_tick(ws, key, || Ok(()))?;
    }

    let bytes_after_inserts = scope.heap().estimated_total_bytes();
    (bytes_after_keys, bytes_after_inserts)
  };

  assert!(
    bytes_after_inserts > bytes_after_keys,
    "expected WeakSet entry storage to increase heap bytes (after_keys={bytes_after_keys}, after_inserts={bytes_after_inserts})"
  );

  // Set a heap limit between the two values so we can allocate the keys, but cannot grow the WeakSet
  // all the way to N entries.
  let growth = bytes_after_inserts.saturating_sub(bytes_after_keys);
  let max_bytes = bytes_after_keys
    .saturating_add(growth / 2)
    .saturating_add(4096);

  let mut heap = Heap::new(HeapLimits::new(max_bytes, max_bytes));
  let mut scope = heap.scope();

  let ws = scope.alloc_weak_set()?;
  scope.push_root(Value::Object(ws))?;

  let mut keys: Vec<Value> = Vec::new();
  keys.try_reserve_exact(N).map_err(|_| VmError::OutOfMemory)?;
  for _ in 0..N {
    let key = scope.alloc_object()?;
    keys.push(Value::Object(key));
  }
  scope.push_roots(&keys)?;

  let mut inserted = 0usize;
  let mut saw_oom = false;
  for value in keys {
    let Value::Object(key) = value else {
      unreachable!();
    };
    match scope.heap_mut().weak_set_add_with_tick(ws, key, || Ok(())) {
      Ok(()) => inserted += 1,
      Err(VmError::OutOfMemory) => {
        saw_oom = true;
        break;
      }
      Err(e) => return Err(e),
    }
  }

  assert!(
    saw_oom,
    "expected WeakSet growth to eventually hit VmError::OutOfMemory (inserted={inserted}, max_bytes={max_bytes}, estimated_total_bytes={})",
    scope.heap().estimated_total_bytes()
  );
  assert!(inserted > 0, "expected to insert at least one entry before OOM");
  assert!(inserted < N, "expected OOM before inserting all entries");

  // Allow a small epsilon since `estimated_total_bytes` includes vector capacities/rounding and the
  // limit check is based on an estimate.
  let epsilon = 4096;
  assert!(
    scope.heap().estimated_total_bytes() <= max_bytes + epsilon,
    "heap.estimated_total_bytes should be bounded by max_bytes (max_bytes={max_bytes}, estimated_total_bytes={}, epsilon={epsilon})",
    scope.heap().estimated_total_bytes(),
  );
  Ok(())
}
