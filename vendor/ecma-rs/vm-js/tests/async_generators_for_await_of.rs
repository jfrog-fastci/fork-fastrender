use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

mod _async_generator_support;

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // `for await...of` in async generators exercises async iteration + generator request queues +
  // Promise/job queuing. With ongoing vm-js builtin growth, a 1MiB heap can be too tight and cause
  // spurious `VmError::OutOfMemory` failures that are not relevant to the semantics being tested.
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
  // First, ensure core async generator execution support exists. This checks `.next()` semantics and
  // also drains any microtasks queued by the probe.
  if !_async_generator_support::supports_async_generators(rt)? {
    return Ok(false);
  }

  let result: Result<bool, VmError> = (|| {
    // Now probe `for await..of` specifically inside async generators.
    match rt.exec_script(
      r#"
        async function* __probe() {
          for await (const x of [Promise.resolve(1)]) {
            yield x;
          }
        }
        __probe().next();
      "#,
    ) {
      Ok(_) => {}
      Err(err)
        if _async_generator_support::is_unimplemented_async_generator_error(rt, &err)?
          || matches!(err, VmError::Unimplemented(msg) if msg.contains("for await..of")) =>
      {
        return Ok(false);
      }
      Err(err) => return Err(err),
    }

    // Running the microtask queue is required to surface host-level errors during async generator
    // execution (including the `for await..of` unimplemented path).
    match rt.vm.perform_microtask_checkpoint(&mut rt.heap) {
      Ok(()) => Ok(true),
      Err(err)
        if _async_generator_support::is_unimplemented_async_generator_error(rt, &err)?
          || matches!(err, VmError::Unimplemented(msg) if msg.contains("for await..of")) =>
      {
        Ok(false)
      }
      Err(err) => Err(err),
    }
  })();

  // Always clear the microtask/job queue after feature detection so we don't leak work into later
  // assertions (or other tests) if the probe partially succeeded before reporting "unsupported".
  rt.teardown_microtasks();

  result
}

#[test]
fn for_await_of_inside_async_generator_yields_awaited_values() -> Result<(), VmError> {
  let mut rt = new_runtime();

  if !async_generators_supported(&mut rt)? {
    return Ok(());
  }

  // Ensure we don't leak queued microtasks even if this test fails.
  let result: Result<(), VmError> = (|| {
    let value = rt.exec_script(
      r#"
        var out = "";

        async function* g() {
          for await (const x of [Promise.resolve(1), 2]) {
            yield x;
          }
        }

        async function f() {
          const it = g();
          const r1 = await it.next();
          const r2 = await it.next();
          const r3 = await it.next();
          return (
            r1.value + "," + r1.done + "," +
            r2.value + "," + r2.done + "," +
            String(r3.value) + "," + r3.done
          );
        }

        f().then(
          function (v) { out = v; },
          function (e) { out = "err:" + ((e && e.name) || e); }
        );

        out
      "#,
    )?;
    assert_eq!(value_to_string(&rt, value), "");

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let out = rt.exec_script("out")?;
    assert_eq!(value_to_string(&rt, out), "1,false,2,false,undefined,true");
    Ok(())
  })();

  rt.teardown_microtasks();
  result
}

#[test]
fn for_await_of_break_inside_async_generator_awaits_iterator_return() -> Result<(), VmError> {
  let mut rt = new_runtime();

  if !async_generators_supported(&mut rt)? {
    return Ok(());
  }

  let result: Result<(), VmError> = (|| {
    let value = rt.exec_script(
      r#"
        var out = "";

        async function f() {
          var closed = false;
          const iterable = {};
          iterable[Symbol.asyncIterator] = function () {
            return {
              next() {
                return Promise.resolve({ value: 1, done: false });
              },
              return() {
                // Side effect happens asynchronously to ensure `for await..of` awaits `return()`.
                return Promise.resolve().then(function () {
                  closed = true;
                  return { done: true };
                });
              },
            };
          };

          async function* g() {
            for await (const _x of iterable) {
              break;
            }
            yield closed;
          }

          const it = g();
          const r1 = await it.next();
          const r2 = await it.next();
          return String(r1.value) + "," + String(r1.done) + "," + String(r2.done);
        }

        f().then(
          function (v) { out = v; },
          function (e) { out = "err:" + ((e && e.name) || e); }
        );

        out
      "#,
    )?;
    assert_eq!(value_to_string(&rt, value), "");

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let out = rt.exec_script("out")?;
    assert_eq!(value_to_string(&rt, out), "true,false,true");
    Ok(())
  })();

  rt.teardown_microtasks();
  result
}

