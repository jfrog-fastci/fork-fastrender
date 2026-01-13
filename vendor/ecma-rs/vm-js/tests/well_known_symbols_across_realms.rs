use vm_js::{GcObject, Heap, HeapLimits, PropertyKey, Realm, Value, Vm, VmError, VmOptions};

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

fn get_global_symbol_static_property(
  heap: &mut Heap,
  global: GcObject,
  property_name: &str,
) -> Result<Value, VmError> {
  let symbol_value = get_own_data_property(heap, global, "Symbol")?
    .expect("expected global.Symbol to exist");
  let Value::Object(symbol_ctor) = symbol_value else {
    panic!("expected global.Symbol to be an object");
  };

  Ok(
    get_own_data_property(heap, symbol_ctor, property_name)?
      .unwrap_or_else(|| panic!("expected Symbol.{property_name} to exist")),
  )
}

#[test]
fn well_known_symbols_are_shared_across_realms_on_same_heap() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut vm = Vm::new(VmOptions::default());

  let mut realm_a = Realm::new(&mut vm, &mut heap)?;
  let mut realm_b = Realm::new(&mut vm, &mut heap)?;

  // Host-side identity: realms should share the exact same `GcSymbol` values.
  let wks_a = *realm_a.intrinsics().well_known_symbols();
  let wks_b = *realm_b.intrinsics().well_known_symbols();
  assert_eq!(wks_a.iterator, wks_b.iterator);
  assert_eq!(wks_a.to_string_tag, wks_b.to_string_tag);
  assert_eq!(wks_a.has_instance, wks_b.has_instance);

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

  // Avoid leaking persistent roots (and tripping the Realm drop assertion).
  realm_a.teardown(&mut heap);
  realm_b.teardown(&mut heap);
  Ok(())
}
