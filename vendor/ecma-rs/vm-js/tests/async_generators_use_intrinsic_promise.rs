use vm_js::{Heap, HeapLimits, JsRuntime, PropertyKey, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // These tests exercise async generator iteration + Promise job queuing. Use a slightly larger
  // heap than the 1MiB default used by many unit tests to avoid spurious OOM failures.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
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
  let Some(Value::String(message_s)) = scope.heap().object_get_own_data_property_value(err_obj, &message_key)? else {
    return Ok(false);
  };

  let message = scope.heap().get_string(message_s)?.to_utf8_lossy();
  Ok(message == "async generator functions")
}

fn feature_detect_async_generators(rt: &mut JsRuntime) -> Result<bool, VmError> {
  match rt.exec_script("async function* __ag_support() { yield 1; }\n__ag_support().next();") {
    Ok(_) => {
      rt.teardown_microtasks();
      Ok(true)
    }
    Err(err) if is_unimplemented_async_generator_error(rt, &err)? => Ok(false),
    Err(err) => Err(err),
  }
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
    if !feature_detect_async_generators(&mut rt)? {
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
    if !feature_detect_async_generators(&mut rt)? {
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
