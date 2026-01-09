use vm_js::{
  GcObject, Heap, HeapLimits, PropertyDescriptor, PropertyKey, PropertyKind, Realm, Scope, Value,
  Vm, VmError, VmHostHooks, VmOptions,
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

fn return_two_native(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Ok(Value::Number(2.0))
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

fn define_enumerable_data_property(
  scope: &mut Scope<'_>,
  obj: GcObject,
  name: &str,
  value: Value,
) -> Result<(), VmError> {
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;
  scope.push_root(value)?;
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

fn to_utf8_string(heap: &Heap, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string value");
  };
  heap.get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn object_builtins_smoke() -> Result<(), VmError> {
  let mut rt = TestRealm::new()?;

  let object = rt.realm.intrinsics().object_constructor();

  let mut scope = rt.heap.scope();
  #[derive(Default)]
  struct NoopHostHooks;

  impl vm_js::VmHostHooks for NoopHostHooks {
    fn host_enqueue_promise_job(&mut self, _job: vm_js::Job, _realm: Option<vm_js::RealmId>) {
      panic!("unexpected Promise job enqueued during Object builtins smoke test");
    }
  }

  let mut host_hooks = NoopHostHooks::default();

  // Global binding exists and is callable.
  assert_eq!(
    get_own_data_property(&mut scope, rt.realm.global_object(), "Object")?,
    Some(Value::Object(object))
  );
  let _ = rt
    .vm
    .call_with_host(&mut scope, &mut host_hooks, Value::Object(object), Value::Undefined, &[])?;

  // Object.defineProperty
  let define_property = get_own_data_property(&mut scope, object, "defineProperty")?
    .expect("Object.defineProperty should exist");
  let Value::Object(define_property) = define_property else {
    panic!("Object.defineProperty should be a function object");
  };

  let o = scope.alloc_object()?;
  scope.push_root(Value::Object(o))?;

  // { value: 1 }
  let desc = scope.alloc_object()?;
  scope.push_root(Value::Object(desc))?;
  define_enumerable_data_property(&mut scope, desc, "value", Value::Number(1.0))?;

  let x = scope.alloc_string("x")?;
  let args = [Value::Object(o), Value::String(x), Value::Object(desc)];
  let _ = rt.vm.call_with_host(
    &mut scope,
    &mut host_hooks,
    Value::Object(define_property),
    Value::Object(object),
    &args,
  )?;

  let x_key = PropertyKey::from_string(x);
  assert_eq!(
    scope.heap().object_get_own_data_property_value(o, &x_key)?,
    Some(Value::Number(1.0))
  );

  // Object.create + Object.getPrototypeOf
  let create =
    get_own_data_property(&mut scope, object, "create")?.expect("Object.create should exist");
  let Value::Object(create) = create else {
    panic!("Object.create should be a function object");
  };

  let get_proto = get_own_data_property(&mut scope, object, "getPrototypeOf")?
    .expect("Object.getPrototypeOf should exist");
  let Value::Object(get_proto) = get_proto else {
    panic!("Object.getPrototypeOf should be a function object");
  };

  // { y: 2 }
  let p = scope.alloc_object()?;
  scope.push_root(Value::Object(p))?;
  define_enumerable_data_property(&mut scope, p, "y", Value::Number(2.0))?;
  let y_key = PropertyKey::from_string(scope.alloc_string("y")?);

  let args = [Value::Object(p)];
  let created = rt.vm.call_with_host(
    &mut scope,
    &mut host_hooks,
    Value::Object(create),
    Value::Object(object),
    &args,
  )?;
  let Value::Object(created) = created else {
    panic!("Object.create should return an object");
  };
  scope.push_root(Value::Object(created))?;

  // Inherited property lookup via prototype chain.
  let desc = scope
    .heap()
    .get_property(created, &y_key)?
    .expect("property should be found via prototype");
  let PropertyKind::Data { value, .. } = desc.kind else {
    panic!("expected data property");
  };
  assert_eq!(value, Value::Number(2.0));

  // getPrototypeOf(created) === p
  let args = [Value::Object(created)];
  let proto = rt.vm.call_with_host(
    &mut scope,
    &mut host_hooks,
    Value::Object(get_proto),
    Value::Object(object),
    &args,
  )?;
  assert_eq!(proto, Value::Object(p));

  // Object.keys
  let keys = get_own_data_property(&mut scope, object, "keys")?.expect("Object.keys should exist");
  let Value::Object(keys) = keys else {
    panic!("Object.keys should be a function object");
  };

  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  define_enumerable_data_property(&mut scope, obj, "a", Value::Number(1.0))?;
  define_enumerable_data_property(&mut scope, obj, "b", Value::Number(2.0))?;

  let args = [Value::Object(obj)];
  let result = rt.vm.call_with_host(
    &mut scope,
    &mut host_hooks,
    Value::Object(keys),
    Value::Object(object),
    &args,
  )?;
  let Value::Object(arr) = result else {
    panic!("Object.keys should return an object");
  };

  let length = get_own_data_property(&mut scope, arr, "length")?.expect("length should exist");
  assert_eq!(length, Value::Number(2.0));

  // Keys are returned in insertion order for non-index string keys.
  let first = get_own_data_property(&mut scope, arr, "0")?.expect("key 0 should exist");
  let second = get_own_data_property(&mut scope, arr, "1")?.expect("key 1 should exist");
  assert_eq!(to_utf8_string(scope.heap(), first), "a");
  assert_eq!(to_utf8_string(scope.heap(), second), "b");

  // Object.assign
  let assign =
    get_own_data_property(&mut scope, object, "assign")?.expect("Object.assign should exist");
  let Value::Object(assign) = assign else {
    panic!("Object.assign should be a function object");
  };

  let target = scope.alloc_object()?;
  scope.push_root(Value::Object(target))?;
  let source = scope.alloc_object()?;
  scope.push_root(Value::Object(source))?;
  define_enumerable_data_property(&mut scope, source, "a", Value::Number(1.0))?;

  // Enumerable accessor property: ensure `Object.assign` invokes getters (`Get` semantics).
  let getter_id = rt.vm.register_native_call(return_two_native)?;
  let getter_name = scope.alloc_string("")?;
  let getter = scope.alloc_native_function(getter_id, None, getter_name, 0)?;
  scope.push_root(Value::Object(getter))?;
  let key_b = PropertyKey::from_string(scope.alloc_string("b")?);
  scope.define_property(
    source,
    key_b,
    PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Accessor {
        get: Value::Object(getter),
        set: Value::Undefined,
      },
    },
  )?;

  let args = [Value::Object(target), Value::Object(source)];
  let out = rt.vm.call_with_host(
    &mut scope,
    &mut host_hooks,
    Value::Object(assign),
    Value::Object(object),
    &args,
  )?;
  assert_eq!(out, Value::Object(target));
  assert_eq!(
    get_own_data_property(&mut scope, target, "a")?,
    Some(Value::Number(1.0))
  );
  assert_eq!(
    get_own_data_property(&mut scope, target, "b")?,
    Some(Value::Number(2.0))
  );

  // Assigning onto a non-writable target property should throw (failed `Set`).
  let ro_target = scope.alloc_object()?;
  scope.push_root(Value::Object(ro_target))?;
  let ro_source = scope.alloc_object()?;
  scope.push_root(Value::Object(ro_source))?;

  let key_x = PropertyKey::from_string(scope.alloc_string("x")?);
  scope.define_property(
    ro_target,
    key_x,
    PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::Number(1.0),
        writable: false,
      },
    },
  )?;
  define_enumerable_data_property(&mut scope, ro_source, "x", Value::Number(2.0))?;

  let args = [Value::Object(ro_target), Value::Object(ro_source)];
  let err = rt
    .vm
    .call_with_host(
      &mut scope,
      &mut host_hooks,
      Value::Object(assign),
      Value::Object(object),
      &args,
    )
    .unwrap_err();
  assert!(matches!(err, VmError::TypeError(_)));

  // Object.setPrototypeOf
  let set_proto = get_own_data_property(&mut scope, object, "setPrototypeOf")?
    .expect("Object.setPrototypeOf should exist");
  let Value::Object(set_proto) = set_proto else {
    panic!("Object.setPrototypeOf should be a function object");
  };

  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  let args = [Value::Object(obj), Value::Object(p)];
  let out = rt.vm.call_with_host(
    &mut scope,
    &mut host_hooks,
    Value::Object(set_proto),
    Value::Object(object),
    &args,
  )?;
  assert_eq!(out, Value::Object(obj));

  let args = [Value::Object(obj)];
  let proto = rt.vm.call_with_host(
    &mut scope,
    &mut host_hooks,
    Value::Object(get_proto),
    Value::Object(object),
    &args,
  )?;
  assert_eq!(proto, Value::Object(p));

  Ok(())
}
