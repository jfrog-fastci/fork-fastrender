use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

mod _async_generator_support;

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // These tests exercise async generator iteration + Promise job queuing. Use a slightly larger
  // heap than the 1MiB default used by many unit tests to avoid spurious OOM failures.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_bool(value: Value) -> bool {
  let Value::Bool(b) = value else {
    panic!("expected bool, got {value:?}");
  };
  b
}

fn value_to_number(value: Value) -> f64 {
  let Value::Number(n) = value else {
    panic!("expected number, got {value:?}");
  };
  n
}

fn value_to_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn async_generator_next_returns_intrinsic_promise() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let result: Result<(), VmError> = (|| {
    if !_async_generator_support::supports_async_generators(&mut rt)? {
      return Ok(());
    }

    // Async generator iteration methods use `NewPromiseCapability(%Promise%)` and must not consult
    // the mutable global `Promise` binding.
    let value = rt.exec_script(
      r#"
        var out = undefined;
        var P0 = Promise;
        Promise = function FakePromise(executor) { throw 'should not be called'; };

        async function* g(){ yield 1; }
        var it = g();
        var p = it.next();

        // Should be a real intrinsic Promise, not FakePromise.
        var ok = Object.getPrototypeOf(p) === P0.prototype;

        // Also ensure it resolves correctly.
        p.then(function (r) { out = r.value; });

        ok;
      "#,
    )?;
    assert!(value_to_bool(value));

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let out = rt.exec_script("out")?;
    assert_eq!(value_to_number(out), 1.0);
    Ok(())
  })();

  // Avoid failing the test due to the `Job` root-leak debug assertion if any step returns early.
  rt.teardown_microtasks();
  result
}

#[test]
fn async_generator_return_throw_returns_intrinsic_promise() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let result: Result<(), VmError> = (|| {
    if !_async_generator_support::supports_async_generators(&mut rt)? {
      return Ok(());
    }

    let value = rt.exec_script(
      r#"
        var out_return = undefined;
        var out_throw = undefined;

        var P0 = Promise;
        Promise = function FakePromise(executor) { throw 'should not be called'; };

        async function* g(){ yield 1; }

        var it1 = g();
        var p1 = it1.return(1);
        var ok1 = Object.getPrototypeOf(p1) === P0.prototype;
        p1.then(function (r) { out_return = r.value; });

        var it2 = g();
        var p2 = it2.throw('x');
        var ok2 = Object.getPrototypeOf(p2) === P0.prototype;
        p2.then(function () { out_throw = 'fulfilled'; }, function (e) { out_throw = e; });

        ok1 && ok2;
      "#,
    )?;
    assert!(value_to_bool(value));

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let out_return = rt.exec_script("out_return")?;
    assert_eq!(value_to_number(out_return), 1.0);

    let out_throw = rt.exec_script("out_throw")?;
    assert_eq!(value_to_string(&rt, out_throw), "x");
    Ok(())
  })();

  rt.teardown_microtasks();
  result
}
