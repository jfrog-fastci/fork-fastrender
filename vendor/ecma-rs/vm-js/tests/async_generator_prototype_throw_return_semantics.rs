use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

mod _async_generator_support;

// Async generator conformance tests allocate Promises and Promise jobs. Use a slightly larger heap
// to avoid spurious `VmError::OutOfMemory` failures as vm-js grows its builtin surface area.
const HEAP_BYTES: usize = 4 * 1024 * 1024;

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(HEAP_BYTES, HEAP_BYTES));
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
fn async_generator_throw_on_suspended_start_rejects_and_completes_without_executing_body(
) -> Result<(), VmError> {
  let Some(mut rt) = new_runtime_if_supported()? else {
    return Ok(());
  };

  let value = match rt.exec_script(
    r#"
      var log = "";
      async function* g() { log += "body"; yield 1; }
      var it = g();

      it.throw(42).then(
        function () { log += "bad"; },
        function (e) { log += "catch:" + e; }
      );

      log
    "#,
  ) {
    Ok(value) => value,
    Err(err)
      if _async_generator_support::is_unimplemented_async_generator_error(&mut rt, &err)? =>
    {
      return Ok(())
    }
    Err(err) => return Err(err),
  };
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let log = rt.exec_script("log")?;
  assert_eq!(value_to_string(&rt, log), "catch:42");

  rt.exec_script(
    r#"
      it.next().then(function (r) {
        log += "|done:" + r.done + ",value:" + r.value;
      });
    "#,
  )?;

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let log = rt.exec_script("log")?;
  assert_eq!(
    value_to_string(&rt, log),
    "catch:42|done:true,value:undefined"
  );
  assert!(
    rt.vm.microtask_queue().is_empty(),
    "expected microtask queue to be empty after checkpoint"
  );
  Ok(())
}

#[test]
fn async_generator_throw_on_completed_generator_rejects_with_argument() -> Result<(), VmError> {
  let Some(mut rt) = new_runtime_if_supported()? else {
    return Ok(());
  };

  let value = match rt.exec_script(
    r#"
      var out = -1;
      async function* g() { yield 1; }
      var it = g();

      it.next()
        .then(function () { return it.next(); }) // complete
        .then(function () { return it.throw(7); })
        .then(
          function () { out = 123; },
          function (e) { out = e; }
        );

      out
    "#,
  ) {
    Ok(value) => value,
    Err(err)
      if _async_generator_support::is_unimplemented_async_generator_error(&mut rt, &err)? =>
    {
      return Ok(())
    }
    Err(err) => return Err(err),
  };
  assert_eq!(value, Value::Number(-1.0));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(out, Value::Number(7.0));
  assert!(
    rt.vm.microtask_queue().is_empty(),
    "expected microtask queue to be empty after checkpoint"
  );
  Ok(())
}

#[test]
fn async_generator_throw_on_completed_generator_does_not_await_promise_argument(
) -> Result<(), VmError> {
  let Some(mut rt) = new_runtime_if_supported()? else {
    return Ok(());
  };

  let value = match rt.exec_script(
    r#"
      var log = "";
      async function* g() {}
      var it = g();
      var p = Promise.resolve("x");

      it.next()
        .then(function (r) {
          log += "done:" + r.done + ",value:" + r.value;
          return it.throw(p);
        })
        .then(
          function () { log += "|bad"; },
          function (e) { log += "|throw:" + (e === p); }
        );

      log
    "#,
  ) {
    Ok(value) => value,
    Err(err)
      if _async_generator_support::is_unimplemented_async_generator_error(&mut rt, &err)? =>
    {
      return Ok(())
    }
    Err(err) => return Err(err),
  };
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let log = rt.exec_script("log")?;
  assert_eq!(
    value_to_string(&rt, log),
    "done:true,value:undefined|throw:true"
  );

  rt.exec_script(
    r#"
      it.next().then(function (r) {
        log += "|next:" + r.done + ",value:" + r.value;
      });
    "#,
  )?;
  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let log = rt.exec_script("log")?;
  assert_eq!(
    value_to_string(&rt, log),
    "done:true,value:undefined|throw:true|next:true,value:undefined"
  );
  assert!(
    rt.vm.microtask_queue().is_empty(),
    "expected microtask queue to be empty after checkpoint"
  );
  Ok(())
}

