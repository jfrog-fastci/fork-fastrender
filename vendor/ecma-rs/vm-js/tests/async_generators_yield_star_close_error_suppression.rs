use vm_js::{Heap, HeapLimits, JsRuntime, PropertyKey, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Async generator `yield*` exercises Promise jobs + iterator closing. Use a slightly larger heap
  // than the default 1MiB used by many unit tests to avoid spurious OOM failures when
  // implementation details change.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 2 * 1024 * 1024));
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

#[test]
fn yield_star_throw_suppresses_close_error_from_return_getter() -> Result<(), VmError> {
  let mut rt = new_runtime();

  // Ensure we don't leak queued microtasks even if this test fails.
  let result: Result<(), VmError> = (|| {
    let value = match rt.exec_script(
      r#"
        function errToString(e) {
          if (typeof e === "string") return e;
          if (e && e.name) return e.name;
          return "" + e;
        }

        var next1Value = null;
        var next1Done = null;
        var throwValue = null;
        var throwDone = null;
        var returnGetterCalled = false;
        var err = "";

        const delegate = {};
        delegate[Symbol.asyncIterator] = function () {
          return {
            next() { return Promise.resolve({ value: 1, done: false }); },
            // Close error: getting `return` throws.
            get return() { returnGetterCalled = true; throw "close"; },
          };
        };

        async function* g() {
          try { yield* delegate; }
          catch (e) { yield "caught:" + e; }
        }

        const it = g();
        it.next().then(function (r) {
          next1Value = r.value;
          next1Done = r.done;
          return it.throw("boom");
        }).then(function (r) {
          throwValue = r.value;
          throwDone = r.done;
        }, function (e) {
          err = errToString(e);
        });

        err
      "#,
    ) {
      Ok(v) => v,
      Err(err) if is_unimplemented_async_generator_error(&mut rt, &err)? => return Ok(()),
      Err(err) => return Err(err),
    };
    assert_eq!(value_to_string(&rt, value), "");

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let next1_value = rt.exec_script("next1Value")?;
    assert_eq!(next1_value, Value::Number(1.0));
    let next1_done = rt.exec_script("next1Done")?;
    assert_eq!(next1_done, Value::Bool(false));

    let throw_value = rt.exec_script("throwValue")?;
    assert_eq!(value_to_string(&rt, throw_value), "caught:boom");
    let throw_done = rt.exec_script("throwDone")?;
    assert_eq!(throw_done, Value::Bool(false));

    let return_getter_called = rt.exec_script("returnGetterCalled")?;
    assert_eq!(return_getter_called, Value::Bool(true));

    let err = rt.exec_script("err")?;
    assert_eq!(value_to_string(&rt, err), "");

    Ok(())
  })();

  rt.teardown_microtasks();
  result
}

#[test]
fn yield_star_return_does_not_suppress_close_error_from_return_getter() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let result: Result<(), VmError> = (|| {
    let value = match rt.exec_script(
      r#"
        function errToString(e) {
          if (typeof e === "string") return e;
          if (e && e.name) return e.name;
          return "" + e;
        }

        var next1Value = null;
        var next1Done = null;
        var returnGetterCalled = false;
        var out = "";

        const delegate = {};
        delegate[Symbol.asyncIterator] = function () {
          return {
            next() { return Promise.resolve({ value: 1, done: false }); },
            // Close error: getting `return` throws.
            get return() { returnGetterCalled = true; throw "close"; },
          };
        };

        async function* g() { yield* delegate; }

        const it = g();
        it.next().then(function (r) {
          next1Value = r.value;
          next1Done = r.done;
          return it.return("ok");
        }).then(
          function () { out = "resolved"; },
          function (e) { out = errToString(e); }
        );

        out
      "#,
    ) {
      Ok(v) => v,
      Err(err) if is_unimplemented_async_generator_error(&mut rt, &err)? => return Ok(()),
      Err(err) => return Err(err),
    };
    assert_eq!(value_to_string(&rt, value), "");

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let next1_value = rt.exec_script("next1Value")?;
    assert_eq!(next1_value, Value::Number(1.0));
    let next1_done = rt.exec_script("next1Done")?;
    assert_eq!(next1_done, Value::Bool(false));

    let out = rt.exec_script("out")?;
    assert_eq!(value_to_string(&rt, out), "close");

    let return_getter_called = rt.exec_script("returnGetterCalled")?;
    assert_eq!(return_getter_called, Value::Bool(true));
    Ok(())
  })();

  rt.teardown_microtasks();
  result
}

