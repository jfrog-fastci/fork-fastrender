use vm_js::{GcObject, Heap, HeapLimits, PropertyDescriptor, PropertyKey, PropertyKind, Value, VmError, WeakGcObject};

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

#[test]
fn weakmap_values_are_ephemeron_traced_during_gc() -> Result<(), VmError> {
  // Use generous limits so no GC is triggered while populating the WeakMap. The value is only kept
  // alive by the WeakMap entry (not GC roots) until we explicitly collect.
  let mut heap = Heap::new(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));

  // Keep the WeakMap and key alive across GCs.
  let (wm, wm_root, key, key_root, value): (GcObject, _, GcObject, _, GcObject) = {
    let mut scope = heap.scope();

    let wm = scope.alloc_weak_map()?;
    let wm_root = scope.heap_mut().add_root(Value::Object(wm))?;

    let key = scope.alloc_object()?;
    let key_root = scope.heap_mut().add_root(Value::Object(key))?;

    let value = scope.alloc_object()?;
    scope
      .heap_mut()
      .weak_map_set(wm, key, Value::Object(value))?;

    assert_eq!(scope.heap().weak_map_entry_count(wm)?, 1);
    assert_eq!(scope.heap().weak_map_get(wm, key)?, Some(Value::Object(value)));

    (wm, wm_root, key, key_root, value)
  };

  // The value should be kept alive by ephemeron processing because:
  // - the WeakMap object is reachable, and
  // - the key is reachable.
  heap.collect_garbage();

  assert!(heap.is_valid_object(value));
  assert!(WeakGcObject::from(value).upgrade(&heap).is_some());
  assert_eq!(heap.weak_map_entry_count(wm)?, 1);
  assert_eq!(heap.weak_map_get(wm, key)?, Some(Value::Object(value)));

  // Once the key is unreachable, the value should no longer be kept alive via the WeakMap.
  heap.remove_root(key_root);
  heap.collect_garbage();

  assert!(WeakGcObject::from(key).upgrade(&heap).is_none());
  assert!(WeakGcObject::from(value).upgrade(&heap).is_none());
  assert_eq!(heap.weak_map_entry_count(wm)?, 0);

  heap.remove_root(wm_root);
  Ok(())
}

#[test]
fn weakmap_ephemeron_marking_reaches_fixpoint() -> Result<(), VmError> {
  // This test specifically ensures ephemeron marking is a *fixpoint* computation:
  //
  // If `k1` is live, then `v1` must become live. If `v1` holds a strong reference to `k2`, then
  // `k2` becomes live too, which should in turn make `v2` live via the WeakMap entry `k2 -> v2`.
  //
  // A one-pass WeakMap scan would miss `v2` because `k2` is only discovered while tracing `v1`.
  let mut heap = Heap::new(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));

  let (wm, wm_root, k1, k1_root, k2, v2): (GcObject, _, GcObject, _, GcObject, GcObject) = {
    let mut scope = heap.scope();

    let wm = scope.alloc_weak_map()?;
    let wm_root = scope.heap_mut().add_root(Value::Object(wm))?;

    let k1 = scope.alloc_object()?;
    let k1_root = scope.heap_mut().add_root(Value::Object(k1))?;

    let k2 = scope.alloc_object()?;

    let v1 = scope.alloc_object()?;
    // Root `v1` and `k2` across string allocation for the property key.
    scope.push_roots(&[Value::Object(v1), Value::Object(k2)])?;
    let k2_key_s = scope.alloc_string("k2")?;
    scope.push_root(Value::String(k2_key_s))?;
    let k2_key = PropertyKey::from_string(k2_key_s);
    scope.define_property(v1, k2_key, data_desc(Value::Object(k2)))?;

    let v2 = scope.alloc_object()?;

    scope.heap_mut().weak_map_set(wm, k1, Value::Object(v1))?;
    scope.heap_mut().weak_map_set(wm, k2, Value::Object(v2))?;

    assert_eq!(scope.heap().weak_map_entry_count(wm)?, 2);

    (wm, wm_root, k1, k1_root, k2, v2)
  };

  heap.collect_garbage();

  assert!(heap.is_valid_object(k2), "k2 should be kept alive via v1");
  assert!(
    heap.is_valid_object(v2),
    "WeakMap ephemeron processing should reach a fixpoint (k2 becomes live while tracing v1)"
  );
  assert_eq!(heap.weak_map_entry_count(wm)?, 2);

  // Once `k1` is unreachable, `v1` should die. With that, `k2` should also die, which should make
  // `v2` unreachable.
  heap.remove_root(k1_root);
  heap.collect_garbage();

  assert!(WeakGcObject::from(k1).upgrade(&heap).is_none());
  assert!(WeakGcObject::from(k2).upgrade(&heap).is_none());
  assert!(WeakGcObject::from(v2).upgrade(&heap).is_none());
  assert_eq!(heap.weak_map_entry_count(wm)?, 0);

  heap.remove_root(wm_root);
  Ok(())
}
