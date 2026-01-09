use vm_js::{
  Heap, HeapLimits, PropertyDescriptor, PropertyDescriptorPatch, PropertyKey, PropertyKind, Value,
  VmError,
};

// This is a lightweight integration-smoke test for `vm-js`'s ordinary-object
// `[[DefineOwnProperty]]` implementation.
//
// `vm-js` has its own unit tests, but keeping one or two invariants covered here helps catch
// accidental regressions when bumping the `engines/ecma-rs` submodule.

#[test]
fn define_own_property_rejects_changing_enumerable_on_non_configurable_property() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

  let (obj, key) = {
    let mut scope = heap.scope();
    let obj = scope.alloc_object()?;
    let key = PropertyKey::from_string(scope.alloc_string("x")?);
    scope.define_property(
      obj,
      key,
      PropertyDescriptor {
        enumerable: true,
        configurable: false,
        kind: PropertyKind::Data {
          value: Value::Undefined,
          writable: true,
        },
      },
    )?;
    (obj, key)
  };

  assert!(!heap.define_own_property(
    obj,
    key,
    PropertyDescriptorPatch {
      enumerable: Some(false),
      ..Default::default()
    },
  )?);

  Ok(())
}

#[test]
fn define_own_property_empty_patch_creates_default_data_descriptor() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

  let (obj, key) = {
    let mut scope = heap.scope();
    let obj = scope.alloc_object()?;
    let key = PropertyKey::from_string(scope.alloc_string("x")?);
    (obj, key)
  };

  assert!(heap.define_own_property(obj, key, PropertyDescriptorPatch::default())?);

  let desc = heap
    .object_get_own_property(obj, &key)?
    .expect("property should exist");

  assert!(!desc.enumerable);
  assert!(!desc.configurable);
  match desc.kind {
    PropertyKind::Data { value, writable } => {
      assert!(matches!(value, Value::Undefined));
      assert!(!writable);
    }
    PropertyKind::Accessor { .. } => panic!("expected a data property"),
  }

  Ok(())
}

#[test]
fn define_own_property_rejects_value_changes_on_non_writable_non_configurable_data_property(
) -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

  let (obj, key) = {
    let mut scope = heap.scope();
    let obj = scope.alloc_object()?;
    let key = PropertyKey::from_string(scope.alloc_string("x")?);
    scope.define_property(
      obj,
      key,
      PropertyDescriptor {
        enumerable: true,
        configurable: false,
        kind: PropertyKind::Data {
          value: Value::Number(1.0),
          writable: false,
        },
      },
    )?;
    (obj, key)
  };

  // Changing the value should be rejected.
  assert!(!heap.define_own_property(
    obj,
    key,
    PropertyDescriptorPatch {
      value: Some(Value::Number(2.0)),
      ..Default::default()
    },
  )?);

  // Changing writable from false -> true should be rejected.
  assert!(!heap.define_own_property(
    obj,
    key,
    PropertyDescriptorPatch {
      writable: Some(true),
      ..Default::default()
    },
  )?);

  // Re-defining with the same value is allowed (SameValue).
  assert!(heap.define_own_property(
    obj,
    key,
    PropertyDescriptorPatch {
      value: Some(Value::Number(1.0)),
      ..Default::default()
    },
  )?);

  Ok(())
}

#[test]
fn define_own_property_rejects_getter_changes_on_non_configurable_accessor_property(
) -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

  let (obj, key, get1, get2) = {
    let mut scope = heap.scope();

    let get1 = scope.alloc_object()?;
    let get2 = scope.alloc_object()?;

    let obj = scope.alloc_object()?;
    let key = PropertyKey::from_string(scope.alloc_string("x")?);
    scope.define_property(
      obj,
      key,
      PropertyDescriptor {
        enumerable: true,
        configurable: false,
        kind: PropertyKind::Accessor {
          get: Value::Object(get1),
          set: Value::Undefined,
        },
      },
    )?;
    (obj, key, get1, get2)
  };

  assert!(!heap.define_own_property(
    obj,
    key,
    PropertyDescriptorPatch {
      get: Some(Value::Object(get2)),
      ..Default::default()
    },
  )?);

  // Re-defining with the same getter is allowed (SameValue).
  assert!(heap.define_own_property(
    obj,
    key,
    PropertyDescriptorPatch {
      get: Some(Value::Object(get1)),
      ..Default::default()
    },
  )?);

  Ok(())
}

#[test]
fn define_own_property_respects_non_extensible_object() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

  let (obj, key) = {
    let mut scope = heap.scope();
    let obj = scope.alloc_object()?;
    scope.object_prevent_extensions(obj)?;
    let key = PropertyKey::from_string(scope.alloc_string("x")?);
    (obj, key)
  };

  assert!(!heap.define_own_property(obj, key, PropertyDescriptorPatch::default())?);
  assert!(heap.object_get_own_property(obj, &key)?.is_none());
  Ok(())
}