#[test]
fn async_generator_throw_on_suspended_start_does_not_await_promise_argument() -> Result<(), VmError>
{
  let Some(mut rt) = new_runtime_if_supported()? else {
    return Ok(());
  };

  let value = match rt.exec_script(
    r#"
      var log = "";
      var out = 0;
      async function* g() { log += "body"; yield 1; }
      var it = g();
      var p = Promise.resolve("x");

      it.throw(p).then(
        function () { log += "bad"; },
        function (e) {
          log += "catch";
          out = (e === p) ? 1 : 2;
        }
      );

      log
    "#,
  ) {
    Ok(value) => value,
    Err(err)
      if _async_generator_support::is_unimplemented_async_generator_error(&mut rt, &err)? =>
    {
      return Ok(())
    }
    Err(err) => return Err(err),
  };
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let log = rt.exec_script("log")?;
  assert_eq!(value_to_string(&rt, log), "catch");
  let out = rt.exec_script("out")?;
  assert_eq!(out, Value::Number(1.0));

  rt.exec_script(
    r#"
      it.next().then(function (r) {
        log += "|done:" + r.done + ",value:" + r.value;
      });
    "#,
  )?;
  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let log = rt.exec_script("log")?;
  assert_eq!(value_to_string(&rt, log), "catch|done:true,value:undefined");
  assert!(
    rt.vm.microtask_queue().is_empty(),
    "expected microtask queue to be empty after checkpoint"
  );
  Ok(())
}

#[test]
fn async_generator_throw_can_be_caught_inside_generator() -> Result<(), VmError> {
  let Some(mut rt) = new_runtime_if_supported()? else {
    return Ok(());
  };

  let value = match rt.exec_script(
    r#"
      var log = "";
      async function* g() {
        try {
          yield 1;
        } catch (e) {
          yield e;
        }
        return 9;
      }

      var it = g();
      it.next()
        .then(function (r1) {
          log += r1.value + "," + r1.done;
          return it.throw(5);
        })
        .then(function (r2) {
          log += "|" + r2.value + "," + r2.done;
          return it.next();
        })
        .then(
          function (r3) { log += "|" + r3.value + "," + r3.done; },
          function (e) { log += "bad:" + e; }
        );
      log
    "#,
  ) {
    Ok(value) => value,
    Err(err)
      if _async_generator_support::is_unimplemented_async_generator_error(&mut rt, &err)? =>
    {
      return Ok(())
    }
    Err(err) => return Err(err),
  };
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let log = rt.exec_script("log")?;
  assert_eq!(value_to_string(&rt, log), "1,false|5,false|9,true");
  assert!(
    rt.vm.microtask_queue().is_empty(),
    "expected microtask queue to be empty after checkpoint"
  );
  Ok(())
}

#[test]
fn async_generator_return_on_suspended_start_resolves_and_awaits_argument_without_executing_body(
) -> Result<(), VmError> {
  let Some(mut rt) = new_runtime_if_supported()? else {
    return Ok(());
  };

  let value = match rt.exec_script(
    r#"
      var log = "";
      async function* g() { log += "body"; yield 1; }
      var it = g();

      it.return(Promise.resolve("x")).then(function (r) {
        log += r.value + ":" + r.done;
      });

      log
    "#,
  ) {
    Ok(value) => value,
    Err(err)
      if _async_generator_support::is_unimplemented_async_generator_error(&mut rt, &err)? =>
    {
      return Ok(())
    }
    Err(err) => return Err(err),
  };
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let log = rt.exec_script("log")?;
  assert_eq!(value_to_string(&rt, log), "x:true");
  assert!(
    rt.vm.microtask_queue().is_empty(),
    "expected microtask queue to be empty after checkpoint"
  );
  Ok(())
}

