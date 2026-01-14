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

    // Revoked callable Proxy: `Object.prototype.toString` must throw on `Get(O, @@toStringTag)`.
    let revoked_handler = scope.alloc_object()?;
    scope.push_root(Value::Object(revoked_handler))?;
    let revoked_proxy = scope.alloc_proxy(Some(target), Some(revoked_handler))?;
    scope.revoke_proxy(revoked_proxy)?;
    define_global(
      &mut scope,
      global,
      "revokedCallableProxy",
      Value::Object(revoked_proxy),
    )?;

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

  // Arguments objects.
  let out = rt.exec_script("Object.prototype.toString.call((function(){ return arguments; })(1, 2))")?;
  assert_eq!(expect_string(&rt, out), "[object Arguments]");

  // Iterators.
  let out = rt.exec_script("Object.prototype.toString.call([1, 2].values())")?;
  assert_eq!(expect_string(&rt, out), "[object Array Iterator]");

  let out = rt.exec_script(r#"Object.prototype.toString.call("x"[Symbol.iterator]())"#)?;
  assert_eq!(expect_string(&rt, out), "[object String Iterator]");

  // Generators.
  let out = rt.exec_script("Object.prototype.toString.call(function*(){})")?;
  assert_eq!(expect_string(&rt, out), "[object GeneratorFunction]");

  let out = rt.exec_script("Object.prototype.toString.call((function*(){yield 1;})())")?;
  assert_eq!(expect_string(&rt, out), "[object Generator]");

  // ArrayBuffer / Uint8Array.
  let out = rt.exec_script("Object.prototype.toString.call(new ArrayBuffer(0))")?;
  assert_eq!(expect_string(&rt, out), "[object ArrayBuffer]");

  let out = rt.exec_script("Object.prototype.toString.call(new Uint8Array(0))")?;
  assert_eq!(expect_string(&rt, out), "[object Uint8Array]");

  let out = rt.exec_script("Object.prototype.toString.call(new Int8Array(0))")?;
  assert_eq!(expect_string(&rt, out), "[object Int8Array]");

  let out = rt.exec_script("Object.prototype.toString.call(new Float32Array(0))")?;
  assert_eq!(expect_string(&rt, out), "[object Float32Array]");

  let out = rt.exec_script("Object.prototype.toString.call(new DataView(new ArrayBuffer(0)))")?;
  assert_eq!(expect_string(&rt, out), "[object DataView]");

  // Wrapper objects.
  let out = rt.exec_script("Object.prototype.toString.call(Object(\"x\"))")?;
  assert_eq!(expect_string(&rt, out), "[object String]");

  let out = rt.exec_script("Object.prototype.toString.call(Object(1))")?;
  assert_eq!(expect_string(&rt, out), "[object Number]");

  let out = rt.exec_script("Object.prototype.toString.call(Object(true))")?;
  assert_eq!(expect_string(&rt, out), "[object Boolean]");

  let out = rt.exec_script("Object.prototype.toString.call(Object(1n))")?;
  assert_eq!(expect_string(&rt, out), "[object BigInt]");

  // Date.
  let out = rt.exec_script("Object.prototype.toString.call(new Date(0))")?;
  assert_eq!(expect_string(&rt, out), "[object Date]");

  // RegExp.
  let out = rt.exec_script("Object.prototype.toString.call(new RegExp(\"a\"))")?;
  assert_eq!(expect_string(&rt, out), "[object RegExp]");

  // Promise.
  let out = rt.exec_script("Object.prototype.toString.call(Promise.resolve(1))")?;
  assert_eq!(expect_string(&rt, out), "[object Promise]");

  // Proxied Promise should tag as Promise via Promise.prototype[@@toStringTag].
  let out = rt.exec_script("Object.prototype.toString.call(new Proxy(Promise.resolve(1), {}))")?;
  assert_eq!(expect_string(&rt, out), "[object Promise]");

  // `%Math%` / `%Reflect%`.
  let out = rt.exec_script("Object.prototype.toString.call(Math)")?;
  assert_eq!(expect_string(&rt, out), "[object Math]");

  let out = rt.exec_script("Object.prototype.toString.call(Reflect)")?;
  assert_eq!(expect_string(&rt, out), "[object Reflect]");

  // Errors.
  let out = rt.exec_script("Object.prototype.toString.call(new Error(\"x\"))")?;
  assert_eq!(expect_string(&rt, out), "[object Error]");

  let out = rt.exec_script("Object.prototype.toString.call(new TypeError(\"x\"))")?;
  assert_eq!(expect_string(&rt, out), "[object Error]");

  let out = rt.exec_script("Object.prototype.toString.call(new Proxy(new TypeError(\"x\"), {}))")?;
  assert_eq!(expect_string(&rt, out), "[object Error]");

  // Callable Proxies.
  let out = rt.exec_script("Object.prototype.toString.call(callableProxy)")?;
  assert_eq!(expect_string(&rt, out), "[object Function]");

  // Weak collections via @@toStringTag on the prototype.
  let out = rt.exec_script("Object.prototype.toString.call(weakMap)")?;
  assert_eq!(expect_string(&rt, out), "[object WeakMap]");

  let out = rt.exec_script("Object.prototype.toString.call(weakSet)")?;
  assert_eq!(expect_string(&rt, out), "[object WeakSet]");

  // Iterator objects via @@toStringTag on their intrinsic iterator prototypes.
  let out = rt.exec_script(r#"Object.prototype.toString.call([].values())"#)?;
  assert_eq!(expect_string(&rt, out), "[object Array Iterator]");

  let out = rt.exec_script(r#"Object.prototype.toString.call(""[Symbol.iterator]())"#)?;
  assert_eq!(expect_string(&rt, out), "[object String Iterator]");

  // Revoked Proxies must throw when `Object.prototype.toString` performs `Get(O, @@toStringTag)`.
  let out = rt.exec_script(
    r#"try { Object.prototype.toString.call(revokedCallableProxy) } catch (e) { e.message }"#,
  )?;
  let msg = expect_string(&rt, out);
  assert!(
    msg.contains("revoked"),
    "expected revoked-proxy message, got {msg}"
  );

  // --- builtinTag fallbacks ---
  //
  // Many built-in objects define `@@toStringTag`, but `Object.prototype.toString` has additional
  // legacy fallback logic for certain internal-slot-bearing objects.

  // Errors fall back to "Error" via [[ErrorData]] even when `@@toStringTag` is removed.
  let out = rt.exec_script(
    r#"delete Error.prototype[Symbol.toStringTag]; Object.prototype.toString.call(new Error("x"))"#,
  )?;
  assert_eq!(expect_string(&rt, out), "[object Error]");

  // RegExp falls back to "RegExp" via [[RegExpMatcher]] even when `@@toStringTag` is removed.
  let out =
    rt.exec_script("delete RegExp.prototype[Symbol.toStringTag]; Object.prototype.toString.call(new RegExp(\"a\"))")?;
  assert_eq!(expect_string(&rt, out), "[object RegExp]");

  // Promise is not part of the legacy builtinTag table; removing `@@toStringTag` falls back to
  // "Object".
  let out = rt.exec_script(
    "delete Promise.prototype[Symbol.toStringTag]; Object.prototype.toString.call(Promise.resolve(1))",
  )?;
  assert_eq!(expect_string(&rt, out), "[object Object]");

  // Typed arrays are not part of the legacy builtinTag table; removing `@@toStringTag` falls back
  // to "Object". The `@@toStringTag` getter lives on `%TypedArray%.prototype`, not on the
  // concrete typed array prototypes.
  let out = rt.exec_script(
    "delete Object.getPrototypeOf(Uint8Array.prototype)[Symbol.toStringTag]; Object.prototype.toString.call(new Uint8Array(0))",
  )?;
  assert_eq!(expect_string(&rt, out), "[object Object]");

  Ok(())
}
