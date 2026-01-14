use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn computed_member_nullish_base_throws_before_property_key_conversion() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let v = rt.exec_script(
    r#"
      (function () {
        function check(base, op, label) {
          var called = false;
          var prop = {
            toString: function () {
              called = true;
              throw "property key evaluated";
            }
          };
          try {
            op(base, prop);
            return label + ": did not throw";
          } catch (e) {
            if (called !== false) return label + ": ToPropertyKey invoked";
            if (!(e && e.name === "TypeError")) return label + ": wrong error " + e;
            return true;
          }
        }

        function checkAll(base, label) {
          function get(base, prop) { base[prop]; }
          function del(base, prop) { delete base[prop]; }
          function call(base, prop) { base[prop](); }
          function assign(base, prop) { base[prop] = 1; }

          var r;
          r = check(base, get, label + ":get");
          if (r !== true) return r;
          r = check(base, del, label + ":delete");
          if (r !== true) return r;
          r = check(base, call, label + ":call");
          if (r !== true) return r;
          r = check(base, assign, label + ":assign");
          if (r !== true) return r;
          return true;
        }

        // Sloppy mode.
        var r;
        r = checkAll(null, "sloppy null");
        if (r !== true) return r;
        r = checkAll(undefined, "sloppy undefined");
        if (r !== true) return r;

        // Strict mode.
        r = (function () {
          "use strict";
          var r;
          r = checkAll(null, "strict null");
          if (r !== true) return r;
          r = checkAll(undefined, "strict undefined");
          if (r !== true) return r;
          return true;
        })();
        if (r !== true) return r;

        return true;
      })()
    "#,
  )?;
  if v != Value::Bool(true) {
    if let Value::String(s) = v {
      let msg = rt.heap().get_string(s)?.to_utf8_lossy();
      panic!("expected true, got failure: {msg}");
    }
    panic!("expected true, got {v:?}");
  }
  Ok(())
}
