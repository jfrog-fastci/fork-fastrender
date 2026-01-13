use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

mod _async_generator_support;

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Async generators allocate Promises and queue multiple jobs; use a slightly larger heap than the
  // 1MiB default used by many unit tests to avoid spurious OOM failures.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn new_runtime_if_supported() -> Result<Option<JsRuntime>, VmError> {
  let mut rt = new_runtime();
  if !_async_generator_support::supports_async_generators(&mut rt)? {
    return Ok(None);
  }
  Ok(Some(rt))
}

fn value_to_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn async_generator_return_awaits_async_finally() -> Result<(), VmError> {
  let Some(mut rt) = new_runtime_if_supported()? else {
    return Ok(());
  };

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
      Err(err)
        if _async_generator_support::is_unimplemented_async_generator_error(&mut rt, &err)? =>
      {
        return Ok(());
      }
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
  let Some(mut rt) = new_runtime_if_supported()? else {
    return Ok(());
  };

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
      Err(err)
        if _async_generator_support::is_unimplemented_async_generator_error(&mut rt, &err)? =>
      {
        return Ok(());
      }
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
  let Some(mut rt) = new_runtime_if_supported()? else {
    return Ok(());
  };

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
      Err(err)
        if _async_generator_support::is_unimplemented_async_generator_error(&mut rt, &err)? =>
      {
        return Ok(());
      }
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
  let Some(mut rt) = new_runtime_if_supported()? else {
    return Ok(());
  };

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
      Err(err)
        if _async_generator_support::is_unimplemented_async_generator_error(&mut rt, &err)? =>
      {
        return Ok(());
      }
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
