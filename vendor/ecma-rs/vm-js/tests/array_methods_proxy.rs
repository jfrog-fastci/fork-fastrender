use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn array_map_observes_proxy_length_get_trap() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var hit = 0;
      var p = new Proxy([1, 2, 3], {
        get: function(t, prop, recv) {
          if (prop === "length") hit++;
          return t[prop];
        }
      });
      var out = Array.prototype.map.call(p, function(x) { return x * 2; });
      hit === 1 && out.length === 3 && out[0] === 2 && out[1] === 4 && out[2] === 6
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn array_push_pop_observe_proxy_traps() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var log = [];
      var target = [1, 2];
      var p = new Proxy(target, {
        get: function(t, prop, recv) {
          log.push("get:" + String(prop));
          return t[prop];
        },
        set: function(t, prop, value, recv) {
          log.push("set:" + String(prop));
          t[prop] = value;
          return true;
        },
        deleteProperty: function(t, prop) {
          log.push("delete:" + String(prop));
          return delete t[prop];
        }
      });

      var n = Array.prototype.push.call(p, 3);
      var v = Array.prototype.pop.call(p);

      n === 3 &&
      v === 3 &&
      target.length === 2 &&
      target[0] === 1 &&
      target[1] === 2 &&
      log.some(function(s) { return s === "get:length"; }) &&
      log.some(function(s) { return s === "set:length"; }) &&
      log.some(function(s) { return s === "delete:2"; })
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn array_methods_throw_on_revoked_proxy() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var r = Proxy.revocable([1, 2], {});
      r.revoke();
      try {
        Array.prototype.map.call(r.proxy, function(x) { return x; });
        false;
      } catch (e) {
        e instanceof TypeError;
      }
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}
