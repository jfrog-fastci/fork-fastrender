use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn array_from_proxy_array() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"var a = Array.from(new Proxy([1,2], {})); a.length===2 && a[0]===1 && a[1]===2"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_iterator_proxy_get_trap_observed() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var len = 0;
      var idx = 0;
      var p = new Proxy([1,2], {
        get: function (t, prop, receiver) {
          if (prop === "length") len++;
          if (prop === "0" || prop === "1") idx++;
          return Reflect.get(t, prop, receiver);
        }
      });

      Array.from(p);
      len > 0 && idx >= 2
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn revoked_proxy_throws_during_iteration() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var r = Proxy.revocable([1], {});
      r.revoke();

      var ok = false;
      try {
        for (var x of r.proxy) {}
      } catch (e) {
        ok = e instanceof TypeError;
      }
      ok
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

