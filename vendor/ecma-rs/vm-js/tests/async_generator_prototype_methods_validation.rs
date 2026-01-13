use vm_js::{
  GcObject, Heap, HeapLimits, JsRuntime, PromiseState, PropertyKey, RootId, Scope, Value, Vm,
  VmError, VmOptions,
};

mod _async_generator_support;

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
  // with tight heap limits, so leaking roots across many error-path calls can spuriously trip
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
  assert_eq!(
    scope.heap().get_string(name_s)?.to_utf8_lossy(),
    expected_name
  );

  let length = get_own_data_property(scope, func, "length")?.expect("missing function length");
  assert_eq!(length, Value::Number(expected_length));
  Ok(())
}

#[test]
fn async_generator_prototype_methods_validate_this_and_basic_next() -> Result<(), VmError> {
  // This test allocates many TypeError instances (receiver validation) and, when async generators
  // are supported, also allocates Promises/iterator result objects. Give it a little more headroom
  // than the minimum 1MiB heap used by most tests.
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;
  let intr = *rt.realm().intrinsics();

  if !_async_generator_support::supports_async_generators(&mut rt)? {
    return Ok(());
  }

  rt.exec_script("async function* g() { yield 1; }")?;

  // Materialize one async generator object so we can walk its prototype chain to find
  // `%AsyncGeneratorPrototype%` without relying on a dedicated intrinsics accessor.
  rt.exec_script("var it = g();")?;
  let it = match rt.exec_script("it")? {
    Value::Object(o) => o,
    other => panic!("expected async generator object, got {other:?}"),
  };

  // `OrdinaryCreateFromConstructor(F, "%AsyncGeneratorPrototype%")` uses `F.prototype` if it is an
  // object. For `async function* g() {}`, that means:
  //   Object.getPrototypeOf(g()) === g.prototype
  // and:
  //   Object.getPrototypeOf(g.prototype) === %AsyncGeneratorPrototype%
  let async_generator_prototype = {
    let mut scope = rt.heap.scope();
    scope.push_root(Value::Object(it))?;
    let it_prototype = scope
      .heap()
      .object_prototype(it)?
      .expect("async generator should have a prototype");
    scope.push_root(Value::Object(it_prototype))?;

    // In the spec-shaped case, `it_prototype` is `g.prototype`, which does *not* have own
    // `next/return/throw` methods. Those live on `%AsyncGeneratorPrototype%`, i.e. `[[Prototype]]`
    // of `g.prototype`. If `g.prototype` is not an object, `it_prototype` may already be
    // `%AsyncGeneratorPrototype%`; detect that by looking for an own `next` method.
    if get_own_data_property(&mut scope, it_prototype, "next")?.is_some() {
      it_prototype
    } else {
      scope
        .heap()
        .object_prototype(it_prototype)?
        .expect("async generator prototype should have a prototype")
    }
  };

  let (next, _return_, _throw_) = {
    let mut scope = rt.heap.scope();
    scope.push_root(Value::Object(async_generator_prototype))?;

    let next = get_own_data_property(&mut scope, async_generator_prototype, "next")?
      .expect("AsyncGenerator.prototype.next should exist");
    let return_ = get_own_data_property(&mut scope, async_generator_prototype, "return")?
      .expect("AsyncGenerator.prototype.return should exist");
    let throw_ = get_own_data_property(&mut scope, async_generator_prototype, "throw")?
      .expect("AsyncGenerator.prototype.throw should exist");

    let Value::Object(next) = next else {
      panic!("AsyncGenerator.prototype.next should be a function object");
    };
    let Value::Object(return_) = return_ else {
      panic!("AsyncGenerator.prototype.return should be a function object");
    };
    let Value::Object(throw_) = throw_ else {
      panic!("AsyncGenerator.prototype.throw should be a function object");
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

    // Primitive `this` values must throw TypeError synchronously (not return rejected Promises).
    for &this in &primitive_this_values {
      let err = rt
        .vm
        .call_without_host(&mut scope, Value::Object(next), this, &[Value::Undefined])
        .unwrap_err();
      assert_is_type_error(&mut scope, &intr, err)?;

      let err = rt
        .vm
        .call_without_host(
          &mut scope,
          Value::Object(return_),
          this,
          &[Value::Undefined],
        )
        .unwrap_err();
      assert_is_type_error(&mut scope, &intr, err)?;

      let err = rt
        .vm
        .call_without_host(&mut scope, Value::Object(throw_), this, &[Value::Undefined])
        .unwrap_err();
      assert_is_type_error(&mut scope, &intr, err)?;
    }

    // Non-async-generator objects throw TypeError.
    let obj = scope.alloc_object()?;
    scope.push_root(Value::Object(obj))?;

    let err = rt
      .vm
      .call_without_host(
        &mut scope,
        Value::Object(next),
        Value::Object(obj),
        &[Value::Undefined],
      )
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

    // Fake "async generator object": inherits from `%AsyncGeneratorPrototype%` but does not have
    // the async generator internal slots. Brand checks must reject it.
    let fake = scope.alloc_object()?;
    scope.push_root(Value::Object(fake))?;
    scope
      .heap_mut()
      .object_set_prototype(fake, Some(async_generator_prototype))?;

    let err = rt
      .vm
      .call_without_host(
        &mut scope,
        Value::Object(next),
        Value::Object(fake),
        &[Value::Undefined],
      )
      .unwrap_err();
    assert_is_type_error(&mut scope, &intr, err)?;

    let err = rt
      .vm
      .call_without_host(
        &mut scope,
        Value::Object(return_),
        Value::Object(fake),
        &[Value::Undefined],
      )
      .unwrap_err();
    assert_is_type_error(&mut scope, &intr, err)?;

    let err = rt
      .vm
      .call_without_host(
        &mut scope,
        Value::Object(throw_),
        Value::Object(fake),
        &[Value::Undefined],
      )
      .unwrap_err();
    assert_is_type_error(&mut scope, &intr, err)?;

    (next, return_, throw_)
  };

  // Basic positive sanity: `it.next()` returns a Promise and resolves to `{ value: 1, done: false }`.
  let promise_root_id: RootId = {
    let mut scope = rt.heap.scope();
    let result = rt.vm.call_without_host(
      &mut scope,
      Value::Object(next),
      Value::Object(it),
      &[Value::Undefined],
    )?;
    let Value::Object(promise_obj) = result else {
      panic!("expected it.next() to return an object, got {result:?}");
    };
    assert!(scope.heap().is_promise_object(promise_obj));

    // Root the Promise across the microtask checkpoint: it may only be reachable from the Rust
    // stack, which is not traced by the GC.
    scope.heap_mut().add_root(Value::Object(promise_obj))?
  };

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  {
    let promise = rt
      .heap
      .get_root(promise_root_id)
      .expect("promise root should exist");
    let Value::Object(promise_obj) = promise else {
      panic!("expected rooted promise to be an object");
    };
    assert_eq!(rt.heap.promise_state(promise_obj)?, PromiseState::Fulfilled);
    let result = rt
      .heap
      .promise_result(promise_obj)?
      .expect("fulfilled promise must have a result");
    let Value::Object(result_obj) = result else {
      panic!("expected promise result to be an object, got {result:?}");
    };

    let mut scope = rt.heap.scope();
    scope.push_root(Value::Object(result_obj))?;
    let value_key = PropertyKey::from_string(scope.alloc_string("value")?);
    let done_key = PropertyKey::from_string(scope.alloc_string("done")?);
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
  }

  rt.heap.remove_root(promise_root_id);

  Ok(())
}
