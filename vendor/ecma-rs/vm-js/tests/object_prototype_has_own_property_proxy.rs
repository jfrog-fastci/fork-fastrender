use vm_js::{
  GcObject, Heap, HeapLimits, JsRuntime, PropertyDescriptor, PropertyKey, PropertyKind, Scope, Value,
  Vm, VmError, VmHost, VmHostHooks, VmOptions,
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

fn gopd_trap_returns_desc_for_a(
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
  if scope.heap().get_string(s)?.to_utf8_lossy() != "a" {
    return Ok(Value::Undefined);
  }

  let desc_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(desc_obj))?;

  let key_s = scope.alloc_string("value")?;
  scope.push_root(Value::String(key_s))?;
  scope.define_property(
    desc_obj,
    PropertyKey::from_string(key_s),
    global_var_desc(Value::Number(1.0)),
  )?;

  let key_s = scope.alloc_string("writable")?;
  scope.push_root(Value::String(key_s))?;
  scope.define_property(
    desc_obj,
    PropertyKey::from_string(key_s),
    global_var_desc(Value::Bool(true)),
  )?;

  let key_s = scope.alloc_string("enumerable")?;
  scope.push_root(Value::String(key_s))?;
  scope.define_property(
    desc_obj,
    PropertyKey::from_string(key_s),
    global_var_desc(Value::Bool(true)),
  )?;

  let key_s = scope.alloc_string("configurable")?;
  scope.push_root(Value::String(key_s))?;
  scope.define_property(
    desc_obj,
    PropertyKey::from_string(key_s),
    global_var_desc(Value::Bool(true)),
  )?;

  Ok(Value::Object(desc_obj))
}

fn gopd_trap_returns_non_configurable_desc_for_a(
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
  if scope.heap().get_string(s)?.to_utf8_lossy() != "a" {
    return Ok(Value::Undefined);
  }

  let desc_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(desc_obj))?;

  let key_s = scope.alloc_string("value")?;
  scope.push_root(Value::String(key_s))?;
  scope.define_property(
    desc_obj,
    PropertyKey::from_string(key_s),
    global_var_desc(Value::Number(1.0)),
  )?;

  // Report `configurable: false` for a property that does not exist on the target.
  // Per spec, this must throw a TypeError.
  let key_s = scope.alloc_string("configurable")?;
  scope.push_root(Value::String(key_s))?;
  scope.define_property(
    desc_obj,
    PropertyKey::from_string(key_s),
    global_var_desc(Value::Bool(false)),
  )?;

  Ok(Value::Object(desc_obj))
}

fn gopd_trap_returns_undefined_for_a(
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
  if scope.heap().get_string(s)?.to_utf8_lossy() != "a" {
    return Ok(Value::Undefined);
  }
  Ok(Value::Undefined)
}

fn own_keys_trap_returns_a(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let array = scope.alloc_array(1)?;
  scope.push_root(Value::Object(array))?;

  let idx_s = scope.alloc_string("0")?;
  scope.push_root(Value::String(idx_s))?;
  let a_s = scope.alloc_string("a")?;
  scope.push_root(Value::String(a_s))?;

  scope.define_property(
    array,
    PropertyKey::from_string(idx_s),
    global_var_desc(Value::String(a_s)),
  )?;

  Ok(Value::Object(array))
}

fn get_trap_returns_42_for_a(
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
  if scope.heap().get_string(s)?.to_utf8_lossy() != "a" {
    return Ok(Value::Undefined);
  }
  Ok(Value::Number(42.0))
}

