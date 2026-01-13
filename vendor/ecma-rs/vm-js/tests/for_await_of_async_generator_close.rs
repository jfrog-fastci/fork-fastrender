use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // `for await...of` over async generator objects exercises async iteration + Promise/job queuing.
  // With ongoing vm-js builtin growth, a 1MiB heap can be too tight and cause spurious
  // `VmError::OutOfMemory` failures that are not relevant to the semantics being tested here.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
}

fn async_generators_supported(rt: &mut JsRuntime) -> Result<bool, VmError> {
  // vm-js historically parsed `async function*` but deliberately rejected it at runtime (via a
  // throwable SyntaxError) while async generator semantics were unimplemented. These tests should
  // start running automatically once that support lands.
  let value = match rt.exec_script(
    r#"
      try {
        var f = (async function* () { yield 1; });
        // Call `.next()` to ensure async generator execution is implemented, not just syntax.
        f().next();
        true;
      } catch (e) {
        // Only treat the known feature-detection SyntaxError as "unsupported". Any other exception
        // should fail the test so we don't accidentally mask bugs once async generators exist.
        if (e && e.name === "SyntaxError" && String(e.message).includes("async generator functions")) {
          false;
        } else {
          throw e;
        }
      }
    "#,
  ) {
    Ok(v) => v,
    Err(VmError::Unimplemented(msg)) if msg.contains("async generator functions") => {
      return Ok(false);
    }
    Err(err) => return Err(err),
  };
  let supported = value == Value::Bool(true);
  if supported {
    rt.teardown_microtasks();
  }
  Ok(supported)
}

#[test]
fn for_await_break_closes_async_generator() -> Result<(), VmError> {
  let mut rt = new_runtime();

  if !async_generators_supported(&mut rt)? {
    return Ok(());
  }

  let value = rt.exec_script(
    r#"
      var out = "";
      var log = "";

      async function* gen() {
        try {
          yield 1;
          yield 2;
        } finally {
          log += "F";
        }
      }

      async function run() {
        for await (const x of gen()) {
          break;
        }
        return log;
      }

      run().then(v => out = v);
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "F");
  Ok(())
}

