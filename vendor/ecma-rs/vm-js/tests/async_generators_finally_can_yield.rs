use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

mod _async_generator_support;

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Async generators allocate Promises and enqueue microtasks. Keep the heap limit large enough to
  // avoid spurious `VmError::OutOfMemory` failures as builtin coverage grows.
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
fn async_generator_return_triggers_finally_and_finally_can_yield() -> Result<(), VmError> {
  let Some(mut rt) = new_runtime_if_supported()? else {
    return Ok(());
  };

  // Ensure we don't leak queued microtasks even if this test fails.
  let result: Result<(), VmError> = (|| {
    let value = match rt.exec_script(
      r#"
        var ok = "pending";

        async function* g() {
          try { yield 1; }
          finally { yield 2; }
        }

        async function run() {
          const it = g();
          const r1 = await it.next();
          const r2 = await it.return(42);
          const r3 = await it.next();
          return (
            r1.value === 1 && r1.done === false &&
            r2.value === 2 && r2.done === false &&
            r3.value === 42 && r3.done === true
          );
        }

        run().then(
          v => { ok = v; },
          e => { ok = e; }
        );

        ok
      "#,
    ) {
      Ok(v) => v,
      Err(err)
        if _async_generator_support::is_unimplemented_async_generator_error(&mut rt, &err)? =>
      {
        return Ok(());
      }
      Err(err) => return Err(err),
    };
    assert_eq!(value_to_string(&rt, value), "pending");

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let ok = rt.exec_script("ok")?;
    assert_eq!(ok, Value::Bool(true));

    Ok(())
  })();

  rt.teardown_microtasks();
  result
}

#[test]
fn async_generator_throw_triggers_finally_and_finally_can_yield() -> Result<(), VmError> {
  let Some(mut rt) = new_runtime_if_supported()? else {
    return Ok(());
  };

  let result: Result<(), VmError> = (|| {
    let value = match rt.exec_script(
      r#"
        var ok = "pending";

        async function* g() {
          try { yield 1; }
          finally { yield 2; }
        }

        async function run() {
          const it = g();
          const r0 = await it.next();

          // This `.throw()` should enter the `finally` block, which is allowed to `yield` a value.
          const r1 = await it.throw("boom");

          // Once the `finally` yield is consumed, resuming should propagate the original throw.
          var caught = false;
          try {
            await it.next();
          } catch (e) {
            caught = (e === "boom");
          }

          return (
            r0.value === 1 && r0.done === false &&
            r1.value === 2 && r1.done === false &&
            caught
          );
        }

        run().then(
          v => { ok = v; },
          e => { ok = e; }
        );

        ok
      "#,
    ) {
      Ok(v) => v,
      Err(err)
        if _async_generator_support::is_unimplemented_async_generator_error(&mut rt, &err)? =>
      {
        return Ok(());
      }
      Err(err) => return Err(err),
    };
    assert_eq!(value_to_string(&rt, value), "pending");

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let ok = rt.exec_script("ok")?;
    assert_eq!(ok, Value::Bool(true));

    Ok(())
  })();

  rt.teardown_microtasks();
  result
}
