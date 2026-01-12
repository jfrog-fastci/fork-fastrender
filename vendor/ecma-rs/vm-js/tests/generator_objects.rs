use vm_js::{Heap, HeapLimits, JsRuntime, PropertyDescriptor, PropertyKey, PropertyKind, Value, Vm, VmError, VmOptions};

fn data_desc(value: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: true,
    configurable: true,
    kind: PropertyKind::Data {
      value,
      writable: true,
    },
  }
}

fn new_runtime() -> Result<JsRuntime, VmError> {
  let vm = Vm::new(VmOptions::default());
  // Generator + parser setup can allocate a fair bit; keep the heap comfortably above metadata
  // overhead so this test can focus on GC tracing behaviour.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap)
}

#[test]
fn generator_objects_are_ordinary_objects_and_gc_traces_internal_slots() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let gen;
  let this_obj;
  let arg_obj;
  let captured_obj;
  let prop_obj;

  // Create a generator object whose continuation captures:
  // - `this_obj` via `.call(this_obj, ...)`
  // - `arg_obj` via an argument
  // - `captured_obj` via a closure variable
  let (this_key, arg_key, captured_key) = {
    let (_vm, realm, heap) = rt.vm_realm_and_heap_mut();
    let global = realm.global_object();
    let mut scope = heap.scope();

    this_obj = scope.alloc_object()?;
    arg_obj = scope.alloc_object()?;
    captured_obj = scope.alloc_object()?;

    // Keep the objects alive across subsequent allocations (string keys + property definitions).
    scope.push_roots(&[
      Value::Object(this_obj),
      Value::Object(arg_obj),
      Value::Object(captured_obj),
    ])?;

    let this_key = scope.alloc_string("thisObj")?;
    let arg_key = scope.alloc_string("argObj")?;
    let captured_key = scope.alloc_string("capturedObj")?;

    scope.define_property(global, PropertyKey::from_string(this_key), data_desc(Value::Object(this_obj)))?;
    scope.define_property(global, PropertyKey::from_string(arg_key), data_desc(Value::Object(arg_obj)))?;
    scope.define_property(
      global,
      PropertyKey::from_string(captured_key),
      data_desc(Value::Object(captured_obj)),
    )?;

    (this_key, arg_key, captured_key)
  };

  let gen_value = rt.exec_script(
    r#"(function(captured) {
         return function* (arg) { yield captured; yield arg; };
       })(capturedObj).call(thisObj, argObj)"#,
  )?;
  gen = match gen_value {
    Value::Object(obj) => obj,
    _ => return Err(VmError::InvariantViolation("expected generator allocation to return an object")),
  };

  // Keep the generator live across collection (without rooting captured objects directly).
  let gen_root = rt.heap.add_root(Value::Object(gen))?;

  // Remove global references to the captured objects. After this point, they should only be kept
  // alive via the generator's internal slots / continuation.
  {
    let global = rt.realm().global_object();
    rt
      .heap
      .ordinary_delete(global, PropertyKey::from_string(this_key))?;
    rt
      .heap
      .ordinary_delete(global, PropertyKey::from_string(arg_key))?;
    rt
      .heap
      .ordinary_delete(global, PropertyKey::from_string(captured_key))?;
  }

  // Exercise property definition/deletion plumbing for generator objects (they are ordinary
  // objects with additional internal slots).
  prop_obj = {
    let mut scope = rt.heap.scope();
    let prop_obj = scope.alloc_object()?;
    // Keep the generator and value alive across any allocation/GC while manipulating properties.
    scope.push_roots(&[Value::Object(gen), Value::Object(prop_obj)])?;
    let key = scope.alloc_string("x")?;
    scope.define_property(gen, PropertyKey::from_string(key), data_desc(Value::Object(prop_obj)))?;
    assert_eq!(scope.heap().get(gen, &PropertyKey::from_string(key))?, Value::Object(prop_obj));
    assert!(scope.heap_mut().ordinary_delete(gen, PropertyKey::from_string(key))?);
    assert_eq!(scope.heap().get(gen, &PropertyKey::from_string(key))?, Value::Undefined);

    prop_obj
  };

  // `prop_obj` is now unreachable (the property was deleted), but the generator continuation should
  // keep `this_obj`/`arg_obj`/`captured_obj` live.
  rt.heap.collect_garbage();
  assert!(rt.heap.is_valid_object(gen));
  assert!(rt.heap.is_valid_object(this_obj));
  assert!(rt.heap.is_valid_object(arg_obj));
  assert!(rt.heap.is_valid_object(captured_obj));
  assert!(!rt.heap.is_valid_object(prop_obj));

  // Once the generator root is removed, everything becomes unreachable and should be collected.
  rt.heap.remove_root(gen_root);
  rt.heap.collect_garbage();
  assert!(!rt.heap.is_valid_object(gen));
  assert!(!rt.heap.is_valid_object(this_obj));
  assert!(!rt.heap.is_valid_object(arg_obj));
  assert!(!rt.heap.is_valid_object(captured_obj));
  assert!(!rt.heap.is_valid_object(prop_obj));
  Ok(())
}
