use vm_js::{Heap, HeapLimits, PropertyKey, PropertyKind, Realm, Value, Vm, VmError, VmOptions};

#[test]
fn iterator_prototype_intrinsic_has_object_prototype_and_symbol_iterator() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let intr = *realm.intrinsics();
  let iterator_prototype = intr.iterator_prototype();
  let iterator_key = PropertyKey::from_symbol(intr.well_known_symbols().iterator);

  {
    let mut scope = heap.scope();

    assert_eq!(
      scope.heap().object_prototype(iterator_prototype)?,
      Some(intr.object_prototype())
    );

    let Some(desc) = scope.heap().get_property(iterator_prototype, &iterator_key)? else {
      panic!("%IteratorPrototype% should have a @@iterator property");
    };
    assert!(!desc.enumerable);
    assert!(desc.configurable);
    let iter_fn = match desc.kind {
      PropertyKind::Data { value, writable } => {
        assert!(writable);
        value
      }
      PropertyKind::Accessor { .. } => return Err(VmError::PropertyNotData),
    };

    // Built-in function properties: name/length.
    let Value::Object(iter_fn_obj) = iter_fn else {
      panic!("expected %IteratorPrototype%[@@iterator] to be a function object");
    };
    let name_key = PropertyKey::from_string(scope.alloc_string("name")?);
    let Some(name_desc) = scope.heap().object_get_own_property(iter_fn_obj, &name_key)? else {
      panic!("expected builtin @@iterator to have an own 'name' property");
    };
    assert!(!name_desc.enumerable);
    assert!(name_desc.configurable);
    match name_desc.kind {
      PropertyKind::Data {
        value: Value::String(name),
        writable,
      } => {
        assert!(!writable);
        assert_eq!(scope.heap().get_string(name)?.to_utf8_lossy(), "[Symbol.iterator]");
      }
      _ => panic!("expected builtin @@iterator name to be a non-writable data property"),
    }

    let length_key = PropertyKey::from_string(scope.alloc_string("length")?);
    let Some(length_desc) = scope.heap().object_get_own_property(iter_fn_obj, &length_key)? else {
      panic!("expected builtin @@iterator to have an own 'length' property");
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
      _ => panic!("expected builtin @@iterator length to be a non-writable data property"),
    }

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

    // `Object.getPrototypeOf(Object.getPrototypeOf([][Symbol.iterator]())) === %IteratorPrototype%`
    let array_ctor = Value::Object(intr.array_constructor());
    let array_value = vm.construct_without_host(
      &mut scope,
      array_ctor,
      &[Value::Number(1.0), Value::Number(2.0)],
      array_ctor,
    )?;
    let Value::Object(array_obj) = array_value else {
      panic!("Array constructor returned non-object");
    };
    scope.push_root(Value::Object(array_obj))?;

    let Some(array_iter_method_desc) = scope.heap().get_property(array_obj, &iterator_key)? else {
      panic!("expected Array instance to have a @@iterator method");
    };
    let array_iter_method = match array_iter_method_desc.kind {
      PropertyKind::Data { value, .. } => value,
      _ => panic!("expected Array @@iterator to be a data property"),
    };
    let array_iter_value = vm.call_without_host(&mut scope, array_iter_method, array_value, &[])?;
    let Value::Object(array_iter_obj) = array_iter_value else {
      panic!("Array @@iterator returned non-object");
    };
    scope.push_root(Value::Object(array_iter_obj))?;

    let array_iter_proto = scope
      .heap()
      .object_prototype(array_iter_obj)?
      .expect("Array iterator should have a prototype");
    assert_eq!(
      scope.heap().object_prototype(array_iter_proto)?,
      Some(iterator_prototype)
    );

    // Iterator objects are iterable: `iter[@@iterator]()` returns the iterator itself.
    let Some(iter_iter_method_desc) = scope.heap().get_property(array_iter_obj, &iterator_key)? else {
      panic!("expected array iterator to have an @@iterator method");
    };
    let iter_iter_method = match iter_iter_method_desc.kind {
      PropertyKind::Data { value, .. } => value,
      _ => panic!("expected iterator @@iterator to be a data property"),
    };
    let out = vm.call_without_host(&mut scope, iter_iter_method, array_iter_value, &[])?;
    assert_eq!(out, array_iter_value);

    // `%StringIteratorPrototype%` should also inherit from `%IteratorPrototype%`.
    let Some(string_iter_method_desc) =
      scope.heap().get_property(intr.string_prototype(), &iterator_key)?
    else {
      panic!("expected String.prototype to have an @@iterator method");
    };
    let string_iter_method = match string_iter_method_desc.kind {
      PropertyKind::Data { value, .. } => value,
      _ => panic!("expected String.prototype @@iterator to be a data property"),
    };
    let s = scope.alloc_string("ab")?;
    scope.push_root(Value::String(s))?;
    let string_iter_value =
      vm.call_without_host(&mut scope, string_iter_method, Value::String(s), &[])?;
    let Value::Object(string_iter_obj) = string_iter_value else {
      panic!("String @@iterator returned non-object");
    };
    scope.push_root(Value::Object(string_iter_obj))?;

    let string_iter_proto = scope
      .heap()
      .object_prototype(string_iter_obj)?
      .expect("String iterator should have a prototype");
    assert_eq!(
      scope.heap().object_prototype(string_iter_proto)?,
      Some(iterator_prototype)
    );

    let out = vm.call_without_host(&mut scope, iter_iter_method, string_iter_value, &[])?;
    assert_eq!(out, string_iter_value);
  }

  realm.teardown(&mut heap);
  Ok(())
}
