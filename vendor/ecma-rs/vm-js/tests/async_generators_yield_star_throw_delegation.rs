use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Async generators allocate promise jobs, async iterator records, and yield* state. Use a larger
  // heap than the 1MiB default used by many unit tests to avoid spurious OOM failures.
  let heap = Heap::new(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn yield_star_throw_delegates_to_delegate_throw_and_continues() -> Result<(), VmError> {
  let mut rt = new_runtime();

  // Ensure queued microtasks are torn down even if this test fails.
  let result: Result<(), VmError> = (|| {
    let value = rt.exec_script(
      r#"
        var log = "";
        var ok = false;
        var err = "";

        var delegate = {
          i: 0,
          next: function (v) {
            log += "n" + v;
            if (this.i++ === 0) return Promise.resolve({ value: 1, done: false });
            return Promise.resolve({ value: 2, done: true });
          },
          throw: function (e) {
            log += "t" + e;
            return Promise.resolve({ value: 99, done: true });
          },
          // A return method should NOT be used when `throw` exists. Log if it is invoked so this
          // test catches incorrect AsyncIteratorClose behavior.
          return: function (v) {
            log += "r" + v;
            return Promise.resolve({ value: 77, done: true });
          },
          [Symbol.asyncIterator]: function () {
            log += "i";
            return this;
          }
        };

        async function* g() {
          const r = yield* delegate;
          yield "r:" + r;
        }

        var it = g();

        async function run() {
          try {
            const r1 = await it.next();
            const r2 = await it.throw("X");
            const r3 = await it.next();
            ok =
              r1.value === 1 && r1.done === false &&
              r2.value === "r:99" && r2.done === false &&
              r3.value === undefined && r3.done === true &&
              log === "inundefinedtX";
          } catch (e) {
            err = "" + e;
          }
        }

        run();
        ok
      "#,
    )?;
    assert_eq!(value, Value::Bool(false));

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let ok = rt.exec_script("ok")?;
    assert_eq!(ok, Value::Bool(true));

    let err = rt.exec_script("err")?;
    assert_eq!(value_to_string(&rt, err), "");

    let log = rt.exec_script("log")?;
    assert_eq!(value_to_string(&rt, log), "inundefinedtX");

    Ok(())
  })();

  rt.teardown_microtasks();
  result
}

#[test]
fn yield_star_throw_without_delegate_throw_closes_then_throws_into_generator() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let result: Result<(), VmError> = (|| {
    let value = rt.exec_script(
      r#"
        var ok = false;
        var err = "";
        var closed = false;
        var returnCalls = 0;

        var delegate = {
          i: 0,
          next: function () {
            if (this.i++ === 0) return Promise.resolve({ value: 1, done: false });
            return Promise.resolve({ value: 2, done: true });
          },
          // No `throw` method.
          return: function () {
            returnCalls++;
            // Make the close async so the test asserts that AsyncIteratorClose is awaited before the
            // generator's catch block runs.
            return Promise.resolve().then(() => {
              closed = true;
              return { done: true };
            });
          },
          [Symbol.asyncIterator]: function () { return this; }
        };

        async function* g() {
          try {
            yield* delegate;
          } catch (e) {
            yield "caught:" + e + ":" + closed;
          }
        }

        var it = g();

        async function run() {
          try {
            const r1 = await it.next();
            const r2 = await it.throw("boom");
            const r3 = await it.next();
            ok =
              r1.value === 1 && r1.done === false &&
              r2.value === "caught:boom:true" && r2.done === false &&
              r3.value === undefined && r3.done === true &&
              returnCalls === 1;
          } catch (e) {
            err = "" + e;
          }
        }

        run();
        ok
      "#,
    )?;
    assert_eq!(value, Value::Bool(false));

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let ok = rt.exec_script("ok")?;
    assert_eq!(ok, Value::Bool(true));

    let err = rt.exec_script("err")?;
    assert_eq!(value_to_string(&rt, err), "");

    Ok(())
  })();

  rt.teardown_microtasks();
  result
}
