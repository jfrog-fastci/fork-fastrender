use vm_js::{
  GcObject, Heap, HeapLimits, JsRuntime, PropertyDescriptor, PropertyKey, PropertyKind, Scope, Value,
  Vm, VmError, VmOptions,
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
  // Root inputs across string allocation and property definition.
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

#[test]
fn for_in_calls_proxy_ownkeys_and_gopd_traps() -> Result<(), VmError> {
  let mut rt = new_runtime();

  // Build `target`, `handler`, and `log` in JS so the trap bodies can be expressed naturally.
  rt.exec_script(
    r#"
    log = [];
    target = { a: 1 };
    handler = {
      ownKeys(t) {
        log.push("ownKeys");
        return ["a"];
      },
      getOwnPropertyDescriptor(t, k) {
        log.push("gopd:" + k);
        return { value: 1, writable: true, enumerable: true, configurable: true };
      }
    };
    "#,
  )?;

  let target = match rt.exec_script("target")? {
    Value::Object(o) => o,
    other => panic!("expected target object, got {other:?}"),
  };
  let handler = match rt.exec_script("handler")? {
    Value::Object(o) => o,
    other => panic!("expected handler object, got {other:?}"),
  };

  let global = rt.realm().global_object();
  {
    let (_vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_roots(&[Value::Object(target), Value::Object(handler)])?;
    let proxy = scope.alloc_proxy(Some(target), Some(handler))?;
    define_global(&mut scope, global, "p", Value::Object(proxy))?;
  }

  let value = rt.exec_script(
    r#"
    out = [];
    for (var k in p) out.push(k);
    log.join(",") + "|" + out.join(",")
    "#,
  )?;
  assert_eq!(expect_string(&rt, value), "ownKeys,gopd:a|a");
  Ok(())
}

#[test]
fn for_in_observes_proxy_getprototypeof_trap() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
    log = [];
    proto = { p: 1 };
    target = {};
    handler = {
      getPrototypeOf(t) {
        log.push("getPrototypeOf");
        return proto;
      }
    };
    "#,
  )?;

  let target = match rt.exec_script("target")? {
    Value::Object(o) => o,
    other => panic!("expected target object, got {other:?}"),
  };
  let handler = match rt.exec_script("handler")? {
    Value::Object(o) => o,
    other => panic!("expected handler object, got {other:?}"),
  };

  let global = rt.realm().global_object();
  {
    let (_vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_roots(&[Value::Object(target), Value::Object(handler)])?;
    let proxy = scope.alloc_proxy(Some(target), Some(handler))?;
    define_global(&mut scope, global, "p", Value::Object(proxy))?;
  }

  let value = rt.exec_script(
    r#"
    var s = "";
    for (var k in p) s += k;
    log.join(",") + "|" + s
    "#,
  )?;
  assert_eq!(expect_string(&rt, value), "getPrototypeOf|p");
  Ok(())
}

#[test]
fn for_in_throws_on_revoked_proxy() -> Result<(), VmError> {
  let mut rt = new_runtime();
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

  let value = rt.exec_script(r#"try { for (var k in p) {} "no"; } catch(e) { e.message }"#)?;
  let msg = expect_string(&rt, value);
  assert!(
    msg.contains("revoked"),
    "expected revoked-proxy message, got {msg}"
  );
  Ok(())
}