#[test]
fn yield_star_throw_suppresses_close_error_from_return_non_callable() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let result: Result<(), VmError> = (|| {
    let value = match rt.exec_script(
      r#"
        function errToString(e) {
          if (typeof e === "string") return e;
          if (e && e.name) return e.name;
          return "" + e;
        }

        var next1Value = null;
        var next1Done = null;
        var throwValue = null;
        var throwDone = null;
        var returnGetterCalled = false;
        var err = "";

        const delegate = {};
        delegate[Symbol.asyncIterator] = function () {
          return {
            next() { return Promise.resolve({ value: 1, done: false }); },
            // Close error: `GetMethod(iterator, "return")` finds a non-callable value.
            get return() { returnGetterCalled = true; return 1; },
          };
        };

        async function* g() {
          try { yield* delegate; }
          catch (e) { yield "caught:" + e; }
        }

        const it = g();
        it.next().then(function (r) {
          next1Value = r.value;
          next1Done = r.done;
          return it.throw("boom");
        }).then(function (r) {
          throwValue = r.value;
          throwDone = r.done;
        }, function (e) {
          err = errToString(e);
        });

        err
      "#,
    ) {
      Ok(v) => v,
      Err(err) if is_unimplemented_async_generator_error(&mut rt, &err)? => return Ok(()),
      Err(err) => return Err(err),
    };
    assert_eq!(value_to_string(&rt, value), "");

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let next1_value = rt.exec_script("next1Value")?;
    assert_eq!(next1_value, Value::Number(1.0));
    let next1_done = rt.exec_script("next1Done")?;
    assert_eq!(next1_done, Value::Bool(false));

    let throw_value = rt.exec_script("throwValue")?;
    assert_eq!(value_to_string(&rt, throw_value), "caught:boom");
    let throw_done = rt.exec_script("throwDone")?;
    assert_eq!(throw_done, Value::Bool(false));

    let return_getter_called = rt.exec_script("returnGetterCalled")?;
    assert_eq!(return_getter_called, Value::Bool(true));

    let err = rt.exec_script("err")?;
    assert_eq!(value_to_string(&rt, err), "");

    Ok(())
  })();

  rt.teardown_microtasks();
  result
}

#[test]
fn yield_star_return_does_not_suppress_close_error_from_return_non_callable() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let result: Result<(), VmError> = (|| {
    let value = match rt.exec_script(
      r#"
        function errToString(e) {
          if (typeof e === "string") return e;
          if (e && e.name) return e.name;
          return "" + e;
        }

        var next1Value = null;
        var next1Done = null;
        var returnGetterCalled = false;
        var out = "";

        const delegate = {};
        delegate[Symbol.asyncIterator] = function () {
          return {
            next() { return Promise.resolve({ value: 1, done: false }); },
            get return() { returnGetterCalled = true; return 1; },
          };
        };

        async function* g() { yield* delegate; }

        const it = g();
        it.next().then(function (r) {
          next1Value = r.value;
          next1Done = r.done;
          return it.return("ok");
        }).then(
          function () { out = "resolved"; },
          function (e) { out = errToString(e); }
        );

        out
      "#,
    ) {
      Ok(v) => v,
      Err(err) if is_unimplemented_async_generator_error(&mut rt, &err)? => return Ok(()),
      Err(err) => return Err(err),
    };
    assert_eq!(value_to_string(&rt, value), "");

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let next1_value = rt.exec_script("next1Value")?;
    assert_eq!(next1_value, Value::Number(1.0));
    let next1_done = rt.exec_script("next1Done")?;
    assert_eq!(next1_done, Value::Bool(false));

    let out = rt.exec_script("out")?;
    assert_eq!(value_to_string(&rt, out), "TypeError");

    let return_getter_called = rt.exec_script("returnGetterCalled")?;
    assert_eq!(return_getter_called, Value::Bool(true));
    Ok(())
  })();

  rt.teardown_microtasks();
  result
}