#[test]
fn for_await_of_return_inside_async_generator_awaits_iterator_return() -> Result<(), VmError> {
  let mut rt = new_runtime();

  if !async_generators_supported(&mut rt)? {
    return Ok(());
  }

  let result: Result<(), VmError> = (|| {
    let value = rt.exec_script(
      r#"
        var out = "";

        async function f() {
          var closed = false;
          const iterable = {};
          iterable[Symbol.asyncIterator] = function () {
            return {
              next() {
                return Promise.resolve({ value: 1, done: false });
              },
              return() {
                // Side effect happens asynchronously to ensure `for await..of` awaits `return()`.
                return Promise.resolve().then(function () {
                  closed = true;
                  return { done: true };
                });
              },
            };
          };

          async function* g() {
            for await (const _x of iterable) {
              return "done";
            }
          }

          const it = g();
          const r1 = await it.next();
          return String(r1.value) + "," + String(r1.done) + "," + String(closed);
        }

        f().then(
          function (v) { out = v; },
          function (e) { out = "err:" + ((e && e.name) || e); }
        );

        out
      "#,
    )?;
    assert_eq!(value_to_string(&rt, value), "");

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let out = rt.exec_script("out")?;
    assert_eq!(value_to_string(&rt, out), "done,true,true");
    Ok(())
  })();

  rt.teardown_microtasks();
  result
}

#[test]
fn for_await_of_throw_inside_async_generator_awaits_iterator_return() -> Result<(), VmError> {
  let mut rt = new_runtime();

  if !async_generators_supported(&mut rt)? {
    return Ok(());
  }

  let result: Result<(), VmError> = (|| {
    let value = rt.exec_script(
      r#"
        var out = "";

        async function f() {
          var closed = false;
          const iterable = {};
          iterable[Symbol.asyncIterator] = function () {
            return {
              next() {
                return Promise.resolve({ value: 1, done: false });
              },
              return() {
                // Side effect happens asynchronously to ensure `for await..of` awaits `return()`.
                return Promise.resolve().then(function () {
                  closed = true;
                  return { done: true };
                });
              },
            };
          };

          async function* g() {
            for await (const _x of iterable) {
              throw "boom";
            }
          }

          const it = g();
          try {
            await it.next();
            return "no-throw";
          } catch (e) {
            return String(closed) + "," + String(e);
          }
        }

        f().then(
          function (v) { out = v; },
          function (e) { out = "err:" + ((e && e.name) || e); }
        );

        out
      "#,
    )?;
    assert_eq!(value_to_string(&rt, value), "");

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let out = rt.exec_script("out")?;
    assert_eq!(value_to_string(&rt, out), "true,boom");
    Ok(())
  })();

  rt.teardown_microtasks();
  result
}

