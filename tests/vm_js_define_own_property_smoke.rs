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

