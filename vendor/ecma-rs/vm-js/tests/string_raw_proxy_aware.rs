use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn string_raw_observes_proxy_get_traps() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var log = [];
      var rawTarget = { length: 2, 0: "a", 1: "b" };
      var rawProxy = new Proxy(rawTarget, {
        get: function (t, k, r) {
          if (k === "length" || k === "0" || k === "1") log.push("raw.get:" + String(k));
          return Reflect.get(t, k, r);
        },
      });

      var callSiteTarget = { raw: rawProxy };
      var callSiteProxy = new Proxy(callSiteTarget, {
        get: function (t, k, r) {
          if (k === "raw") log.push("callSite.get:" + String(k));
          return Reflect.get(t, k, r);
        },
      });

      var result = String.raw(callSiteProxy, "X");
      result === "aXb" && log.join(",") === "callSite.get:raw,raw.get:length,raw.get:0,raw.get:1";
    "#,
  )?;

  assert_eq!(value, Value::Bool(true));
  Ok(())
}