#[test]
fn yield_star_throw_suppresses_close_error_from_return_non_object() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let result: Result<(), VmError> = (|| {
    let value = match rt.exec_script(
      r#"
        function errToString(e) {
          if (typeof e === "string") return e;
          if (e && e.name) return e.name;
          return "" + e;
        }

        var next1Value = null;
        var next1Done = null;
        var throwValue = null;
        var throwDone = null;
        var returnCalls = 0;
        var returnArgsLen = null;
        var err = "";

        const delegate = {};
        delegate[Symbol.asyncIterator] = function () {
          return {
            next() { return Promise.resolve({ value: 1, done: false }); },
            // Close error: return() returns a non-object.
            return() { returnCalls++; returnArgsLen = arguments.length; return 1; },
          };
        };

        async function* g() {
          try { yield* delegate; }
          catch (e) { yield "caught:" + e; }
        }

        const it = g();
        it.next().then(function (r) {
          next1Value = r.value;
          next1Done = r.done;
          return it.throw("boom");
        }).then(function (r) {
          throwValue = r.value;
          throwDone = r.done;
        }, function (e) {
          err = errToString(e);
        });

        err
      "#,
    ) {
      Ok(v) => v,
      Err(err) if is_unimplemented_async_generator_error(&mut rt, &err)? => return Ok(()),
      Err(err) => return Err(err),
    };
    assert_eq!(value_to_string(&rt, value), "");

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let next1_value = rt.exec_script("next1Value")?;
    assert_eq!(next1_value, Value::Number(1.0));
    let next1_done = rt.exec_script("next1Done")?;
    assert_eq!(next1_done, Value::Bool(false));

    let throw_value = rt.exec_script("throwValue")?;
    assert_eq!(value_to_string(&rt, throw_value), "caught:boom");
    let throw_done = rt.exec_script("throwDone")?;
    assert_eq!(throw_done, Value::Bool(false));

    let return_calls = rt.exec_script("returnCalls")?;
    assert_eq!(return_calls, Value::Number(1.0));
    let return_args_len = rt.exec_script("returnArgsLen")?;
    assert_eq!(return_args_len, Value::Number(0.0));

    let err = rt.exec_script("err")?;
    assert_eq!(value_to_string(&rt, err), "");

    Ok(())
  })();

  rt.teardown_microtasks();
  result
}

#[test]
fn yield_star_return_does_not_suppress_close_error_from_return_non_object() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let result: Result<(), VmError> = (|| {
    let value = match rt.exec_script(
      r#"
        function errToString(e) {
          if (typeof e === "string") return e;
          if (e && e.name) return e.name;
          return "" + e;
        }

        var next1Value = null;
        var next1Done = null;
        var returnCalls = 0;
        var returnArgsLen = null;
        var out = "";

        const delegate = {};
        delegate[Symbol.asyncIterator] = function () {
          return {
            next() { return Promise.resolve({ value: 1, done: false }); },
            return(v) { returnCalls++; returnArgsLen = arguments.length; return 1; },
          };
        };

        async function* g() { yield* delegate; }

        const it = g();
        it.next().then(function (r) {
          next1Value = r.value;
          next1Done = r.done;
          return it.return("ok");
        }).then(
          function () { out = "resolved"; },
          function (e) { out = errToString(e); }
        );

        out
      "#,
    ) {
      Ok(v) => v,
      Err(err) if is_unimplemented_async_generator_error(&mut rt, &err)? => return Ok(()),
      Err(err) => return Err(err),
    };
    assert_eq!(value_to_string(&rt, value), "");

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let next1_value = rt.exec_script("next1Value")?;
    assert_eq!(next1_value, Value::Number(1.0));
    let next1_done = rt.exec_script("next1Done")?;
    assert_eq!(next1_done, Value::Bool(false));

    let out = rt.exec_script("out")?;
    assert_eq!(value_to_string(&rt, out), "TypeError");

    let return_calls = rt.exec_script("returnCalls")?;
    assert_eq!(return_calls, Value::Number(1.0));
    let return_args_len = rt.exec_script("returnArgsLen")?;
    assert_eq!(return_args_len, Value::Number(1.0));

    Ok(())
  })();

  rt.teardown_microtasks();
  result
}

