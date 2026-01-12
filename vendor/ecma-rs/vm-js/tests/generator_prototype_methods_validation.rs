use vm_js::{
  GcObject, Heap, HeapLimits, PropertyDescriptor, PropertyKey, PropertyKind, Realm, Scope, Value,
  Vm, VmError, VmOptions,
};

fn get_own_data_property(
  scope: &mut Scope<'_>,
  obj: GcObject,
  name: &str,
) -> Result<Option<Value>, VmError> {
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;
  let key = PropertyKey::from_string(scope.alloc_string(name)?);
  scope.heap().object_get_own_data_property_value(obj, &key)
}

fn assert_is_type_error(
  scope: &mut Scope<'_>,
  intr: &vm_js::Intrinsics,
  err: VmError,
) -> Result<(), VmError> {
  let thrown = match err {
    VmError::Throw(v) => v,
    VmError::ThrowWithStack { value, .. } => value,
    other => panic!("expected TypeError throw, got {other:?}"),
  };
  scope.push_root(thrown)?;
  let Value::Object(obj) = thrown else {
    panic!("expected thrown value to be an object, got {thrown:?}");
  };
  assert_eq!(
    scope.heap().object_prototype(obj)?,
    Some(intr.type_error_prototype())
  );
  Ok(())
}

fn assert_function_name_and_length(
  scope: &mut Scope<'_>,
  func: GcObject,
  expected_name: &str,
  expected_length: f64,
) -> Result<(), VmError> {
  let name = get_own_data_property(scope, func, "name")?.expect("missing function name");
  let Value::String(name_s) = name else {
    panic!("expected function name to be a string, got {name:?}");
  };
  assert_eq!(scope.heap().get_string(name_s)?.to_utf8_lossy(), expected_name);

  let length = get_own_data_property(scope, func, "length")?.expect("missing function length");
  assert_eq!(length, Value::Number(expected_length));
  Ok(())
}

#[test]
fn generator_prototype_methods_validate_this_and_are_stubbed() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut vm = Vm::new(VmOptions::default());
  let mut realm = Realm::new(&mut vm, &mut heap)?;
  let intr = *realm.intrinsics();

  let generator_prototype = intr.generator_prototype();

  {
    let mut scope = heap.scope();

    let next = get_own_data_property(&mut scope, generator_prototype, "next")?
      .expect("Generator.prototype.next should exist");
    let return_ = get_own_data_property(&mut scope, generator_prototype, "return")?
      .expect("Generator.prototype.return should exist");
    let throw_ = get_own_data_property(&mut scope, generator_prototype, "throw")?
      .expect("Generator.prototype.throw should exist");

    let Value::Object(next) = next else {
      panic!("Generator.prototype.next should be a function object");
    };
    let Value::Object(return_) = return_ else {
      panic!("Generator.prototype.return should be a function object");
    };
    let Value::Object(throw_) = throw_ else {
      panic!("Generator.prototype.throw should be a function object");
    };

    assert_function_name_and_length(&mut scope, next, "next", 1.0)?;
    assert_function_name_and_length(&mut scope, return_, "return", 1.0)?;
    assert_function_name_and_length(&mut scope, throw_, "throw", 1.0)?;

    let primitive_string = scope.alloc_string("x")?;
    scope.push_root(Value::String(primitive_string))?;
    let primitive_this_values = [
      Value::Undefined,
      Value::Null,
      Value::Number(0.0),
      Value::Bool(true),
      Value::String(primitive_string),
    ];

    // Primitive `this` values throw TypeError.
    for &this in &primitive_this_values {
      let err = vm
        .call_without_host(&mut scope, Value::Object(next), this, &[Value::Undefined])
        .unwrap_err();
      assert_is_type_error(&mut scope, &intr, err)?;

      let err = vm
        .call_without_host(&mut scope, Value::Object(return_), this, &[Value::Undefined])
        .unwrap_err();
      assert_is_type_error(&mut scope, &intr, err)?;

      let err = vm
        .call_without_host(&mut scope, Value::Object(throw_), this, &[Value::Undefined])
        .unwrap_err();
      assert_is_type_error(&mut scope, &intr, err)?;
    }

    // Non-generator objects throw TypeError.
    let obj = scope.alloc_object()?;
    scope.push_root(Value::Object(obj))?;

    let err = vm
      .call_without_host(&mut scope, Value::Object(next), Value::Object(obj), &[Value::Undefined])
      .unwrap_err();
    assert_is_type_error(&mut scope, &intr, err)?;

    let err = vm
      .call_without_host(
        &mut scope,
        Value::Object(return_),
        Value::Object(obj),
        &[Value::Undefined],
      )
      .unwrap_err();
    assert_is_type_error(&mut scope, &intr, err)?;

    let err = vm
      .call_without_host(
        &mut scope,
        Value::Object(throw_),
        Value::Object(obj),
        &[Value::Undefined],
      )
      .unwrap_err();
    assert_is_type_error(&mut scope, &intr, err)?;

    // Fake "generator object": has the internal [[GeneratorState]] marker and inherits from
    // `%GeneratorPrototype%` so builtin receiver validation passes.
    //
    // Since it has no continuation id, `%GeneratorPrototype%.next` should still treat it as an
    // incompatible receiver (TypeError). `%GeneratorPrototype%.return` / `.throw` remain stubbed.
    let gen = scope.alloc_object()?;
    scope.push_root(Value::Object(gen))?;
    scope
      .heap_mut()
      .object_set_prototype(gen, Some(generator_prototype))?;

    // Define the internal slot marker as an own symbol-keyed data property.
    let marker_s = scope.alloc_string("vm-js.internal.GeneratorState")?;
    scope.push_root(Value::String(marker_s))?;
    let marker_sym = scope.heap_mut().symbol_for(marker_s)?;
    let marker_key = PropertyKey::from_symbol(marker_sym);
    scope.define_property(
      gen,
      marker_key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Data {
          value: Value::Number(0.0),
          writable: true,
        },
      },
    )?;

    let err = vm
      .call_without_host(
        &mut scope,
        Value::Object(next),
        Value::Object(gen),
        &[Value::Undefined],
      )
      .unwrap_err();
    assert_is_type_error(&mut scope, &intr, err)?;

    let err = vm
      .call_without_host(
        &mut scope,
        Value::Object(return_),
        Value::Object(gen),
        &[Value::Undefined],
      )
      .unwrap_err();
    assert!(matches!(err, VmError::Unimplemented("GeneratorResumeAbrupt")));

    let err = vm
      .call_without_host(
        &mut scope,
        Value::Object(throw_),
        Value::Object(gen),
        &[Value::Undefined],
      )
      .unwrap_err();
    assert!(matches!(err, VmError::Unimplemented("GeneratorResumeAbrupt")));
  }

  realm.teardown(&mut heap);
  Ok(())
}
