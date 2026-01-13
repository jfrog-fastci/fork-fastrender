use crate::{GcObject, Heap, HeapLimits, PropertyKey, Realm, Scope, Value, Vm, VmError, VmOptions};

struct TestRealm {
  vm: Vm,
  heap: Heap,
  realm: Realm,
}

impl TestRealm {
  fn new() -> Result<Self, VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let realm = Realm::new(&mut vm, &mut heap)?;
    Ok(Self { vm, heap, realm })
  }
}

impl Drop for TestRealm {
  fn drop(&mut self) {
    self.realm.teardown(&mut self.heap);
  }
}

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

#[test]
fn symbol_has_construct_internal_method_but_new_symbol_throws() -> Result<(), VmError> {
  let mut rt = TestRealm::new()?;
  let intr = *rt.realm.intrinsics();

  // `Symbol` should be recognized as a constructor (it has `[[Construct]]`), even though
  // constructing it always throws.
  assert!(rt.heap.is_constructor(Value::Object(intr.symbol_constructor()))?);

  let mut scope = rt.heap.scope();

  // Regression: `Reflect.construct(function(){}, [], Symbol)` should not throw when `Symbol` is the
  // `newTarget` (this is what test262's `isConstructor(Symbol)` helper checks).
  let args_list_ctor = Value::Object(intr.array_constructor());
  let args_list = rt
    .vm
    .construct_without_host(&mut scope, args_list_ctor, &[], args_list_ctor)?;
  scope.push_root(args_list)?;
  let Value::Object(args_list_obj) = args_list else {
    return Err(VmError::Unimplemented("Array constructor did not return object"));
  };

  let reflect_obj = intr.reflect();
  let construct = get_own_data_property(&mut scope, reflect_obj, "construct")?
    .expect("Reflect.construct should exist");
  scope.push_root(construct)?;

  let out = rt.vm.call_without_host(
    &mut scope,
    construct,
    Value::Object(reflect_obj),
    &[
      Value::Object(intr.object_constructor()),
      Value::Object(args_list_obj),
      Value::Object(intr.symbol_constructor()),
    ],
  )?;
  let Value::Object(out_obj) = out else {
    return Err(VmError::Unimplemented("Reflect.construct did not return object"));
  };
  assert_eq!(
    scope.heap().object_prototype(out_obj)?,
    Some(intr.symbol_prototype())
  );

  // `new Symbol()` must throw a TypeError (not `NotConstructable`).
  let symbol_ctor = Value::Object(intr.symbol_constructor());
  let err = rt.vm.construct_without_host(&mut scope, symbol_ctor, &[], symbol_ctor);
  let thrown = match err {
    Ok(v) => panic!("expected new Symbol() to throw, got {v:?}"),
    Err(VmError::Throw(v) | VmError::ThrowWithStack { value: v, .. }) => v,
    Err(e) => return Err(e),
  };
  let Value::Object(err_obj) = thrown else {
    return Err(VmError::Unimplemented("Symbol constructor threw non-object"));
  };
  assert_eq!(
    scope.heap().object_prototype(err_obj)?,
    Some(intr.type_error_prototype())
  );

  Ok(())
}

