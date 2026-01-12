use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn await_promise_resolve_runs_constructor_getter_before_microtasks() -> Result<(), VmError> {
  let mut rt = new_runtime();

  // `await` uses `PromiseResolve(%Promise%, value)`, which must perform `Get(value, "constructor")`
  // even when `value` is already a Promise.
  let value = rt.exec_script(
    r#"
      var log = "";
      var p = Promise.resolve(1);
      Object.defineProperty(p, "constructor", { get() { log += "c"; return Promise; } });
      async function f() { await p; log += "a"; }
      f();
      log
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "c");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("log")?;
  assert_eq!(value_to_string(&rt, value), "ca");
  Ok(())
}

#[test]
fn await_promise_resolve_constructor_getter_throw_rejects_async_promise() -> Result<(), VmError> {
  let mut rt = new_runtime();

  // If `PromiseResolve` throws (e.g. user-defined `constructor` getter), the async function promise
  // must be rejected rather than the call throwing synchronously.
  let value = rt.exec_script(
    r#"
      var out = "";
      var sync = "no";
      var p = Promise.resolve(1);
      Object.defineProperty(p, "constructor", { get() { throw "boom"; } });
      var pr;
      try {
        pr = (async function () { await p; })();
      } catch (e) {
        sync = e;
      }
      if (pr !== undefined) {
        pr.then(function () { out = "fulfilled"; }, function (e) { out = e; });
      }
      sync
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "no");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "boom");
  Ok(())
}

