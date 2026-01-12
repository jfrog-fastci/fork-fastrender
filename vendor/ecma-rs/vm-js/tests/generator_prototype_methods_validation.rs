use vm_js::{
  GcObject, Heap, HeapLimits, JsRuntime, PropertyDescriptor, PropertyKey, PropertyKind, Scope,
  Value, Vm, VmError, VmOptions,
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
  // Root the thrown value only for the duration of this assertion. These tests intentionally run
  // with small heap limits, so leaking roots across many error-path calls can spuriously trip
  // `VmError::OutOfMemory` instead of exercising the intended TypeError throws.
  let mut scope = scope.reborrow();
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
fn generator_prototype_methods_validate_this_and_resume_generator() -> Result<(), VmError> {
  // This test deliberately allocates many TypeError instances (receiver validation), so give it a
  // little more headroom than the minimum 1MiB heap used by most tests.
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;
  let intr = *rt.realm().intrinsics();

  let generator_prototype = intr.generator_prototype();

  let (next, return_, throw_) = {
    let mut scope = rt.heap.scope();

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
      let err = rt
        .vm
        .call_without_host(&mut scope, Value::Object(next), this, &[Value::Undefined])
        .unwrap_err();
      assert_is_type_error(&mut scope, &intr, err)?;

      let err = rt
        .vm
        .call_without_host(&mut scope, Value::Object(return_), this, &[Value::Undefined])
        .unwrap_err();
      assert_is_type_error(&mut scope, &intr, err)?;

      let err = rt
        .vm
        .call_without_host(&mut scope, Value::Object(throw_), this, &[Value::Undefined])
        .unwrap_err();
      assert_is_type_error(&mut scope, &intr, err)?;
    }

    // Non-generator objects throw TypeError.
    let obj = scope.alloc_object()?;
    scope.push_root(Value::Object(obj))?;

    let err = rt
      .vm
      .call_without_host(&mut scope, Value::Object(next), Value::Object(obj), &[Value::Undefined])
      .unwrap_err();
    assert_is_type_error(&mut scope, &intr, err)?;

    let err = rt
      .vm
      .call_without_host(
        &mut scope,
        Value::Object(return_),
        Value::Object(obj),
        &[Value::Undefined],
      )
      .unwrap_err();
    assert_is_type_error(&mut scope, &intr, err)?;

    let err = rt
      .vm
      .call_without_host(
        &mut scope,
        Value::Object(throw_),
        Value::Object(obj),
        &[Value::Undefined],
      )
      .unwrap_err();
    assert_is_type_error(&mut scope, &intr, err)?;

    // Fake "generator object": inherits from `%GeneratorPrototype%` and carries the internal
    // `[[GeneratorState]]` marker, but does **not** have real generator internal slots.
    //
    // Builtins must reject it as an incompatible receiver (TypeError); you cannot spoof generator
    // brand checks by setting `[[Prototype]]` or adding internal-marker properties.
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

    let err = rt
      .vm
      .call_without_host(
        &mut scope,
        Value::Object(next),
        Value::Object(gen),
        &[Value::Undefined],
      )
      .unwrap_err();
    assert_is_type_error(&mut scope, &intr, err)?;

    let err = rt
      .vm
      .call_without_host(
        &mut scope,
        Value::Object(return_),
        Value::Object(gen),
        &[Value::Undefined],
      )
      .unwrap_err();
    assert_is_type_error(&mut scope, &intr, err)?;

    let err = rt
      .vm
      .call_without_host(
        &mut scope,
        Value::Object(throw_),
        Value::Object(gen),
        &[Value::Undefined],
      )
      .unwrap_err();
    assert_is_type_error(&mut scope, &intr, err)?;

    (next, return_, throw_)
  };

  // Create a generator function we can instantiate multiple times for `return` vs `throw` tests.
  rt.exec_script(r#"function* g() { yield 1; }"#)?;

  // `return` should close the generator and return an iterator result object with `done: true`.
  let gen_return = match rt.exec_script("g()")? {
    Value::Object(o) => o,
    other => panic!("expected generator object, got {other:?}"),
  };
  {
    let mut scope = rt.heap.scope();
    scope.push_root(Value::Object(gen_return))?;

    let value_key = PropertyKey::from_string(scope.alloc_string("value")?);
    let done_key = PropertyKey::from_string(scope.alloc_string("done")?);

    let result = rt.vm.call_without_host(
      &mut scope,
      Value::Object(next),
      Value::Object(gen_return),
      &[Value::Undefined],
    )?;
    let Value::Object(result_obj) = result else {
      panic!("expected iterator result object");
    };
    scope.push_root(Value::Object(result_obj))?;
    assert_eq!(
      scope
        .heap()
        .object_get_own_data_property_value(result_obj, &value_key)?,
      Some(Value::Number(1.0))
    );
    assert_eq!(
      scope
        .heap()
        .object_get_own_data_property_value(result_obj, &done_key)?,
      Some(Value::Bool(false))
    );

    let result = rt.vm.call_without_host(
      &mut scope,
      Value::Object(return_),
      Value::Object(gen_return),
      &[Value::Undefined],
    )?;
    let Value::Object(result_obj) = result else {
      panic!("expected iterator result object");
    };
    scope.push_root(Value::Object(result_obj))?;
    assert_eq!(
      scope
        .heap()
        .object_get_own_data_property_value(result_obj, &value_key)?,
      Some(Value::Undefined)
    );
    assert_eq!(
      scope
        .heap()
        .object_get_own_data_property_value(result_obj, &done_key)?,
      Some(Value::Bool(true))
    );
  }

  // `throw` should throw the provided value into the generator. When not caught, it propagates and
  // closes the generator.
  let gen_throw = match rt.exec_script("g()")? {
    Value::Object(o) => o,
    other => panic!("expected generator object, got {other:?}"),
  };
  {
    let mut scope = rt.heap.scope();
    scope.push_root(Value::Object(gen_throw))?;

    let value_key = PropertyKey::from_string(scope.alloc_string("value")?);
    let done_key = PropertyKey::from_string(scope.alloc_string("done")?);

    let result = rt.vm.call_without_host(
      &mut scope,
      Value::Object(next),
      Value::Object(gen_throw),
      &[Value::Undefined],
    )?;
    let Value::Object(result_obj) = result else {
      panic!("expected iterator result object");
    };
    scope.push_root(Value::Object(result_obj))?;
    assert_eq!(
      scope
        .heap()
        .object_get_own_data_property_value(result_obj, &value_key)?,
      Some(Value::Number(1.0))
    );
    assert_eq!(
      scope
        .heap()
        .object_get_own_data_property_value(result_obj, &done_key)?,
      Some(Value::Bool(false))
    );

    let thrown = rt
      .vm
      .call_without_host(
        &mut scope,
        Value::Object(throw_),
        Value::Object(gen_throw),
        &[Value::Number(42.0)],
      )
      .unwrap_err();
    let thrown_value = match thrown {
      VmError::Throw(v) => v,
      VmError::ThrowWithStack { value, .. } => value,
      other => panic!("expected thrown completion, got {other:?}"),
    };
    assert_eq!(thrown_value, Value::Number(42.0));

    // Once closed, subsequent `next` calls should return `done: true`.
    let result = rt.vm.call_without_host(
      &mut scope,
      Value::Object(next),
      Value::Object(gen_throw),
      &[Value::Undefined],
    )?;
    let Value::Object(result_obj) = result else {
      panic!("expected iterator result object");
    };
    scope.push_root(Value::Object(result_obj))?;
    assert_eq!(
      scope
        .heap()
        .object_get_own_data_property_value(result_obj, &done_key)?,
      Some(Value::Bool(true))
    );
  }

  Ok(())
}

