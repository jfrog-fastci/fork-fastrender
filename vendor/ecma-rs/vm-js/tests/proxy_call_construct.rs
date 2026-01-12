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

fn native_len_call(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  Ok(Value::Number(args.len() as f64))
}

fn native_len_construct(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;

  let key_s = scope.alloc_string("len")?;
  scope.push_root(Value::String(key_s))?;
  let key = PropertyKey::from_string(key_s);

  let desc = PropertyDescriptor {
    enumerable: true,
    configurable: true,
    kind: PropertyKind::Data {
      value: Value::Number(args.len() as f64),
      writable: true,
    },
  };
  scope.define_property(obj, key, desc)?;
  Ok(Value::Object(obj))
}

fn apply_trap_returns_target_is_valid(
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

fn construct_trap_returns_object_if_target_valid(
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
    return Ok(Value::Undefined);
  };
  if !scope.heap().is_valid_object(target_obj) {
    return Ok(Value::Undefined);
  }
  Ok(Value::Object(scope.alloc_object()?))
}

fn trap_getter_revokes_proxy_and_forces_gc(
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
    return Err(VmError::InvariantViolation("trap getter missing proxy slot"));
  };
  let Some(Value::Object(trap)) = slots.get(1).copied() else {
    return Err(VmError::InvariantViolation("trap getter missing trap slot"));
  };

  scope.revoke_proxy(proxy)?;
  // Force a GC while the proxy is revoked and its target is otherwise unreachable.
  scope.heap_mut().collect_garbage();
  Ok(Value::Object(trap))
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

#[test]
fn proxy_typeof_call_and_construct() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let global = rt.realm().global_object();

  {
    let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let call_id = vm.register_native_call(native_len_call)?;
    let construct_id = vm.register_native_construct(native_len_construct)?;

    let mut scope = heap.scope();

    // Callable proxy (target is a native function).
    let callable_name = scope.alloc_string("callableTarget")?;
    scope.push_root(Value::String(callable_name))?;
    let callable_target = scope.alloc_native_function(call_id, None, callable_name, 0)?;
    scope.push_root(Value::Object(callable_target))?;
    let callable_handler = scope.alloc_object()?;
    scope.push_root(Value::Object(callable_handler))?;
    let callable_proxy = scope.alloc_proxy(callable_target, callable_handler)?;
    define_global(
      &mut scope,
      global,
      "callableProxy",
      Value::Object(callable_proxy),
    )?;

    // Non-callable proxy (target is a plain object).
    let non_callable_target = scope.alloc_object()?;
    scope.push_root(Value::Object(non_callable_target))?;
    let non_callable_handler = scope.alloc_object()?;
    scope.push_root(Value::Object(non_callable_handler))?;
    let non_callable_proxy =
      scope.alloc_proxy(Some(non_callable_target), Some(non_callable_handler))?;
    define_global(
      &mut scope,
      global,
      "nonCallableProxy",
      Value::Object(non_callable_proxy),
    )?;

    // Constructable proxy (target is a native constructor).
    let ctor_name = scope.alloc_string("ctorTarget")?;
    scope.push_root(Value::String(ctor_name))?;
    let ctor_target = scope.alloc_native_function(call_id, Some(construct_id), ctor_name, 0)?;
    scope.push_root(Value::Object(ctor_target))?;
    let ctor_handler = scope.alloc_object()?;
    scope.push_root(Value::Object(ctor_handler))?;
    let ctor_proxy = scope.alloc_proxy(ctor_target, ctor_handler)?;
    define_global(
      &mut scope,
      global,
      "constructableProxy",
      Value::Object(ctor_proxy),
    )?;

    // Revoked proxies should throw on call/construct.
    let revoked_call_handler = scope.alloc_object()?;
    scope.push_root(Value::Object(revoked_call_handler))?;
    let revoked_callable_proxy =
      scope.alloc_proxy(Some(callable_target), Some(revoked_call_handler))?;
    scope.revoke_proxy(revoked_callable_proxy)?;
    define_global(
      &mut scope,
      global,
      "revokedCallableProxy",
      Value::Object(revoked_callable_proxy),
    )?;

    let revoked_ctor_handler = scope.alloc_object()?;
    scope.push_root(Value::Object(revoked_ctor_handler))?;
    let revoked_constructable_proxy =
      scope.alloc_proxy(Some(ctor_target), Some(revoked_ctor_handler))?;
    scope.revoke_proxy(revoked_constructable_proxy)?;
    define_global(
      &mut scope,
      global,
      "revokedConstructableProxy",
      Value::Object(revoked_constructable_proxy),
    )?;
  }

  let v = rt.exec_script("typeof callableProxy")?;
  assert_eq!(expect_string(&rt, v), "function");
  let v = rt.exec_script("typeof nonCallableProxy")?;
  assert_eq!(expect_string(&rt, v), "object");
  // Callable/constructable proxies remain callable even after revocation; `typeof` must still
  // report `"function"` (spec: `IsCallable` checks `[[Call]]` internal method presence).
  let v = rt.exec_script("typeof revokedCallableProxy")?;
  assert_eq!(expect_string(&rt, v), "function");
  let v = rt.exec_script("typeof revokedConstructableProxy")?;
  assert_eq!(expect_string(&rt, v), "function");

  assert_eq!(rt.exec_script("callableProxy(1, 2, 3)")?, Value::Number(3.0));
  assert_eq!(
    rt.exec_script("var o = new constructableProxy(1, 2); o.len")?,
    Value::Number(2.0)
  );

  let v = rt.exec_script("try { revokedCallableProxy(); } catch (e) { e.message }")?;
  let msg = expect_string(&rt, v);
  assert!(msg.contains("revoked"), "expected revoked-proxy message, got {msg}");

  let v = rt.exec_script("try { new revokedConstructableProxy(); } catch (e) { e.message }")?;
  let msg = expect_string(&rt, v);
  assert!(msg.contains("revoked"), "expected revoked-proxy message, got {msg}");

  Ok(())
}

