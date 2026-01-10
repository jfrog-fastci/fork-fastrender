use vm_js::{
  GcObject, Heap, HeapLimits, PropertyDescriptor, PropertyKey, PropertyKind, Realm, Scope, Value,
  Vm, VmError, VmHost, VmHostHooks, VmOptions,
};

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

fn return_two_native(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Ok(Value::Number(2.0))
}

#[test]
fn object_constructor_is_callable() -> Result<(), VmError> {
  let mut rt = TestRealm::new()?;

  let object = rt.realm.intrinsics().object_constructor();

  let mut scope = rt.heap.scope();

  // Global binding exists.
  assert_eq!(
    get_own_data_property(&mut scope, rt.realm.global_object(), "Object")?,
    Some(Value::Object(object))
  );

  // `typeof Object === "function"` (approximated by "call doesn't error").
  let _ = rt
    .vm
    .call_without_host(&mut scope, Value::Object(object), Value::Undefined, &[])?;

  Ok(())
}

#[test]
fn object_define_property_defines_value() -> Result<(), VmError> {
  let mut rt = TestRealm::new()?;
  let object = rt.realm.intrinsics().object_constructor();

  let mut scope = rt.heap.scope();

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

  let _ = rt
    .vm
    .call_without_host(&mut scope, Value::Object(define_property), Value::Object(object), &args)?;

  let x_key = PropertyKey::from_string(x);
  assert_eq!(
    scope.heap().object_get_own_data_property_value(o, &x_key)?,
    Some(Value::Number(1.0))
  );

  Ok(())
}

#[test]
fn object_define_property_boxes_primitive_target() -> Result<(), VmError> {
  let mut rt = TestRealm::new()?;
  let object = rt.realm.intrinsics().object_constructor();
  let number_proto = rt.realm.intrinsics().number_prototype();

  let mut scope = rt.heap.scope();

  let define_property = get_own_data_property(&mut scope, object, "defineProperty")?
    .expect("Object.defineProperty should exist");
  let Value::Object(define_property) = define_property else {
    panic!("Object.defineProperty should be a function object");
  };

  // { value: 1 }
  let desc = scope.alloc_object()?;
  scope.push_root(Value::Object(desc))?;
  define_enumerable_data_property(&mut scope, desc, "value", Value::Number(1.0))?;

  let x = scope.alloc_string("x")?;
  let args = [Value::Number(5.0), Value::String(x), Value::Object(desc)];

  let out = rt
    .vm
    .call_without_host(&mut scope, Value::Object(define_property), Value::Object(object), &args)?;
  let Value::Object(out_obj) = out else {
    panic!("Object.defineProperty should return an object");
  };
  scope.push_root(Value::Object(out_obj))?;

  // Returned object is a boxed Number with an own `x` property.
  assert_eq!(
    get_own_data_property(&mut scope, out_obj, "x")?,
    Some(Value::Number(1.0))
  );
  assert_eq!(scope.heap().object_prototype(out_obj)?, Some(number_proto));

  Ok(())
}

#[test]
fn object_create_sets_prototype() -> Result<(), VmError> {
  let mut rt = TestRealm::new()?;
  let object = rt.realm.intrinsics().object_constructor();

  let mut scope = rt.heap.scope();

  let create = get_own_data_property(&mut scope, object, "create")?.expect("Object.create exists");
  let Value::Object(create) = create else {
    panic!("Object.create should be a function object");
  };

  // { y: 2 }
  let p = scope.alloc_object()?;
  scope.push_root(Value::Object(p))?;
  define_enumerable_data_property(&mut scope, p, "y", Value::Number(2.0))?;

  let y_key = PropertyKey::from_string(scope.alloc_string("y")?);

  let args = [Value::Object(p)];
  let o = rt
    .vm
    .call_without_host(&mut scope, Value::Object(create), Value::Object(object), &args)?;
  let Value::Object(o) = o else {
    panic!("Object.create should return an object");
  };
  let desc = scope
    .heap()
    .get_property(o, &y_key)?
    .expect("property should be found via prototype");
  let PropertyKind::Data { value, .. } = desc.kind else {
    panic!("expected data property");
  };
  assert_eq!(value, Value::Number(2.0));

  Ok(())
}

