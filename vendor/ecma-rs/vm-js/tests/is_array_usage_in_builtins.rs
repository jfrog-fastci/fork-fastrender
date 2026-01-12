use vm_js::{
  GcObject, Heap, HeapLimits, PropertyDescriptor, PropertyKey, PropertyKind, Realm, Scope, Value, Vm, VmError,
  VmHost, VmHostHooks, VmOptions,
};

struct TestRt {
  vm: Vm,
  heap: Heap,
  realm: Realm,
}

impl TestRt {
  fn new(limits: HeapLimits) -> Result<Self, VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(limits);
    let realm = Realm::new(&mut vm, &mut heap)?;
    Ok(Self { vm, heap, realm })
  }
}

impl Drop for TestRt {
  fn drop(&mut self) {
    self.realm.teardown(&mut self.heap);
  }
}

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

fn get_data_property(
  scope: &mut Scope<'_>,
  obj: GcObject,
  name: &str,
) -> Result<Option<Value>, VmError> {
  let key = PropertyKey::from_string(scope.alloc_string(name)?);
  let Some(desc) = scope.heap().get_property(obj, &key)? else {
    return Ok(None);
  };
  match desc.kind {
    PropertyKind::Data { value, .. } => Ok(Some(value)),
    PropertyKind::Accessor { .. } => Err(VmError::PropertyNotData),
  }
}

fn proxy_get_trap_concat_spreadable(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let [target, prop, receiver] = args else {
    return Err(VmError::Unimplemented(
      "Proxy `get` trap expected (target, property, receiver)",
    ));
  };
  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;

  if let Value::Symbol(sym) = *prop {
    if sym == intr.well_known_symbols().is_concat_spreadable {
      return Ok(Value::Bool(true));
    }
  }

  let Value::Object(target_obj) = *target else {
    return Err(VmError::TypeError("Proxy `get` trap target is not an object"));
  };
  let key = match *prop {
    Value::String(s) => PropertyKey::from_string(s),
    Value::Symbol(sym) => PropertyKey::from_symbol(sym),
    _ => return Err(VmError::TypeError("Proxy `get` trap property is not a string/symbol")),
  };

  scope.ordinary_get_with_host_and_hooks(vm, host, hooks, target_obj, key, *receiver)
}

fn proxy_has_trap_always_true(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Ok(Value::Bool(true))
}

#[test]
fn array_concat_spreads_proxy_to_array() -> Result<(), VmError> {
  let mut rt = TestRt::new(HeapLimits::new(1024 * 1024, 1024 * 1024))?;
  let intr = *rt.realm.intrinsics();

  let mut scope = rt.heap.scope();

  let array_ctor = Value::Object(intr.array_constructor());

  // [].
  let empty = rt
    .vm
    .construct_without_host(&mut scope, array_ctor, &[], array_ctor)?;
  let Value::Object(empty_obj) = empty else {
    return Err(VmError::Unimplemented("Array constructor did not return object"));
  };

  // Target: [1, 2]
  let target = rt.vm.construct_without_host(
    &mut scope,
    array_ctor,
    &[Value::Number(1.0), Value::Number(2.0)],
    array_ctor,
  )?;
  let Value::Object(target_obj) = target else {
    return Err(VmError::Unimplemented("Array constructor did not return object"));
  };

  // Handler: {}
  let handler = scope.alloc_object()?;
  scope
    .heap_mut()
    .object_set_prototype(handler, Some(intr.object_prototype()))?;

  // Proxy -> target array
  let proxy = scope.alloc_proxy(Some(target_obj), Some(handler))?;

  // [].concat(proxy)
  let concat = get_data_property(&mut scope, empty_obj, "concat")?.unwrap();
  let out = rt
    .vm
    .call_without_host(&mut scope, concat, empty, &[Value::Object(proxy)])?;
  let Value::Object(out_obj) = out else {
    return Err(VmError::Unimplemented("Array.prototype.concat did not return object"));
  };

  assert_eq!(
    get_data_property(&mut scope, out_obj, "length")?,
    Some(Value::Number(2.0))
  );
  assert_eq!(
    get_data_property(&mut scope, out_obj, "0")?,
    Some(Value::Number(1.0))
  );
  assert_eq!(
    get_data_property(&mut scope, out_obj, "1")?,
    Some(Value::Number(2.0))
  );
  Ok(())
}

