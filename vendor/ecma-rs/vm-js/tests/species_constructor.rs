use vm_js::{
  species_constructor, Heap, HeapLimits, PropertyDescriptor, PropertyKey, PropertyKind, Realm, Value,
  Vm, VmError, VmOptions,
};

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

#[test]
fn species_constructor_returns_default_when_constructor_missing() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  {
    let mut scope = heap.scope();
    let obj = scope.alloc_object()?;
    let default_ctor = scope.alloc_object()?;

    let got = species_constructor(&mut vm, &mut scope, obj, Value::Object(default_ctor))?;
    assert_eq!(got, Value::Object(default_ctor));
  }

  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn species_constructor_throws_type_error_when_constructor_is_not_object() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  {
    let mut scope = heap.scope();
    let obj = scope.alloc_object()?;
    let default_ctor = scope.alloc_object()?;

    let constructor_key = PropertyKey::from_string(scope.alloc_string("constructor")?);
    scope.define_property(obj, constructor_key, data_desc(Value::Number(1.0)))?;

    let err = species_constructor(&mut vm, &mut scope, obj, Value::Object(default_ctor)).unwrap_err();
    assert!(matches!(err, VmError::TypeError(_)));
  }

  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn species_constructor_returns_default_when_species_is_undefined() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  {
    let mut scope = heap.scope();
    let obj = scope.alloc_object()?;
    let default_ctor = scope.alloc_object()?;
    let ctor_obj = scope.alloc_object()?;

    let constructor_key = PropertyKey::from_string(scope.alloc_string("constructor")?);
    scope.define_property(obj, constructor_key, data_desc(Value::Object(ctor_obj)))?;

    let got = species_constructor(&mut vm, &mut scope, obj, Value::Object(default_ctor))?;
    assert_eq!(got, Value::Object(default_ctor));
  }

  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn species_constructor_returns_default_when_species_is_null() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  {
    let mut scope = heap.scope();
    let obj = scope.alloc_object()?;
    let default_ctor = scope.alloc_object()?;
    let ctor_obj = scope.alloc_object()?;

    let constructor_key = PropertyKey::from_string(scope.alloc_string("constructor")?);
    scope.define_property(obj, constructor_key, data_desc(Value::Object(ctor_obj)))?;

    let species_sym = vm.intrinsics().unwrap().well_known_symbols().species;
    let species_key = PropertyKey::from_symbol(species_sym);
    scope.define_property(ctor_obj, species_key, data_desc(Value::Null))?;

    let got = species_constructor(&mut vm, &mut scope, obj, Value::Object(default_ctor))?;
    assert_eq!(got, Value::Object(default_ctor));
  }

  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn species_constructor_throws_type_error_when_species_is_not_constructor() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  {
    let mut scope = heap.scope();
    let obj = scope.alloc_object()?;
    let default_ctor = scope.alloc_object()?;
    let ctor_obj = scope.alloc_object()?;

    let constructor_key = PropertyKey::from_string(scope.alloc_string("constructor")?);
    scope.define_property(obj, constructor_key, data_desc(Value::Object(ctor_obj)))?;

    let species_sym = vm.intrinsics().unwrap().well_known_symbols().species;
    let species_key = PropertyKey::from_symbol(species_sym);
    let not_a_constructor = scope.alloc_object()?;
    scope.define_property(ctor_obj, species_key, data_desc(Value::Object(not_a_constructor)))?;

    let err = species_constructor(&mut vm, &mut scope, obj, Value::Object(default_ctor)).unwrap_err();
    assert!(matches!(err, VmError::TypeError(_)));
  }

  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn species_constructor_returns_species_when_constructor() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  {
    let mut scope = heap.scope();
    let obj = scope.alloc_object()?;
    let default_ctor = scope.alloc_object()?;
    let ctor_obj = scope.alloc_object()?;

    let constructor_key = PropertyKey::from_string(scope.alloc_string("constructor")?);
    scope.define_property(obj, constructor_key, data_desc(Value::Object(ctor_obj)))?;

    let intr = vm.intrinsics().unwrap();
    let species_ctor = intr.promise();
    let species_sym = intr.well_known_symbols().species;
    let species_key = PropertyKey::from_symbol(species_sym);
    scope.define_property(ctor_obj, species_key, data_desc(Value::Object(species_ctor)))?;

    let got = species_constructor(&mut vm, &mut scope, obj, Value::Object(default_ctor))?;
    assert_eq!(got, Value::Object(species_ctor));
  }

  realm.teardown(&mut heap);
  Ok(())
}

