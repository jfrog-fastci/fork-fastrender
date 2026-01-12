use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn promise_resolve_get_constructor_is_proxy_aware() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var log = [];

      var p = new Promise(() => {});
      delete Promise.prototype.constructor;

      var proxy = new Proxy({}, {
        get(target, prop, receiver) {
          if (prop === "constructor") log.push("constructor");
          return Promise;
        }
      });
      Object.setPrototypeOf(Promise.prototype, proxy);

      var ok1 = Promise.resolve(p) === p && log.length === 1 && log[0] === "constructor";

      var p2 = new Promise(() => {});
      var r = Proxy.revocable({}, {});
      Object.setPrototypeOf(Promise.prototype, r.proxy);
      r.revoke();

      var threw = false;
      try {
        Promise.resolve(p2);
      } catch (e) {
        threw = e && e.name === "TypeError";
      }

      ok1 && threw;
    "#,
  )?;

  assert_eq!(value, Value::Bool(true));
  Ok(())
}

