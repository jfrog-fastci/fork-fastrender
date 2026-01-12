use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn weak_map_constructor_observes_proxy_get_traps_for_entry_values() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var log = [];

      // Each iterator value is an "entry" object. This entry is a Proxy so `Get(entry, "0")` /
      // `Get(entry, "1")` must observe the `get` trap.
      var entry = new Proxy({}, {
        get: function (t, k, r) {
          log.push(String(k));
          if (k === "0") return {};
          if (k === "1") return 123;
        },
      });

      var iterable = {};
      iterable[Symbol.iterator] = function () {
        var done = false;
        return {
          next: function () {
            if (done) return { done: true };
            done = true;
            return { done: false, value: entry };
          },
        };
      };

      var ok = true;
      try {
        new WeakMap(iterable);
      } catch (e) {
        ok = false;
      }

      ok && log.join(",") === "0,1";
    "#,
  )?;

  assert_eq!(value, Value::Bool(true));
  Ok(())
}

