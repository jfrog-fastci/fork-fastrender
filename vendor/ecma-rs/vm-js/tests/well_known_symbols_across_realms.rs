use vm_js::{GcObject, Heap, HeapLimits, PropertyKey, PropertyKind, Realm, Value, Vm, VmError, VmOptions};

fn get_own_data_property(
  heap: &mut Heap,
  obj: GcObject,
  name: &str,
) -> Result<Option<Value>, VmError> {
  let mut scope = heap.scope();
  let key = PropertyKey::from_string(scope.alloc_string(name)?);
  scope
    .heap()
    .object_get_own_data_property_value(obj, &key)
}

fn get_own_property(
  heap: &mut Heap,
  obj: GcObject,
  name: &str,
) -> Result<Option<vm_js::PropertyDescriptor>, VmError> {
  let mut scope = heap.scope();
  let key = PropertyKey::from_string(scope.alloc_string(name)?);
  scope.heap().object_get_own_property(obj, &key)
}

fn get_global_symbol_ctor(heap: &mut Heap, global: GcObject) -> Result<GcObject, VmError> {
  let symbol_value = get_own_data_property(heap, global, "Symbol")?.expect("expected global.Symbol to exist");
  let Value::Object(symbol_ctor) = symbol_value else {
    panic!("expected global.Symbol to be an object");
  };
  Ok(symbol_ctor)
}

fn get_global_symbol_static_property(
  heap: &mut Heap,
  global: GcObject,
  property_name: &str,
) -> Result<Value, VmError> {
  let symbol_ctor = get_global_symbol_ctor(heap, global)?;

  Ok(
    get_own_data_property(heap, symbol_ctor, property_name)?
      .unwrap_or_else(|| panic!("expected Symbol.{property_name} to exist")),
  )
}

#[test]
fn well_known_symbols_are_shared_across_realms_on_same_heap() -> Result<(), VmError> {
  // This test constructs multiple realms (each with a full set of intrinsics). Keep the heap large
  // enough that baseline intrinsic growth doesn't intermittently trip OOM.
  let mut heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  let mut vm = Vm::new(VmOptions::default());

  let mut realm_a = Realm::new(&mut vm, &mut heap)?;
  let mut realm_b = Realm::new(&mut vm, &mut heap)?;

  // Host-side identity: realms should share the exact same `GcSymbol` values.
  let wks_a = *realm_a.intrinsics().well_known_symbols();
  let wks_b = *realm_b.intrinsics().well_known_symbols();
  assert_eq!(wks_a.iterator, wks_b.iterator);
  assert_eq!(wks_a.to_string_tag, wks_b.to_string_tag);
  assert_eq!(wks_a.has_instance, wks_b.has_instance);
  assert_eq!(wks_a.dispose, wks_b.dispose);
  assert_eq!(wks_a.async_dispose, wks_b.async_dispose);

  // JS-visible identity: `Symbol.*` well-known symbol properties should match across realms.
  let global_a = realm_a.global_object();
  let global_b = realm_b.global_object();

  let iter_a = get_global_symbol_static_property(&mut heap, global_a, "iterator")?;
  let iter_b = get_global_symbol_static_property(&mut heap, global_b, "iterator")?;
  assert_eq!(iter_a, iter_b);
  assert_eq!(iter_a, Value::Symbol(wks_a.iterator));

  let tag_a = get_global_symbol_static_property(&mut heap, global_a, "toStringTag")?;
  let tag_b = get_global_symbol_static_property(&mut heap, global_b, "toStringTag")?;
  assert_eq!(tag_a, tag_b);
  assert_eq!(tag_a, Value::Symbol(wks_a.to_string_tag));

  let inst_a = get_global_symbol_static_property(&mut heap, global_a, "hasInstance")?;
  let inst_b = get_global_symbol_static_property(&mut heap, global_b, "hasInstance")?;
  assert_eq!(inst_a, inst_b);
  assert_eq!(inst_a, Value::Symbol(wks_a.has_instance));

  let dispose_a = get_global_symbol_static_property(&mut heap, global_a, "dispose")?;
  let dispose_b = get_global_symbol_static_property(&mut heap, global_b, "dispose")?;
  assert_eq!(dispose_a, dispose_b);
  assert_eq!(dispose_a, Value::Symbol(wks_a.dispose));

  let async_dispose_a = get_global_symbol_static_property(&mut heap, global_a, "asyncDispose")?;
  let async_dispose_b = get_global_symbol_static_property(&mut heap, global_b, "asyncDispose")?;
  assert_eq!(async_dispose_a, async_dispose_b);
  assert_eq!(async_dispose_a, Value::Symbol(wks_a.async_dispose));

  // Property attributes: well-known symbol properties are non-writable, non-enumerable, non-configurable.
  let symbol_ctor_a = get_global_symbol_ctor(&mut heap, global_a)?;
  for (name, expected) in [
    ("dispose", Value::Symbol(wks_a.dispose)),
    ("asyncDispose", Value::Symbol(wks_a.async_dispose)),
  ] {
    let desc = get_own_property(&mut heap, symbol_ctor_a, name)?
      .unwrap_or_else(|| panic!("expected Symbol.{name} to exist as own property"));
    assert!(!desc.enumerable, "Symbol.{name} should be non-enumerable");
    assert!(!desc.configurable, "Symbol.{name} should be non-configurable");
    match desc.kind {
      PropertyKind::Data { value, writable } => {
        assert!(!writable, "Symbol.{name} should be non-writable");
        assert_eq!(value, expected);
      }
      PropertyKind::Accessor { .. } => panic!("expected Symbol.{name} to be a data property"),
    }
  }

  // Avoid leaking persistent roots (and tripping the Realm drop assertion).
  realm_a.teardown(&mut heap);
  realm_b.teardown(&mut heap);
  Ok(())
}
