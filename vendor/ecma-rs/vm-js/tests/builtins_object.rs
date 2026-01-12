use vm_js::{
  GcObject, Heap, HeapLimits, PropertyDescriptor, PropertyDescriptorPatch, PropertyKey, PropertyKind,
  Realm, Scope, Value, Vm, VmError, VmHost, VmHostHooks, VmOptions,
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

fn return_ok_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Ok(Value::String(scope.alloc_string("ok")?))
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
fn object_define_property_reads_descriptor_fields_via_get() -> Result<(), VmError> {
  let mut rt = TestRealm::new()?;
  let object = rt.realm.intrinsics().object_constructor();

  let getter_call_id = rt.vm.register_native_call(return_two_native)?;

  let mut scope = rt.heap.scope();

  let define_property = get_own_data_property(&mut scope, object, "defineProperty")?
    .expect("Object.defineProperty should exist");
  let Value::Object(define_property) = define_property else {
    panic!("Object.defineProperty should be a function object");
  };

  let o = scope.alloc_object()?;
  scope.push_root(Value::Object(o))?;

  let desc = scope.alloc_object()?;
  scope.push_root(Value::Object(desc))?;

  // Define an accessor `value` property so ToPropertyDescriptor must use `Get` (not `GetOwn`).
  let getter_name = scope.alloc_string("getValue")?;
  let getter = scope.alloc_native_function(getter_call_id, None, getter_name, 0)?;
  scope.push_root(Value::Object(getter))?;

  let value_key = PropertyKey::from_string(scope.alloc_string("value")?);
  scope.define_property(
    desc,
    value_key,
    PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Accessor {
        get: Value::Object(getter),
        set: Value::Undefined,
      },
    },
  )?;

  let x = scope.alloc_string("x")?;
  let args = [Value::Object(o), Value::String(x), Value::Object(desc)];

  let _ = rt.vm.call_without_host(
    &mut scope,
    Value::Object(define_property),
    Value::Object(object),
    &args,
  )?;

  let x_key = PropertyKey::from_string(x);
  assert_eq!(
    scope.heap().object_get_own_data_property_value(o, &x_key)?,
    Some(Value::Number(2.0))
  );

  Ok(())
}