#[test]
fn for_await_of_async_generator_return_closes_inner_iterator() -> Result<(), VmError> {
  let mut rt = new_runtime();

  if !async_generators_supported(&mut rt)? {
    return Ok(());
  }

  let result: Result<(), VmError> = (|| {
    let value = rt.exec_script(
      r#"
        var out = "";

        async function f() {
          var closed = false;
          const iterable = {};
          iterable[Symbol.asyncIterator] = function () {
            var i = 0;
            return {
              next() {
                i++;
                if (i === 1) return Promise.resolve({ value: 1, done: false });
                if (i === 2) return Promise.resolve({ value: 2, done: false });
                return Promise.resolve({ done: true });
              },
              return() {
                // Side effect happens asynchronously to ensure the async generator's `.return()`
                // request awaits the inner iterator close.
                return Promise.resolve().then(function () {
                  closed = true;
                  return { done: true };
                });
              },
            };
          };

          async function* g() {
            for await (const x of iterable) {
              yield x;
            }
          }

          const it = g();
          const r1 = await it.next();
          const rret = await it.return("stop");
          return (
            String(r1.value) + "," +
            String(rret.value) + "," + String(rret.done) + "," +
            String(closed)
          );
        }

        f().then(
          function (v) { out = v; },
          function (e) { out = "err:" + ((e && e.name) || e); }
        );

        out
      "#,
    )?;
    assert_eq!(value_to_string(&rt, value), "");

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let out = rt.exec_script("out")?;
    assert_eq!(value_to_string(&rt, out), "1,stop,true,true");
    Ok(())
  })();

  rt.teardown_microtasks();
  result
}

#[test]
fn for_await_of_async_generator_throw_closes_inner_iterator() -> Result<(), VmError> {
  let mut rt = new_runtime();

  if !async_generators_supported(&mut rt)? {
    return Ok(());
  }

  let result: Result<(), VmError> = (|| {
    let value = rt.exec_script(
      r#"
        var out = "";

        async function f() {
          var closed = false;
          const iterable = {};
          iterable[Symbol.asyncIterator] = function () {
            var i = 0;
            return {
              next() {
                i++;
                if (i === 1) return Promise.resolve({ value: 1, done: false });
                if (i === 2) return Promise.resolve({ value: 2, done: false });
                return Promise.resolve({ done: true });
              },
              return() {
                // Side effect happens asynchronously to ensure the async generator's `.throw()`
                // request awaits the inner iterator close.
                return Promise.resolve().then(function () {
                  closed = true;
                  return { done: true };
                });
              },
            };
          };

          async function* g() {
            for await (const x of iterable) {
              yield x;
            }
          }

          const it = g();
          const r1 = await it.next();
          try {
            await it.throw("boom");
            return "no-throw";
          } catch (e) {
            return String(r1.value) + "," + String(e) + "," + String(closed);
          }
        }

        f().then(
          function (v) { out = v; },
          function (e) { out = "err:" + ((e && e.name) || e); }
        );

        out
      "#,
    )?;
    assert_eq!(value_to_string(&rt, value), "");

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let out = rt.exec_script("out")?;
    assert_eq!(value_to_string(&rt, out), "1,boom,true");
    Ok(())
  })();

  rt.teardown_microtasks();
  result
}

#[test]
fn for_await_of_break_propagates_async_iterator_return_error() -> Result<(), VmError> {
  let mut rt = new_runtime();

  if !async_generators_supported(&mut rt)? {
    return Ok(());
  }

  let result: Result<(), VmError> = (|| {
    let value = rt.exec_script(
      r#"
        var out = "";

        async function f() {
          var closed = false;
          const iterable = {};
          iterable[Symbol.asyncIterator] = function () {
            return {
              next() {
                return Promise.resolve({ value: 1, done: false });
              },
              return() {
                // Ensure the close error is observed asynchronously.
                return Promise.resolve().then(function () {
                  closed = true;
                  throw "close";
                });
              },
            };
          };

          async function* g() {
            for await (const _x of iterable) {
              break;
            }
            yield "after";
          }

          const it = g();
          try {
            await it.next();
            return "no-error";
          } catch (e) {
            return String(e) + "," + String(closed);
          }
        }

        f().then(
          function (v) { out = v; },
          function (e) { out = "err:" + ((e && e.name) || e); }
        );

        out
      "#,
    )?;
    assert_eq!(value_to_string(&rt, value), "");

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let out = rt.exec_script("out")?;
    assert_eq!(value_to_string(&rt, out), "close,true");
    Ok(())
  })();

  rt.teardown_microtasks();
  result
}

