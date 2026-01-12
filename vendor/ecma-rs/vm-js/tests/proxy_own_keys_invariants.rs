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

fn handler_getter_revokes_proxy_and_forces_gc_returns_undefined(
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

  scope.heap_mut().proxy_revoke(proxy)?;
  // Force a GC cycle while the Proxy is revoked and its target is otherwise unreachable. The engine
  // must still keep using the original `[[ProxyTarget]]` for this operation.
  scope.heap_mut().collect_garbage();
  Ok(Value::Undefined)
}

#[test]
fn proxy_own_keys_trap_invariants_are_enforced() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  // Duplicate keys => TypeError.
  assert_eq!(
    rt.exec_script(
      r#"
      var ok = false;
      try { Reflect.ownKeys(new Proxy({}, { ownKeys(){ return ["a","a"]; } })); }
      catch (e) { ok = e.name === "TypeError"; }
      ok
      "#,
    )?,
    Value::Bool(true)
  );

  // Non-configurable target key must be reported.
  assert_eq!(
    rt.exec_script(
      r#"
      var target = {};
      Object.defineProperty(target, "a", { value: 1, configurable: false });
      var p = new Proxy(target, { ownKeys(){ return []; } });
      var ok = false;
      try { Reflect.ownKeys(p); } catch (e) { ok = e.name === "TypeError"; }
      ok
      "#,
    )?,
    Value::Bool(true)
  );

  // Non-extensible target: all keys must be reported.
  assert_eq!(
    rt.exec_script(
      r#"
      var target = { a: 1 };
      Object.preventExtensions(target);
      var p = new Proxy(target, { ownKeys(){ return []; } });
      var ok = false;
      try { Reflect.ownKeys(p); } catch (e) { ok = e.name === "TypeError"; }
      ok
      "#,
    )?,
    Value::Bool(true)
  );

  // Non-extensible target: extra keys are not allowed.
  assert_eq!(
    rt.exec_script(
      r#"
      var target = { a: 1 };
      Object.preventExtensions(target);
      var p = new Proxy(target, { ownKeys(){ return ["a","b"]; } });
      var ok = false;
      try { Reflect.ownKeys(p); } catch (e) { ok = e.name === "TypeError"; }
      ok
      "#,
    )?,
    Value::Bool(true)
  );

  // Extensible target with no non-configurable keys: extra keys are allowed.
  assert_eq!(
    rt.exec_script(
      r#"
      var target = {};
      var p = new Proxy(target, { ownKeys(){ return ["a"]; } });
      var k = Reflect.ownKeys(p);
      k.length === 1 && k[0] === "a"
      "#,
    )?,
    Value::Bool(true)
  );

  Ok(())
}

#[test]
fn proxy_own_keys_trap_can_revoke_during_trap_lookup_without_breaking_operation(
) -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let global = rt.realm().global_object();

  {
    let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let getter_id = vm.register_native_call(handler_getter_revokes_proxy_and_forces_gc_returns_undefined)?;

    let mut scope = heap.scope();

    let target = scope.alloc_object()?;
    scope.push_root(Value::Object(target))?;
    let foo_s = scope.alloc_string("foo")?;
    scope.push_root(Value::String(foo_s))?;
    scope.define_property(
      target,
      PropertyKey::from_string(foo_s),
      global_var_desc(Value::Number(1.0)),
    )?;

    let handler = scope.alloc_object()?;
    scope.push_root(Value::Object(handler))?;

    let proxy = scope.alloc_proxy(Some(target), Some(handler))?;

    // Accessor getter for `handler.ownKeys` that revokes `proxy` during `GetMethod(handler, "ownKeys")`
    // and forces a GC before returning `undefined` (so the trap is treated as absent and the operation
    // forwards to the target).
    let getter_name = scope.alloc_string("ownKeysGetter")?;
    scope.push_root(Value::String(getter_name))?;
    let slots = [Value::Object(proxy)];
    let getter_fn = scope.alloc_native_function_with_slots(getter_id, None, getter_name, 0, &slots)?;
    scope.push_root(Value::Object(getter_fn))?;

    let own_keys_key_s = scope.alloc_string("ownKeys")?;
    scope.push_root(Value::String(own_keys_key_s))?;
    let own_keys_key = PropertyKey::from_string(own_keys_key_s);
    scope.define_property(
      handler,
      own_keys_key,
      PropertyDescriptor {
        enumerable: true,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(getter_fn),
          set: Value::Undefined,
        },
      },
    )?;

    define_global(&mut scope, global, "p", Value::Object(proxy))?;
  }

  // The operation should still succeed even though the proxy is revoked during trap lookup, and it
  // should still forward to the original target.
  assert_eq!(
    rt.exec_script(
      r#"
      var k = Reflect.ownKeys(p);
      k.length === 1 && k[0] === "foo"
      "#,
    )?,
    Value::Bool(true)
  );

  Ok(())
}
