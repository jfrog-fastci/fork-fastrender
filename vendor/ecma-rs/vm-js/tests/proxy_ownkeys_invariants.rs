use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn proxy_own_keys_trap_duplicate_keys_throw_type_error() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var p = new Proxy({}, { ownKeys: function () { return ["a", "a"]; } });
      var ok = false;
      try { Reflect.ownKeys(p); } catch (e) { ok = e.name === "TypeError"; }
      ok
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn proxy_own_keys_trap_must_report_non_configurable_target_keys() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var target = {};
      Object.defineProperty(target, "a", { value: 1, configurable: false });
      var p = new Proxy(target, { ownKeys: function () { return []; } });
      var ok = false;
      try { Reflect.ownKeys(p); } catch (e) { ok = e.name === "TypeError"; }
      ok
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn proxy_own_keys_trap_cannot_report_extra_keys_for_non_extensible_target() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var target = { a: 1 };
      Object.preventExtensions(target);
      var p = new Proxy(target, { ownKeys: function () { return ["a", "b"]; } });
      var ok = false;
      try { Reflect.ownKeys(p); } catch (e) { ok = e.name === "TypeError"; }
      ok
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

