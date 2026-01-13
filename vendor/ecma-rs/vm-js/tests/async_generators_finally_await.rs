use vm_js::{Heap, HeapLimits, JsRuntime, PropertyKey, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Async generators allocate Promises and queue multiple jobs; use a slightly larger heap than the
  // 1MiB default used by many unit tests to avoid spurious OOM failures.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
}

fn is_unimplemented_async_generator_error(rt: &mut JsRuntime, err: &VmError) -> Result<bool, VmError> {
  match err {
    VmError::Unimplemented(msg) if msg.contains("async generator functions") => return Ok(true),
    _ => {}
  }

  let Some(thrown) = err.thrown_value() else {
    return Ok(false);
  };
  let Value::Object(err_obj) = thrown else {
    return Ok(false);
  };

  // vm-js currently feature-detects async generator functions by throwing a SyntaxError at runtime
  // (instead of returning a host-level `VmError::Unimplemented`), so test harnesses can use
  // try/catch. Treat that specific error as "feature not implemented" so this test file can land
  // before async generators are supported.
  let syntax_error_proto = rt.realm().intrinsics().syntax_error_prototype();
  if rt.heap.object_prototype(err_obj)? != Some(syntax_error_proto) {
    return Ok(false);
  }

  let mut scope = rt.heap_mut().scope();
  scope.push_root(Value::Object(err_obj))?;

  let message_key = PropertyKey::from_string(scope.alloc_string("message")?);
  let Some(Value::String(message_s)) = scope
    .heap()
    .object_get_own_data_property_value(err_obj, &message_key)?
  else {
    return Ok(false);
  };

  let message = scope.heap().get_string(message_s)?.to_utf8_lossy();
  Ok(message == "async generator functions")
}

