use vm_js::{
  Heap, HeapLimits, NativeFunctionId, PropertyDescriptor, PropertyKey, PropertyKind, Value, VmError,
};

// Lightweight integration-smoke test for function objects behaving like ordinary objects:
// - can carry own properties
// - can participate in `[[Prototype]]` chains
//
// vm-js has its own unit tests for this behaviour, but keeping a small high-level check here helps
// catch accidental regressions when updating vendor/ecma-rs.

#[test]
fn function_objects_support_properties_and_prototype_chain_smoke() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut scope = heap.scope();

  let name = scope.alloc_string("f")?;
  let func = scope.alloc_native_function(NativeFunctionId(1), None, name, 0)?;
  scope.push_root(Value::Object(func))?;

  // Own data property on a function object.
  let x_key_s = scope.alloc_string("x")?;
  scope.push_root(Value::String(x_key_s))?;
  let x_key = PropertyKey::from_string(x_key_s);
  scope.define_property(
    func,
    x_key,
    PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::Number(1.0),
        writable: true,
      },
    },
  )?;
  assert_eq!(
    scope
      .heap()
      .object_get_own_data_property_value(func, &x_key)?,
    Some(Value::Number(1.0))
  );

  // Mutate that property via a Heap API that historically rejected function objects.
  scope
    .heap_mut()
    .object_set_existing_data_property_value(func, &x_key, Value::Number(2.0))?;
  assert_eq!(
    scope
      .heap()
      .object_get_own_data_property_value(func, &x_key)?,
    Some(Value::Number(2.0))
  );

  // Prototype chain lookup where the receiver is a function object.
  let proto = scope.alloc_object()?;
  scope.push_root(Value::Object(proto))?;
  scope.heap_mut().object_set_prototype(func, Some(proto))?;

  let y_key_s = scope.alloc_string("y")?;
  scope.push_root(Value::String(y_key_s))?;
  let y_key = PropertyKey::from_string(y_key_s);
  scope.define_property(
    proto,
    y_key,
    PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::Number(42.0),
        writable: true,
      },
    },
  )?;

  let desc = scope
    .heap()
    .get_property(func, &y_key)?
    .expect("prototype property should be found via get_property");
  let PropertyKind::Data { value, .. } = desc.kind else {
    panic!("expected a data property");
  };
  assert_eq!(value, Value::Number(42.0));

  Ok(())
}
