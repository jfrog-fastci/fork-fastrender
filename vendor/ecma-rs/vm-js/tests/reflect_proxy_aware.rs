use vm_js::{
  GcObject, Heap, HeapLimits, JsRuntime, PropertyDescriptor, PropertyKey, PropertyKind, Scope, Value, Vm, VmError,
  VmHost, VmHostHooks, VmOptions,
};

fn global_var_desc(value: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: true,
    configurable: true,
    kind: PropertyKind::Data {
      value,
      writable: true,
    },
  }
}

fn define_global(
  scope: &mut Scope<'_>,
  global: GcObject,
  name: &str,
  value: Value,
) -> Result<(), VmError> {
  // Root inputs so `alloc_string` and `define_property` can allocate freely.
  scope.push_roots(&[Value::Object(global), value])?;

  let key_s = scope.alloc_string(name)?;
  scope.push_root(Value::String(key_s))?;
  let key = PropertyKey::from_string(key_s);
  scope.define_property(global, key, global_var_desc(value))
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

fn expect_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string value, got {value:?}");
  };
  rt.heap()
    .get_string(s)
    .expect("string handle should be valid")
    .to_utf8_lossy()
}

fn proxy_get_trap_return_receiver(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let [_, _, receiver] = args else {
    return Err(VmError::Unimplemented(
      "Proxy get trap expected (target, property, receiver)",
    ));
  };
  Ok(*receiver)
}

fn proxy_set_trap_return_false(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let [_target, _prop, _value, _receiver] = args else {
    return Err(VmError::Unimplemented(
      "Proxy set trap expected (target, property, value, receiver)",
    ));
  };
  Ok(Value::Bool(false))
}

fn proxy_has_trap_always_true(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let [_target, _prop] = args else {
    return Err(VmError::Unimplemented(
      "Proxy has trap expected (target, property)",
    ));
  };
  Ok(Value::Bool(true))
}

fn proxy_delete_trap_return_false(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let [_target, _prop] = args else {
    return Err(VmError::Unimplemented(
      "Proxy deleteProperty trap expected (target, property)",
    ));
  };
  Ok(Value::Bool(false))
}

fn proxy_own_keys_trap_return_a_b(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let [_target] = args else {
    return Err(VmError::Unimplemented("Proxy ownKeys trap expected (target)"));
  };
  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;

  // Return an array-like object: ["a", "b"].
  let arr = scope.alloc_array(0)?;
  scope.push_root(Value::Object(arr))?;
  scope
    .heap_mut()
    .object_set_prototype(arr, Some(intr.array_prototype()))?;

  let a = scope.alloc_string("a")?;
  scope.push_root(Value::String(a))?;
  let b = scope.alloc_string("b")?;
  scope.push_root(Value::String(b))?;

  let idx0_s = scope.alloc_string("0")?;
  scope.push_root(Value::String(idx0_s))?;
  scope.create_data_property_or_throw(arr, PropertyKey::from_string(idx0_s), Value::String(a))?;

  let idx1_s = scope.alloc_string("1")?;
  scope.push_root(Value::String(idx1_s))?;
  scope.create_data_property_or_throw(arr, PropertyKey::from_string(idx1_s), Value::String(b))?;

  Ok(Value::Object(arr))
}

fn proxy_get_prototype_of_trap_return_null(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let [_target] = args else {
    return Err(VmError::Unimplemented(
      "Proxy getPrototypeOf trap expected (target)",
    ));
  };
  Ok(Value::Null)
}

fn proxy_set_prototype_of_trap_return_false(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let [_target, _proto] = args else {
    return Err(VmError::Unimplemented(
      "Proxy setPrototypeOf trap expected (target, proto)",
    ));
  };
  Ok(Value::Bool(false))
}

fn proxy_define_property_trap_return_false(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let [_target, _prop, _desc] = args else {
    return Err(VmError::Unimplemented(
      "Proxy defineProperty trap expected (target, property, descriptor)",
    ));
  };
  Ok(Value::Bool(false))
}

