use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn get_trap_in_prototype_chain_is_observed() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var log = [];
      var proto = new Proxy(
        { x: 1 },
        { get: function(t, k, r) { log.push('get:' + k); return Reflect.get(t, k, r); } }
      );
      var obj = Object.create(proto);
      var v = obj.x;
      v === 1 && log.join(',') === 'get:x'
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn has_trap_in_prototype_chain_is_observed() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var log = [];
      var proto = new Proxy(
        { x: 1 },
        { has: function(t, k) { log.push('has:' + k); return Reflect.has(t, k); } }
      );
      var obj = Object.create(proto);
      ('x' in obj) && log.join(',') === 'has:x'
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn set_trap_in_prototype_chain_is_observed() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var log = [];
      var proto = new Proxy(
        {},
        { set: function(t, k, v, r) { log.push('set:' + k); t[k] = v; return true; } }
      );
      var obj = Object.create(proto);
      obj.x = 3;
      log.join(',') === 'set:x'
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn revoked_proxy_in_prototype_chain_throws_for_get_has_set() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var r = Proxy.revocable({ x: 1 }, {});
      var obj = Object.create(r.proxy);
      r.revoke();

      var ok = true;
      try { obj.x; ok = false; } catch (e) {}
      try { 'x' in obj; ok = false; } catch (e) {}
      try { obj.x = 1; ok = false; } catch (e) {}
      ok
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

