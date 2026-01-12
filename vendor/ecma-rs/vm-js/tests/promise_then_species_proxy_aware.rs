use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn promise_then_species_constructor_observes_proxy_get_trap() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var log = [];
      var ctor = new Proxy(function(){}, {
        get: function (t, k, r) {
          log.push(String(k));
          if (k === Symbol.species) return undefined;
          return Reflect.get(t, k, r);
        },
      });

      // Use a pending Promise so `.then(..)` does not enqueue a microtask job (which would require
      // draining the microtask queue in this test harness).
      var p = new Promise(function () {});
      p.constructor = ctor;

      var ok = true;
      try {
        p.then(function (x) { return x; });
      } catch (e) {
        ok = false;
      }

      ok && log.join(",").indexOf("Symbol.species") !== -1;
    "#,
  )?;

  assert_eq!(value, Value::Bool(true));
  Ok(())
}
