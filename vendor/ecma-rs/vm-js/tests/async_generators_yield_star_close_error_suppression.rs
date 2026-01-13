use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

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

#[test]
fn yield_star_throw_suppresses_close_error_from_return_getter() -> Result<(), VmError> {
  let mut rt = new_runtime();

  // Ensure we don't leak queued microtasks even if this test fails.
  let result: Result<(), VmError> = (|| {
    let value = rt.exec_script(
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
        var err = "";

        const delegate = {};
        delegate[Symbol.asyncIterator] = function () {
          return {
            next() { return Promise.resolve({ value: 1, done: false }); },
            // Close error: getting `return` throws.
            get return() { throw "close"; },
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
    )?;
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
    let value = rt.exec_script(
      r#"
        function errToString(e) {
          if (typeof e === "string") return e;
          if (e && e.name) return e.name;
          return "" + e;
        }

        var next1Value = null;
        var next1Done = null;
        var out = "";

        const delegate = {};
        delegate[Symbol.asyncIterator] = function () {
          return {
            next() { return Promise.resolve({ value: 1, done: false }); },
            // Close error: getting `return` throws.
            get return() { throw "close"; },
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
    )?;
    assert_eq!(value_to_string(&rt, value), "");

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let next1_value = rt.exec_script("next1Value")?;
    assert_eq!(next1_value, Value::Number(1.0));
    let next1_done = rt.exec_script("next1Done")?;
    assert_eq!(next1_done, Value::Bool(false));

    let out = rt.exec_script("out")?;
    assert_eq!(value_to_string(&rt, out), "close");
    Ok(())
  })();

  rt.teardown_microtasks();
  result
}

