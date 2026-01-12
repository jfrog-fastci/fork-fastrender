use vm_js::{
  get_prototype_from_constructor, GcObject, Heap, HeapLimits, JsRuntime, PropertyDescriptor,
  PropertyKey, PropertyKind, Scope, Value, Vm, VmError, VmOptions,
};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

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

#[test]
fn proxy_get_trap_observed_for_new_target_prototype_in_ecma_function_construct() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let global = rt.realm().global_object();

  // Create:
  // - an ECMAScript constructor `F`
  // - a proxy handler with a `"get"` trap that logs keys and overrides `.prototype`
  rt.exec_script(
    r#"
      var sawPrototype = false;
      function F() {}
      var proto = { marker: 1 };
      function getTrap(t, k, r) {
        if (k === "prototype") { sawPrototype = true; return proto; }
        return Reflect.get(t, k, r);
      }
      var handler = { get: getTrap };
    "#,
  )?;

  let target = rt.exec_script("F")?;
  let handler = rt.exec_script("handler")?;
  let Value::Object(target_obj) = target else {
    panic!("expected F to be an object, got {target:?}");
  };
  let Value::Object(handler_obj) = handler else {
    panic!("expected handler to be an object, got {handler:?}");
  };

  // Install proxy constructor as global `P`.
  {
    let (_vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let proxy = scope.alloc_proxy(Some(target_obj), Some(handler_obj))?;
    define_global(&mut scope, global, "P", Value::Object(proxy))?;
  }

  let value = rt.exec_script(
    r#"
      var o = new P();
      o.marker === 1 && sawPrototype
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn proxy_get_trap_observed_for_new_target_prototype_in_string_construct() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let global = rt.realm().global_object();

  rt.exec_script(
    r#"
      var sawPrototype = false;
      var proto = { marker: 2 };
      function getTrap(t, k, r) {
        if (k === "prototype") { sawPrototype = true; return proto; }
        return Reflect.get(t, k, r);
      }
      var handler = { get: getTrap };
    "#,
  )?;

  let target = rt.exec_script("String")?;
  let handler = rt.exec_script("handler")?;
  let Value::Object(target_obj) = target else {
    panic!("expected String to be an object, got {target:?}");
  };
  let Value::Object(handler_obj) = handler else {
    panic!("expected handler to be an object, got {handler:?}");
  };

  // Install proxy constructor as global `P`.
  {
    let (_vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let proxy = scope.alloc_proxy(Some(target_obj), Some(handler_obj))?;
    define_global(&mut scope, global, "P", Value::Object(proxy))?;
  }

  let value = rt.exec_script(
    r#"
      var o = new P("hi");
      o.marker === 2 && sawPrototype
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn get_prototype_from_constructor_throws_on_revoked_proxy() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut scope = heap.scope();

  let target = scope.alloc_object()?;
  let handler = scope.alloc_object()?;
  let proxy = scope.alloc_proxy(Some(target), Some(handler))?;
  scope.revoke_proxy(proxy)?;

  let default_proto = scope.alloc_object()?;

  let err =
    get_prototype_from_constructor(&mut vm, &mut scope, Value::Object(proxy), default_proto)
      .unwrap_err();
  match err {
    VmError::TypeError(msg) => {
      assert!(
        msg.contains("revoked"),
        "expected revoked-proxy TypeError, got {msg}"
      );
    }
    other => panic!("expected VmError::TypeError, got {other:?}"),
  }

  Ok(())
}

#[test]
fn proxy_get_trap_observed_for_error_cause_option() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let global = rt.realm().global_object();

  rt.exec_script(
    r#"
      var sawCause = false;
      var opts = { cause: 123 };
      function getTrap(t, k, r) {
        if (k === "cause") { sawCause = true; }
        return Reflect.get(t, k, r);
      }
      var handler = { get: getTrap };
    "#,
  )?;

  let opts = rt.exec_script("opts")?;
  let handler = rt.exec_script("handler")?;
  let Value::Object(opts_obj) = opts else {
    panic!("expected opts to be an object, got {opts:?}");
  };
  let Value::Object(handler_obj) = handler else {
    panic!("expected handler to be an object, got {handler:?}");
  };

  // Install proxy options object as global `optsProxy`.
  {
    let (_vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let proxy = scope.alloc_proxy(Some(opts_obj), Some(handler_obj))?;
    define_global(&mut scope, global, "optsProxy", Value::Object(proxy))?;
  }

  let value = rt.exec_script(
    r#"
      var e = new Error("msg", optsProxy);
      e.cause === 123 && sawCause
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn proxy_get_trap_observed_for_reflect_get() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let global = rt.realm().global_object();

  rt.exec_script(
    r#"
      var sawGet = false;
      var sawReceiver = false;
      var target = { x: 1 };
      function getTrap(t, k, r) {
        if (k === "x") { sawGet = true; sawReceiver = (r === P); return 42; }
        return Reflect.get(t, k, r);
      }
      var handler = { get: getTrap };
    "#,
  )?;

  let target = rt.exec_script("target")?;
  let handler = rt.exec_script("handler")?;
  let Value::Object(target_obj) = target else {
    panic!("expected target to be an object, got {target:?}");
  };
  let Value::Object(handler_obj) = handler else {
    panic!("expected handler to be an object, got {handler:?}");
  };

  // Install proxy as global `P`.
  {
    let (_vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let proxy = scope.alloc_proxy(Some(target_obj), Some(handler_obj))?;
    define_global(&mut scope, global, "P", Value::Object(proxy))?;
  }

  let value = rt.exec_script(
    r#"
      Reflect.get(P, "x") === 42 && sawGet && sawReceiver
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn proxy_set_trap_observed_for_reflect_set() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let global = rt.realm().global_object();

  rt.exec_script(
    r#"
      var sawSet = false;
      var sawReceiver = false;
      var target = {};
      function setTrap(t, k, v, r) {
        if (k === "x" && v === 5) { sawSet = true; sawReceiver = (r === P); return true; }
        return false;
      }
      var handler = { set: setTrap };
    "#,
  )?;

  let target = rt.exec_script("target")?;
  let handler = rt.exec_script("handler")?;
  let Value::Object(target_obj) = target else {
    panic!("expected target to be an object, got {target:?}");
  };
  let Value::Object(handler_obj) = handler else {
    panic!("expected handler to be an object, got {handler:?}");
  };

  // Install proxy as global `P`.
  {
    let (_vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let proxy = scope.alloc_proxy(Some(target_obj), Some(handler_obj))?;
    define_global(&mut scope, global, "P", Value::Object(proxy))?;
  }

  let value = rt.exec_script(
    r#"
      Reflect.set(P, "x", 5) === true && sawSet && sawReceiver
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn proxy_has_trap_observed_for_reflect_has() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let global = rt.realm().global_object();

  rt.exec_script(
    r#"
      var sawHas = false;
      var target = {};
      function hasTrap(t, k) {
        if (k === "x") { sawHas = true; return true; }
        return false;
      }
      var handler = { has: hasTrap };
    "#,
  )?;

  let target = rt.exec_script("target")?;
  let handler = rt.exec_script("handler")?;
  let Value::Object(target_obj) = target else {
    panic!("expected target to be an object, got {target:?}");
  };
  let Value::Object(handler_obj) = handler else {
    panic!("expected handler to be an object, got {handler:?}");
  };

  // Install proxy as global `P`.
  {
    let (_vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let proxy = scope.alloc_proxy(Some(target_obj), Some(handler_obj))?;
    define_global(&mut scope, global, "P", Value::Object(proxy))?;
  }

  let value = rt.exec_script(
    r#"
      Reflect.has(P, "x") === true && sawHas
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn proxy_delete_property_trap_observed_for_reflect_delete_property() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let global = rt.realm().global_object();

  rt.exec_script(
    r#"
      var sawDelete = false;
      var target = { x: 1 };
      function deleteTrap(t, k) {
        if (k === "x") { sawDelete = true; return true; }
        return false;
      }
      var handler = { deleteProperty: deleteTrap };
    "#,
  )?;

  let target = rt.exec_script("target")?;
  let handler = rt.exec_script("handler")?;
  let Value::Object(target_obj) = target else {
    panic!("expected target to be an object, got {target:?}");
  };
  let Value::Object(handler_obj) = handler else {
    panic!("expected handler to be an object, got {handler:?}");
  };

  // Install proxy as global `P`.
  {
    let (_vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let proxy = scope.alloc_proxy(Some(target_obj), Some(handler_obj))?;
    define_global(&mut scope, global, "P", Value::Object(proxy))?;
  }

  let value = rt.exec_script(
    r#"
      Reflect.deleteProperty(P, "x") === true && sawDelete
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn proxy_own_keys_trap_observed_for_reflect_own_keys() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let global = rt.realm().global_object();

  rt.exec_script(
    r#"
      var sawOwnKeys = false;
      var target = {};
      function ownKeysTrap(t) {
        sawOwnKeys = true;
        return ["a", "b"];
      }
      var handler = { ownKeys: ownKeysTrap };
    "#,
  )?;

  let target = rt.exec_script("target")?;
  let handler = rt.exec_script("handler")?;
  let Value::Object(target_obj) = target else {
    panic!("expected target to be an object, got {target:?}");
  };
  let Value::Object(handler_obj) = handler else {
    panic!("expected handler to be an object, got {handler:?}");
  };

  // Install proxy as global `P`.
  {
    let (_vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let proxy = scope.alloc_proxy(Some(target_obj), Some(handler_obj))?;
    define_global(&mut scope, global, "P", Value::Object(proxy))?;
  }

  let value = rt.exec_script(
    r#"
      var keys = Reflect.ownKeys(P);
      keys.length === 2 && keys[0] === "a" && keys[1] === "b" && sawOwnKeys
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn proxy_get_prototype_of_trap_observed_for_reflect_get_prototype_of() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let global = rt.realm().global_object();

  rt.exec_script(
    r#"
      var sawGetProto = false;
      var proto = { marker: 1 };
      var target = {};
      function getProtoTrap(t) {
        sawGetProto = true;
        return proto;
      }
      var handler = { getPrototypeOf: getProtoTrap };
    "#,
  )?;

  let target = rt.exec_script("target")?;
  let handler = rt.exec_script("handler")?;
  let Value::Object(target_obj) = target else {
    panic!("expected target to be an object, got {target:?}");
  };
  let Value::Object(handler_obj) = handler else {
    panic!("expected handler to be an object, got {handler:?}");
  };

  // Install proxy as global `P`.
  {
    let (_vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let proxy = scope.alloc_proxy(Some(target_obj), Some(handler_obj))?;
    define_global(&mut scope, global, "P", Value::Object(proxy))?;
  }

  let value = rt.exec_script(
    r#"
      Reflect.getPrototypeOf(P) === proto && sawGetProto
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}
