use vm_js::{
  Budget, GcObject, Heap, HeapLimits, JsRuntime, PropertyKey, PropertyKind, Realm, Scope,
  TerminationReason, Value, Vm, VmError, VmOptions,
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

fn iterator_next(vm: &mut Vm, scope: &mut Scope<'_>, iter: Value) -> Result<(Value, bool), VmError> {
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

fn new_runtime() -> Result<JsRuntime, VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
  JsRuntime::new(vm, heap)
}

fn assert_out_of_fuel(err: VmError) {
  match err {
    VmError::Termination(term) => assert_eq!(term.reason, TerminationReason::OutOfFuel),
    other => panic!("expected VmError::Termination(OutOfFuel), got {other:?}"),
  }
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

#[test]
fn map_insertion_order_is_preserved() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let value = rt.exec_script(
    r#"
      var m = new Map();
      m.set("a", 1);
      m.set("b", 2);
      m.set("c", 3);

      // Delete then re-add moves the key to the end.
      m.delete("b");
      m.set("b", 4);

      // Updating an existing key does not change iteration order.
      m.set("a", 9);

      var out = "";
      for (var e of m) out += e[0];

      out === "acb" && m.get("a") === 9 && m.get("b") === 4 && m.size === 3
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn set_insertion_order_is_preserved() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let value = rt.exec_script(
    r#"
      var s = new Set();
      s.add("a");
      s.add("b");
      s.add("c");

      // Delete then re-add moves the value to the end.
      s.delete("b");
      s.add("b");

      // Re-adding an existing value does not change order.
      s.add("a");

      var out = "";
      for (var v of s) out += v;

      out === "acb" && s.size === 3
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn iterator_shape_and_well_known_iterator_wiring() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let value = rt.exec_script(
    r#"
      var ok = true;

      ok = ok && (Map.prototype[Symbol.iterator] === Map.prototype.entries);
      ok = ok && (Set.prototype[Symbol.iterator] === Set.prototype.values);
      ok = ok && (Set.prototype.keys === Set.prototype.values);
      ok = ok && (Set.prototype.keys.name === "values");

      var m = new Map([["a", 1]]);
      var it1 = m.keys();
      ok = ok && (it1[Symbol.iterator]() === it1);
      ok = ok && (it1.next().value === "a");

      var mEntry = m.entries().next().value;
      ok = ok && (mEntry.length === 2) && (mEntry[0] === "a") && (mEntry[1] === 1);

      var s = new Set([1]);
      var it2 = s.values();
      ok = ok && (it2[Symbol.iterator]() === it2);
      ok = ok && (it2.next().value === 1);

      var sEntry = s.entries().next().value;
      ok = ok && (sEntry.length === 2) && (sEntry[0] === 1) && (sEntry[1] === 1);

      ok
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn map_iteration_observes_mutations() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let value = rt.exec_script(
    r#"
      var ok = true;

      // Add during iteration is visited.
      var m1 = new Map();
      m1.set("a", 1);
      m1.set("b", 2);
      var out1 = "";
      for (var e of m1) {
        out1 += e[0];
        if (e[0] === "a") m1.set("c", 3);
      }
      ok = ok && (out1 === "abc");

      // Delete during iteration is skipped.
      var m2 = new Map();
      m2.set("a", 1);
      m2.set("b", 2);
      m2.set("c", 3);
      var out2 = "";
      for (var e of m2) {
        out2 += e[0];
        if (e[0] === "a") m2.delete("b");
      }
      ok = ok && (out2 === "ac");

      // Clear during iteration ends the traversal.
      var m3 = new Map();
      m3.set("a", 1);
      m3.set("b", 2);
      m3.set("c", 3);
      var out3 = "";
      for (var e of m3) {
        out3 += e[0];
        m3.clear();
      }
      ok = ok && (out3 === "a");

      ok
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn set_iteration_observes_mutations() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let value = rt.exec_script(
    r#"
      var ok = true;

      // Add during iteration is visited.
      var s1 = new Set([1, 2]);
      var out1 = "";
      for (var v of s1) {
        out1 += v;
        if (v === 1) s1.add(3);
      }
      ok = ok && (out1 === "123");

      // Delete during iteration is skipped.
      var s2 = new Set([1, 2, 3]);
      var out2 = "";
      for (var v of s2) {
        out2 += v;
        if (v === 1) s2.delete(2);
      }
      ok = ok && (out2 === "13");

      // Clear during iteration ends the traversal.
      var s3 = new Set([1, 2, 3]);
      var out3 = "";
      for (var v of s3) {
        out3 += v;
        s3.clear();
      }
      ok = ok && (out3 === "1");

      ok
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn map_iterator_next_is_budgeted_over_deleted_entries() -> Result<(), VmError> {
  // Use a Rust-side loop rather than `exec_script` so this test remains fast even if Map operations
  // are implemented with linear scans.
  //
  // Note: keep `N` >= `tick::DEFAULT_TICK_EVERY` (currently 1024) so an iterator that only ticks
  // periodically (and not on the first iteration) would still be forced to tick while scanning.
  const N: usize = 1024;

  let mut rt = TestRt::new(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024))?;
  let intr = *rt.realm.intrinsics();
  let mut scope = rt.heap.scope();

  let map_ctor = Value::Object(intr.map());
  let map = rt.vm.construct_without_host(&mut scope, map_ctor, &[], map_ctor)?;
  let Value::Object(map_obj) = map else {
    return Err(VmError::Unimplemented("Map constructor did not return object"));
  };

  let set_fn = get_data_property(&mut scope, map_obj, "set")?.unwrap();
  let delete_fn = get_data_property(&mut scope, map_obj, "delete")?.unwrap();
  let keys_fn = get_data_property(&mut scope, map_obj, "keys")?.unwrap();

  for i in 0..N {
    let n = Value::Number(i as f64);
    rt.vm.call_without_host(&mut scope, set_fn, map, &[n, n])?;
  }
  for i in 0..N {
    rt.vm
      .call_without_host(&mut scope, delete_fn, map, &[Value::Number(i as f64)])?;
  }

  let iter = rt.vm.call_without_host(&mut scope, keys_fn, map, &[])?;
  let Value::Object(iter_obj) = iter else {
    return Err(VmError::Unimplemented("expected Map keys iterator to be object"));
  };
  let next_fn = get_data_property(&mut scope, iter_obj, "next")?.unwrap();

  // The call itself ticks once. With a fuel budget of 1, the iterator must tick internally while
  // scanning deleted entries or it would incorrectly complete without termination.
  rt.vm.set_budget(Budget {
    fuel: Some(1),
    deadline: None,
    check_time_every: 1,
  });

  let err = rt
    .vm
    .call_without_host(&mut scope, next_fn, iter, &[])
    .unwrap_err();
  assert_out_of_fuel(err);
  Ok(())
}

#[test]
fn set_iterator_next_is_budgeted_over_deleted_entries() -> Result<(), VmError> {
  const N: usize = 1024;

  let mut rt = TestRt::new(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024))?;
  let intr = *rt.realm.intrinsics();
  let mut scope = rt.heap.scope();

  let set_ctor = Value::Object(intr.set());
  let set = rt.vm.construct_without_host(&mut scope, set_ctor, &[], set_ctor)?;
  let Value::Object(set_obj) = set else {
    return Err(VmError::Unimplemented("Set constructor did not return object"));
  };

  let add_fn = get_data_property(&mut scope, set_obj, "add")?.unwrap();
  let delete_fn = get_data_property(&mut scope, set_obj, "delete")?.unwrap();
  let values_fn = get_data_property(&mut scope, set_obj, "values")?.unwrap();

  for i in 0..N {
    rt.vm
      .call_without_host(&mut scope, add_fn, set, &[Value::Number(i as f64)])?;
  }
  for i in 0..N {
    rt.vm
      .call_without_host(&mut scope, delete_fn, set, &[Value::Number(i as f64)])?;
  }

  let iter = rt.vm.call_without_host(&mut scope, values_fn, set, &[])?;
  let Value::Object(iter_obj) = iter else {
    return Err(VmError::Unimplemented("expected Set values iterator to be object"));
  };
  let next_fn = get_data_property(&mut scope, iter_obj, "next")?.unwrap();

  rt.vm.set_budget(Budget {
    fuel: Some(1),
    deadline: None,
    check_time_every: 1,
  });

  let err = rt
    .vm
    .call_without_host(&mut scope, next_fn, iter, &[])
    .unwrap_err();
  assert_out_of_fuel(err);
  Ok(())
}