#[test]
fn object_keys_returns_enumerable_string_keys() -> Result<(), VmError> {
  let mut rt = TestRealm::new()?;
  let object = rt.realm.intrinsics().object_constructor();

  let mut scope = rt.heap.scope();

  let keys = get_own_data_property(&mut scope, object, "keys")?.expect("Object.keys exists");
  let Value::Object(keys) = keys else {
    panic!("Object.keys should be a function object");
  };

  // { a: 1, b: 2 }
  let o = scope.alloc_object()?;
  scope.push_root(Value::Object(o))?;
  define_enumerable_data_property(&mut scope, o, "a", Value::Number(1.0))?;
  define_enumerable_data_property(&mut scope, o, "b", Value::Number(2.0))?;

  let args = [Value::Object(o)];
  let result = rt
    .vm
    .call_without_host(&mut scope, Value::Object(keys), Value::Object(object), &args)?;
  let Value::Object(arr) = result else {
    panic!("Object.keys should return an object");
  };

  let length = get_own_data_property(&mut scope, arr, "length")?.expect("length exists");
  assert_eq!(length, Value::Number(2.0));

  Ok(())
}

#[test]
fn object_keys_boxes_primitive_target() -> Result<(), VmError> {
  let mut rt = TestRealm::new()?;
  let object = rt.realm.intrinsics().object_constructor();

  let mut scope = rt.heap.scope();

  let keys = get_own_data_property(&mut scope, object, "keys")?.expect("Object.keys exists");
  let Value::Object(keys) = keys else {
    panic!("Object.keys should be a function object");
  };

  let args = [Value::Number(1.0)];
  let result = rt
    .vm
    .call_without_host(&mut scope, Value::Object(keys), Value::Object(object), &args)?;
  let Value::Object(arr) = result else {
    panic!("Object.keys should return an object");
  };

  let length = get_own_data_property(&mut scope, arr, "length")?.expect("length exists");
  assert_eq!(length, Value::Number(0.0));

  Ok(())
}

#[test]
fn object_keys_on_string_returns_index_keys() -> Result<(), VmError> {
  let mut rt = TestRealm::new()?;
  let object = rt.realm.intrinsics().object_constructor();

  let mut scope = rt.heap.scope();

  let keys = get_own_data_property(&mut scope, object, "keys")?.expect("Object.keys exists");
  let Value::Object(keys) = keys else {
    panic!("Object.keys should be a function object");
  };

  let s = scope.alloc_string("ab")?;
  let args = [Value::String(s)];
  let result = rt
    .vm
    .call_without_host(&mut scope, Value::Object(keys), Value::Object(object), &args)?;
  let Value::Object(arr) = result else {
    panic!("Object.keys should return an object");
  };

  let length = get_own_data_property(&mut scope, arr, "length")?.expect("length exists");
  assert_eq!(length, Value::Number(2.0));

  let v0 = get_own_data_property(&mut scope, arr, "0")?.expect("key 0 exists");
  let Value::String(v0s) = v0 else {
    panic!("expected Object.keys result[0] to be a string");
  };
  assert_eq!(scope.heap().get_string(v0s)?.to_utf8_lossy(), "0");

  let v1 = get_own_data_property(&mut scope, arr, "1")?.expect("key 1 exists");
  let Value::String(v1s) = v1 else {
    panic!("expected Object.keys result[1] to be a string");
  };
  assert_eq!(scope.heap().get_string(v1s)?.to_utf8_lossy(), "1");

  Ok(())
}

#[test]
fn object_keys_on_uint8_array_returns_index_keys() -> Result<(), VmError> {
  let mut rt = TestRealm::new()?;
  let object = rt.realm.intrinsics().object_constructor();
  let uint8_array = rt.realm.intrinsics().uint8_array();

  let mut scope = rt.heap.scope();

  let keys = get_own_data_property(&mut scope, object, "keys")?.expect("Object.keys exists");
  let Value::Object(keys) = keys else {
    panic!("Object.keys should be a function object");
  };

  let args = [Value::Number(2.0)];
  let view = rt.vm.construct_without_host(
    &mut scope,
    Value::Object(uint8_array),
    &args,
    Value::Object(uint8_array),
  )?;
  let Value::Object(view) = view else {
    panic!("Uint8Array constructor should return an object");
  };
  scope.push_root(Value::Object(view))?;

  let args = [Value::Object(view)];
  let result = rt
    .vm
    .call_without_host(&mut scope, Value::Object(keys), Value::Object(object), &args)?;
  let Value::Object(arr) = result else {
    panic!("Object.keys should return an object");
  };

  let length = get_own_data_property(&mut scope, arr, "length")?.expect("length exists");
  assert_eq!(length, Value::Number(2.0));

  let v0 = get_own_data_property(&mut scope, arr, "0")?.expect("key 0 exists");
  let Value::String(v0s) = v0 else {
    panic!("expected Object.keys result[0] to be a string");
  };
  assert_eq!(scope.heap().get_string(v0s)?.to_utf8_lossy(), "0");

  let v1 = get_own_data_property(&mut scope, arr, "1")?.expect("key 1 exists");
  let Value::String(v1s) = v1 else {
    panic!("expected Object.keys result[1] to be a string");
  };
  assert_eq!(scope.heap().get_string(v1s)?.to_utf8_lossy(), "1");

  Ok(())
}

