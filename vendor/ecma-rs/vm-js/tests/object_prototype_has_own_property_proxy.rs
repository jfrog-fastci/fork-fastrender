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