#[test]
fn array_concat_respects_proxy_get_trap_for_is_concat_spreadable() -> Result<(), VmError> {
  let mut rt = TestRt::new(HeapLimits::new(1024 * 1024, 1024 * 1024))?;
  let intr = *rt.realm.intrinsics();

  let mut scope = rt.heap.scope();

  let array_ctor = Value::Object(intr.array_constructor());

  // [].
  let empty = rt
    .vm
    .construct_without_host(&mut scope, array_ctor, &[], array_ctor)?;
  let Value::Object(empty_obj) = empty else {
    return Err(VmError::Unimplemented("Array constructor did not return object"));
  };

  // Target: { length: 1, 0: "x" }
  let target = scope.alloc_object()?;
  scope
    .heap_mut()
    .object_set_prototype(target, Some(intr.object_prototype()))?;

  let length_key_s = scope.alloc_string("length")?;
  scope.push_root(Value::String(length_key_s))?;
  let length_key = PropertyKey::from_string(length_key_s);
  scope.define_property(target, length_key, data_desc(Value::Number(1.0)))?;

  let x = scope.alloc_string("x")?;
  scope.push_root(Value::String(x))?;
  let idx_s = scope.alloc_string("0")?;
  scope.push_root(Value::String(idx_s))?;
  let idx_key = PropertyKey::from_string(idx_s);
  scope.define_property(target, idx_key, data_desc(Value::String(x)))?;

  // Handler with `get` trap that makes the proxy `IsConcatSpreadable` by returning `true` for
  // `Symbol.isConcatSpreadable`.
  let handler = scope.alloc_object()?;
  scope
    .heap_mut()
    .object_set_prototype(handler, Some(intr.object_prototype()))?;

  let get_call_id = rt.vm.register_native_call(proxy_get_trap_concat_spreadable)?;
  let get_s = scope.alloc_string("get")?;
  scope.push_root(Value::String(get_s))?;
  let get_fn = scope.alloc_native_function(get_call_id, None, get_s, 3)?;
  scope.push_root(Value::Object(get_fn))?;
  scope.define_property(
    handler,
    PropertyKey::from_string(get_s),
    data_desc(Value::Object(get_fn)),
  )?;

  // Proxy -> target object
  let proxy = scope.alloc_proxy(Some(target), Some(handler))?;

  // [].concat(proxy) should spread, producing ["x"].
  let concat = get_data_property(&mut scope, empty_obj, "concat")?.unwrap();
  let out = rt
    .vm
    .call_without_host(&mut scope, concat, empty, &[Value::Object(proxy)])?;
  let Value::Object(out_obj) = out else {
    return Err(VmError::Unimplemented("Array.prototype.concat did not return object"));
  };

  assert_eq!(
    get_data_property(&mut scope, out_obj, "length")?,
    Some(Value::Number(1.0))
  );
  let Value::String(s) = get_data_property(&mut scope, out_obj, "0")?.unwrap() else {
    return Err(VmError::Unimplemented("concat element was not a string"));
  };
  assert_eq!(scope.heap().get_string(s)?.to_utf8_lossy(), "x");
  Ok(())
}

#[test]
fn array_concat_respects_proxy_has_trap_for_holes() -> Result<(), VmError> {
  let mut rt = TestRt::new(HeapLimits::new(1024 * 1024, 1024 * 1024))?;
  let intr = *rt.realm.intrinsics();

  let mut scope = rt.heap.scope();

  let array_ctor = Value::Object(intr.array_constructor());

  // [].
  let empty = rt
    .vm
    .construct_without_host(&mut scope, array_ctor, &[], array_ctor)?;
  let Value::Object(empty_obj) = empty else {
    return Err(VmError::Unimplemented("Array constructor did not return object"));
  };

  // Target: new Array(1) => length 1 with a hole at index 0.
  let target = rt.vm.construct_without_host(
    &mut scope,
    array_ctor,
    &[Value::Number(1.0)],
    array_ctor,
  )?;
  let Value::Object(target_obj) = target else {
    return Err(VmError::Unimplemented("Array constructor did not return object"));
  };

  // Handler with `has` trap that claims every property exists.
  let handler = scope.alloc_object()?;
  scope
    .heap_mut()
    .object_set_prototype(handler, Some(intr.object_prototype()))?;

  let has_call_id = rt.vm.register_native_call(proxy_has_trap_always_true)?;
  let has_s = scope.alloc_string("has")?;
  scope.push_root(Value::String(has_s))?;
  let has_fn = scope.alloc_native_function(has_call_id, None, has_s, 2)?;
  scope.push_root(Value::Object(has_fn))?;
  scope.define_property(
    handler,
    PropertyKey::from_string(has_s),
    data_desc(Value::Object(has_fn)),
  )?;

  let proxy = scope.alloc_proxy(Some(target_obj), Some(handler))?;

  // Without the `has` trap, concat would preserve the hole. With the trap, the element is treated
  // as present and copied as `undefined`.
  let concat = get_data_property(&mut scope, empty_obj, "concat")?.unwrap();
  let out = rt
    .vm
    .call_without_host(&mut scope, concat, empty, &[Value::Object(proxy)])?;
  let Value::Object(out_obj) = out else {
    return Err(VmError::Unimplemented("Array.prototype.concat did not return object"));
  };

  assert_eq!(
    get_data_property(&mut scope, out_obj, "length")?,
    Some(Value::Number(1.0))
  );
  assert_eq!(
    get_data_property(&mut scope, out_obj, "0")?,
    Some(Value::Undefined)
  );
  Ok(())
}

