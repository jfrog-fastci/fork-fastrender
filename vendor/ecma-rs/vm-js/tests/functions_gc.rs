use vm_js::{EcmaFunctionId, Heap, HeapLimits, NativeFunctionId, ThisMode, Value, VmError};

#[test]
fn gc_collects_unreachable_functions() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

  let func;
  {
    let mut scope = heap.scope();
    let name = scope.alloc_string("f")?;
    func = scope.alloc_native_function(NativeFunctionId(1), None, name, 0)?;

    assert!(scope.heap().is_valid_object(func));
    assert!(scope.heap().used_bytes() > 0);
  }

  let used_before_gc = heap.used_bytes();
  heap.collect_garbage();
  assert!(!heap.is_valid_object(func));

  // Function allocation interns a small set of common property key strings (e.g. "name"/"length")
  // that are kept alive by the heap. Ensure GC reclaimed the function allocation itself by
  // checking that used bytes decrease, and that repeated allocations return to the same baseline.
  let used_after_gc = heap.used_bytes();
  assert!(
    used_after_gc < used_before_gc,
    "expected GC to reclaim unreachable function payload bytes (before={used_before_gc}, after={used_after_gc})"
  );

  let baseline = used_after_gc;

  // Allocate and collect again: used_bytes should return to the same post-GC baseline.
  let func2;
  {
    let mut scope = heap.scope();
    let name = scope.alloc_string("g")?;
    func2 = scope.alloc_native_function(NativeFunctionId(1), None, name, 0)?;
    assert!(scope.heap().is_valid_object(func2));
  }

  heap.collect_garbage();
  assert!(!heap.is_valid_object(func2));
  assert_eq!(heap.used_bytes(), baseline);
  Ok(())
}

#[test]
fn gc_preserves_stack_rooted_functions() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

  let func;
  {
    let mut scope = heap.scope();
    let name = scope.alloc_string("rooted")?;
    func = scope.alloc_native_function(NativeFunctionId(1), None, name, 0)?;
    scope.push_root(Value::Object(func))?;

    scope.heap_mut().collect_garbage();
    assert!(scope.heap().is_valid_object(func));
  }

  heap.collect_garbage();
  assert!(!heap.is_valid_object(func));
  Ok(())
}

#[test]
fn function_traces_its_name_string() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

  let func;
  let name;
  {
    let mut scope = heap.scope();
    name = scope.alloc_string("my_func")?;
    func = scope.alloc_native_function(NativeFunctionId(1), None, name, 0)?;
    scope.push_root(Value::Object(func))?;

    scope.heap_mut().collect_garbage();
    assert_eq!(scope.heap().get_string(name)?.to_utf8_lossy(), "my_func");
  }

  // Stack roots were removed when the scope was dropped.
  heap.collect_garbage();
  assert!(matches!(heap.get_string(name), Err(VmError::InvalidHandle { .. })));
  assert!(!heap.is_valid_object(func));
  Ok(())
}

#[test]
fn gc_traces_closure_env_from_ecma_function() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

  let env;
  let func;
  {
    let mut scope = heap.scope();
    let name = scope.alloc_string("closure")?;
    // Root the name across environment allocation in case it triggers a GC.
    scope.push_root(Value::String(name))?;

    env = scope.env_create(None)?;
    func = scope.alloc_ecma_function(
      EcmaFunctionId(1),
      true,
      name,
      0,
      ThisMode::Global,
      false,
      Some(env),
    )?;

    // Only root the function; if it doesn't trace `closure_env`, `env` would be collected.
    scope.push_root(Value::Object(func))?;

    scope.heap_mut().collect_garbage();
    assert!(scope.heap().is_valid_object(func));
    assert!(scope.heap().is_valid_env(env));
  }

  // Stack roots were removed when the scope was dropped.
  heap.collect_garbage();
  assert!(!heap.is_valid_object(func));
  assert!(!heap.is_valid_env(env));
  Ok(())
}
