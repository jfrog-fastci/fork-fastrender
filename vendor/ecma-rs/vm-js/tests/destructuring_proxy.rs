use vm_js::{
  GcObject, Heap, HeapLimits, JsRuntime, PropertyDescriptor, PropertyKey, PropertyKind, Scope, Value, Vm,
  VmError, VmOptions,
};

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
  scope.define_property(global, key, data_desc(value))
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
fn object_destructuring_observes_proxy_get_trap() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;
  let global = rt.realm().global_object();

  let get_trap = rt.exec_script(
    r#"
      globalThis.getA = 0;
      (function (target, prop, receiver) {
        if (prop === "a") globalThis.getA++;
        return target[prop];
      })
    "#,
  )?;
  let target = rt.exec_script("({ a: 1 })")?;

  {
    let (_vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(get_trap)?;
    scope.push_root(target)?;

    let Value::Object(target_obj) = target else {
      panic!("expected target object, got {target:?}");
    };

    let handler = scope.alloc_object()?;
    scope.push_root(Value::Object(handler))?;

    let get_key_s = scope.alloc_string("get")?;
    scope.push_root(Value::String(get_key_s))?;
    let get_key = PropertyKey::from_string(get_key_s);
    scope.define_property(handler, get_key, data_desc(get_trap))?;

    let proxy = scope.alloc_proxy(Some(target_obj), Some(handler))?;
    define_global(&mut scope, global, "proxyObj", Value::Object(proxy))?;
  }

  let ok = rt.exec_script(r#"var {a} = proxyObj; a === 1 && getA === 1"#)?;
  assert_eq!(ok, Value::Bool(true));
  Ok(())
}

#[test]
fn object_destructuring_rest_observes_proxy_traps() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;
  let global = rt.realm().global_object();

  let get_trap = rt.exec_script(
    r#"
      globalThis.getA = 0;
      globalThis.getB = 0;
      (function (target, prop, receiver) {
        if (prop === "a") globalThis.getA++;
        if (prop === "b") globalThis.getB++;
        return target[prop];
      })
    "#,
  )?;
  let own_keys_trap = rt.exec_script(
    r#"
      globalThis.ownKeysCount = 0;
      (function (target) {
        globalThis.ownKeysCount++;
        return ["a", "b"];
      })
    "#,
  )?;
  let gopd_trap = rt.exec_script(
    r#"
      globalThis.gopdB = 0;
      (function (target, prop) {
        if (prop === "b") globalThis.gopdB++;
        return Object.getOwnPropertyDescriptor(target, prop);
      })
    "#,
  )?;
  let target = rt.exec_script("({ a: 1, b: 2 })")?;

  {
    let (_vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_roots(&[get_trap, own_keys_trap, gopd_trap, target])?;

    let Value::Object(target_obj) = target else {
      panic!("expected target object, got {target:?}");
    };

    let handler = scope.alloc_object()?;
    scope.push_root(Value::Object(handler))?;

    for (name, func) in [
      ("get", get_trap),
      ("ownKeys", own_keys_trap),
      ("getOwnPropertyDescriptor", gopd_trap),
    ] {
      let key_s = scope.alloc_string(name)?;
      scope.push_root(Value::String(key_s))?;
      let key = PropertyKey::from_string(key_s);
      scope.define_property(handler, key, data_desc(func))?;
    }

    let proxy = scope.alloc_proxy(Some(target_obj), Some(handler))?;
    define_global(&mut scope, global, "proxyRestObj", Value::Object(proxy))?;
  }

  let ok = rt.exec_script(
    r#"
      var {a, ...r} = proxyRestObj;
      a === 1 &&
        r.b === 2 &&
        r.a === undefined &&
        ownKeysCount === 1 &&
        gopdB === 1 &&
        getA === 1 &&
        getB === 1
    "#,
  )?;
  assert_eq!(ok, Value::Bool(true));
  Ok(())
}

#[test]
fn object_destructuring_revoked_proxy_throws_type_error() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;
  let global = rt.realm().global_object();

  let target = rt.exec_script("({ a: 1 })")?;
  {
    let (_vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(target)?;

    let Value::Object(target_obj) = target else {
      panic!("expected target object, got {target:?}");
    };
    let handler = scope.alloc_object()?;
    scope.push_root(Value::Object(handler))?;

    let proxy = scope.alloc_proxy(Some(target_obj), Some(handler))?;
    scope.revoke_proxy(proxy)?;
    define_global(&mut scope, global, "revokedObj", Value::Object(proxy))?;
  }

  let msg = rt.exec_script(
    r#"
      try {
        var {a} = revokedObj;
        "no error"
      } catch (e) {
        e.message
      }
    "#,
  )?;
  let msg = expect_string(&rt, msg);
  assert!(
    msg.contains("revoked"),
    "expected revoked-proxy error message, got {msg}"
  );
  Ok(())
}