#[test]
fn for_await_throw_closes_async_generator_before_catch() -> Result<(), VmError> {
  let mut rt = new_runtime();

  if !async_generators_supported(&mut rt)? {
    return Ok(());
  }

  let value = rt.exec_script(
    r#"
      var out = "";
      var log = "";

      async function* gen() {
        try {
          yield 1;
          yield 2;
        } finally {
          log += "F";
        }
      }

      async function run() {
        try {
          for await (const x of gen()) {
            throw "boom";
          }
        } catch (e) {}
        return log;
      }

      run().then(v => out = v);
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  let out = value_to_string(&rt, out);
  assert!(
    out.contains('F'),
    "expected `finally` to run and write 'F', got {out:?}"
  );
  Ok(())
}

#[test]
fn for_await_return_closes_async_generator() -> Result<(), VmError> {
  let mut rt = new_runtime();

  if !async_generators_supported(&mut rt)? {
    return Ok(());
  }

  let value = rt.exec_script(
    r#"
      var out = "";
      var log = "";

      async function* gen() {
        try {
          yield 1;
          yield 2;
        } finally {
          log += "F";
        }
      }

      async function run() {
        for await (const x of gen()) {
          return "R";
        }
        return "bad";
      }

      run().then(v => out = v + log);
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "RF");
  Ok(())
}

#[test]
fn for_await_break_awaits_async_generator_finally_await() -> Result<(), VmError> {
  let mut rt = new_runtime();

  if !async_generators_supported(&mut rt)? {
    return Ok(());
  }

  // Ensure we don't leak queued microtasks even if this test fails.
  let result: Result<(), VmError> = (|| {
    let value = rt.exec_script(
      r#"
        var out = "";
        var state = "pending";
        var log = "";

        var resolveFinally;
        var finallyPromise = new Promise(function (resolve) {
          resolveFinally = resolve;
        });

        async function* gen() {
          try {
            yield 1;
            yield 2;
          } finally {
            log += "F";
            await finallyPromise;
            log += "A";
          }
        }

        async function run() {
          for await (const _x of gen()) {
            break;
          }
          state = "fulfilled";
          return log;
        }

        run().then(
          function (v) { out = v; },
          function (e) { out = "err:" + String(e); state = "rejected"; }
        );
        out
      "#,
    )?;
    assert_eq!(value_to_string(&rt, value), "");

    // Break should trigger `AsyncIteratorClose`, which calls `return()` on the async generator.
    // The overall loop completion must stay pending until the awaited `finally` finishes.
    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let value = rt.exec_script("log")?;
    assert_eq!(value_to_string(&rt, value), "F");

    let value = rt.exec_script("state")?;
    assert_eq!(value_to_string(&rt, value), "pending");

    let value = rt.exec_script("out")?;
    assert_eq!(value_to_string(&rt, value), "");

    rt.exec_script("resolveFinally()")?;
    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let value = rt.exec_script("log")?;
    assert_eq!(value_to_string(&rt, value), "FA");

    let value = rt.exec_script("state")?;
    assert_eq!(value_to_string(&rt, value), "fulfilled");

    let value = rt.exec_script("out")?;
    assert_eq!(value_to_string(&rt, value), "FA");

    Ok(())
  })();

  rt.teardown_microtasks();
  result
}

#[test]
fn for_await_return_awaits_async_generator_finally_await() -> Result<(), VmError> {
  let mut rt = new_runtime();

  if !async_generators_supported(&mut rt)? {
    return Ok(());
  }

  // Ensure we don't leak queued microtasks even if this test fails.
  let result: Result<(), VmError> = (|| {
    let value = rt.exec_script(
      r#"
        var out = "";
        var state = "pending";
        var log = "";

        var resolveFinally;
        var finallyPromise = new Promise(function (resolve) {
          resolveFinally = resolve;
        });

        async function* gen() {
          try {
            yield 1;
            yield 2;
          } finally {
            log += "F";
            await finallyPromise;
            log += "A";
          }
        }

        async function run() {
          for await (const _x of gen()) {
            return "R";
          }
          return "bad";
        }

        run().then(
          function (v) { out = v + log; state = "fulfilled"; },
          function (e) { out = "err:" + String(e); state = "rejected"; }
        );
        out
      "#,
    )?;
    assert_eq!(value_to_string(&rt, value), "");

    // Early return should still close the async iterator, and must await the `finally` cleanup
    // before the async function's returned promise settles.
    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let value = rt.exec_script("log")?;
    assert_eq!(value_to_string(&rt, value), "F");

    let value = rt.exec_script("state")?;
    assert_eq!(value_to_string(&rt, value), "pending");

    let value = rt.exec_script("out")?;
    assert_eq!(value_to_string(&rt, value), "");

    rt.exec_script("resolveFinally()")?;
    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let value = rt.exec_script("log")?;
    assert_eq!(value_to_string(&rt, value), "FA");

    let value = rt.exec_script("state")?;
    assert_eq!(value_to_string(&rt, value), "fulfilled");

    let value = rt.exec_script("out")?;
    assert_eq!(value_to_string(&rt, value), "RFA");

    Ok(())
  })();

  rt.teardown_microtasks();
  result
}

#[test]
fn for_await_throw_awaits_async_generator_finally_await() -> Result<(), VmError> {
  let mut rt = new_runtime();

  if !async_generators_supported(&mut rt)? {
    return Ok(());
  }

  let result: Result<(), VmError> = (|| {
    let value = rt.exec_script(
      r#"
        var out = "";
        var state = "pending";
        var log = "";

        var resolveFinally;
        var finallyPromise = new Promise(function (resolve) {
          resolveFinally = resolve;
        });

        async function* gen() {
          try {
            yield 1;
            yield 2;
          } finally {
            log += "F";
            await finallyPromise;
            log += "A";
          }
        }

        async function run() {
          for await (const _x of gen()) {
            throw "boom";
          }
        }

        run().then(
          function (_v) { out = "ok"; state = "fulfilled"; },
          function (e) { out = String(e); state = "rejected"; }
        );
        out
      "#,
    )?;
    assert_eq!(value_to_string(&rt, value), "");

    // Throwing from the loop body triggers `AsyncIteratorClose`. The rejection of the async
    // function's returned promise must be delayed until the generator's awaited `finally` finishes.
    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let value = rt.exec_script("log")?;
    assert_eq!(value_to_string(&rt, value), "F");

    let value = rt.exec_script("state")?;
    assert_eq!(value_to_string(&rt, value), "pending");

    let value = rt.exec_script("out")?;
    assert_eq!(value_to_string(&rt, value), "");

    rt.exec_script("resolveFinally()")?;
    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let value = rt.exec_script("log")?;
    assert_eq!(value_to_string(&rt, value), "FA");

    let value = rt.exec_script("state")?;
    assert_eq!(value_to_string(&rt, value), "rejected");

    let value = rt.exec_script("out")?;
    assert_eq!(value_to_string(&rt, value), "boom");

    Ok(())
  })();

  rt.teardown_microtasks();
  result
}

#[test]
fn for_await_break_rejects_if_async_generator_finally_await_rejects() -> Result<(), VmError> {
  let mut rt = new_runtime();

  if !async_generators_supported(&mut rt)? {
    return Ok(());
  }

  // Ensure we don't leak queued microtasks even if this test fails.
  let result: Result<(), VmError> = (|| {
    let value = rt.exec_script(
      r#"
        var out = "";
        var state = "pending";
        var log = "";

        var rejectFinally;
        var finallyPromise = new Promise(function (_resolve, reject) {
          rejectFinally = reject;
        });

        async function* gen() {
          try {
            yield 1;
            yield 2;
          } finally {
            log += "F";
            await finallyPromise;
            log += "A";
          }
        }

        async function run() {
          for await (const _x of gen()) {
            break;
          }
          return "ok";
        }

        run().then(
          function (_v) { out = "ok"; state = "fulfilled"; },
          function (e) { out = String(e); state = "rejected"; }
        );
        out
      "#,
    )?;
    assert_eq!(value_to_string(&rt, value), "");

    // The loop should be waiting for the generator's `finally` to finish.
    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let value = rt.exec_script("log")?;
    assert_eq!(value_to_string(&rt, value), "F");

    let value = rt.exec_script("state")?;
    assert_eq!(value_to_string(&rt, value), "pending");

    let value = rt.exec_script("out")?;
    assert_eq!(value_to_string(&rt, value), "");

    rt.exec_script("rejectFinally('fail')")?;
    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let value = rt.exec_script("log")?;
    assert_eq!(value_to_string(&rt, value), "F");

    let value = rt.exec_script("state")?;
    assert_eq!(value_to_string(&rt, value), "rejected");

    let value = rt.exec_script("out")?;
    assert_eq!(value_to_string(&rt, value), "fail");

    Ok(())
  })();

  rt.teardown_microtasks();
  result
}

#[test]
fn for_await_return_rejects_if_async_generator_finally_await_rejects() -> Result<(), VmError> {
  let mut rt = new_runtime();

  if !async_generators_supported(&mut rt)? {
    return Ok(());
  }

  let result: Result<(), VmError> = (|| {
    let value = rt.exec_script(
      r#"
        var out = "";
        var state = "pending";
        var log = "";

        var rejectFinally;
        var finallyPromise = new Promise(function (_resolve, reject) {
          rejectFinally = reject;
        });

        async function* gen() {
          try {
            yield 1;
            yield 2;
          } finally {
            log += "F";
            await finallyPromise;
            log += "A";
          }
        }

        async function run() {
          for await (const _x of gen()) {
            return "R";
          }
          return "bad";
        }

        run().then(
          function (_v) { out = "ok"; state = "fulfilled"; },
          function (e) { out = String(e); state = "rejected"; }
        );
        out
      "#,
    )?;
    assert_eq!(value_to_string(&rt, value), "");

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let value = rt.exec_script("log")?;
    assert_eq!(value_to_string(&rt, value), "F");

    let value = rt.exec_script("state")?;
    assert_eq!(value_to_string(&rt, value), "pending");

    let value = rt.exec_script("out")?;
    assert_eq!(value_to_string(&rt, value), "");

    rt.exec_script("rejectFinally('fail')")?;
    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let value = rt.exec_script("log")?;
    assert_eq!(value_to_string(&rt, value), "F");

    let value = rt.exec_script("state")?;
    assert_eq!(value_to_string(&rt, value), "rejected");

    let value = rt.exec_script("out")?;
    assert_eq!(value_to_string(&rt, value), "fail");

    Ok(())
  })();

  rt.teardown_microtasks();
  result
}

#[test]
fn for_await_throw_rejects_if_async_generator_finally_await_rejects() -> Result<(), VmError> {
  let mut rt = new_runtime();

  if !async_generators_supported(&mut rt)? {
    return Ok(());
  }

  let result: Result<(), VmError> = (|| {
    let value = rt.exec_script(
      r#"
        var out = "";
        var state = "pending";
        var log = "";

        var rejectFinally;
        var finallyPromise = new Promise(function (_resolve, reject) {
          rejectFinally = reject;
        });

        async function* gen() {
          try {
            yield 1;
            yield 2;
          } finally {
            log += "F";
            await finallyPromise;
            log += "A";
          }
        }

        async function run() {
          for await (const _x of gen()) {
            throw "boom";
          }
        }

        run().then(
          function (_v) { out = "ok"; state = "fulfilled"; },
          function (e) { out = String(e); state = "rejected"; }
        );
        out
      "#,
    )?;
    assert_eq!(value_to_string(&rt, value), "");

    // Throw completion still triggers `AsyncIteratorClose`, and (per `AsyncIteratorClose`) errors
    // from `return()` must override the loop body error.
    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let value = rt.exec_script("log")?;
    assert_eq!(value_to_string(&rt, value), "F");

    let value = rt.exec_script("state")?;
    assert_eq!(value_to_string(&rt, value), "pending");

    let value = rt.exec_script("out")?;
    assert_eq!(value_to_string(&rt, value), "");

    rt.exec_script("rejectFinally('fail')")?;
    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let value = rt.exec_script("log")?;
    assert_eq!(value_to_string(&rt, value), "F");

    let value = rt.exec_script("state")?;
    assert_eq!(value_to_string(&rt, value), "rejected");

    let value = rt.exec_script("out")?;
    assert_eq!(value_to_string(&rt, value), "fail");

    Ok(())
  })();

  rt.teardown_microtasks();
  result
}