#[test]
fn array_concat_spreads_symbol_is_concat_spreadable_object() -> Result<(), VmError> {
  let mut rt = TestRt::new(HeapLimits::new(1024 * 1024, 1024 * 1024))?;
  let intr = *rt.realm.intrinsics();

  let mut scope = rt.heap.scope();

  let array_ctor = Value::Object(intr.array_constructor());

  // [].
  let empty = rt
    .vm
    .construct_without_host(&mut scope, array_ctor, &[], array_ctor)?;
  let Value::Object(empty_obj) = empty else {
    return Err(VmError::Unimplemented("Array constructor did not return object"));
  };

  // { [Symbol.isConcatSpreadable]: true, length: 1, 0: "x" }
  let spreadable_obj = scope.alloc_object()?;
  scope
    .heap_mut()
    .object_set_prototype(spreadable_obj, Some(intr.object_prototype()))?;

  let sym = intr.well_known_symbols().is_concat_spreadable;
  scope.define_property(
    spreadable_obj,
    PropertyKey::from_symbol(sym),
    data_desc(Value::Bool(true)),
  )?;

  let length_key = PropertyKey::from_string(scope.alloc_string("length")?);
  scope.define_property(spreadable_obj, length_key, data_desc(Value::Number(1.0)))?;

  // Root key/value strings across allocation so GC can't collect them before definition.
  let x = scope.alloc_string("x")?;
  scope.push_root(Value::String(x))?;
  let idx_s = scope.alloc_string("0")?;
  scope.push_root(Value::String(idx_s))?;
  let idx_key = PropertyKey::from_string(idx_s);
  scope.define_property(spreadable_obj, idx_key, data_desc(Value::String(x)))?;

  // [].concat(spreadable_obj)
  let concat = get_data_property(&mut scope, empty_obj, "concat")?.unwrap();
  let out = rt.vm.call_without_host(
    &mut scope,
    concat,
    empty,
    &[Value::Object(spreadable_obj)],
  )?;
  let Value::Object(out_obj) = out else {
    return Err(VmError::Unimplemented("Array.prototype.concat did not return object"));
  };

  assert_eq!(
    get_data_property(&mut scope, out_obj, "length")?,
    Some(Value::Number(1.0))
  );
  let Value::String(s) = get_data_property(&mut scope, out_obj, "0")?.unwrap() else {
    return Err(VmError::Unimplemented("concat element was not a string"));
  };
  assert_eq!(scope.heap().get_string(s)?.to_utf8_lossy(), "x");
  Ok(())
}

