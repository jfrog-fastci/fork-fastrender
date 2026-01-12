use vm_js::{
  GcObject, Heap, HeapLimits, JsRuntime, PropertyDescriptor, PropertyKey, PropertyKind, Scope, Value, Vm,
  VmError, VmHost, VmHostHooks, VmOptions,
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
  scope.push_root(Value::Object(global))?;
  scope.push_root(value)?;
  let key_s = scope.alloc_string(name)?;
  scope.push_root(Value::String(key_s))?;
  let key = PropertyKey::from_string(key_s);
  scope.define_property(global, key, global_var_desc(value))
}

fn proxy_get_trap_foo_is_123(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let prop = args.get(1).copied().unwrap_or(Value::Undefined);
  let Value::String(s) = prop else {
    return Ok(Value::Undefined);
  };
  if scope.heap().get_string(s)?.to_utf8_lossy() == "foo" {
    return Ok(Value::Number(123.0));
  }
  Ok(Value::Undefined)
}

fn proxy_set_trap_defines_target_property(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let target = args.get(0).copied().unwrap_or(Value::Undefined);
  let prop = args.get(1).copied().unwrap_or(Value::Undefined);
  let value = args.get(2).copied().unwrap_or(Value::Undefined);

  let Value::Object(target_obj) = target else {
    return Ok(Value::Bool(true));
  };
  let Value::String(s) = prop else {
    return Ok(Value::Bool(true));
  };

  scope.create_data_property_or_throw(target_obj, PropertyKey::from_string(s), value)?;
  Ok(Value::Bool(true))
}

fn proxy_has_trap_foo_is_true(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let prop = args.get(1).copied().unwrap_or(Value::Undefined);
  let Value::String(s) = prop else {
    return Ok(Value::Bool(false));
  };
  Ok(Value::Bool(scope.heap().get_string(s)?.to_utf8_lossy() == "foo"))
}

fn proxy_delete_trap_deletes_target_property(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let target = args.get(0).copied().unwrap_or(Value::Undefined);
  let prop = args.get(1).copied().unwrap_or(Value::Undefined);

  let Value::Object(target_obj) = target else {
    return Ok(Value::Bool(true));
  };
  let Value::String(s) = prop else {
    return Ok(Value::Bool(true));
  };

  let ok = scope.ordinary_delete(target_obj, PropertyKey::from_string(s))?;
  Ok(Value::Bool(ok))
}

fn proxy_define_property_trap_defines_target_property(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let target = args.get(0).copied().unwrap_or(Value::Undefined);
  let prop = args.get(1).copied().unwrap_or(Value::Undefined);
  let desc = args.get(2).copied().unwrap_or(Value::Undefined);

  let Value::Object(target_obj) = target else {
    return Ok(Value::Bool(false));
  };
  let key = match prop {
    Value::String(s) => PropertyKey::from_string(s),
    Value::Symbol(s) => PropertyKey::from_symbol(s),
    _ => return Ok(Value::Bool(false)),
  };
  let Value::Object(desc_obj) = desc else {
    return Ok(Value::Bool(false));
  };

  let patch = vm_js::to_property_descriptor_with_host_and_hooks(vm, scope, host, hooks, desc_obj)?;
  let ok = scope.define_own_property_with_tick(target_obj, key, patch, || vm.tick())?;
  Ok(Value::Bool(ok))
}

