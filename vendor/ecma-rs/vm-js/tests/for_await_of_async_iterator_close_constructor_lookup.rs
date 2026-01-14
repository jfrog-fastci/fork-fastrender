use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // `for await...of` exercises async iteration + Promise/job queuing. With ongoing vm-js builtin
  // growth, a 1MiB heap can be too tight and cause spurious `VmError::OutOfMemory` failures that are
  // not relevant to the semantics being tested here.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn for_await_of_break_close_promise_constructor_getter_is_not_observed_twice() -> Result<(), VmError> {
  let mut rt = new_runtime();

  // Avoid leaking queued microtasks even if an assertion fails.
  let result: Result<(), VmError> = (|| {
    let value = rt.exec_script(
      r#"
        var ctorCalls = 0;
        var returnCalls = 0;
        var out = false;

        const closePromise = Promise.resolve({});
        Object.defineProperty(closePromise, "constructor", {
          get() {
            ctorCalls++;
            return Promise;
          },
        });

        const iterable = {};
        iterable[Symbol.asyncIterator] = function () {
          return {
            next() {
              return Promise.resolve({ value: 1, done: false });
            },
            return() {
              returnCalls++;
              return closePromise;
            },
          };
        };

        async function f() {
          for await (const _ of iterable) {
            break;
          }
        }

        f().then(function () { out = true; });

        out
      "#,
    )?;
    assert_eq!(value, Value::Bool(false));

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    // `AsyncIteratorClose` must only perform its internal `PromiseResolve` once; the outer async
    // suspension machinery must not `PromiseResolve` the returned promise again (or
    // `closePromise.constructor` would be observed twice).
    let ctor_calls = rt.exec_script("ctorCalls")?;
    assert_eq!(ctor_calls, Value::Number(1.0));

    let return_calls = rt.exec_script("returnCalls")?;
    assert_eq!(return_calls, Value::Number(1.0));

    let out = rt.exec_script("out")?;
    assert_eq!(out, Value::Bool(true));

    Ok(())
  })();

  rt.teardown_microtasks();
  result
}