#[test]
fn yield_star_throw_suppresses_close_error_from_return_promise_reject() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let result: Result<(), VmError> = (|| {
    let value = match rt.exec_script(
      r#"
        function errToString(e) {
          if (typeof e === "string") return e;
          if (e && e.name) return e.name;
          return "" + e;
        }

        var next1Value = null;
        var next1Done = null;
        var throwValue = null;
        var throwDone = null;
        var returnCalls = 0;
        var closed = false;
        var err = "";

        const delegate = {};
        delegate[Symbol.asyncIterator] = function () {
          return {
            next() { return Promise.resolve({ value: 1, done: false }); },
            // Close error: `return()` returns a Promise that rejects.
            return() {
              returnCalls++;
              return Promise.resolve().then(function () {
                closed = true;
                return Promise.reject("close");
              });
            },
          };
        };

        async function* g() {
          try { yield* delegate; }
          catch (e) { yield "caught:" + e + ":" + closed; }
        }

        const it = g();
        it.next().then(function (r) {
          next1Value = r.value;
          next1Done = r.done;
          return it.throw("boom");
        }).then(function (r) {
          throwValue = r.value;
          throwDone = r.done;
        }, function (e) {
          err = errToString(e);
        });

        err
      "#,
    ) {
      Ok(v) => v,
      Err(err) if is_unimplemented_async_generator_error(&mut rt, &err)? => return Ok(()),
      Err(err) => return Err(err),
    };
    assert_eq!(value_to_string(&rt, value), "");

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let next1_value = rt.exec_script("next1Value")?;
    assert_eq!(next1_value, Value::Number(1.0));
    let next1_done = rt.exec_script("next1Done")?;
    assert_eq!(next1_done, Value::Bool(false));

    let throw_value = rt.exec_script("throwValue")?;
    assert_eq!(value_to_string(&rt, throw_value), "caught:boom:true");
    let throw_done = rt.exec_script("throwDone")?;
    assert_eq!(throw_done, Value::Bool(false));

    let return_calls = rt.exec_script("returnCalls")?;
    assert_eq!(return_calls, Value::Number(1.0));

    let err = rt.exec_script("err")?;
    assert_eq!(value_to_string(&rt, err), "");

    Ok(())
  })();

  rt.teardown_microtasks();
  result
}

#[test]
fn yield_star_return_does_not_suppress_close_error_from_return_promise_reject() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let result: Result<(), VmError> = (|| {
    let value = match rt.exec_script(
      r#"
        function errToString(e) {
          if (typeof e === "string") return e;
          if (e && e.name) return e.name;
          return "" + e;
        }

        var next1Value = null;
        var next1Done = null;
        var returnCalls = 0;
        var closed = false;
        var out = "";

        const delegate = {};
        delegate[Symbol.asyncIterator] = function () {
          return {
            next() { return Promise.resolve({ value: 1, done: false }); },
            return(v) {
              returnCalls++;
              return Promise.resolve().then(function () {
                closed = true;
                return Promise.reject("close");
              });
            },
          };
        };

        async function* g() { yield* delegate; }

        const it = g();
        it.next().then(function (r) {
          next1Value = r.value;
          next1Done = r.done;
          return it.return("ok");
        }).then(
          function () { out = "resolved:" + closed; },
          function (e) { out = errToString(e) + ":" + closed; }
        );

        out
      "#,
    ) {
      Ok(v) => v,
      Err(err) if is_unimplemented_async_generator_error(&mut rt, &err)? => return Ok(()),
      Err(err) => return Err(err),
    };
    assert_eq!(value_to_string(&rt, value), "");

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let next1_value = rt.exec_script("next1Value")?;
    assert_eq!(next1_value, Value::Number(1.0));
    let next1_done = rt.exec_script("next1Done")?;
    assert_eq!(next1_done, Value::Bool(false));

    let out = rt.exec_script("out")?;
    assert_eq!(value_to_string(&rt, out), "close:true");

    let return_calls = rt.exec_script("returnCalls")?;
    assert_eq!(return_calls, Value::Number(1.0));

    Ok(())
  })();

  rt.teardown_microtasks();
  result
}
