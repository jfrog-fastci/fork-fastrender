use vm_js::{GcObject, Heap, HeapLimits, Value, VmError, WeakGcObject};

#[test]
fn weakset_prunes_dead_keys_during_gc() -> Result<(), VmError> {
  // Use generous limits so no GC is triggered while populating the WeakSet. The keys are only kept
  // alive by host locals (not GC roots) until we explicitly collect at the end of the scope.
  let mut heap = Heap::new(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));

  // Keep the WeakSet alive across GCs.
  let (ws, ws_root) = {
    let mut scope = heap.scope();
    let ws = scope.alloc_weak_set()?;
    let root = scope.heap_mut().add_root(Value::Object(ws))?;
    (ws, root)
  };

  // Add a batch of short-lived objects.
  let dead_key: GcObject;
  const N: usize = 2048;
  {
    let mut scope = heap.scope();
    let mut first: Option<GcObject> = None;
    for i in 0..N {
      let key = scope.alloc_object()?;
      if i == 0 {
        first = Some(key);
      }
      scope.heap_mut().weak_set_add(ws, key)?;
    }
    dead_key = first.unwrap();

    assert!(
      scope.heap().weak_set_has(ws, dead_key)?,
      "expected WeakSet to contain key before GC"
    );
    assert_eq!(scope.heap().weak_set_entry_count(ws)?, N);
  }

  // Drop all strong references to the keys and collect.
  heap.collect_garbage();

  // The captured handle should now be dead.
  assert!(WeakGcObject::from(dead_key).upgrade(&heap).is_none());
  assert!(!heap.is_valid_object(dead_key));

  // `has` treats dead keys as absent.
  assert!(!heap.weak_set_has(ws, dead_key)?);

  // The GC should also prune dead entries from the WeakSet's internal table.
  assert_eq!(heap.weak_set_entry_count(ws)?, 0);

  heap.remove_root(ws_root);
  Ok(())
}
