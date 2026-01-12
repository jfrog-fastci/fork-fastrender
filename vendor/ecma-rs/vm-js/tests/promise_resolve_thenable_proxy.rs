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
  // Root inputs across key allocation and property definition.
  scope.push_root(Value::Object(global))?;
  scope.push_root(value)?;
  let key_s = scope.alloc_string(name)?;
  scope.push_root(Value::String(key_s))?;
  let key = PropertyKey::from_string(key_s);
  scope.define_property(global, key, global_var_desc(value))
}

#[test]
fn promise_resolve_thenable_proxy_get_trap_is_observed_synchronously() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let global = rt.realm().global_object();

  // Set up the Proxy target/handler in JS so we can attach a `get` trap that updates `hit`.
  rt.exec_script(
    r#"
      globalThis.hit = 0;
      globalThis.target = {};
      globalThis.handler = {
        get: function (t, p, r) {
          if (p === "then") {
            hit++;
            return undefined;
          }
          return undefined;
        }
      };
    "#,
  )?;

  let Value::Object(target) = rt.exec_script("target")? else {
    return Err(VmError::Unimplemented("expected `target` to be an object"));
  };
  let Value::Object(handler) = rt.exec_script("handler")? else {
    return Err(VmError::Unimplemented("expected `handler` to be an object"));
  };

  {
    let mut scope = rt.heap.scope();
    let thenable = scope.alloc_proxy(Some(target), Some(handler))?;
    define_global(&mut scope, global, "thenable", Value::Object(thenable))?;
  }

  // `Promise.resolve` must synchronously perform `Get(thenable, "then")`, which must invoke the
  // Proxy's `get` trap.
  let value = rt.exec_script("Promise.resolve(thenable); hit")?;
  assert_eq!(value, Value::Number(1.0));
  Ok(())
}

#[test]
fn promise_resolve_thenable_revoked_proxy_rejects() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let global = rt.realm().global_object();

  rt.exec_script("globalThis.hit = 0;")?;

  {
    // Create a revoked Proxy (no [[ProxyTarget]] / [[ProxyHandler]]).
    let mut scope = rt.heap.scope();
    let target = scope.alloc_object()?;
    let handler = scope.alloc_object()?;
    let proxy = scope.alloc_proxy(Some(target), Some(handler))?;
    scope.revoke_proxy(proxy)?;
    define_global(&mut scope, global, "revoked", Value::Object(proxy))?;
  }

  rt.exec_script(
    r#"
      Promise.resolve(revoked).catch(function (e) {
        hit = e instanceof TypeError ? 1 : 0;
      });
    "#,
  )?;

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("hit")?;
  assert_eq!(value, Value::Number(1.0));
  Ok(())
}

