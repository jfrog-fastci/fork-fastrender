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

fn expect_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string value, got {value:?}");
  };
  rt.heap()
    .get_string(s)
    .expect("string handle should be valid")
    .to_utf8_lossy()
}

fn native_noop_call(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Ok(Value::Undefined)
}

#[test]
fn object_prototype_to_string_tags() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let intr = *rt.realm().intrinsics();
  let global = rt.realm().global_object();

  // Install a callable Proxy and WeakMap/WeakSet-shaped objects as globals.
  {
    let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let call_id = vm.register_native_call(native_noop_call)?;

    let mut scope = heap.scope();

    // Callable Proxy: target is a native function object.
    let target_name = scope.alloc_string("target")?;
    scope.push_root(Value::String(target_name))?;
    let target = scope.alloc_native_function(call_id, None, target_name, 0)?;
    scope.push_root(Value::Object(target))?;
    let handler = scope.alloc_object()?;
    scope.push_root(Value::Object(handler))?;
    let proxy = scope.alloc_proxy(Some(target), Some(handler))?;
    define_global(&mut scope, global, "callableProxy", Value::Object(proxy))?;

    // WeakMap / WeakSet objects: ordinary objects with the intrinsic prototype.
    let weak_map = scope.alloc_object()?;
    scope
      .heap_mut()
      .object_set_prototype(weak_map, Some(intr.weak_map_prototype()))?;
    define_global(&mut scope, global, "weakMap", Value::Object(weak_map))?;

    let weak_set = scope.alloc_object()?;
    scope
      .heap_mut()
      .object_set_prototype(weak_set, Some(intr.weak_set_prototype()))?;
    define_global(&mut scope, global, "weakSet", Value::Object(weak_set))?;
  }

  // Arrays.
  let out = rt.exec_script("Object.prototype.toString.call([1, 2, 3])")?;
  assert_eq!(expect_string(&rt, out), "[object Array]");

  // Callable Proxies.
  let out = rt.exec_script("Object.prototype.toString.call(callableProxy)")?;
  assert_eq!(expect_string(&rt, out), "[object Function]");

  // Weak collections via @@toStringTag on the prototype.
  let out = rt.exec_script("Object.prototype.toString.call(weakMap)")?;
  assert_eq!(expect_string(&rt, out), "[object WeakMap]");

  let out = rt.exec_script("Object.prototype.toString.call(weakSet)")?;
  assert_eq!(expect_string(&rt, out), "[object WeakSet]");

  Ok(())
}

