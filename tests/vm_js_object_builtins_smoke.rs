use vm_js::{
  GcObject, Heap, HeapLimits, PropertyDescriptor, PropertyKey, PropertyKind, Realm, Scope, Value,
  Vm, VmError, VmOptions,
};

// Lightweight integration-smoke test for vm-js' Object intrinsics/builtins.
//
// vm-js has its own unit tests, but keeping a small high-level check here helps catch accidental
// regressions when bumping the engines/ecma-rs submodule.

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
  scope.push_root(Value::Object(obj));
  let key = PropertyKey::from_string(scope.alloc_string(name)?);
  scope.heap().object_get_own_data_property_value(obj, &key)
}

fn define_enumerable_data_property(
  scope: &mut Scope<'_>,
  obj: GcObject,
  name: &str,
  value: Value,
) -> Result<(), VmError> {
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj));
  scope.push_root(value);
  let key = PropertyKey::from_string(scope.alloc_string(name)?);
  let desc = PropertyDescriptor {
    enumerable: true,
    configurable: true,
    kind: PropertyKind::Data {
      value,
      writable: true,
    },
  };
  scope.define_property(obj, key, desc)
}

#[test]
fn object_builtins_smoke() -> Result<(), VmError> {
  let mut rt = TestRealm::new()?;

  let object = rt.realm.intrinsics().object_constructor();

  let mut scope = rt.heap.scope();

  // Global binding exists and is callable.
  assert_eq!(
    get_own_data_property(&mut scope, rt.realm.global_object(), "Object")?,
    Some(Value::Object(object))
  );
  let _ = rt
    .vm
    .call(&mut scope, Value::Object(object), Value::Undefined, &[])?;

  // Object.defineProperty
  let define_property = get_own_data_property(&mut scope, object, "defineProperty")?
    .expect("Object.defineProperty should exist");
  let Value::Object(define_property) = define_property else {
    panic!("Object.defineProperty should be a function object");
  };

  let o = scope.alloc_object()?;
  scope.push_root(Value::Object(o));

  // { value: 1 }
  let desc = scope.alloc_object()?;
  scope.push_root(Value::Object(desc));
  define_enumerable_data_property(&mut scope, desc, "value", Value::Number(1.0))?;

  let x = scope.alloc_string("x")?;
  let args = [Value::Object(o), Value::String(x), Value::Object(desc)];
  let _ = rt.vm.call(
    &mut scope,
    Value::Object(define_property),
    Value::Object(object),
    &args,
  )?;

  let x_key = PropertyKey::from_string(x);
  assert_eq!(
    scope
      .heap()
      .object_get_own_data_property_value(o, &x_key)?,
    Some(Value::Number(1.0))
  );

  // Object.keys
  let keys = get_own_data_property(&mut scope, object, "keys")?.expect("Object.keys should exist");
  let Value::Object(keys) = keys else {
    panic!("Object.keys should be a function object");
  };

  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj));
  define_enumerable_data_property(&mut scope, obj, "a", Value::Number(1.0))?;
  define_enumerable_data_property(&mut scope, obj, "b", Value::Number(2.0))?;

  let args = [Value::Object(obj)];
  let result = rt
    .vm
    .call(&mut scope, Value::Object(keys), Value::Object(object), &args)?;
  let Value::Object(arr) = result else {
    panic!("Object.keys should return an object");
  };

  let length = get_own_data_property(&mut scope, arr, "length")?.expect("length should exist");
  assert_eq!(length, Value::Number(2.0));

  Ok(())
}

