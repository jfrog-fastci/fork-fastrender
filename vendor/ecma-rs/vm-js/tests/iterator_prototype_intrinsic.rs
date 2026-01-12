use vm_js::{Heap, HeapLimits, PropertyKey, PropertyKind, Realm, Value, Vm, VmError, VmOptions};

#[test]
fn iterator_prototype_intrinsic_has_object_prototype_and_symbol_iterator() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let intr = *realm.intrinsics();
  let iterator_prototype = intr.iterator_prototype();

  assert_eq!(
    heap.object_prototype(iterator_prototype)?,
    Some(intr.object_prototype())
  );

  {
    let mut scope = heap.scope();

    let key = PropertyKey::from_symbol(intr.well_known_symbols().iterator);
    let Some(desc) = scope.heap().get_property(iterator_prototype, &key)? else {
      panic!("%IteratorPrototype% should have a @@iterator property");
    };
    let iter_fn = match desc.kind {
      PropertyKind::Data { value, .. } => value,
      PropertyKind::Accessor { .. } => return Err(VmError::PropertyNotData),
    };

    // The builtin must return the `this` value verbatim.
    let obj = scope.alloc_object()?;
    let out = vm.call_without_host(&mut scope, iter_fn, Value::Object(obj), &[])?;
    assert_eq!(out, Value::Object(obj));

    let out = vm.call_without_host(&mut scope, iter_fn, Value::Number(42.0), &[])?;
    assert_eq!(out, Value::Number(42.0));

    let s = scope.alloc_string("x")?;
    let out = vm.call_without_host(&mut scope, iter_fn, Value::String(s), &[])?;
    assert_eq!(out, Value::String(s));

    let out = vm.call_without_host(&mut scope, iter_fn, Value::Null, &[])?;
    assert_eq!(out, Value::Null);
  }

  realm.teardown(&mut heap);
  Ok(())
}

