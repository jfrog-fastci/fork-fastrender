use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn array_for_each_is_proxy_get_and_has_trap_aware() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var log = [];
      var out = [];

      var target = { length: 2, 0: "x", 1: "y" };
      var p = new Proxy(target, {
        has: function (t, k) {
          if (k === "0" || k === "1") log.push("has:" + k);
          return k in t;
        },
        get: function (t, k, r) {
          if (k === "length" || k === "0" || k === "1") log.push("get:" + String(k));
          return Reflect.get(t, k, r);
        },
      });

      var ok = true;
      try {
        Array.prototype.forEach.call(p, function (v) { out.push(v); });
      } catch (e) {
        ok = false;
      }

      ok
        && out.join("") === "xy"
        && log.join(",") === "get:length,has:0,get:0,has:1,get:1";
    "#,
  )?;

  assert_eq!(value, Value::Bool(true));
  Ok(())
}