#[test]
fn async_generator_return_on_suspended_start_rejects_if_promise_resolve_throws(
) -> Result<(), VmError> {
  let Some(mut rt) = new_runtime_if_supported()? else {
    return Ok(());
  };

  let value = match rt.exec_script(
    r#"
      var log = "";
      var out = "";
      async function* g() { log += "body"; yield 1; }
      var it = g();

      var brokenPromise = Promise.resolve(42);
      Object.defineProperty(brokenPromise, "constructor", {
        get: function () { throw new Error("broken promise"); },
        configurable: true,
      });

      it.return(brokenPromise).then(
        function () { out = "resolved"; },
        function (e) { out = e.message; }
      );

      out
    "#,
  ) {
    Ok(value) => value,
    Err(err)
      if _async_generator_support::is_unimplemented_async_generator_error(&mut rt, &err)? =>
    {
      return Ok(())
    }
    Err(err) => return Err(err),
  };
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "broken promise");

  // Return on SuspendedStart must not execute the generator body, even on PromiseResolve errors.
  let log = rt.exec_script("log")?;
  assert_eq!(value_to_string(&rt, log), "");

  rt.exec_script(
    r#"
      it.next().then(function (r) {
        log += "|done:" + r.done + ",value:" + r.value;
      });
    "#,
  )?;
  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let log = rt.exec_script("log")?;
  assert_eq!(value_to_string(&rt, log), "|done:true,value:undefined");
  assert!(
    rt.vm.microtask_queue().is_empty(),
    "expected microtask queue to be empty after checkpoint"
  );
  Ok(())
}

#[test]
fn async_generator_return_on_completed_generator_resolves_to_done_true_with_awaited_argument(
) -> Result<(), VmError> {
  let Some(mut rt) = new_runtime_if_supported()? else {
    return Ok(());
  };

  let value = match rt.exec_script(
    r#"
      var out = "";
      async function* g() { yield 1; }
      var it = g();

      it.next()
        .then(function () { return it.next(); }) // complete
        .then(function () { return it.return(Promise.resolve("y")); })
        .then(
          function (r) { out = r.value + ":" + r.done; },
          function (e) { out = "bad:" + e; }
        );

      out
    "#,
  ) {
    Ok(value) => value,
    Err(err)
      if _async_generator_support::is_unimplemented_async_generator_error(&mut rt, &err)? =>
    {
      return Ok(())
    }
    Err(err) => return Err(err),
  };
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "y:true");
  assert!(
    rt.vm.microtask_queue().is_empty(),
    "expected microtask queue to be empty after checkpoint"
  );
  Ok(())
}

#[test]
fn async_generator_return_on_completed_generator_rejects_if_promise_resolve_throws(
) -> Result<(), VmError> {
  let Some(mut rt) = new_runtime_if_supported()? else {
    return Ok(());
  };

  let value = match rt.exec_script(
    r#"
      var out = "";
      var unblocked = false;
      var unblock;
      var blocking = new Promise(function (resolve) { unblock = resolve; });

      async function* g() { await blocking; unblocked = true; }
      var it = g();

      var brokenPromise = Promise.resolve(42);
      Object.defineProperty(brokenPromise, "constructor", {
        get: function () { throw new Error("broken promise"); },
        configurable: true,
      });

      it.next().then(function (r) {
        // Ensure generator has completed before calling `return`.
        if (r.done !== true) out = "bad:next-not-done";
        it.return(brokenPromise).then(
          function () { out = "bad:resolved"; },
          function (e) { out = unblocked + ":" + e.message; }
        );
      });

      unblock();
      out
    "#,
  ) {
    Ok(value) => value,
    Err(err)
      if _async_generator_support::is_unimplemented_async_generator_error(&mut rt, &err)? =>
    {
      return Ok(())
    }
    Err(err) => return Err(err),
  };
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "true:broken promise");
  assert!(
    rt.vm.microtask_queue().is_empty(),
    "expected microtask queue to be empty after checkpoint"
  );
  Ok(())
}

#[test]
fn async_generator_first_next_argument_is_ignored() -> Result<(), VmError> {
  let Some(mut rt) = new_runtime_if_supported()? else {
    return Ok(());
  };

  let value = match rt.exec_script(
    r#"
      var out = "";
      async function* g() { const x = yield 1; return x; }
      var it = g();

      it.next("ignored")
        .then(function (r1) {
          out += r1.value + ":" + r1.done;
          return it.next("sent");
        })
        .then(
          function (r2) { out += "|" + r2.value + ":" + r2.done; },
          function (e) { out = "bad:" + e; }
        );

      out
    "#,
  ) {
    Ok(value) => value,
    Err(err)
      if _async_generator_support::is_unimplemented_async_generator_error(&mut rt, &err)? =>
    {
      return Ok(())
    }
    Err(err) => return Err(err),
  };
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "1:false|sent:true");
  assert!(
    rt.vm.microtask_queue().is_empty(),
    "expected microtask queue to be empty after checkpoint"
  );
  Ok(())
}
