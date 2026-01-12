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
fn proxy_is_extensible_trap_is_invoked() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !runtime_has_proxy(&mut rt)? {
    return Ok(());
  }

  let v = rt.exec_script(
    r#"
      let called = 0;
      let target = {};
      let p = new Proxy(target, {
        isExtensible(t) {
          called++;
          return Reflect.isExtensible(t);
        }
      });
      Reflect.isExtensible(p) === true && called === 1;
    "#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn proxy_is_extensible_invariant_mismatch_throws() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !runtime_has_proxy(&mut rt)? {
    return Ok(());
  }

  let v = rt.exec_script(
    r#"
      let ok1 = (function () {
        let target = {};
        let p = new Proxy(target, { isExtensible() { return false; } });
        try { Reflect.isExtensible(p); return false } catch (e) { return e instanceof TypeError }
      })();

      let ok2 = (function () {
        let target = {};
        Reflect.preventExtensions(target);
        let p = new Proxy(target, { isExtensible() { return true; } });
        try { Reflect.isExtensible(p); return false } catch (e) { return e instanceof TypeError }
      })();

      ok1 && ok2;
    "#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn proxy_is_extensible_proxy_chain_forwards_to_inner_trap() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !runtime_has_proxy(&mut rt)? {
    return Ok(());
  }

  let v = rt.exec_script(
    r#"
      let called = 0;
      let target = {};
      let inner = new Proxy(target, {
        isExtensible(t) {
          called++;
          return Reflect.isExtensible(t);
        }
      });
      let outer = new Proxy(inner, {}); // no isExtensible trap
      Reflect.isExtensible(outer) === true && called === 1;
    "#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn proxy_prevent_extensions_trap_is_invoked() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !runtime_has_proxy(&mut rt)? {
    return Ok(());
  }

  let v = rt.exec_script(
    r#"
      let called = 0;
      let target = {};
      let p = new Proxy(target, {
        preventExtensions(t) {
          called++;
          return Reflect.preventExtensions(t);
        }
      });
      Reflect.preventExtensions(p) === true && called === 1 && Reflect.isExtensible(target) === false;
    "#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn proxy_prevent_extensions_invariant_violation_throws() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !runtime_has_proxy(&mut rt)? {
    return Ok(());
  }

  // The trap reports success without actually preventing extensions on the target.
  let v = rt.exec_script(
    r#"
      let target = {};
      let p = new Proxy(target, { preventExtensions() { return true; } });
      try { Reflect.preventExtensions(p); false } catch (e) { e instanceof TypeError }
    "#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn proxy_is_extensible_and_prevent_extensions_revoked_proxy_throws() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !runtime_has_proxy(&mut rt)? {
    return Ok(());
  }

  let v = rt.exec_script(
    r#"
      let r = Proxy.revocable({}, {});
      r.revoke();
      let ok1 = (function () {
        try { Reflect.isExtensible(r.proxy); return false } catch (e) { return e instanceof TypeError }
      })();
      let ok2 = (function () {
        try { Reflect.preventExtensions(r.proxy); return false } catch (e) { return e instanceof TypeError }
      })();
      ok1 && ok2;
    "#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}