#[test]
fn object_prototype_has_own_property_is_proxy_aware() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let global = rt.realm().global_object();

  {
    let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let call_id = vm.register_native_call(gopd_trap_returns_desc_for_a)?;

    let mut scope = heap.scope();

    let target = scope.alloc_object()?;
    scope.push_root(Value::Object(target))?;

    let handler = scope.alloc_object()?;
    scope.push_root(Value::Object(handler))?;

    let name = scope.alloc_string("gopd")?;
    scope.push_root(Value::String(name))?;
    let trap = scope.alloc_native_function(call_id, None, name, 2)?;
    scope.push_root(Value::Object(trap))?;

    let key_s = scope.alloc_string("getOwnPropertyDescriptor")?;
    scope.push_root(Value::String(key_s))?;
    scope.define_property(handler, PropertyKey::from_string(key_s), global_var_desc(Value::Object(trap)))?;

    let proxy = scope.alloc_proxy(Some(target), Some(handler))?;
    define_global(&mut scope, global, "p", Value::Object(proxy))?;
  }

  let value = rt.exec_script(r#"Object.prototype.hasOwnProperty.call(p, "a")"#)?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn object_prototype_has_own_property_throws_on_revoked_proxy() -> Result<(), VmError> {
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
    try { Object.prototype.hasOwnProperty.call(p, "a"); } catch (e) { ok = e.name === "TypeError"; }
    ok
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn reflect_get_own_property_descriptor_is_proxy_aware() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let global = rt.realm().global_object();

  {
    let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let call_id = vm.register_native_call(gopd_trap_returns_desc_for_a)?;

    let mut scope = heap.scope();

    let target = scope.alloc_object()?;
    scope.push_root(Value::Object(target))?;

    let handler = scope.alloc_object()?;
    scope.push_root(Value::Object(handler))?;

    let name = scope.alloc_string("gopd")?;
    scope.push_root(Value::String(name))?;
    let trap = scope.alloc_native_function(call_id, None, name, 2)?;
    scope.push_root(Value::Object(trap))?;

    let key_s = scope.alloc_string("getOwnPropertyDescriptor")?;
    scope.push_root(Value::String(key_s))?;
    scope.define_property(
      handler,
      PropertyKey::from_string(key_s),
      global_var_desc(Value::Object(trap)),
    )?;

    let proxy = scope.alloc_proxy(Some(target), Some(handler))?;
    define_global(&mut scope, global, "p", Value::Object(proxy))?;
  }

  let value = rt.exec_script(
    r#"
    var d = Reflect.getOwnPropertyDescriptor(p, "a");
    d !== undefined &&
      d.value === 1 &&
      d.writable === true &&
      d.enumerable === true &&
      d.configurable === true
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn reflect_get_own_property_descriptor_throws_on_revoked_proxy() -> Result<(), VmError> {
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
    try { Reflect.getOwnPropertyDescriptor(p, "a"); } catch (e) { ok = e.name === "TypeError"; }
    ok
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn reflect_get_own_property_descriptor_throws_on_non_configurable_desc_for_missing_target_property(
) -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let global = rt.realm().global_object();

  {
    let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let call_id = vm.register_native_call(gopd_trap_returns_non_configurable_desc_for_a)?;

    let mut scope = heap.scope();

    let target = scope.alloc_object()?;
    scope.push_root(Value::Object(target))?;

    let handler = scope.alloc_object()?;
    scope.push_root(Value::Object(handler))?;

    let name = scope.alloc_string("gopd")?;
    scope.push_root(Value::String(name))?;
    let trap = scope.alloc_native_function(call_id, None, name, 2)?;
    scope.push_root(Value::Object(trap))?;

    let key_s = scope.alloc_string("getOwnPropertyDescriptor")?;
    scope.push_root(Value::String(key_s))?;
    scope.define_property(
      handler,
      PropertyKey::from_string(key_s),
      global_var_desc(Value::Object(trap)),
    )?;

    let proxy = scope.alloc_proxy(Some(target), Some(handler))?;
    define_global(&mut scope, global, "p", Value::Object(proxy))?;
  }

  let value = rt.exec_script(
    r#"
    var ok = false;
    try { Reflect.getOwnPropertyDescriptor(p, "a"); } catch (e) { ok = e.name === "TypeError"; }
    ok
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn reflect_get_own_property_descriptor_throws_when_trap_returns_undefined_for_non_configurable_target_property(
) -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let global = rt.realm().global_object();

  {
    let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let call_id = vm.register_native_call(gopd_trap_returns_undefined_for_a)?;

    let mut scope = heap.scope();

    let target = scope.alloc_object()?;
    scope.push_root(Value::Object(target))?;

    // Define a non-configurable own property `a` on the target.
    let a_key_s = scope.alloc_string("a")?;
    scope.push_root(Value::String(a_key_s))?;
    scope.define_property(
      target,
      PropertyKey::from_string(a_key_s),
      PropertyDescriptor {
        enumerable: true,
        configurable: false,
        kind: PropertyKind::Data {
          value: Value::Number(1.0),
          writable: true,
        },
      },
    )?;

    let handler = scope.alloc_object()?;
    scope.push_root(Value::Object(handler))?;

    let name = scope.alloc_string("gopd")?;
    scope.push_root(Value::String(name))?;
    let trap = scope.alloc_native_function(call_id, None, name, 2)?;
    scope.push_root(Value::Object(trap))?;

    let key_s = scope.alloc_string("getOwnPropertyDescriptor")?;
    scope.push_root(Value::String(key_s))?;
    scope.define_property(
      handler,
      PropertyKey::from_string(key_s),
      global_var_desc(Value::Object(trap)),
    )?;

    let proxy = scope.alloc_proxy(Some(target), Some(handler))?;
    define_global(&mut scope, global, "p", Value::Object(proxy))?;
  }

  let value = rt.exec_script(
    r#"
    var ok = false;
    try { Reflect.getOwnPropertyDescriptor(p, "a"); } catch (e) { ok = e.name === "TypeError"; }
    ok
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn object_keys_values_and_entries_are_proxy_aware() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let global = rt.realm().global_object();

  {
    let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let gopd_call_id = vm.register_native_call(gopd_trap_returns_desc_for_a)?;
    let own_keys_call_id = vm.register_native_call(own_keys_trap_returns_a)?;
    let get_call_id = vm.register_native_call(get_trap_returns_42_for_a)?;

    let mut scope = heap.scope();

    let target = scope.alloc_object()?;
    scope.push_root(Value::Object(target))?;

    let handler = scope.alloc_object()?;
    scope.push_root(Value::Object(handler))?;

    // getOwnPropertyDescriptor trap
    let name = scope.alloc_string("gopd")?;
    scope.push_root(Value::String(name))?;
    let trap = scope.alloc_native_function(gopd_call_id, None, name, 2)?;
    scope.push_root(Value::Object(trap))?;

    let key_s = scope.alloc_string("getOwnPropertyDescriptor")?;
    scope.push_root(Value::String(key_s))?;
    scope.define_property(
      handler,
      PropertyKey::from_string(key_s),
      global_var_desc(Value::Object(trap)),
    )?;

    // ownKeys trap
    let name = scope.alloc_string("ownKeys")?;
    scope.push_root(Value::String(name))?;
    let trap = scope.alloc_native_function(own_keys_call_id, None, name, 1)?;
    scope.push_root(Value::Object(trap))?;
    scope.define_property(
      handler,
      PropertyKey::from_string(name),
      global_var_desc(Value::Object(trap)),
    )?;

    // get trap
    let name = scope.alloc_string("get")?;
    scope.push_root(Value::String(name))?;
    let trap = scope.alloc_native_function(get_call_id, None, name, 3)?;
    scope.push_root(Value::Object(trap))?;
    scope.define_property(
      handler,
      PropertyKey::from_string(name),
      global_var_desc(Value::Object(trap)),
    )?;

    let proxy = scope.alloc_proxy(Some(target), Some(handler))?;
    define_global(&mut scope, global, "p", Value::Object(proxy))?;
  }

  let value = rt.exec_script(
    r#"
    var k = Object.keys(p);
    var v = Object.values(p);
    var e = Object.entries(p);
    k.length === 1 && k[0] === "a" &&
      v.length === 1 && v[0] === 42 &&
      e.length === 1 && e[0][0] === "a" && e[0][1] === 42
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn object_keys_throws_on_revoked_proxy() -> Result<(), VmError> {
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
    try { Object.keys(p); } catch (e) { ok = e.name === "TypeError"; }
    ok
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn reflect_own_keys_is_proxy_aware() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let global = rt.realm().global_object();

  {
    let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let call_id = vm.register_native_call(own_keys_trap_returns_a)?;

    let mut scope = heap.scope();

    let target = scope.alloc_object()?;
    scope.push_root(Value::Object(target))?;

    let handler = scope.alloc_object()?;
    scope.push_root(Value::Object(handler))?;

    let name = scope.alloc_string("ownKeys")?;
    scope.push_root(Value::String(name))?;
    let trap = scope.alloc_native_function(call_id, None, name, 1)?;
    scope.push_root(Value::Object(trap))?;

    scope.define_property(
      handler,
      PropertyKey::from_string(name),
      global_var_desc(Value::Object(trap)),
    )?;

    let proxy = scope.alloc_proxy(Some(target), Some(handler))?;
    define_global(&mut scope, global, "p", Value::Object(proxy))?;
  }

  let value = rt.exec_script(
    r#"
    var k = Reflect.ownKeys(p);
    k.length === 1 && k[0] === "a"
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn reflect_own_keys_throws_on_revoked_proxy() -> Result<(), VmError> {
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
    try { Reflect.ownKeys(p); } catch (e) { ok = e.name === "TypeError"; }
    ok
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}
