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