#[test]
fn object_define_property_throws_on_primitive_target() -> Result<(), VmError> {
  let mut rt = TestRealm::new()?;
  let object = rt.realm.intrinsics().object_constructor();
  let type_error_proto = rt.realm.intrinsics().type_error_prototype();

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

  let err = rt
    .vm
    .call_without_host(&mut scope, Value::Object(define_property), Value::Object(object), &args);
  let thrown = match err {
    Ok(v) => panic!("expected Object.defineProperty to throw, got {v:?}"),
    Err(VmError::Throw(v) | VmError::ThrowWithStack { value: v, .. }) => v,
    Err(e) => return Err(e),
  };
  let Value::Object(err_obj) = thrown else {
    panic!("expected thrown value to be an object, got {thrown:?}");
  };
  assert_eq!(scope.heap().object_prototype(err_obj)?, Some(type_error_proto));

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
fn object_create_defines_properties() -> Result<(), VmError> {
  let mut rt = TestRealm::new()?;
  let object = rt.realm.intrinsics().object_constructor();

  let mut scope = rt.heap.scope();

  let create = get_own_data_property(&mut scope, object, "create")?.expect("Object.create exists");
  let Value::Object(create) = create else {
    panic!("Object.create should be a function object");
  };

  let p = scope.alloc_object()?;
  scope.push_root(Value::Object(p))?;

  // { x: { value: 1, enumerable: true } }
  let desc_x = scope.alloc_object()?;
  scope.push_root(Value::Object(desc_x))?;
  define_enumerable_data_property(&mut scope, desc_x, "value", Value::Number(1.0))?;
  define_enumerable_data_property(&mut scope, desc_x, "enumerable", Value::Bool(true))?;

  let props = scope.alloc_object()?;
  scope.push_root(Value::Object(props))?;
  define_enumerable_data_property(&mut scope, props, "x", Value::Object(desc_x))?;

  let args = [Value::Object(p), Value::Object(props)];
  let o = rt
    .vm
    .call_without_host(&mut scope, Value::Object(create), Value::Object(object), &args)?;
  let Value::Object(o) = o else {
    panic!("Object.create should return an object");
  };
  scope.push_root(Value::Object(o))?;

  assert_eq!(scope.heap().object_prototype(o)?, Some(p));
  assert_eq!(
    get_own_data_property(&mut scope, o, "x")?,
    Some(Value::Number(1.0))
  );

  // Descriptor defaults: configurable/writable default to false when omitted.
  let x_key = PropertyKey::from_string(scope.alloc_string("x")?);
  let desc = scope
    .heap()
    .object_get_own_property(o, &x_key)?
    .expect("x should be an own property");
  assert!(desc.enumerable);
  assert!(!desc.configurable);
  let PropertyKind::Data { writable, value } = desc.kind else {
    panic!("expected x to be a data property");
  };
  assert_eq!(value, Value::Number(1.0));
  assert!(!writable);

  Ok(())
}

#[test]
fn object_define_properties_defines_multiple() -> Result<(), VmError> {
  let mut rt = TestRealm::new()?;
  let object = rt.realm.intrinsics().object_constructor();

  let mut scope = rt.heap.scope();

  let define_properties = get_own_data_property(&mut scope, object, "defineProperties")?
    .expect("Object.defineProperties exists");
  let Value::Object(define_properties) = define_properties else {
    panic!("Object.defineProperties should be a function object");
  };

  let target = scope.alloc_object()?;
  scope.push_root(Value::Object(target))?;

  let desc_a = scope.alloc_object()?;
  scope.push_root(Value::Object(desc_a))?;
  define_enumerable_data_property(&mut scope, desc_a, "value", Value::Number(1.0))?;

  let desc_b = scope.alloc_object()?;
  scope.push_root(Value::Object(desc_b))?;
  define_enumerable_data_property(&mut scope, desc_b, "value", Value::Number(2.0))?;

  let props = scope.alloc_object()?;
  scope.push_root(Value::Object(props))?;
  define_enumerable_data_property(&mut scope, props, "a", Value::Object(desc_a))?;
  define_enumerable_data_property(&mut scope, props, "b", Value::Object(desc_b))?;

  let args = [Value::Object(target), Value::Object(props)];
  let out = rt.vm.call_without_host(
    &mut scope,
    Value::Object(define_properties),
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
  Ok(())
}

#[test]
fn object_get_own_property_descriptor_reports_attributes() -> Result<(), VmError> {
  let mut rt = TestRealm::new()?;
  let object = rt.realm.intrinsics().object_constructor();

  let mut scope = rt.heap.scope();

  let get_own_desc =
    get_own_data_property(&mut scope, object, "getOwnPropertyDescriptor")?.expect("exists");
  let Value::Object(get_own_desc) = get_own_desc else {
    panic!("Object.getOwnPropertyDescriptor should be a function object");
  };

  let o = scope.alloc_object()?;
  scope.push_root(Value::Object(o))?;
  let x_key = PropertyKey::from_string(scope.alloc_string("x")?);
  scope.define_property(
    o,
    x_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::Number(1.0),
        writable: false,
      },
    },
  )?;

  let args = [Value::Object(o), Value::String(scope.alloc_string("x")?)];
  let out = rt
    .vm
    .call_without_host(&mut scope, Value::Object(get_own_desc), Value::Object(object), &args)?;
  let Value::Object(desc_obj) = out else {
    panic!("expected descriptor object");
  };
  scope.push_root(Value::Object(desc_obj))?;

  assert_eq!(
    get_own_data_property(&mut scope, desc_obj, "value")?,
    Some(Value::Number(1.0))
  );
  assert_eq!(
    get_own_data_property(&mut scope, desc_obj, "writable")?,
    Some(Value::Bool(false))
  );
  assert_eq!(
    get_own_data_property(&mut scope, desc_obj, "enumerable")?,
    Some(Value::Bool(false))
  );
  assert_eq!(
    get_own_data_property(&mut scope, desc_obj, "configurable")?,
    Some(Value::Bool(true))
  );

  let args = [Value::Object(o), Value::String(scope.alloc_string("y")?)];
  let out = rt
    .vm
    .call_without_host(&mut scope, Value::Object(get_own_desc), Value::Object(object), &args)?;
  assert_eq!(out, Value::Undefined);

  Ok(())
}