fn proxy_define_property_trap_returns_true(
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

fn proxy_define_property_trap_returns_target_is_valid(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let target = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Object(target_obj) = target else {
    return Ok(Value::Bool(false));
  };
  Ok(Value::Bool(scope.heap().is_valid_object(target_obj)))
}

fn proxy_get_trap_returns_target_is_valid(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let target = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Object(target_obj) = target else {
    return Ok(Value::Bool(false));
  };
  Ok(Value::Bool(scope.heap().is_valid_object(target_obj)))
}

fn handler_getter_revokes_proxy_and_forces_gc(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let Some(Value::Object(proxy)) = slots.first().copied() else {
    return Err(VmError::InvariantViolation(
      "handler getter missing proxy slot",
    ));
  };
  let Some(Value::Object(trap)) = slots.get(1).copied() else {
    return Err(VmError::InvariantViolation(
      "handler getter missing trap slot",
    ));
  };

  scope.heap_mut().proxy_revoke(proxy)?;
  // Force a GC cycle while the Proxy is revoked and its target is otherwise unreachable. The
  // engine must still keep using the original `[[ProxyTarget]]` for this operation.
  scope.heap_mut().collect_garbage();
  Ok(Value::Object(trap))
}

#[test]
fn proxy_property_traps_are_observable_from_js() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let global = rt.realm().global_object();

  {
    let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let get_id = vm.register_native_call(proxy_get_trap_foo_is_123)?;
    let set_id = vm.register_native_call(proxy_set_trap_defines_target_property)?;
    let has_id = vm.register_native_call(proxy_has_trap_foo_is_true)?;
    let del_id = vm.register_native_call(proxy_delete_trap_deletes_target_property)?;

    let mut scope = heap.scope();

    let target = scope.alloc_object()?;
    scope.push_root(Value::Object(target))?;
    define_global(&mut scope, global, "t", Value::Object(target))?;

    let handler = scope.alloc_object()?;
    scope.push_root(Value::Object(handler))?;

    // handler.get = <native>
    let get_name = scope.alloc_string("get")?;
    scope.push_root(Value::String(get_name))?;
    let get_fn = scope.alloc_native_function(get_id, None, get_name, 3)?;
    scope.push_root(Value::Object(get_fn))?;
    scope.define_property(
      handler,
      PropertyKey::from_string(get_name),
      global_var_desc(Value::Object(get_fn)),
    )?;

    // handler.set = <native>
    let set_name = scope.alloc_string("set")?;
    scope.push_root(Value::String(set_name))?;
    let set_fn = scope.alloc_native_function(set_id, None, set_name, 4)?;
    scope.push_root(Value::Object(set_fn))?;
    scope.define_property(
      handler,
      PropertyKey::from_string(set_name),
      global_var_desc(Value::Object(set_fn)),
    )?;

    // handler.has = <native>
    let has_name = scope.alloc_string("has")?;
    scope.push_root(Value::String(has_name))?;
    let has_fn = scope.alloc_native_function(has_id, None, has_name, 2)?;
    scope.push_root(Value::Object(has_fn))?;
    scope.define_property(
      handler,
      PropertyKey::from_string(has_name),
      global_var_desc(Value::Object(has_fn)),
    )?;

    // handler.deleteProperty = <native>
    let del_name = scope.alloc_string("deleteProperty")?;
    scope.push_root(Value::String(del_name))?;
    let del_fn = scope.alloc_native_function(del_id, None, del_name, 2)?;
    scope.push_root(Value::Object(del_fn))?;
    scope.define_property(
      handler,
      PropertyKey::from_string(del_name),
      global_var_desc(Value::Object(del_fn)),
    )?;

    let proxy = scope.alloc_proxy(Some(target), Some(handler))?;
    define_global(&mut scope, global, "p", Value::Object(proxy))?;
  }

  assert_eq!(rt.exec_script("p.foo")?, Value::Number(123.0));
  assert_eq!(rt.exec_script("p.foo = 321; t.foo")?, Value::Number(321.0));
  assert_eq!(rt.exec_script(r#""foo" in p"#)?, Value::Bool(true));
  assert_eq!(rt.exec_script("delete p.foo")?, Value::Bool(true));
  assert_eq!(rt.exec_script("t.foo")?, Value::Undefined);
  Ok(())
}

#[test]
fn proxy_define_property_trap_is_observable_from_js() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let global = rt.realm().global_object();

  {
    let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let define_id = vm.register_native_call(proxy_define_property_trap_defines_target_property)?;

    let mut scope = heap.scope();

    let target = scope.alloc_object()?;
    scope.push_root(Value::Object(target))?;
    define_global(&mut scope, global, "t", Value::Object(target))?;

    let handler = scope.alloc_object()?;
    scope.push_root(Value::Object(handler))?;

    // handler.defineProperty = <native>
    let trap_name = scope.alloc_string("defineProperty")?;
    scope.push_root(Value::String(trap_name))?;
    let trap_fn = scope.alloc_native_function(define_id, None, trap_name, 3)?;
    scope.push_root(Value::Object(trap_fn))?;
    scope.define_property(
      handler,
      PropertyKey::from_string(trap_name),
      global_var_desc(Value::Object(trap_fn)),
    )?;

    let proxy = scope.alloc_proxy(Some(target), Some(handler))?;
    define_global(&mut scope, global, "p", Value::Object(proxy))?;
  }

  assert_eq!(
    rt.exec_script(r#"Reflect.defineProperty(p, "foo", { value: 123 })"#)?,
    Value::Bool(true)
  );
  assert_eq!(rt.exec_script("t.foo")?, Value::Number(123.0));

  assert_eq!(
    rt.exec_script(r#"Object.defineProperty(p, "bar", { value: 321 }) === p"#)?,
    Value::Bool(true)
  );
  assert_eq!(rt.exec_script("t.bar")?, Value::Number(321.0));

  Ok(())
}

#[test]
fn proxy_define_property_trap_invariants_are_enforced() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let global = rt.realm().global_object();

  // Target is non-extensible and does not have `foo`, but trap reports success: must throw.
  {
    let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let trap_id = vm.register_native_call(proxy_define_property_trap_returns_true)?;

    let mut scope = heap.scope();

    let target = scope.alloc_object()?;
    scope.push_root(Value::Object(target))?;
    scope.object_prevent_extensions(target)?;

    let handler = scope.alloc_object()?;
    scope.push_root(Value::Object(handler))?;

    let trap_name = scope.alloc_string("defineProperty")?;
    scope.push_root(Value::String(trap_name))?;
    let trap_fn = scope.alloc_native_function(trap_id, None, trap_name, 3)?;
    scope.push_root(Value::Object(trap_fn))?;
    scope.define_property(
      handler,
      PropertyKey::from_string(trap_name),
      global_var_desc(Value::Object(trap_fn)),
    )?;

    let proxy = scope.alloc_proxy(Some(target), Some(handler))?;
    define_global(&mut scope, global, "p", Value::Object(proxy))?;
  }

  assert_eq!(
    rt.exec_script(
      r#"
      var ok = false;
      try { Reflect.defineProperty(p, "foo", { value: 1 }); } catch (e) { ok = e.name === "TypeError"; }
      ok
      "#,
    )?,
    Value::Bool(true)
  );

  // Target has a configurable property `foo`, but trap reports success for a non-configurable
  // definition without actually making it non-configurable: must throw.
  {
    let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let trap_id = vm.register_native_call(proxy_define_property_trap_returns_true)?;

    let mut scope = heap.scope();

    let target = scope.alloc_object()?;
    scope.push_root(Value::Object(target))?;
    let foo_s = scope.alloc_string("foo")?;
    scope.push_root(Value::String(foo_s))?;
    scope.define_property(
      target,
      PropertyKey::from_string(foo_s),
      global_var_desc(Value::Number(0.0)),
    )?;

    let handler = scope.alloc_object()?;
    scope.push_root(Value::Object(handler))?;

    let trap_name = scope.alloc_string("defineProperty")?;
    scope.push_root(Value::String(trap_name))?;
    let trap_fn = scope.alloc_native_function(trap_id, None, trap_name, 3)?;
    scope.push_root(Value::Object(trap_fn))?;
    scope.define_property(
      handler,
      PropertyKey::from_string(trap_name),
      global_var_desc(Value::Object(trap_fn)),
    )?;

    let proxy = scope.alloc_proxy(Some(target), Some(handler))?;
    define_global(&mut scope, global, "p", Value::Object(proxy))?;
  }

  assert_eq!(
    rt.exec_script(
      r#"
      var ok = false;
      try { Reflect.defineProperty(p, "foo", { configurable: false, value: 1 }); } catch (e) { ok = e.name === "TypeError"; }
      ok
      "#,
    )?,
    Value::Bool(true)
  );

  Ok(())
}

#[test]
fn proxy_define_property_trap_can_revoke_during_trap_lookup_without_breaking_operation(
) -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let global = rt.realm().global_object();

  {
    let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let trap_id = vm.register_native_call(proxy_define_property_trap_returns_target_is_valid)?;
    let getter_id = vm.register_native_call(handler_getter_revokes_proxy_and_forces_gc)?;

    let mut scope = heap.scope();

    let target = scope.alloc_object()?;
    scope.push_root(Value::Object(target))?;

    let handler = scope.alloc_object()?;
    scope.push_root(Value::Object(handler))?;

    let proxy = scope.alloc_proxy(Some(target), Some(handler))?;

    // The actual defineProperty trap (returns whether the passed `target` handle is still valid).
    let trap_name = scope.alloc_string("definePropertyTrap")?;
    scope.push_root(Value::String(trap_name))?;
    let trap_fn = scope.alloc_native_function(trap_id, None, trap_name, 3)?;
    scope.push_root(Value::Object(trap_fn))?;

    // Accessor getter for `handler.defineProperty` that revokes `proxy` during
    // `GetMethod(handler, "defineProperty")` and forces a GC before returning the trap function.
    let getter_name = scope.alloc_string("definePropertyTrapGetter")?;
    scope.push_root(Value::String(getter_name))?;
    let slots = [Value::Object(proxy), Value::Object(trap_fn)];
    let getter_fn =
      scope.alloc_native_function_with_slots(getter_id, None, getter_name, 0, &slots)?;
    scope.push_root(Value::Object(getter_fn))?;

    let define_key_s = scope.alloc_string("defineProperty")?;
    scope.push_root(Value::String(define_key_s))?;
    let define_key = PropertyKey::from_string(define_key_s);
    let desc = PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Accessor {
        get: Value::Object(getter_fn),
        set: Value::Undefined,
      },
    };
    scope.define_property(handler, define_key, desc)?;

    define_global(&mut scope, global, "p", Value::Object(proxy))?;
  }

  // The operation should still succeed even though the proxy is revoked during trap lookup, and
  // the `target` passed to the trap must be a live object.
  assert_eq!(
    rt.exec_script(r#"Reflect.defineProperty(p, "foo", { value: 1 })"#)?,
    Value::Bool(true)
  );
  Ok(())
}

#[test]
fn proxy_property_access_throws_on_revoked_proxy() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let global = rt.realm().global_object();

  {
    let (_vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let mut scope = heap.scope();

    let target = scope.alloc_object()?;
    scope.push_root(Value::Object(target))?;
    let handler = scope.alloc_object()?;
    scope.push_root(Value::Object(handler))?;

    let proxy = scope.alloc_proxy(Some(target), Some(handler))?;
    scope.revoke_proxy(proxy)?;
    define_global(&mut scope, global, "p", Value::Object(proxy))?;
  }

  let value = rt.exec_script(
    r#"
    var ok = false;
    try { p.foo; } catch (e) { ok = e.name === "TypeError"; }
    ok
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn proxy_get_trap_can_revoke_during_trap_lookup_without_breaking_operation() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let global = rt.realm().global_object();

  {
    let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let trap_id = vm.register_native_call(proxy_get_trap_returns_target_is_valid)?;
    let getter_id = vm.register_native_call(handler_getter_revokes_proxy_and_forces_gc)?;

    let mut scope = heap.scope();

    let target = scope.alloc_object()?;
    scope.push_root(Value::Object(target))?;

    let handler = scope.alloc_object()?;
    scope.push_root(Value::Object(handler))?;

    let proxy = scope.alloc_proxy(Some(target), Some(handler))?;

    // The actual get trap (returns whether the passed `target` handle is still valid).
    let trap_name = scope.alloc_string("getTrap")?;
    scope.push_root(Value::String(trap_name))?;
    let trap_fn = scope.alloc_native_function(trap_id, None, trap_name, 3)?;
    scope.push_root(Value::Object(trap_fn))?;

    // Accessor getter for `handler.get` that revokes `proxy` during `GetMethod(handler, "get")` and
    // forces a GC before returning the trap function.
    let getter_name = scope.alloc_string("getTrapGetter")?;
    scope.push_root(Value::String(getter_name))?;
    let slots = [Value::Object(proxy), Value::Object(trap_fn)];
    let getter_fn = scope.alloc_native_function_with_slots(getter_id, None, getter_name, 0, &slots)?;
    scope.push_root(Value::Object(getter_fn))?;

    let get_key_s = scope.alloc_string("get")?;
    scope.push_root(Value::String(get_key_s))?;
    let get_key = PropertyKey::from_string(get_key_s);
    let desc = PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Accessor {
        get: Value::Object(getter_fn),
        set: Value::Undefined,
      },
    };
    scope.define_property(handler, get_key, desc)?;

    define_global(&mut scope, global, "p", Value::Object(proxy))?;
  }

  // The operation should still succeed even though the proxy is revoked during trap lookup, and
  // the `target` passed to the trap must be a live object.
  assert_eq!(rt.exec_script("p.foo")?, Value::Bool(true));
  Ok(())
}
