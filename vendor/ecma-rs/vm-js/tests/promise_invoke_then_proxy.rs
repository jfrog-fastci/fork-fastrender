use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn promise_invoke_then_proxy_get_trap_observed() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      let hit = 0;
      let p = new Proxy(
        { then() { return 1; } },
        {
          get(t, p) {
            if (p === "then") hit++;
            return t[p];
          }
        }
      );
      Promise.prototype.catch.call(p, () => {});
      hit === 1
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn promise_invoke_then_revoked_proxy_throws_type_error() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      let r = Proxy.revocable({ then() {} }, {});
      r.revoke();
      try {
        Promise.prototype.catch.call(r.proxy, () => {});
        false
      } catch (e) {
        e instanceof TypeError
      }
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

