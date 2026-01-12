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
fn proxy_set_prototype_of_trap_is_invoked() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !runtime_has_proxy(&mut rt)? {
    return Ok(());
  }

  let v = rt.exec_script(
    r#"
      let called = 0;
      let target = {};
      let newProto = {};
      let p = new Proxy(target, {
        setPrototypeOf(t, proto) {
          called++;
          return Reflect.setPrototypeOf(t, proto);
        }
      });

      let ok = Reflect.setPrototypeOf(p, newProto);
      ok === true && called === 1 && Object.getPrototypeOf(p) === newProto;
    "#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn proxy_set_prototype_of_forwards_when_trap_missing() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !runtime_has_proxy(&mut rt)? {
    return Ok(());
  }

  let v = rt.exec_script(
    r#"
      let target = {};
      let newProto = {};
      let p = new Proxy(target, {});
      Reflect.setPrototypeOf(p, newProto) === true && Object.getPrototypeOf(p) === newProto;
    "#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn proxy_set_prototype_of_revoked_proxy_throws() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !runtime_has_proxy(&mut rt)? {
    return Ok(());
  }

  let v = rt.exec_script(
    r#"
      let r = Proxy.revocable({}, {});
      r.revoke();
      try { Reflect.setPrototypeOf(r.proxy, {}); false }
      catch (e) { e instanceof TypeError }
    "#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn proxy_set_prototype_of_invariant_violation_throws() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !runtime_has_proxy(&mut rt)? {
    return Ok(());
  }

  // The trap reports success for a non-extensible target while the actual target prototype differs.
  let v = rt.exec_script(
    r#"
      let target = {};
      Reflect.preventExtensions(target);
      let p = new Proxy(target, { setPrototypeOf() { return true; } });
      try { Reflect.setPrototypeOf(p, {}); false }
      catch (e) { e instanceof TypeError }
    "#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn proxy_set_prototype_of_proxy_chain_forwards_to_inner_trap() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !runtime_has_proxy(&mut rt)? {
    return Ok(());
  }

  let v = rt.exec_script(
    r#"
      let called = 0;
      let target = {};
      let newProto = {};
      let inner = new Proxy(target, {
        setPrototypeOf(t, proto) {
          called++;
          return Reflect.setPrototypeOf(t, proto);
        }
      });
      let outer = new Proxy(inner, {}); // no setPrototypeOf trap

      let ok = Reflect.setPrototypeOf(outer, newProto);
      ok === true && called === 1 && Object.getPrototypeOf(target) === newProto;
    "#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

