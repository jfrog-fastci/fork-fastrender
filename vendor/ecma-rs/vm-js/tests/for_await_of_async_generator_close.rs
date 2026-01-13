use vm_js::{Heap, HeapLimits, JsRuntime, PropertyKey, Value, Vm, VmError, VmOptions};

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

  let syntax_error_proto = rt.realm().intrinsics().syntax_error_prototype();
  if rt.heap().object_prototype(err_obj)? != Some(syntax_error_proto) {
    return Ok(false);
  }

  let mut scope = rt.heap_mut().scope();
  scope.push_root(Value::Object(err_obj))?;

  let message_key = PropertyKey::from_string(scope.alloc_string("message")?);
  let Some(Value::String(message_s)) =
    scope.heap().object_get_own_data_property_value(err_obj, &message_key)?
  else {
    return Ok(false);
  };

  Ok(scope.heap().get_string(message_s)?.to_utf8_lossy() == "async generator functions")
}

fn async_generators_supported(rt: &mut JsRuntime) -> Result<bool, VmError> {
  // Detect runtime support (call semantics), not just parsing/prototype wiring.
  match rt.exec_script("async function* __ag_support() {} void __ag_support();") {
    Ok(_) => Ok(true),
    Err(err) if is_unimplemented_async_generator_error(rt, &err)? => Ok(false),
    Err(err) => Err(err),
  }
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