#[test]
fn object_get_prototype_of_boxes_primitives() -> Result<(), VmError> {
  let mut rt = TestRealm::new()?;
  let object = rt.realm.intrinsics().object_constructor();
  let number_proto = rt.realm.intrinsics().number_prototype();

  let mut scope = rt.heap.scope();

  let get_proto =
    get_own_data_property(&mut scope, object, "getPrototypeOf")?.expect("Object.getPrototypeOf exists");
  let Value::Object(get_proto) = get_proto else {
    panic!("Object.getPrototypeOf should be a function object");
  };

  let args = [Value::Number(1.0)];
  let result = rt
    .vm
    .call_without_host(&mut scope, Value::Object(get_proto), Value::Object(object), &args)?;
  assert_eq!(result, Value::Object(number_proto));

  Ok(())
}

#[test]
fn object_set_prototype_of_does_not_box_primitives() -> Result<(), VmError> {
  let mut rt = TestRealm::new()?;
  let object = rt.realm.intrinsics().object_constructor();
  let object_proto = rt.realm.intrinsics().object_prototype();

  let mut scope = rt.heap.scope();

  let set_proto =
    get_own_data_property(&mut scope, object, "setPrototypeOf")?.expect("Object.setPrototypeOf exists");
  let Value::Object(set_proto) = set_proto else {
    panic!("Object.setPrototypeOf should be a function object");
  };

  let args = [Value::Number(1.0), Value::Object(object_proto)];
  let out = rt
    .vm
    .call_without_host(&mut scope, Value::Object(set_proto), Value::Object(object), &args)?;
  assert_eq!(out, Value::Number(1.0));
  Ok(())
}

#[test]
fn object_assign_copies_enumerable_properties_and_invokes_getters() -> Result<(), VmError> {
  let mut rt = TestRealm::new()?;
  let object = rt.realm.intrinsics().object_constructor();

  let mut scope = rt.heap.scope();

  let assign = get_own_data_property(&mut scope, object, "assign")?.expect("Object.assign exists");
  let Value::Object(assign) = assign else {
    panic!("Object.assign should be a function object");
  };

  let target = scope.alloc_object()?;
  scope.push_root(Value::Object(target))?;
  let source = scope.alloc_object()?;
  scope.push_root(Value::Object(source))?;

  // Enumerable data property.
  define_enumerable_data_property(&mut scope, source, "a", Value::Number(1.0))?;

  // Enumerable accessor property whose getter returns 2.
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
  let out = rt
    .vm
    .call_without_host(&mut scope, Value::Object(assign), Value::Object(object), &args)?;
  assert_eq!(out, Value::Object(target));

  assert_eq!(
    get_own_data_property(&mut scope, target, "a")?,
    Some(Value::Number(1.0))
  );
  assert_eq!(
    get_own_data_property(&mut scope, target, "b")?,
    Some(Value::Number(2.0))
  );

  Ok(())
}

#[test]
fn object_assign_boxes_primitive_target() -> Result<(), VmError> {
  let mut rt = TestRealm::new()?;
  let object = rt.realm.intrinsics().object_constructor();
  let number_proto = rt.realm.intrinsics().number_prototype();

  let mut scope = rt.heap.scope();

  let assign = get_own_data_property(&mut scope, object, "assign")?.expect("Object.assign exists");
  let Value::Object(assign) = assign else {
    panic!("Object.assign should be a function object");
  };

  let source = scope.alloc_object()?;
  scope.push_root(Value::Object(source))?;
  define_enumerable_data_property(&mut scope, source, "a", Value::Number(2.0))?;

  let args = [Value::Number(7.0), Value::Object(source)];
  let out = rt
    .vm
    .call_without_host(&mut scope, Value::Object(assign), Value::Object(object), &args)?;
  let Value::Object(out_obj) = out else {
    panic!("Object.assign should return an object");
  };
  scope.push_root(Value::Object(out_obj))?;

  assert_eq!(scope.heap().object_prototype(out_obj)?, Some(number_proto));
  assert_eq!(
    get_own_data_property(&mut scope, out_obj, "a")?,
    Some(Value::Number(2.0))
  );

  Ok(())
}

