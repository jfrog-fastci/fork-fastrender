use vm_js::{
  make_generator_function_instance_prototype, Heap, HeapLimits, NativeFunctionId, PropertyKey,
  PropertyKind, Realm, Value, Vm, VmError, VmOptions,
};

#[test]
fn make_generator_function_instance_prototype_defines_per_function_prototype() -> Result<(), VmError>
{
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let intr = *realm.intrinsics();

  {
    let mut scope = heap.scope();

    // Stand-in for `%GeneratorPrototype%` (not yet part of `vm-js` intrinsics).
    let generator_prototype = scope.alloc_object()?;
    scope
      .heap_mut()
      .object_set_prototype(generator_prototype, Some(intr.object_prototype()))?;

    let name = scope.alloc_string("gen")?;
    let func = scope.alloc_native_function(NativeFunctionId(0), None, name, 0)?;
    scope
      .heap_mut()
      .object_set_prototype(func, Some(intr.function_prototype()))?;

    let proto_obj =
      make_generator_function_instance_prototype(&mut scope, func, generator_prototype)?;

    // F.prototype
    let prototype_key = PropertyKey::from_string(scope.alloc_string("prototype")?);
    let desc = scope
      .heap()
      .get_own_property(func, prototype_key)?
      .expect("generator function should have an own `prototype` property");
    assert!(!desc.enumerable);
    assert!(!desc.configurable);
    let PropertyKind::Data { value, writable } = desc.kind else {
      panic!("prototype should be a data property");
    };
    assert!(writable);
    assert_eq!(value, Value::Object(proto_obj));

    // Object.getPrototypeOf(F.prototype) === %GeneratorPrototype%
    assert_eq!(
      scope.heap().object_prototype(proto_obj)?,
      Some(generator_prototype)
    );

    // Unlike ordinary constructors, generator function instance prototype objects do not have an
    // own "constructor" property.
    let constructor_key = PropertyKey::from_string(scope.alloc_string("constructor")?);
    assert!(
      scope.heap().get_own_property(proto_obj, constructor_key)?.is_none(),
      "generator function instance prototype should not have an own `constructor` property"
    );
  }

  realm.teardown(&mut heap);
  Ok(())
}

