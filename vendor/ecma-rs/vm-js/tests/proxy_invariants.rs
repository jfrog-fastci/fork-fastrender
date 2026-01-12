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
fn proxy_get_invariant_non_writable_non_configurable() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !runtime_has_proxy(&mut rt)? {
    return Ok(());
  }

  let v = rt.exec_script(
    r#"
      let target = {};
      Object.defineProperty(target, "x", { value: 1, writable: false, configurable: false });
      let p = new Proxy(target, {
        get(t, prop, recv) {
          if (prop === "x") return 2;
          return Reflect.get(t, prop, recv);
        }
      });
      try { p.x; false } catch (e) { e instanceof TypeError }
    "#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn proxy_set_invariant_non_writable_non_configurable() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !runtime_has_proxy(&mut rt)? {
    return Ok(());
  }

  let v = rt.exec_script(
    r#"
      let target = {};
      Object.defineProperty(target, "x", { value: 1, writable: false, configurable: false });
      let p = new Proxy(target, { set() { return true; } });
      try { Reflect.set(p, "x", 2); false } catch (e) { e instanceof TypeError }
    "#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn proxy_set_invariant_non_extensible_new_property() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !runtime_has_proxy(&mut rt)? {
    return Ok(());
  }

  let v = rt.exec_script(
    r#"
      let target = {};
      Reflect.preventExtensions(target);
      let p = new Proxy(target, { set() { return true; } });
      try { Reflect.set(p, "newProp", 1); false } catch (e) { e instanceof TypeError }
    "#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn proxy_has_invariant_non_configurable_property() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !runtime_has_proxy(&mut rt)? {
    return Ok(());
  }

  let v = rt.exec_script(
    r#"
      let target = {};
      Object.defineProperty(target, "x", { value: 1, configurable: false });
      let p = new Proxy(target, { has() { return false; } });
      try { ("x" in p); false } catch (e) { e instanceof TypeError }
    "#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn proxy_delete_invariant_non_configurable_property() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !runtime_has_proxy(&mut rt)? {
    return Ok(());
  }

  let v = rt.exec_script(
    r#"
      let target = {};
      Object.defineProperty(target, "x", { value: 1, configurable: false });
      let p = new Proxy(target, { deleteProperty() { return true; } });
      try { Reflect.deleteProperty(p, "x"); false } catch (e) { e instanceof TypeError }
    "#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn proxy_own_keys_invariants() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !runtime_has_proxy(&mut rt)? {
    return Ok(());
  }

  let v = rt.exec_script(
    r#"
      let ok1 = (function () {
        // Must include non-configurable keys.
        let target = {};
        Object.defineProperty(target, "x", { value: 1, configurable: false });
        let p = new Proxy(target, { ownKeys() { return []; } });
        try { Reflect.ownKeys(p); return false } catch (e) { return e instanceof TypeError }
      })();

      let ok2 = (function () {
        // For non-extensible targets, must not report extra keys.
        let target = { a: 1 };
        Reflect.preventExtensions(target);
        let p = new Proxy(target, { ownKeys() { return ["a", "b"]; } });
        try { Reflect.ownKeys(p); return false } catch (e) { return e instanceof TypeError }
      })();

      let ok3 = (function () {
        // Must not contain duplicates.
        let target = { a: 1 };
        let p = new Proxy(target, { ownKeys() { return ["a", "a"]; } });
        try { Reflect.ownKeys(p); return false } catch (e) { return e instanceof TypeError }
      })();

      let ok4 = (function () {
        // Proxy chains: forwarding through an outer Proxy should still enforce inner invariants.
        let target = {};
        Object.defineProperty(target, "x", { value: 1, configurable: false });
        let inner = new Proxy(target, { ownKeys() { return []; } });
        let outer = new Proxy(inner, {});
        try { Reflect.ownKeys(outer); return false } catch (e) { return e instanceof TypeError }
      })();

      (ok1 ? 1 : 0) + (ok2 ? 2 : 0) + (ok3 ? 4 : 0) + (ok4 ? 8 : 0);
    "#,
  )?;
  assert_eq!(v, Value::Number(15.0));

  Ok(())
}