#[test]
fn object_get_own_property_descriptors_includes_symbol_keys() -> Result<(), VmError> {
  let mut rt = TestRealm::new()?;
  let object = rt.realm.intrinsics().object_constructor();

  let mut scope = rt.heap.scope();

  let get_own_descs =
    get_own_data_property(&mut scope, object, "getOwnPropertyDescriptors")?.expect("exists");
  let Value::Object(get_own_descs) = get_own_descs else {
    panic!("Object.getOwnPropertyDescriptors should be a function object");
  };

  let o = scope.alloc_object()?;
  scope.push_root(Value::Object(o))?;
  define_enumerable_data_property(&mut scope, o, "x", Value::Number(1.0))?;

  let sym = scope.alloc_symbol(Some("s"))?;
  scope.push_root(Value::Symbol(sym))?;
  scope.define_property(
    o,
    PropertyKey::from_symbol(sym),
    PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::Number(2.0),
        writable: true,
      },
    },
  )?;

  let args = [Value::Object(o)];
  let out = rt.vm.call_without_host(
    &mut scope,
    Value::Object(get_own_descs),
    Value::Object(object),
    &args,
  )?;
  let Value::Object(out_obj) = out else {
    panic!("expected object");
  };
  scope.push_root(Value::Object(out_obj))?;

  let x_desc = get_own_data_property(&mut scope, out_obj, "x")?.expect("x descriptor exists");
  let Value::Object(_x_desc_obj) = x_desc else {
    panic!("expected x descriptor to be an object");
  };

  let sym_key = PropertyKey::from_symbol(sym);
  let sym_desc = scope
    .heap()
    .object_get_own_data_property_value(out_obj, &sym_key)?
    .expect("symbol descriptor exists");
  let Value::Object(_sym_desc_obj) = sym_desc else {
    panic!("expected symbol descriptor to be an object");
  };

  Ok(())
}

#[test]
fn object_get_own_property_names_and_symbols_preserve_order() -> Result<(), VmError> {
  let mut rt = TestRealm::new()?;
  let object = rt.realm.intrinsics().object_constructor();

  let mut scope = rt.heap.scope();

  let get_names = get_own_data_property(&mut scope, object, "getOwnPropertyNames")?.expect("exists");
  let Value::Object(get_names) = get_names else {
    panic!("Object.getOwnPropertyNames should be a function object");
  };
  let get_syms =
    get_own_data_property(&mut scope, object, "getOwnPropertySymbols")?.expect("exists");
  let Value::Object(get_syms) = get_syms else {
    panic!("Object.getOwnPropertySymbols should be a function object");
  };

  let o = scope.alloc_object()?;
  scope.push_root(Value::Object(o))?;

  // Create order: array indices should sort numerically, other strings preserve insertion order.
  define_enumerable_data_property(&mut scope, o, "1", Value::Number(1.0))?;
  define_enumerable_data_property(&mut scope, o, "0", Value::Number(0.0))?;
  define_enumerable_data_property(&mut scope, o, "b", Value::Number(2.0))?;
  define_enumerable_data_property(&mut scope, o, "a", Value::Number(3.0))?;

  let sym = scope.alloc_symbol(Some("s"))?;
  scope.push_root(Value::Symbol(sym))?;
  scope.define_property(
    o,
    PropertyKey::from_symbol(sym),
    PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::Number(4.0),
        writable: true,
      },
    },
  )?;

  let args = [Value::Object(o)];
  let names = rt
    .vm
    .call_without_host(&mut scope, Value::Object(get_names), Value::Object(object), &args)?;
  let Value::Object(names) = names else {
    panic!("expected array object");
  };
  let length = get_own_data_property(&mut scope, names, "length")?.expect("length exists");
  assert_eq!(length, Value::Number(4.0));

  let v0 = get_own_data_property(&mut scope, names, "0")?.expect("names[0] exists");
  let Value::String(v0s) = v0 else {
    panic!("expected names[0] to be a string");
  };
  assert_eq!(scope.heap().get_string(v0s)?.to_utf8_lossy(), "0");

  let v1 = get_own_data_property(&mut scope, names, "1")?.expect("names[1] exists");
  let Value::String(v1s) = v1 else {
    panic!("expected names[1] to be a string");
  };
  assert_eq!(scope.heap().get_string(v1s)?.to_utf8_lossy(), "1");

  let v2 = get_own_data_property(&mut scope, names, "2")?.expect("names[2] exists");
  let Value::String(v2s) = v2 else {
    panic!("expected names[2] to be a string");
  };
  assert_eq!(scope.heap().get_string(v2s)?.to_utf8_lossy(), "b");

  let v3 = get_own_data_property(&mut scope, names, "3")?.expect("names[3] exists");
  let Value::String(v3s) = v3 else {
    panic!("expected names[3] to be a string");
  };
  assert_eq!(scope.heap().get_string(v3s)?.to_utf8_lossy(), "a");

  let syms = rt
    .vm
    .call_without_host(&mut scope, Value::Object(get_syms), Value::Object(object), &args)?;
  let Value::Object(syms) = syms else {
    panic!("expected array object");
  };
  let length = get_own_data_property(&mut scope, syms, "length")?.expect("length exists");
  assert_eq!(length, Value::Number(1.0));
  let v0 = get_own_data_property(&mut scope, syms, "0")?.expect("syms[0] exists");
  assert_eq!(v0, Value::Symbol(sym));

  Ok(())
}