#[test]
fn proxy_apply_trap_can_revoke_during_trap_lookup() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let global = rt.realm().global_object();

  {
    let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let len_call_id = vm.register_native_call(native_len_call)?;
    let apply_trap_id = vm.register_native_call(apply_trap_returns_target_is_valid)?;
    let getter_id = vm.register_native_call(trap_getter_revokes_proxy_and_forces_gc)?;

    let mut scope = heap.scope();

    let target_name = scope.alloc_string("callTarget")?;
    scope.push_root(Value::String(target_name))?;
    let target = scope.alloc_native_function(len_call_id, None, target_name, 0)?;
    scope.push_root(Value::Object(target))?;

    let handler = scope.alloc_object()?;
    scope.push_root(Value::Object(handler))?;

    let proxy = scope.alloc_proxy(Some(target), Some(handler))?;
    scope.push_root(Value::Object(proxy))?;

    let trap_name = scope.alloc_string("applyTrap")?;
    scope.push_root(Value::String(trap_name))?;
    let trap = scope.alloc_native_function(apply_trap_id, None, trap_name, 3)?;
    scope.push_root(Value::Object(trap))?;

    let getter_name = scope.alloc_string("applyGetter")?;
    scope.push_root(Value::String(getter_name))?;
    let slots = [Value::Object(proxy), Value::Object(trap)];
    let getter = scope.alloc_native_function_with_slots(getter_id, None, getter_name, 0, &slots)?;
    scope.push_root(Value::Object(getter))?;

    let apply_key_s = scope.alloc_string("apply")?;
    scope.push_root(Value::String(apply_key_s))?;
    let apply_key = PropertyKey::from_string(apply_key_s);
    scope.define_property(
      handler,
      apply_key,
      PropertyDescriptor {
        enumerable: true,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(getter),
          set: Value::Undefined,
        },
      },
    )?;

    define_global(&mut scope, global, "p", Value::Object(proxy))?;
  }

  // Should succeed even if the proxy is revoked while looking up `handler.apply`.
  assert_eq!(rt.exec_script("p()")?, Value::Bool(true));
  Ok(())
}

#[test]
fn proxy_construct_trap_can_revoke_during_trap_lookup() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let global = rt.realm().global_object();

  {
    let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let len_call_id = vm.register_native_call(native_len_call)?;
    let len_construct_id = vm.register_native_construct(native_len_construct)?;
    let construct_trap_id = vm.register_native_call(construct_trap_returns_object_if_target_valid)?;
    let getter_id = vm.register_native_call(trap_getter_revokes_proxy_and_forces_gc)?;

    let mut scope = heap.scope();

    let target_name = scope.alloc_string("ctorTarget")?;
    scope.push_root(Value::String(target_name))?;
    let target = scope.alloc_native_function(len_call_id, Some(len_construct_id), target_name, 0)?;
    scope.push_root(Value::Object(target))?;

    let handler = scope.alloc_object()?;
    scope.push_root(Value::Object(handler))?;

    let proxy = scope.alloc_proxy(Some(target), Some(handler))?;
    scope.push_root(Value::Object(proxy))?;

    let trap_name = scope.alloc_string("constructTrap")?;
    scope.push_root(Value::String(trap_name))?;
    let trap = scope.alloc_native_function(construct_trap_id, None, trap_name, 3)?;
    scope.push_root(Value::Object(trap))?;

    let getter_name = scope.alloc_string("constructGetter")?;
    scope.push_root(Value::String(getter_name))?;
    let slots = [Value::Object(proxy), Value::Object(trap)];
    let getter = scope.alloc_native_function_with_slots(getter_id, None, getter_name, 0, &slots)?;
    scope.push_root(Value::Object(getter))?;

    let construct_key_s = scope.alloc_string("construct")?;
    scope.push_root(Value::String(construct_key_s))?;
    let construct_key = PropertyKey::from_string(construct_key_s);
    scope.define_property(
      handler,
      construct_key,
      PropertyDescriptor {
        enumerable: true,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(getter),
          set: Value::Undefined,
        },
      },
    )?;

    define_global(&mut scope, global, "p", Value::Object(proxy))?;
  }

  assert_eq!(rt.exec_script("typeof (new p()) === 'object'")?, Value::Bool(true));
  Ok(())
}
