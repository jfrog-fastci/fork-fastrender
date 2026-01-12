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
    let callable_proxy = scope.alloc_proxy(Some(callable_target), Some(callable_handler))?;
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
    let non_callable_proxy = scope.alloc_proxy(Some(non_callable_target), Some(non_callable_handler))?;
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
    let ctor_proxy = scope.alloc_proxy(Some(ctor_target), Some(ctor_handler))?;
    define_global(
      &mut scope,
      global,
      "constructableProxy",
      Value::Object(ctor_proxy),
    )?;

    // Revoked proxies should throw on call/construct.
    let revoked_call_handler = scope.alloc_object()?;
    scope.push_root(Value::Object(revoked_call_handler))?;
    let revoked_callable_proxy = scope.alloc_proxy(Some(callable_target), Some(revoked_call_handler))?;
    scope.revoke_proxy(revoked_callable_proxy)?;
    define_global(
      &mut scope,
      global,
      "revokedCallableProxy",
      Value::Object(revoked_callable_proxy),
    )?;

    let revoked_ctor_handler = scope.alloc_object()?;
    scope.push_root(Value::Object(revoked_ctor_handler))?;
    let revoked_constructable_proxy = scope.alloc_proxy(Some(ctor_target), Some(revoked_ctor_handler))?;
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