#[test]
fn object_prevent_extensions_and_is_extensible() -> Result<(), VmError> {
  let mut rt = TestRealm::new()?;
  let object = rt.realm.intrinsics().object_constructor();

  let mut scope = rt.heap.scope();

  let prevent =
    get_own_data_property(&mut scope, object, "preventExtensions")?.expect("Object.preventExtensions exists");
  let Value::Object(prevent) = prevent else {
    panic!("Object.preventExtensions should be a function object");
  };
  let is_extensible =
    get_own_data_property(&mut scope, object, "isExtensible")?.expect("Object.isExtensible exists");
  let Value::Object(is_extensible) = is_extensible else {
    panic!("Object.isExtensible should be a function object");
  };

  let o = scope.alloc_object()?;
  scope.push_root(Value::Object(o))?;

  let args = [Value::Object(o)];
  let out = rt.vm.call_without_host(
    &mut scope,
    Value::Object(is_extensible),
    Value::Object(object),
    &args,
  )?;
  assert_eq!(out, Value::Bool(true));

  let out =
    rt.vm.call_without_host(&mut scope, Value::Object(prevent), Value::Object(object), &args)?;
  assert_eq!(out, Value::Object(o));

  let out = rt.vm.call_without_host(
    &mut scope,
    Value::Object(is_extensible),
    Value::Object(object),
    &args,
  )?;
  assert_eq!(out, Value::Bool(false));

  // Non-extensible objects reject new properties.
  let x_key = PropertyKey::from_string(scope.alloc_string("x")?);
  let ok = scope.create_data_property(o, x_key, Value::Number(1.0))?;
  assert!(!ok);

  // Primitives are returned unchanged.
  let args = [Value::Number(1.0)];
  let out =
    rt.vm.call_without_host(&mut scope, Value::Object(prevent), Value::Object(object), &args)?;
  assert_eq!(out, Value::Number(1.0));

  Ok(())
}

#[test]
fn object_seal_and_is_sealed() -> Result<(), VmError> {
  let mut rt = TestRealm::new()?;
  let object = rt.realm.intrinsics().object_constructor();

  let mut scope = rt.heap.scope();

  let seal = get_own_data_property(&mut scope, object, "seal")?.expect("Object.seal exists");
  let Value::Object(seal) = seal else {
    panic!("Object.seal should be a function object");
  };
  let is_sealed = get_own_data_property(&mut scope, object, "isSealed")?.expect("Object.isSealed exists");
  let Value::Object(is_sealed) = is_sealed else {
    panic!("Object.isSealed should be a function object");
  };
  let is_extensible =
    get_own_data_property(&mut scope, object, "isExtensible")?.expect("Object.isExtensible exists");
  let Value::Object(is_extensible) = is_extensible else {
    panic!("Object.isExtensible should be a function object");
  };

  let o = scope.alloc_object()?;
  scope.push_root(Value::Object(o))?;
  define_enumerable_data_property(&mut scope, o, "x", Value::Number(1.0))?;

  let args = [Value::Object(o)];
  let out = rt.vm.call_without_host(&mut scope, Value::Object(seal), Value::Object(object), &args)?;
  assert_eq!(out, Value::Object(o));

  let out = rt.vm.call_without_host(
    &mut scope,
    Value::Object(is_extensible),
    Value::Object(object),
    &args,
  )?;
  assert_eq!(out, Value::Bool(false));

  let out =
    rt.vm.call_without_host(&mut scope, Value::Object(is_sealed), Value::Object(object), &args)?;
  assert_eq!(out, Value::Bool(true));

  let x_key = PropertyKey::from_string(scope.alloc_string("x")?);
  let desc = scope
    .heap()
    .object_get_own_property(o, &x_key)?
    .expect("x should exist");
  assert!(!desc.configurable);
  let PropertyKind::Data { writable, .. } = desc.kind else {
    panic!("expected data property");
  };
  assert!(writable, "seal should not make data properties non-writable");

  // Non-configurable properties cannot be deleted.
  assert!(!scope.heap_mut().ordinary_delete(o, x_key)?);

  // Primitives are returned unchanged.
  let args = [Value::Number(1.0)];
  let out =
    rt.vm.call_without_host(&mut scope, Value::Object(seal), Value::Object(object), &args)?;
  assert_eq!(out, Value::Number(1.0));

  Ok(())
}

