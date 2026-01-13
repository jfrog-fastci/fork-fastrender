use vm_js::{Heap, HeapLimits, PropertyKey, PropertyKind, Realm, Value, Vm, VmError, VmOptions};

#[test]
fn async_iterator_prototype_intrinsic_has_object_prototype_and_symbol_async_iterator() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let intr = *realm.intrinsics();
  let async_iterator_prototype = intr.async_iterator_prototype();
  let async_iterator_key = PropertyKey::from_symbol(intr.well_known_symbols().async_iterator);

  {
    let mut scope = heap.scope();

    // `Object.getPrototypeOf(%AsyncIteratorPrototype%) === %Object.prototype%`.
    assert_eq!(
      scope.heap().object_prototype(async_iterator_prototype)?,
      Some(intr.object_prototype())
    );

    // `%AsyncIteratorPrototype%` should have an own `@@asyncIterator` data property.
    let Some(desc) = scope
      .heap()
      .object_get_own_property(async_iterator_prototype, &async_iterator_key)?
    else {
      panic!("%AsyncIteratorPrototype% should have an own @@asyncIterator property");
    };
    assert!(!desc.enumerable);
    assert!(desc.configurable);
    let async_iter_fn = match desc.kind {
      PropertyKind::Data { value, writable } => {
        assert!(writable);
        value
      }
      PropertyKind::Accessor { .. } => return Err(VmError::PropertyNotData),
    };
    assert!(scope.heap().is_callable(async_iter_fn)?);

    // Built-in function properties: name/length.
    let Value::Object(async_iter_fn_obj) = async_iter_fn else {
      panic!("expected %AsyncIteratorPrototype%[@@asyncIterator] to be a function object");
    };

    let name_key = PropertyKey::from_string(scope.alloc_string("name")?);
    let Some(name_desc) = scope.heap().object_get_own_property(async_iter_fn_obj, &name_key)? else {
      panic!("expected builtin @@asyncIterator to have an own 'name' property");
    };
    assert!(!name_desc.enumerable);
    assert!(name_desc.configurable);
    match name_desc.kind {
      PropertyKind::Data {
        value: Value::String(name),
        writable,
      } => {
        assert!(!writable);
        assert_eq!(
          scope.heap().get_string(name)?.to_utf8_lossy(),
          "[Symbol.asyncIterator]"
        );
      }
      _ => panic!("expected builtin @@asyncIterator name to be a non-writable data property"),
    }

    let length_key = PropertyKey::from_string(scope.alloc_string("length")?);
    let Some(length_desc) = scope
      .heap()
      .object_get_own_property(async_iter_fn_obj, &length_key)?
    else {
      panic!("expected builtin @@asyncIterator to have an own 'length' property");
    };
    assert!(!length_desc.enumerable);
    assert!(length_desc.configurable);
    match length_desc.kind {
      PropertyKind::Data {
        value: Value::Number(n),
        writable,
      } => {
        assert!(!writable);
        assert_eq!(n, 0.0);
      }
      _ => panic!("expected builtin @@asyncIterator length to be a non-writable data property"),
    }

    // The builtin must return the `this` value verbatim.
    let obj = scope.alloc_object()?;
    let out = vm.call_without_host(&mut scope, async_iter_fn, Value::Object(obj), &[])?;
    assert_eq!(out, Value::Object(obj));

    let out = vm.call_without_host(&mut scope, async_iter_fn, Value::Number(42.0), &[])?;
    assert_eq!(out, Value::Number(42.0));

    let s = scope.alloc_string("x")?;
    let out = vm.call_without_host(&mut scope, async_iter_fn, Value::String(s), &[])?;
    assert_eq!(out, Value::String(s));

    let out = vm.call_without_host(&mut scope, async_iter_fn, Value::Bool(true), &[])?;
    assert_eq!(out, Value::Bool(true));
  }

  realm.teardown(&mut heap);
  Ok(())
}

