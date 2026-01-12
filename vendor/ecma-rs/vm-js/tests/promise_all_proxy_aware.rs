use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn promise_all_observes_proxy_get_for_constructor_resolve_and_thenable_then() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var log = [];

      // A thenable whose `then` property is provided through a Proxy `get` trap.
      var thenable = new Proxy({
        // Non-callable `then`: `Promise.all` should still perform the `Get` before rejecting.
        then: 1,
      }, {
        get(target, prop, receiver) {
          if (prop === "then") log.push("then");
          return Reflect.get(target, prop, receiver);
        }
      });

      // A stable, reachable resolve function (avoid creating a new function per trap invocation so
      // it stays alive across GC).
      var myResolve = function (x) { return thenable; };

      // A Promise constructor Proxy whose `resolve` property comes from a Proxy `get` trap.
      var C = new Proxy(Promise, {
        get(target, prop, receiver) {
          if (prop === "resolve") { log.push("resolve"); return myResolve; }
          return Reflect.get(target, prop, receiver);
        }
      });

      var threw = false;
      try {
        Promise.all.call(C, [1]);
      } catch (e) {
        threw = true;
      }

      !threw && log.join(",") === "resolve,then";
    "#,
  )?;

  assert_eq!(value, Value::Bool(true));
  Ok(())
}