#[test]
fn array_concat_throws_on_revoked_proxy() -> Result<(), VmError> {
  let mut rt = TestRt::new(HeapLimits::new(1024 * 1024, 1024 * 1024))?;
  let intr = *rt.realm.intrinsics();

  let mut scope = rt.heap.scope();

  let array_ctor = Value::Object(intr.array_constructor());

  // [].
  let empty = rt
    .vm
    .construct_without_host(&mut scope, array_ctor, &[], array_ctor)?;
  let Value::Object(empty_obj) = empty else {
    return Err(VmError::Unimplemented("Array constructor did not return object"));
  };

  // Target: [1]
  let target = rt
    .vm
    .construct_without_host(&mut scope, array_ctor, &[Value::Number(1.0)], array_ctor)?;
  let Value::Object(target_obj) = target else {
    return Err(VmError::Unimplemented("Array constructor did not return object"));
  };

  // Handler: {}
  let handler = scope.alloc_object()?;
  scope
    .heap_mut()
    .object_set_prototype(handler, Some(intr.object_prototype()))?;

  let proxy = scope.alloc_proxy(Some(target_obj), Some(handler))?;
  scope.heap_mut().proxy_revoke(proxy)?;

  let concat = get_data_property(&mut scope, empty_obj, "concat")?.unwrap();
  let err = rt
    .vm
    .call_without_host(&mut scope, concat, empty, &[Value::Object(proxy)])
    .unwrap_err();
  assert!(
    matches!(err, VmError::Throw(_) | VmError::ThrowWithStack { .. }),
    "expected a thrown TypeError, got {err:?}"
  );
  Ok(())
}

#[test]
fn array_is_array_returns_true_for_proxy_to_array() -> Result<(), VmError> {
  let mut rt = TestRt::new(HeapLimits::new(1024 * 1024, 1024 * 1024))?;
  let intr = *rt.realm.intrinsics();

  let mut scope = rt.heap.scope();

  let array_ctor = Value::Object(intr.array_constructor());
  let Value::Object(array_ctor_obj) = array_ctor else {
    return Err(VmError::InvariantViolation("Array constructor is not an object"));
  };

  let target = rt.vm.construct_without_host(
    &mut scope,
    array_ctor,
    &[Value::Number(1.0), Value::Number(2.0)],
    array_ctor,
  )?;
  let Value::Object(target_obj) = target else {
    return Err(VmError::Unimplemented("Array constructor did not return object"));
  };

  let handler = scope.alloc_object()?;
  scope
    .heap_mut()
    .object_set_prototype(handler, Some(intr.object_prototype()))?;
  let proxy = scope.alloc_proxy(Some(target_obj), Some(handler))?;

  let is_array = get_data_property(&mut scope, array_ctor_obj, "isArray")?.unwrap();
  let result = rt.vm.call_without_host(
    &mut scope,
    is_array,
    Value::Object(array_ctor_obj),
    &[Value::Object(proxy)],
  )?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn array_is_array_throws_on_revoked_proxy() -> Result<(), VmError> {
  let mut rt = TestRt::new(HeapLimits::new(1024 * 1024, 1024 * 1024))?;
  let intr = *rt.realm.intrinsics();

  let mut scope = rt.heap.scope();

  let array_ctor = Value::Object(intr.array_constructor());
  let Value::Object(array_ctor_obj) = array_ctor else {
    return Err(VmError::InvariantViolation("Array constructor is not an object"));
  };

  let target = rt
    .vm
    .construct_without_host(&mut scope, array_ctor, &[Value::Number(1.0)], array_ctor)?;
  let Value::Object(target_obj) = target else {
    return Err(VmError::Unimplemented("Array constructor did not return object"));
  };

  let handler = scope.alloc_object()?;
  scope
    .heap_mut()
    .object_set_prototype(handler, Some(intr.object_prototype()))?;

  let proxy = scope.alloc_proxy(Some(target_obj), Some(handler))?;
  scope.heap_mut().proxy_revoke(proxy)?;

  let is_array = get_data_property(&mut scope, array_ctor_obj, "isArray")?.unwrap();
  let err = rt
    .vm
    .call_without_host(
      &mut scope,
      is_array,
      Value::Object(array_ctor_obj),
      &[Value::Object(proxy)],
    )
    .unwrap_err();

  let thrown = match err {
    VmError::Throw(v) => v,
    VmError::ThrowWithStack { value, .. } => value,
    other => panic!("expected throw, got {other:?}"),
  };
  scope.push_root(thrown)?;
  let Value::Object(err_obj) = thrown else {
    panic!("expected thrown object, got {thrown:?}");
  };
  scope.push_root(Value::Object(err_obj))?;

  let Value::String(name) = get_data_property(&mut scope, err_obj, "name")?.unwrap() else {
    return Err(VmError::Unimplemented("TypeError.name was not a string"));
  };
  assert_eq!(scope.heap().get_string(name)?.to_utf8_lossy(), "TypeError");
  Ok(())
}
