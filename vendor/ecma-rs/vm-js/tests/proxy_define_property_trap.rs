use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).expect("create runtime")
}

fn runtime_has_proxy(rt: &mut JsRuntime) -> Result<bool, VmError> {
  Ok(rt.exec_script("typeof Proxy === 'function'")? == Value::Bool(true))
}

#[test]
fn proxy_define_property_trap_is_invoked() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !runtime_has_proxy(&mut rt)? {
    return Ok(());
  }

  let v = rt.exec_script(
    r#"
      let called = 0;
      let target = {};
      let p = new Proxy(target, {
        defineProperty(t, prop, desc) {
          called++;
          return Reflect.defineProperty(t, prop, desc);
        }
      });

      Object.defineProperty(p, "x", { value: 1, configurable: true, writable: true });
      called === 1 && p.x === 1 && target.x === 1;
    "#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn proxy_define_property_forwards_when_trap_missing() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !runtime_has_proxy(&mut rt)? {
    return Ok(());
  }

  let v = rt.exec_script(
    r#"
      let target = {};
      let p = new Proxy(target, {});
      let ok = Reflect.defineProperty(p, "x", { value: 1, configurable: true, writable: true });
      ok === true && p.x === 1 && target.x === 1;
    "#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn proxy_define_property_revoked_proxy_throws() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !runtime_has_proxy(&mut rt)? {
    return Ok(());
  }

  let v = rt.exec_script(
    r#"
      let r = Proxy.revocable({}, {});
      r.revoke();
      try { Reflect.defineProperty(r.proxy, "x", { value: 1 }); false }
      catch (e) { e instanceof TypeError }
    "#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn proxy_define_property_invariant_violation_throws() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !runtime_has_proxy(&mut rt)? {
    return Ok(());
  }

  let v = rt.exec_script(
    r#"
      let ok1 = (function () {
        // The trap reports success while the target is non-extensible and the property does not exist.
        let target = {};
        Reflect.preventExtensions(target);
        let p = new Proxy(target, { defineProperty() { return true; } });
        try { Reflect.defineProperty(p, "x", { value: 1 }); return false }
        catch (e) { return e instanceof TypeError }
      })();

      let ok2 = (function () {
        // The trap reports success for `configurable: false` but does not actually create a
        // non-configurable property.
        let target = {};
        let p = new Proxy(target, {
          defineProperty(t, prop, desc) {
            // Lie: define a configurable property even if `desc` requests non-configurable.
            Object.defineProperty(t, prop, { value: 1, configurable: true, writable: true });
            return true;
          }
        });
        try { Reflect.defineProperty(p, "x", { value: 1, configurable: false }); return false }
        catch (e) { return e instanceof TypeError }
      })();

      ok1 && ok2;
    "#,
  )?;
  assert_eq!(v, Value::Bool(true));

  Ok(())
}

#[test]
fn proxy_define_property_proxy_chain_forwards_to_inner_trap() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !runtime_has_proxy(&mut rt)? {
    return Ok(());
  }

  let v = rt.exec_script(
    r#"
      let called = 0;
      let target = {};
      let inner = new Proxy(target, {
        defineProperty(t, prop, desc) {
          called++;
          return Reflect.defineProperty(t, prop, desc);
        }
      });
      let outer = new Proxy(inner, {}); // no defineProperty trap
      let ok = Reflect.defineProperty(outer, "x", { value: 1, configurable: true });
      ok === true && called === 1 && target.x === 1;
    "#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}
