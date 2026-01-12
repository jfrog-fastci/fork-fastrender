use vm_js::{GcObject, Heap, HeapLimits, Value, VmError, WeakGcObject};

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