#[test]
fn reflect_methods_use_proxy_internal_method_dispatch() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let global = rt.realm().global_object();

  {
    let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();

    let get_id = vm.register_native_call(proxy_get_trap_return_receiver)?;
    let set_id = vm.register_native_call(proxy_set_trap_return_false)?;
    let has_id = vm.register_native_call(proxy_has_trap_always_true)?;
    let delete_id = vm.register_native_call(proxy_delete_trap_return_false)?;
    let own_keys_id = vm.register_native_call(proxy_own_keys_trap_return_a_b)?;
    let get_proto_id = vm.register_native_call(proxy_get_prototype_of_trap_return_null)?;
    let set_proto_id = vm.register_native_call(proxy_set_prototype_of_trap_return_false)?;
    let define_prop_id = vm.register_native_call(proxy_define_property_trap_return_false)?;

    let mut scope = heap.scope();

    let target = scope.alloc_object()?;
    scope.push_root(Value::Object(target))?;
    let handler = scope.alloc_object()?;
    scope.push_root(Value::Object(handler))?;

    // Install traps on the handler.
    let get_s = scope.alloc_string("get")?;
    scope.push_root(Value::String(get_s))?;
    let get_fn = scope.alloc_native_function(get_id, None, get_s, 3)?;
    scope.push_root(Value::Object(get_fn))?;
    scope.define_property(
      handler,
      PropertyKey::from_string(get_s),
      data_desc(Value::Object(get_fn)),
    )?;

    let set_s = scope.alloc_string("set")?;
    scope.push_root(Value::String(set_s))?;
    let set_fn = scope.alloc_native_function(set_id, None, set_s, 4)?;
    scope.push_root(Value::Object(set_fn))?;
    scope.define_property(
      handler,
      PropertyKey::from_string(set_s),
      data_desc(Value::Object(set_fn)),
    )?;

    let has_s = scope.alloc_string("has")?;
    scope.push_root(Value::String(has_s))?;
    let has_fn = scope.alloc_native_function(has_id, None, has_s, 2)?;
    scope.push_root(Value::Object(has_fn))?;
    scope.define_property(
      handler,
      PropertyKey::from_string(has_s),
      data_desc(Value::Object(has_fn)),
    )?;

    let delete_s = scope.alloc_string("deleteProperty")?;
    scope.push_root(Value::String(delete_s))?;
    let delete_fn = scope.alloc_native_function(delete_id, None, delete_s, 2)?;
    scope.push_root(Value::Object(delete_fn))?;
    scope.define_property(
      handler,
      PropertyKey::from_string(delete_s),
      data_desc(Value::Object(delete_fn)),
    )?;

    let own_keys_s = scope.alloc_string("ownKeys")?;
    scope.push_root(Value::String(own_keys_s))?;
    let own_keys_fn = scope.alloc_native_function(own_keys_id, None, own_keys_s, 1)?;
    scope.push_root(Value::Object(own_keys_fn))?;
    scope.define_property(
      handler,
      PropertyKey::from_string(own_keys_s),
      data_desc(Value::Object(own_keys_fn)),
    )?;

    let get_proto_s = scope.alloc_string("getPrototypeOf")?;
    scope.push_root(Value::String(get_proto_s))?;
    let get_proto_fn = scope.alloc_native_function(get_proto_id, None, get_proto_s, 1)?;
    scope.push_root(Value::Object(get_proto_fn))?;
    scope.define_property(
      handler,
      PropertyKey::from_string(get_proto_s),
      data_desc(Value::Object(get_proto_fn)),
    )?;

    let set_proto_s = scope.alloc_string("setPrototypeOf")?;
    scope.push_root(Value::String(set_proto_s))?;
    let set_proto_fn = scope.alloc_native_function(set_proto_id, None, set_proto_s, 2)?;
    scope.push_root(Value::Object(set_proto_fn))?;
    scope.define_property(
      handler,
      PropertyKey::from_string(set_proto_s),
      data_desc(Value::Object(set_proto_fn)),
    )?;

    let define_prop_s = scope.alloc_string("defineProperty")?;
    scope.push_root(Value::String(define_prop_s))?;
    let define_prop_fn = scope.alloc_native_function(define_prop_id, None, define_prop_s, 3)?;
    scope.push_root(Value::Object(define_prop_fn))?;
    scope.define_property(
      handler,
      PropertyKey::from_string(define_prop_s),
      data_desc(Value::Object(define_prop_fn)),
    )?;

    let proxy = scope.alloc_proxy(Some(target), Some(handler))?;
    define_global(&mut scope, global, "proxy", Value::Object(proxy))?;

    let receiver = scope.alloc_object()?;
    define_global(&mut scope, global, "receiver", Value::Object(receiver))?;

    let revoked_handler = scope.alloc_object()?;
    scope.push_root(Value::Object(revoked_handler))?;
    let revoked_proxy = scope.alloc_proxy(Some(target), Some(revoked_handler))?;
    scope.revoke_proxy(revoked_proxy)?;
    define_global(
      &mut scope,
      global,
      "revokedProxy",
      Value::Object(revoked_proxy),
    )?;
  }

  // Reflect.get hits Proxy `get` trap and respects receiver.
  assert_eq!(
    rt.exec_script("Reflect.get(proxy, 'x', receiver) === receiver")?,
    Value::Bool(true)
  );

  // Reflect.set hits Proxy `set` trap and returns boolean.
  assert_eq!(
    rt.exec_script("Reflect.set(proxy, 'x', 1, receiver)")?,
    Value::Bool(false)
  );

  // Reflect.has hits Proxy `has` trap.
  assert_eq!(rt.exec_script("Reflect.has(proxy, 'x')")?, Value::Bool(true));

  // Reflect.deleteProperty hits Proxy `deleteProperty` trap.
  assert_eq!(
    rt.exec_script("Reflect.deleteProperty(proxy, 'x')")?,
    Value::Bool(false)
  );

  // Reflect.ownKeys hits Proxy `ownKeys` trap.
  let v = rt.exec_script("Reflect.ownKeys(proxy).join(',')")?;
  assert_eq!(expect_string(&rt, v), "a,b");

  // Reflect.getPrototypeOf hits Proxy `getPrototypeOf` trap.
  assert_eq!(
    rt.exec_script("Reflect.getPrototypeOf(proxy) === null")?,
    Value::Bool(true)
  );

  // Reflect.setPrototypeOf hits Proxy `setPrototypeOf` trap and returns `false` on rejection.
  assert_eq!(
    rt.exec_script("Reflect.setPrototypeOf(proxy, null)")?,
    Value::Bool(false)
  );

  // Reflect.defineProperty hits Proxy `defineProperty` trap and returns boolean.
  assert_eq!(
    rt.exec_script("Reflect.defineProperty(proxy, 'x', { value: 1 })")?,
    Value::Bool(false)
  );

  // Revoked Proxy throws for relevant operations.
  for expr in [
    "Reflect.get(revokedProxy, 'x')",
    "Reflect.set(revokedProxy, 'x', 1)",
    "Reflect.has(revokedProxy, 'x')",
    "Reflect.deleteProperty(revokedProxy, 'x')",
    "Reflect.ownKeys(revokedProxy)",
    "Reflect.getPrototypeOf(revokedProxy)",
    "Reflect.setPrototypeOf(revokedProxy, null)",
    "Reflect.defineProperty(revokedProxy, 'x', { value: 1 })",
    "Reflect.isExtensible(revokedProxy)",
    "Reflect.preventExtensions(revokedProxy)",
  ] {
    let script = format!("try {{ {expr}; 'NO_THROW' }} catch (e) {{ e.message }}");
    let out = rt.exec_script(&script)?;
    let msg = expect_string(&rt, out);
    assert!(
      msg.contains("revoked"),
      "expected revoked-proxy error for `{expr}`, got {msg}"
    );
  }

  Ok(())
}