#[test]
fn object_assign_copies_string_index_properties() -> Result<(), VmError> {
  let mut rt = TestRealm::new()?;
  let object = rt.realm.intrinsics().object_constructor();

  let mut scope = rt.heap.scope();

  let assign = get_own_data_property(&mut scope, object, "assign")?.expect("Object.assign exists");
  let Value::Object(assign) = assign else {
    panic!("Object.assign should be a function object");
  };

  let target = scope.alloc_object()?;
  scope.push_root(Value::Object(target))?;

  let source = scope.alloc_string("ab")?;
  let args = [Value::Object(target), Value::String(source)];
  let out = rt
    .vm
    .call_without_host(&mut scope, Value::Object(assign), Value::Object(object), &args)?;
  assert_eq!(out, Value::Object(target));

  let v0 = get_own_data_property(&mut scope, target, "0")?.expect("target[0] exists");
  let Value::String(v0s) = v0 else {
    panic!("expected target[0] to be a string");
  };
  assert_eq!(scope.heap().get_string(v0s)?.to_utf8_lossy(), "a");

  let v1 = get_own_data_property(&mut scope, target, "1")?.expect("target[1] exists");
  let Value::String(v1s) = v1 else {
    panic!("expected target[1] to be a string");
  };
  assert_eq!(scope.heap().get_string(v1s)?.to_utf8_lossy(), "b");

  Ok(())
}

#[test]
fn object_assign_copies_uint8_array_index_properties() -> Result<(), VmError> {
  let mut rt = TestRealm::new()?;
  let object = rt.realm.intrinsics().object_constructor();
  let uint8_array = rt.realm.intrinsics().uint8_array();

  let mut scope = rt.heap.scope();

  let assign = get_own_data_property(&mut scope, object, "assign")?.expect("Object.assign exists");
  let Value::Object(assign) = assign else {
    panic!("Object.assign should be a function object");
  };

  let target = scope.alloc_object()?;
  scope.push_root(Value::Object(target))?;

  // new Uint8Array(2)
  let args = [Value::Number(2.0)];
  let source = rt.vm.construct_without_host(
    &mut scope,
    Value::Object(uint8_array),
    &args,
    Value::Object(uint8_array),
  )?;
  let Value::Object(source) = source else {
    panic!("Uint8Array constructor should return an object");
  };
  scope.push_root(Value::Object(source))?;

  // source[0] = 1; source[1] = 2;
  let key0_s = scope.alloc_string("0")?;
  scope.push_root(Value::String(key0_s))?;
  let key0 = PropertyKey::from_string(key0_s);
  scope.define_property(
    source,
    key0,
    PropertyDescriptor {
      enumerable: true,
      configurable: false,
      kind: PropertyKind::Data {
        value: Value::Number(1.0),
        writable: true,
      },
    },
  )?;
  let key1_s = scope.alloc_string("1")?;
  scope.push_root(Value::String(key1_s))?;
  let key1 = PropertyKey::from_string(key1_s);
  scope.define_property(
    source,
    key1,
    PropertyDescriptor {
      enumerable: true,
      configurable: false,
      kind: PropertyKind::Data {
        value: Value::Number(2.0),
        writable: true,
      },
    },
  )?;

  let args = [Value::Object(target), Value::Object(source)];
  let out = rt
    .vm
    .call_without_host(&mut scope, Value::Object(assign), Value::Object(object), &args)?;
  assert_eq!(out, Value::Object(target));

  assert_eq!(
    get_own_data_property(&mut scope, target, "0")?,
    Some(Value::Number(1.0))
  );
  assert_eq!(
    get_own_data_property(&mut scope, target, "1")?,
    Some(Value::Number(2.0))
  );

  Ok(())
}

#[test]
fn object_assign_throws_when_setting_non_writable_target_property() -> Result<(), VmError> {
  let mut rt = TestRealm::new()?;
  let object = rt.realm.intrinsics().object_constructor();

  let mut scope = rt.heap.scope();

  let assign = get_own_data_property(&mut scope, object, "assign")?.expect("Object.assign exists");
  let Value::Object(assign) = assign else {
    panic!("Object.assign should be a function object");
  };

  let target = scope.alloc_object()?;
  scope.push_root(Value::Object(target))?;
  let source = scope.alloc_object()?;
  scope.push_root(Value::Object(source))?;

  // target.x is non-writable.
  let key_x = PropertyKey::from_string(scope.alloc_string("x")?);
  scope.define_property(
    target,
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

  // source.x = 2 (enumerable).
  define_enumerable_data_property(&mut scope, source, "x", Value::Number(2.0))?;

  let args = [Value::Object(target), Value::Object(source)];
  let err = rt
    .vm
    .call_without_host(&mut scope, Value::Object(assign), Value::Object(object), &args)
    .unwrap_err();
  // `Vm::call*` coerces internal `VmError::TypeError` into a thrown `TypeError` value so it is
  // catchable by non-evaluator call sites (Promise jobs, host callbacks, etc.).
  assert!(
    matches!(err, VmError::Throw(_) | VmError::ThrowWithStack { .. }),
    "expected Object.assign to throw, got {err:?}"
  );

  Ok(())
}
