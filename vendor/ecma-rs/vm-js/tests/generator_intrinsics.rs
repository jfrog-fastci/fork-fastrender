use vm_js::{
  GcObject, Heap, HeapLimits, PropertyDescriptor, PropertyKey, PropertyKind, Realm, Scope, Value, Vm,
  VmError, VmOptions,
};

fn get_own_property(
  scope: &mut Scope<'_>,
  obj: GcObject,
  key: PropertyKey,
) -> Result<PropertyDescriptor, VmError> {
  Ok(
    scope
      .heap()
      .object_get_own_property(obj, &key)?
      .expect("expected own property to exist"),
  )
}

fn assert_data_descriptor(
  desc: PropertyDescriptor,
  expected_value: Value,
  writable: bool,
  enumerable: bool,
  configurable: bool,
) {
  assert_eq!(desc.enumerable, enumerable);
  assert_eq!(desc.configurable, configurable);
  match desc.kind {
    PropertyKind::Data { value, writable: w } => {
      assert_eq!(w, writable);
      assert_eq!(value, expected_value);
    }
    PropertyKind::Accessor { .. } => panic!("expected data descriptor"),
  }
}

fn assert_data_descriptor_string(
  scope: &mut Scope<'_>,
  desc: PropertyDescriptor,
  expected: &str,
  writable: bool,
  enumerable: bool,
  configurable: bool,
) -> Result<(), VmError> {
  assert_eq!(desc.enumerable, enumerable);
  assert_eq!(desc.configurable, configurable);
  match desc.kind {
    PropertyKind::Data { value, writable: w } => {
      assert_eq!(w, writable);
      let Value::String(s) = value else {
        panic!("expected string value");
      };
      assert_eq!(scope.heap().get_string(s)?.to_utf8_lossy(), expected);
    }
    PropertyKind::Accessor { .. } => panic!("expected data descriptor"),
  }
  Ok(())
}

#[test]
fn generator_intrinsics_have_correct_wiring_and_descriptors() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut vm = Vm::new(VmOptions::default());
  let mut realm = Realm::new(&mut vm, &mut heap)?;
  let intr = *realm.intrinsics();

  {
    let mut scope = heap.scope();
    let wks = intr.well_known_symbols();

    // --- Prototype wiring ---
    assert_eq!(
      scope.heap().object_prototype(intr.generator_function())?,
      Some(intr.function_constructor())
    );
    assert_eq!(
      scope.heap().object_prototype(intr.generator_function_prototype())?,
      Some(intr.function_prototype())
    );
    assert_eq!(
      scope.heap().object_prototype(intr.generator_prototype())?,
      Some(intr.iterator_prototype())
    );

    // `%GeneratorFunction.prototype%` is an ordinary (non-callable) object.
    assert!(!scope
      .heap()
      .is_callable(Value::Object(intr.generator_function_prototype()))?);

    // --- %GeneratorFunction% own properties ---
    {
      let name_key = PropertyKey::from_string(scope.alloc_string("name")?);
      let name_desc = get_own_property(&mut scope, intr.generator_function(), name_key)?;
      assert_data_descriptor_string(&mut scope, name_desc, "GeneratorFunction", false, false, true)?;

      let length_key = PropertyKey::from_string(scope.alloc_string("length")?);
      let length_desc = get_own_property(&mut scope, intr.generator_function(), length_key)?;
      assert_data_descriptor(length_desc, Value::Number(1.0), false, false, true);

      let proto_key = PropertyKey::from_string(scope.alloc_string("prototype")?);
      let proto_desc = get_own_property(&mut scope, intr.generator_function(), proto_key)?;
      assert_data_descriptor(
        proto_desc,
        Value::Object(intr.generator_function_prototype()),
        false,
        false,
        false,
      );
    }

    // --- %GeneratorFunction.prototype% own properties ---
    {
      let constructor_key = PropertyKey::from_string(scope.alloc_string("constructor")?);
      let constructor_desc =
        get_own_property(&mut scope, intr.generator_function_prototype(), constructor_key)?;
      assert_data_descriptor(
        constructor_desc,
        Value::Object(intr.generator_function()),
        false,
        false,
        true,
      );

      let prototype_key = PropertyKey::from_string(scope.alloc_string("prototype")?);
      let prototype_desc =
        get_own_property(&mut scope, intr.generator_function_prototype(), prototype_key)?;
      assert_data_descriptor(
        prototype_desc,
        Value::Object(intr.generator_prototype()),
        false,
        false,
        true,
      );

      let tag_desc = get_own_property(
        &mut scope,
        intr.generator_function_prototype(),
        PropertyKey::Symbol(wks.to_string_tag),
      )?;
      assert_data_descriptor_string(
        &mut scope,
        tag_desc,
        "GeneratorFunction",
        false,
        false,
        true,
      )?;
    }

    // --- %GeneratorPrototype% own properties ---
    {
      let constructor_key = PropertyKey::from_string(scope.alloc_string("constructor")?);
      let constructor_desc =
        get_own_property(&mut scope, intr.generator_prototype(), constructor_key)?;
      assert_data_descriptor(
        constructor_desc,
        Value::Object(intr.generator_function_prototype()),
        false,
        false,
        true,
      );

      for (name, call_desc) in [
        ("next", true),
        ("return", true),
        ("throw", true),
      ] {
        let key = PropertyKey::from_string(scope.alloc_string(name)?);
        let desc = get_own_property(
          &mut scope,
          intr.generator_prototype(),
          key,
        )?;
        assert_eq!(desc.enumerable, false);
        assert_eq!(desc.configurable, true);
        match desc.kind {
          PropertyKind::Data { value, writable } => {
            assert_eq!(writable, call_desc);
            assert!(scope.heap().is_callable(value)?);
          }
          PropertyKind::Accessor { .. } => panic!("expected data descriptor"),
        }
      }

      let tag_desc = get_own_property(
        &mut scope,
        intr.generator_prototype(),
        PropertyKey::Symbol(wks.to_string_tag),
      )?;
      assert_data_descriptor_string(&mut scope, tag_desc, "Generator", false, false, true)?;

      // IMPORTANT: %GeneratorPrototype% should not have an own @@iterator.
      let own_keys = scope.heap().own_property_keys(intr.generator_prototype())?;
      let mut own_names: Vec<String> = Vec::new();
      let mut own_symbols: Vec<_> = Vec::new();
      for k in own_keys {
        match k {
          PropertyKey::String(s) => own_names.push(scope.heap().get_string(s)?.to_utf8_lossy()),
          PropertyKey::Symbol(sym) => own_symbols.push(sym),
        }
      }
      own_names.sort();
      assert_eq!(own_names, vec!["constructor", "next", "return", "throw"]);
      assert_eq!(own_symbols, vec![wks.to_string_tag]);
    }
  }

  realm.teardown(&mut heap);
  Ok(())
}