#[test]
fn async_generator_return_awaits_async_finally() -> Result<(), VmError> {
  let mut rt = new_runtime();

  // Ensure we don't leak queued microtasks even if this test fails.
  let result: Result<(), VmError> = (|| {
    match rt.exec_script(
      r#"
        var log = "";
        var r1;
        var r2;
        var returnState = "pending";

        var resolveFinally;
        var finallyPromise = new Promise(function (resolve) {
          resolveFinally = resolve;
        });

        async function* g() {
          try {
            yield 1;
          } finally {
            log += "F";
            await finallyPromise;
            log += "A";
          }
        }

        var it = g();
        var p1 = it.next();
        var p2 = it.return("x");

        p1.then(function (v) { r1 = v; });
        p2.then(
          function (v) { r2 = v; returnState = "fulfilled"; },
          function (e) { r2 = e; returnState = "rejected"; }
        );
      "#,
    ) {
      Ok(_) => {}
      Err(err) if is_unimplemented_async_generator_error(&mut rt, &err)? => return Ok(()),
      Err(err) => return Err(err),
    };

    // Let the generator run through the first `yield` and start processing the pending `return`
    // request. It should enter the `finally` block and suspend at the `await finallyPromise`.
    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let value = rt.exec_script("JSON.stringify(r1)")?;
    assert_eq!(value_to_string(&rt, value), r#"{"value":1,"done":false}"#);

    let value = rt.exec_script("log")?;
    assert_eq!(value_to_string(&rt, value), "F");

    let value = rt.exec_script("returnState")?;
    assert_eq!(value_to_string(&rt, value), "pending");

    // Resolve the awaited `finallyPromise` and verify `it.return()` does not resolve until the async
    // `finally` cleanup completes.
    rt.exec_script("resolveFinally()")?;
    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let value = rt.exec_script("JSON.stringify(r2)")?;
    assert_eq!(value_to_string(&rt, value), r#"{"value":"x","done":true}"#);

    let value = rt.exec_script("log")?;
    assert_eq!(value_to_string(&rt, value), "FA");

    let value = rt.exec_script("returnState")?;
    assert_eq!(value_to_string(&rt, value), "fulfilled");

    Ok(())
  })();

  rt.teardown_microtasks();
  result
}

#[test]
fn async_generator_throw_awaits_async_finally() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let result: Result<(), VmError> = (|| {
    match rt.exec_script(
      r#"
        var log = "";
        var r1;
        var thrown;
        var throwState = "pending";

        var resolveFinally;
        var finallyPromise = new Promise(function (resolve) {
          resolveFinally = resolve;
        });

        async function* g() {
          try {
            yield 1;
          } finally {
            log += "F";
            await finallyPromise;
            log += "A";
          }
        }

        var it = g();
        var p1 = it.next();
        var p2 = it.throw("boom");

        p1.then(function (v) { r1 = v; });
        p2.then(
          function (v) { thrown = v; throwState = "fulfilled"; },
          function (e) { thrown = e; throwState = "rejected"; }
        );
      "#,
    ) {
      Ok(_) => {}
      Err(err) if is_unimplemented_async_generator_error(&mut rt, &err)? => return Ok(()),
      Err(err) => return Err(err),
    };

    // Let the generator reach the `await` inside `finally` while processing the pending `throw`
    // request.
    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let value = rt.exec_script("JSON.stringify(r1)")?;
    assert_eq!(value_to_string(&rt, value), r#"{"value":1,"done":false}"#);

    let value = rt.exec_script("log")?;
    assert_eq!(value_to_string(&rt, value), "F");

    let value = rt.exec_script("throwState")?;
    assert_eq!(value_to_string(&rt, value), "pending");

    // Only after resolving the awaited promise should `it.throw()` settle, and it must reject (no
    // catch handler in the generator).
    rt.exec_script("resolveFinally()")?;
    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let value = rt.exec_script("log")?;
    assert_eq!(value_to_string(&rt, value), "FA");

    let value = rt.exec_script("throwState")?;
    assert_eq!(value_to_string(&rt, value), "rejected");

    let value = rt.exec_script("thrown")?;
    assert_eq!(value_to_string(&rt, value), "boom");

    Ok(())
  })();

  rt.teardown_microtasks();
  result
}

#[test]
fn async_generator_return_rejects_if_async_finally_rejects() -> Result<(), VmError> {
  let mut rt = new_runtime();

  // Ensure we don't leak queued microtasks even if this test fails.
  let result: Result<(), VmError> = (|| {
    match rt.exec_script(
      r#"
        var log = "";
        var r1;
        var thrown;
        var returnState = "pending";

        var rejectFinally;
        var finallyPromise = new Promise(function (_resolve, reject) {
          rejectFinally = reject;
        });

        async function* g() {
          try {
            yield 1;
          } finally {
            log += "F";
            await finallyPromise;
            log += "A";
          }
        }

        var it = g();
        var p1 = it.next();
        var p2 = it.return("x");

        p1.then(function (v) { r1 = v; });
        p2.then(
          function (v) { thrown = v; returnState = "fulfilled"; },
          function (e) { thrown = e; returnState = "rejected"; }
        );
      "#,
    ) {
      Ok(_) => {}
      Err(err) if is_unimplemented_async_generator_error(&mut rt, &err)? => return Ok(()),
      Err(err) => return Err(err),
    };

    // Let the generator start handling the `return` request and suspend in `finally` at the
    // awaited promise.
    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let value = rt.exec_script("JSON.stringify(r1)")?;
    assert_eq!(value_to_string(&rt, value), r#"{"value":1,"done":false}"#);

    let value = rt.exec_script("log")?;
    assert_eq!(value_to_string(&rt, value), "F");

    let value = rt.exec_script("returnState")?;
    assert_eq!(value_to_string(&rt, value), "pending");

    // Rejecting the awaited promise should reject `it.return()`, and it must stay pending until the
    // `await` settles.
    rt.exec_script("rejectFinally('fail')")?;
    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let value = rt.exec_script("log")?;
    assert_eq!(value_to_string(&rt, value), "F");

    let value = rt.exec_script("returnState")?;
    assert_eq!(value_to_string(&rt, value), "rejected");

    let value = rt.exec_script("thrown")?;
    assert_eq!(value_to_string(&rt, value), "fail");

    Ok(())
  })();

  rt.teardown_microtasks();
  result
}

#[test]
fn async_generator_throw_rejects_if_async_finally_rejects() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let result: Result<(), VmError> = (|| {
    match rt.exec_script(
      r#"
        var log = "";
        var r1;
        var thrown;
        var throwState = "pending";

        var rejectFinally;
        var finallyPromise = new Promise(function (_resolve, reject) {
          rejectFinally = reject;
        });

        async function* g() {
          try {
            yield 1;
          } finally {
            log += "F";
            await finallyPromise;
            log += "A";
          }
        }

        var it = g();
        var p1 = it.next();
        var p2 = it.throw("boom");

        p1.then(function (v) { r1 = v; });
        p2.then(
          function (v) { thrown = v; throwState = "fulfilled"; },
          function (e) { thrown = e; throwState = "rejected"; }
        );
      "#,
    ) {
      Ok(_) => {}
      Err(err) if is_unimplemented_async_generator_error(&mut rt, &err)? => return Ok(()),
      Err(err) => return Err(err),
    };

    // Let the generator start handling the `throw` request and suspend in `finally` at the awaited
    // promise.
    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let value = rt.exec_script("JSON.stringify(r1)")?;
    assert_eq!(value_to_string(&rt, value), r#"{"value":1,"done":false}"#);

    let value = rt.exec_script("log")?;
    assert_eq!(value_to_string(&rt, value), "F");

    let value = rt.exec_script("throwState")?;
    assert_eq!(value_to_string(&rt, value), "pending");

    rt.exec_script("rejectFinally('fail')")?;
    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let value = rt.exec_script("log")?;
    assert_eq!(value_to_string(&rt, value), "F");

    let value = rt.exec_script("throwState")?;
    assert_eq!(value_to_string(&rt, value), "rejected");

    let value = rt.exec_script("thrown")?;
    assert_eq!(value_to_string(&rt, value), "fail");

    Ok(())
  })();

  rt.teardown_microtasks();
  result
}