#[test]
fn for_await_of_throw_suppresses_iterator_return_error() -> Result<(), VmError> {
  let mut rt = new_runtime();

  if !async_generators_supported(&mut rt)? {
    return Ok(());
  }

  let result: Result<(), VmError> = (|| {
    let value = rt.exec_script(
      r#"
        var out = "";

        async function f() {
          var closed = false;
          const iterable = {};
          iterable[Symbol.asyncIterator] = function () {
            return {
              next() {
                return Promise.resolve({ value: 1, done: false });
              },
              return() {
                closed = true;
                throw "close";
              },
            };
          };

          async function* g() {
            for await (const _x of iterable) {
              throw "boom";
            }
          }

          const it = g();
          try {
            await it.next();
            return "no-error";
          } catch (e) {
            return String(e) + "," + String(closed);
          }
        }

        f().then(
          function (v) { out = v; },
          function (e) { out = "err:" + ((e && e.name) || e); }
        );

        out
      "#,
    )?;
    assert_eq!(value_to_string(&rt, value), "");

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let out = rt.exec_script("out")?;
    assert_eq!(value_to_string(&rt, out), "boom,true");
    Ok(())
  })();

  rt.teardown_microtasks();
  result
}

#[test]
fn for_await_of_async_generator_return_rejects_if_iterator_return_rejects() -> Result<(), VmError> {
  let mut rt = new_runtime();

  if !async_generators_supported(&mut rt)? {
    return Ok(());
  }

  let result: Result<(), VmError> = (|| {
    let value = rt.exec_script(
      r#"
        var out = "";

        async function f() {
          var closed = false;
          const iterable = {};
          iterable[Symbol.asyncIterator] = function () {
            return {
              next() {
                return Promise.resolve({ value: 1, done: false });
              },
              return() {
                return Promise.resolve().then(function () {
                  closed = true;
                  throw "close";
                });
              },
            };
          };

          async function* g() {
            for await (const x of iterable) {
              yield x;
            }
          }

          const it = g();
          const r1 = await it.next();
          try {
            await it.return("stop");
            return "no-error";
          } catch (e) {
            return String(r1.value) + "," + String(e) + "," + String(closed);
          }
        }

        f().then(
          function (v) { out = v; },
          function (e) { out = "err:" + ((e && e.name) || e); }
        );

        out
      "#,
    )?;
    assert_eq!(value_to_string(&rt, value), "");

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let out = rt.exec_script("out")?;
    assert_eq!(value_to_string(&rt, out), "1,close,true");
    Ok(())
  })();

  rt.teardown_microtasks();
  result
}

#[test]
fn for_await_of_async_generator_throw_suppresses_close_error_if_iterator_return_rejects(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

  if !async_generators_supported(&mut rt)? {
    return Ok(());
  }

  let result: Result<(), VmError> = (|| {
    let value = rt.exec_script(
      r#"
        var out = "";

        async function f() {
          var closed = false;
          const iterable = {};
          iterable[Symbol.asyncIterator] = function () {
            return {
              next() {
                return Promise.resolve({ value: 1, done: false });
              },
              return() {
                return Promise.resolve().then(function () {
                  closed = true;
                  throw "close";
                });
              },
            };
          };

          async function* g() {
            for await (const x of iterable) {
              yield x;
            }
          }

          const it = g();
          const r1 = await it.next();
          try {
            await it.throw("boom");
            return "no-error";
          } catch (e) {
            return String(r1.value) + "," + String(e) + "," + String(closed);
          }
        }

        f().then(
          function (v) { out = v; },
          function (e) { out = "err:" + ((e && e.name) || e); }
        );

        out
      "#,
    )?;
    assert_eq!(value_to_string(&rt, value), "");

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let out = rt.exec_script("out")?;
    assert_eq!(value_to_string(&rt, out), "1,boom,true");
    Ok(())
  })();

  rt.teardown_microtasks();
  result
}
