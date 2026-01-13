use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

mod _async_generator_support;

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Async generators combine generator suspension (`yield`) with async suspension (`await`). These
  // tests exercise Promise/job machinery and therefore require a slightly larger heap to avoid
  // spurious `VmError::OutOfMemory` failures as vm-js grows its builtin surface area.
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
fn internal_await_delays_first_yield() -> Result<(), VmError> {
  let Some(mut rt) = new_runtime_if_supported()? else {
    return Ok(());
  };

  let result: Result<(), VmError> = (|| {
    let value = match rt.exec_script(
      r#"
        var log = "";
        var out = "";

        async function* g(){
          log += "s";
          await Promise.resolve().then(()=>{ log += "a"; });
          yield 1;
        }

        var it = g();
        it.next().then(r => { out = String(r.value) + ':' + String(r.done); });

        log + '|' + out
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
    // The generator starts running synchronously until it hits `await`. At this point it is
    // suspended on the internal await and has not yet produced a `yield`.
    assert_eq!(value_to_string(&rt, value), "s|");

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;
    assert!(
      rt.vm.microtask_queue().is_empty(),
      "expected microtask queue to be empty after checkpoint"
    );

    let value = rt.exec_script("log")?;
    assert_eq!(value_to_string(&rt, value), "sa");

    let value = rt.exec_script("out")?;
    assert_eq!(value_to_string(&rt, value), "1:false");

    Ok(())
  })();

  rt.teardown_microtasks();
  result
}

#[test]
fn next_requests_are_queued_across_internal_await() -> Result<(), VmError> {
  let Some(mut rt) = new_runtime_if_supported()? else {
    return Ok(());
  };

  let result: Result<(), VmError> = (|| {
    let value = match rt.exec_script(
      r#"
        var results = [];

        async function* g(){
          await Promise.resolve();
          yield 1;
          yield 2;
        }

        var it = g();
        it.next().then(r=>results.push(r.value));
        it.next().then(r=>results.push(r.value));
        it.next().then(r=>results.push(r.done ? 'done' : 'notdone'));

        results.join(',')
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
    assert_eq!(value_to_string(&rt, value), "");

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;
    assert!(
      rt.vm.microtask_queue().is_empty(),
      "expected microtask queue to be empty after checkpoint"
    );

    let value = rt.exec_script("results.join(',')")?;
    assert_eq!(value_to_string(&rt, value), "1,2,done");

    Ok(())
  })();

  rt.teardown_microtasks();
  result
}

#[test]
fn next_requests_queue_across_internal_await_after_first_yield() -> Result<(), VmError> {
  let Some(mut rt) = new_runtime_if_supported()? else {
    return Ok(());
  };

  let result: Result<(), VmError> = (|| {
    let value = match rt.exec_script(
      r#"
        var results = [];

        async function* g(){
          yield 1;
          await Promise.resolve();
          yield 2;
        }

        var it = g();
        it.next().then(r=>results.push(r.value));
        it.next().then(r=>results.push(r.value));
        it.next().then(r=>results.push(r.done ? 'done' : 'notdone'));

        results.join(',')
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
    assert_eq!(value_to_string(&rt, value), "");

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;
    assert!(
      rt.vm.microtask_queue().is_empty(),
      "expected microtask queue to be empty after checkpoint"
    );

    let value = rt.exec_script("results.join(',')")?;
    assert_eq!(value_to_string(&rt, value), "1,2,done");
    Ok(())
  })();

  rt.teardown_microtasks();
  result
}

#[test]
fn return_request_is_queued_across_internal_await_before_first_yield() -> Result<(), VmError> {
  let Some(mut rt) = new_runtime_if_supported()? else {
    return Ok(());
  };

  let result: Result<(), VmError> = (|| {
    let value = match rt.exec_script(
      r#"
        var log = "";

        async function* g() {
          log += "s";
          await Promise.resolve().then(() => { log += "a"; });
          yield 1;
          log += "y"; // must not run if a queued return closes the generator after the yield
        }

        var it = g();
        it.next().then(r => { log += "|n:" + r.value + ":" + r.done; });
        it.return("x").then(r => { log += "|r:" + r.value + ":" + r.done; });

        log
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
    assert_eq!(value_to_string(&rt, value), "s");

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;
    assert!(
      rt.vm.microtask_queue().is_empty(),
      "expected microtask queue to be empty after checkpoint"
    );

    let value = rt.exec_script("log")?;
    assert_eq!(value_to_string(&rt, value), "sa|n:1:false|r:x:true");
    Ok(())
  })();

  rt.teardown_microtasks();
  result
}

#[test]
fn throw_request_is_queued_across_internal_await_before_first_yield() -> Result<(), VmError> {
  let Some(mut rt) = new_runtime_if_supported()? else {
    return Ok(());
  };

  let result: Result<(), VmError> = (|| {
    let value = match rt.exec_script(
      r#"
        var log = "";

        async function* g() {
          log += "s";
          await Promise.resolve().then(() => { log += "a"; });
          yield 1;
          log += "y"; // must not run if a queued throw closes the generator after the yield
        }

        var it = g();
        it.next().then(r => { log += "|n:" + r.value + ":" + r.done; });
        it.throw("boom").then(
          r => { log += "|t:" + r.value + ":" + r.done; },
          e => { log += "|t:" + e; }
        );

        log
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
    assert_eq!(value_to_string(&rt, value), "s");

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;
    assert!(
      rt.vm.microtask_queue().is_empty(),
      "expected microtask queue to be empty after checkpoint"
    );

    let value = rt.exec_script("log")?;
    assert_eq!(value_to_string(&rt, value), "sa|n:1:false|t:boom");
    Ok(())
  })();

  rt.teardown_microtasks();
  result
}

#[test]
fn return_request_is_queued_across_internal_await_after_first_yield() -> Result<(), VmError> {
  let Some(mut rt) = new_runtime_if_supported()? else {
    return Ok(());
  };

  let result: Result<(), VmError> = (|| {
    let value = match rt.exec_script(
      r#"
        var log = "";

        async function* g() {
          yield 1;
          log += "a";
          await Promise.resolve().then(() => { log += "b"; });
          yield 2;
          log += "c"; // must not run if a queued return closes the generator after the second yield
        }

        var it = g();
        it.next().then(r => { log += "|n1:" + r.value + ":" + r.done; });
        it.next().then(r => { log += "|n2:" + r.value + ":" + r.done; });
        it.return("x").then(r => { log += "|r:" + r.value + ":" + r.done; });

        log
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
    assert_eq!(value_to_string(&rt, value), "");

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;
    assert!(
      rt.vm.microtask_queue().is_empty(),
      "expected microtask queue to be empty after checkpoint"
    );

    let value = rt.exec_script("log")?;
    assert_eq!(
      value_to_string(&rt, value),
      "a|n1:1:falseb|n2:2:false|r:x:true"
    );
    Ok(())
  })();

  rt.teardown_microtasks();
  result
}

#[test]
fn throw_request_is_queued_across_internal_await_after_first_yield() -> Result<(), VmError> {
  let Some(mut rt) = new_runtime_if_supported()? else {
    return Ok(());
  };

  let result: Result<(), VmError> = (|| {
    let value = match rt.exec_script(
      r#"
        var log = "";

        async function* g() {
          yield 1;
          log += "a";
          await Promise.resolve().then(() => { log += "b"; });
          yield 2;
          log += "c"; // must not run if a queued throw closes the generator after the second yield
        }

        var it = g();
        it.next().then(r => { log += "|n1:" + r.value + ":" + r.done; });
        it.next().then(r => { log += "|n2:" + r.value + ":" + r.done; });
        it.throw("boom").then(
          r => { log += "|t:" + r.value + ":" + r.done; },
          e => { log += "|t:" + e; }
        );

        log
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
    assert_eq!(value_to_string(&rt, value), "");

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;
    assert!(
      rt.vm.microtask_queue().is_empty(),
      "expected microtask queue to be empty after checkpoint"
    );

    let value = rt.exec_script("log")?;
    assert_eq!(
      value_to_string(&rt, value),
      "a|n1:1:falseb|n2:2:false|t:boom"
    );
    Ok(())
  })();

  rt.teardown_microtasks();
  result
}
