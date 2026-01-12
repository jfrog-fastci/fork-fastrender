use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).expect("create runtime")
}

fn runtime_has_proxy(rt: &mut JsRuntime) -> Result<bool, VmError> {
  Ok(rt.exec_script("typeof Proxy === 'function'")? == Value::Bool(true))
}

#[test]
fn promise_then_invokes_callable_proxy_handler() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !runtime_has_proxy(&mut rt)? {
    // Proxy support is not always enabled in the VM yet; skip until it is available.
    return Ok(());
  }

  // `then` enqueues a reaction job, so the handler should not run synchronously.
  let v = rt.exec_script(
    r#"
      var called = 0;
      var handler = new Proxy(function (x) { called = x; }, {});
      Promise.resolve(1).then(handler);
      called;
    "#,
  )?;
  assert_eq!(v, Value::Number(0.0));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let v = rt.exec_script("called")?;
  assert_eq!(v, Value::Number(1.0));
  Ok(())
}

#[test]
fn promise_resolve_thenable_calls_callable_proxy_then() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !runtime_has_proxy(&mut rt)? {
    // Proxy support is not always enabled in the VM yet; skip until it is available.
    return Ok(());
  }

  // `Promise.resolve(thenable)` should treat a callable Proxy-valued `then` as callable and enqueue
  // a thenable job that calls it.
  rt.exec_script(
    r#"
      var thenCalled = 0;
      var result = 0;

      var thenable = {
        then: new Proxy(function (resolve, reject) {
          thenCalled++;
          resolve(42);
        }, {})
      };

      Promise.resolve(thenable).then(function (v) {
        result = (typeof v === "number") ? v : -1;
      });
    "#,
  )?;

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let v = rt.exec_script("thenCalled")?;
  assert_eq!(v, Value::Number(1.0));

  let v = rt.exec_script("result")?;
  assert_eq!(v, Value::Number(42.0));

  Ok(())
}