#[test]
fn object_freeze_and_is_frozen() -> Result<(), VmError> {
  let mut rt = TestRealm::new()?;
  let object = rt.realm.intrinsics().object_constructor();

  let mut scope = rt.heap.scope();

  let freeze = get_own_data_property(&mut scope, object, "freeze")?.expect("Object.freeze exists");
  let Value::Object(freeze) = freeze else {
    panic!("Object.freeze should be a function object");
  };
  let is_frozen =
    get_own_data_property(&mut scope, object, "isFrozen")?.expect("Object.isFrozen exists");
  let Value::Object(is_frozen) = is_frozen else {
    panic!("Object.isFrozen should be a function object");
  };
  let is_extensible =
    get_own_data_property(&mut scope, object, "isExtensible")?.expect("Object.isExtensible exists");
  let Value::Object(is_extensible) = is_extensible else {
    panic!("Object.isExtensible should be a function object");
  };

  let o = scope.alloc_object()?;
  scope.push_root(Value::Object(o))?;
  define_enumerable_data_property(&mut scope, o, "x", Value::Number(1.0))?;

  let args = [Value::Object(o)];
  let out =
    rt.vm.call_without_host(&mut scope, Value::Object(freeze), Value::Object(object), &args)?;
  assert_eq!(out, Value::Object(o));

  let out = rt.vm.call_without_host(
    &mut scope,
    Value::Object(is_extensible),
    Value::Object(object),
    &args,
  )?;
  assert_eq!(out, Value::Bool(false));

  let out =
    rt.vm.call_without_host(&mut scope, Value::Object(is_frozen), Value::Object(object), &args)?;
  assert_eq!(out, Value::Bool(true));

  let x_key = PropertyKey::from_string(scope.alloc_string("x")?);
  let desc = scope
    .heap()
    .object_get_own_property(o, &x_key)?
    .expect("x should exist");
  assert!(!desc.configurable);
  let PropertyKind::Data { writable, .. } = desc.kind else {
    panic!("expected data property");
  };
  assert!(!writable);

  // Frozen data properties reject value changes.
  let ok = scope.define_own_property(
    o,
    x_key,
    PropertyDescriptorPatch {
      value: Some(Value::Number(2.0)),
      ..Default::default()
    },
  )?;
  assert!(!ok);

  // Primitives are returned unchanged.
  let args = [Value::Number(1.0)];
  let out =
    rt.vm.call_without_host(&mut scope, Value::Object(freeze), Value::Object(object), &args)?;
  assert_eq!(out, Value::Number(1.0));

  Ok(())
}

#[test]
fn object_prototype_methods_work() -> Result<(), VmError> {
  let mut rt = TestRealm::new()?;
  let object = rt.realm.intrinsics().object_constructor();
  let object_proto = rt.realm.intrinsics().object_prototype();
  let number_proto = rt.realm.intrinsics().number_prototype();

  let mut scope = rt.heap.scope();

  let is_prototype_of =
    get_own_data_property(&mut scope, object_proto, "isPrototypeOf")?.expect("exists");
  let Value::Object(is_prototype_of) = is_prototype_of else {
    panic!("Object.prototype.isPrototypeOf should be a function object");
  };
  let property_is_enumerable =
    get_own_data_property(&mut scope, object_proto, "propertyIsEnumerable")?.expect("exists");
  let Value::Object(property_is_enumerable) = property_is_enumerable else {
    panic!("Object.prototype.propertyIsEnumerable should be a function object");
  };
  let to_locale_string =
    get_own_data_property(&mut scope, object_proto, "toLocaleString")?.expect("exists");
  let Value::Object(to_locale_string) = to_locale_string else {
    panic!("Object.prototype.toLocaleString should be a function object");
  };
  let value_of = get_own_data_property(&mut scope, object_proto, "valueOf")?.expect("exists");
  let Value::Object(value_of) = value_of else {
    panic!("Object.prototype.valueOf should be a function object");
  };

  // isPrototypeOf
  let p = scope.alloc_object()?;
  scope.push_root(Value::Object(p))?;
  let o = scope.alloc_object()?;
  scope.push_root(Value::Object(o))?;
  scope.heap_mut().object_set_prototype(o, Some(p))?;

  let args = [Value::Object(o)];
  let out = rt.vm.call_without_host(
    &mut scope,
    Value::Object(is_prototype_of),
    Value::Object(p),
    &args,
  )?;
  assert_eq!(out, Value::Bool(true));

  let args = [Value::Object(p)];
  let out = rt.vm.call_without_host(
    &mut scope,
    Value::Object(is_prototype_of),
    Value::Object(o),
    &args,
  )?;
  assert_eq!(out, Value::Bool(false));

  // propertyIsEnumerable (own vs inherited)
  define_enumerable_data_property(&mut scope, p, "z", Value::Number(9.0))?;
  define_enumerable_data_property(&mut scope, o, "x", Value::Number(1.0))?;
  let y_key = PropertyKey::from_string(scope.alloc_string("y")?);
  scope.define_property(
    o,
    y_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::Number(2.0),
        writable: true,
      },
    },
  )?;

  let args = [Value::String(scope.alloc_string("x")?)];
  let out = rt.vm.call_without_host(
    &mut scope,
    Value::Object(property_is_enumerable),
    Value::Object(o),
    &args,
  )?;
  assert_eq!(out, Value::Bool(true));

  let args = [Value::String(scope.alloc_string("y")?)];
  let out = rt.vm.call_without_host(
    &mut scope,
    Value::Object(property_is_enumerable),
    Value::Object(o),
    &args,
  )?;
  assert_eq!(out, Value::Bool(false));

  let args = [Value::String(scope.alloc_string("z")?)];
  let out = rt.vm.call_without_host(
    &mut scope,
    Value::Object(property_is_enumerable),
    Value::Object(o),
    &args,
  )?;
  assert_eq!(out, Value::Bool(false));

  // toLocaleString delegates to `toString`.
  let to_string_id = rt.vm.register_native_call(return_ok_native)?;
  let name = scope.alloc_string("")?;
  let to_string_func = scope.alloc_native_function(to_string_id, None, name, 0)?;
  let to_string_key = PropertyKey::from_string(scope.alloc_string("toString")?);
  scope.define_property(
    o,
    to_string_key,
    PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::Object(to_string_func),
        writable: true,
      },
    },
  )?;

  let out = rt.vm.call_without_host(
    &mut scope,
    Value::Object(to_locale_string),
    Value::Object(o),
    &[],
  )?;
  let Value::String(out_s) = out else {
    panic!("expected toLocaleString() result to be a string, got {out:?}");
  };
  assert_eq!(scope.heap().get_string(out_s)?.to_utf8_lossy(), "ok");

  // valueOf returns the object itself, and boxes primitives.
  let out = rt
    .vm
    .call_without_host(&mut scope, Value::Object(value_of), Value::Object(o), &[])?;
  assert_eq!(out, Value::Object(o));

  let out = rt
    .vm
    .call_without_host(&mut scope, Value::Object(value_of), Value::Number(1.0), &[])?;
  let Value::Object(number_obj) = out else {
    panic!("expected boxed object from valueOf on primitive");
  };
  assert_eq!(scope.heap().object_prototype(number_obj)?, Some(number_proto));

  // Sanity: `toLocaleString` is installed on Object.prototype (not on Object).
  assert!(get_own_data_property(&mut scope, object, "toLocaleString")?.is_none());

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
fn object_prototype_annex_b_getter_setter_helpers_work() -> Result<(), VmError> {
  let mut rt = TestRealm::new()?;
  let object_proto = rt.realm.intrinsics().object_prototype();
  let type_error_proto = rt.realm.intrinsics().type_error_prototype();

  let getter_call_id = rt.vm.register_native_call(return_two_native)?;
  let setter_call_id = rt.vm.register_native_call(return_two_native)?;

  let mut scope = rt.heap.scope();

  let define_getter = get_own_data_property(&mut scope, object_proto, "__defineGetter__")?
    .expect("Object.prototype.__defineGetter__ exists");
  let Value::Object(define_getter) = define_getter else {
    panic!("Object.prototype.__defineGetter__ should be a function object");
  };
  let define_setter = get_own_data_property(&mut scope, object_proto, "__defineSetter__")?
    .expect("Object.prototype.__defineSetter__ exists");
  let Value::Object(define_setter) = define_setter else {
    panic!("Object.prototype.__defineSetter__ should be a function object");
  };
  let lookup_getter = get_own_data_property(&mut scope, object_proto, "__lookupGetter__")?
    .expect("Object.prototype.__lookupGetter__ exists");
  let Value::Object(lookup_getter) = lookup_getter else {
    panic!("Object.prototype.__lookupGetter__ should be a function object");
  };
  let lookup_setter = get_own_data_property(&mut scope, object_proto, "__lookupSetter__")?
    .expect("Object.prototype.__lookupSetter__ exists");
  let Value::Object(lookup_setter) = lookup_setter else {
    panic!("Object.prototype.__lookupSetter__ should be a function object");
  };

  let getter_name = scope.alloc_string("")?;
  let getter = scope.alloc_native_function(getter_call_id, None, getter_name, 0)?;
  scope.push_root(Value::Object(getter))?;
  let setter_name = scope.alloc_string("")?;
  let setter = scope.alloc_native_function(setter_call_id, None, setter_name, 1)?;
  scope.push_root(Value::Object(setter))?;

  let x_s = scope.alloc_string("x")?;
  scope.push_root(Value::String(x_s))?;
  let y_s = scope.alloc_string("y")?;
  scope.push_root(Value::String(y_s))?;
  let z_s = scope.alloc_string("z")?;
  scope.push_root(Value::String(z_s))?;

  let o = scope.alloc_object()?;
  scope.push_root(Value::Object(o))?;

  // Define setter first, then getter: should preserve the setter.
  let args = [Value::String(x_s), Value::Object(setter)];
  let out = rt.vm.call_without_host(
    &mut scope,
    Value::Object(define_setter),
    Value::Object(o),
    &args,
  )?;
  assert_eq!(out, Value::Undefined);

  let args = [Value::String(x_s), Value::Object(getter)];
  let out = rt.vm.call_without_host(
    &mut scope,
    Value::Object(define_getter),
    Value::Object(o),
    &args,
  )?;
  assert_eq!(out, Value::Undefined);

  let x_key = PropertyKey::from_string(x_s);
  let desc = scope
    .heap()
    .object_get_own_property(o, &x_key)?
    .expect("x should be defined");
  assert!(desc.enumerable);
  assert!(desc.configurable);
  let PropertyKind::Accessor { get, set } = desc.kind else {
    panic!("expected accessor descriptor");
  };
  assert_eq!(get, Value::Object(getter));
  assert_eq!(set, Value::Object(setter));

  // Lookup on own property.
  let args = [Value::String(x_s)];
  let out = rt.vm.call_without_host(
    &mut scope,
    Value::Object(lookup_getter),
    Value::Object(o),
    &args,
  )?;
  assert_eq!(out, Value::Object(getter));
  let out = rt.vm.call_without_host(
    &mut scope,
    Value::Object(lookup_setter),
    Value::Object(o),
    &args,
  )?;
  assert_eq!(out, Value::Object(setter));

  // Lookup through prototype chain.
  let child = scope.alloc_object()?;
  scope.push_root(Value::Object(child))?;
  scope.heap_mut().object_set_prototype(child, Some(o))?;
  let out = rt.vm.call_without_host(
    &mut scope,
    Value::Object(lookup_getter),
    Value::Object(child),
    &args,
  )?;
  assert_eq!(out, Value::Object(getter));
  let out = rt.vm.call_without_host(
    &mut scope,
    Value::Object(lookup_setter),
    Value::Object(child),
    &args,
  )?;
  assert_eq!(out, Value::Object(setter));

  // Lookups return undefined for data properties.
  define_enumerable_data_property(&mut scope, o, "y", Value::Number(1.0))?;
  let args = [Value::String(y_s)];
  let out = rt.vm.call_without_host(
    &mut scope,
    Value::Object(lookup_getter),
    Value::Object(o),
    &args,
  )?;
  assert_eq!(out, Value::Undefined);
  let out = rt.vm.call_without_host(
    &mut scope,
    Value::Object(lookup_setter),
    Value::Object(o),
    &args,
  )?;
  assert_eq!(out, Value::Undefined);

  // __defineGetter__ requires a callable function.
  let args = [Value::String(z_s), Value::Number(1.0)];
  let err = rt.vm.call_without_host(
    &mut scope,
    Value::Object(define_getter),
    Value::Object(o),
    &args,
  );
  let thrown = match err {
    Ok(v) => panic!("expected __defineGetter__ to throw, got {v:?}"),
    Err(VmError::Throw(v) | VmError::ThrowWithStack { value: v, .. }) => v,
    Err(e) => return Err(e),
  };
  let Value::Object(err_obj) = thrown else {
    panic!("expected thrown value to be an object, got {thrown:?}");
  };
  assert_eq!(scope.heap().object_prototype(err_obj)?, Some(type_error_proto));

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

#[test]
fn object_is_uses_same_value_semantics() -> Result<(), VmError> {
  let mut rt = TestRealm::new()?;
  let object = rt.realm.intrinsics().object_constructor();

  let mut scope = rt.heap.scope();

  let object_is = get_own_data_property(&mut scope, object, "is")?.expect("Object.is exists");
  let Value::Object(object_is) = object_is else {
    panic!("Object.is should be a function object");
  };

  // NaN is SameValue with NaN.
  let args = [Value::Number(f64::NAN), Value::Number(f64::NAN)];
  let out =
    rt.vm
      .call_without_host(&mut scope, Value::Object(object_is), Value::Object(object), &args)?;
  assert_eq!(out, Value::Bool(true));

  // +0 and -0 are distinct under SameValue.
  let args = [Value::Number(0.0), Value::Number(-0.0)];
  let out =
    rt.vm
      .call_without_host(&mut scope, Value::Object(object_is), Value::Object(object), &args)?;
  assert_eq!(out, Value::Bool(false));

  // Objects compare by identity.
  let o1 = scope.alloc_object()?;
  let o2 = scope.alloc_object()?;
  let args = [Value::Object(o1), Value::Object(o2)];
  let out =
    rt.vm
      .call_without_host(&mut scope, Value::Object(object_is), Value::Object(object), &args)?;
  assert_eq!(out, Value::Bool(false));

  let args = [Value::Object(o1), Value::Object(o1)];
  let out =
    rt.vm
      .call_without_host(&mut scope, Value::Object(object_is), Value::Object(object), &args)?;
  assert_eq!(out, Value::Bool(true));

  Ok(())
}

#[test]
fn object_has_own_reports_own_properties() -> Result<(), VmError> {
  let mut rt = TestRealm::new()?;
  let object = rt.realm.intrinsics().object_constructor();
  let type_error_proto = rt.realm.intrinsics().type_error_prototype();

  let mut scope = rt.heap.scope();

  let has_own = get_own_data_property(&mut scope, object, "hasOwn")?.expect("Object.hasOwn exists");
  let Value::Object(has_own) = has_own else {
    panic!("Object.hasOwn should be a function object");
  };

  let p = scope.alloc_object()?;
  scope.push_root(Value::Object(p))?;
  define_enumerable_data_property(&mut scope, p, "y", Value::Number(2.0))?;

  let o = scope.alloc_object()?;
  scope.push_root(Value::Object(o))?;
  scope.heap_mut().object_set_prototype(o, Some(p))?;
  define_enumerable_data_property(&mut scope, o, "x", Value::Number(1.0))?;

  let args = [Value::Object(o), Value::String(scope.alloc_string("x")?)];
  let out = rt
    .vm
    .call_without_host(&mut scope, Value::Object(has_own), Value::Object(object), &args)?;
  assert_eq!(out, Value::Bool(true));

  let args = [Value::Object(o), Value::String(scope.alloc_string("y")?)];
  let out = rt
    .vm
    .call_without_host(&mut scope, Value::Object(has_own), Value::Object(object), &args)?;
  assert_eq!(out, Value::Bool(false));

  // `ToObject(null)` throws.
  let args = [Value::Null, Value::String(scope.alloc_string("x")?)];
  let err =
    rt.vm
      .call_without_host(&mut scope, Value::Object(has_own), Value::Object(object), &args);
  let thrown = match err {
    Ok(v) => panic!("expected Object.hasOwn to throw, got {v:?}"),
    Err(VmError::Throw(v) | VmError::ThrowWithStack { value: v, .. }) => v,
    Err(e) => return Err(e),
  };
  let Value::Object(err_obj) = thrown else {
    panic!("expected thrown value to be an object, got {thrown:?}");
  };
  assert_eq!(scope.heap().object_prototype(err_obj)?, Some(type_error_proto));

  Ok(())
}
