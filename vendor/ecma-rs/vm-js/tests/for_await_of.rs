use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn for_await_over_array_awaits_values() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        let log = "";
        for await (const x of [Promise.resolve("a"), "b"]) {
          log += x;
        }
        return log;
      }
      f().then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "ab");
  Ok(())
}

#[test]
fn await_in_for_await_of_lhs_destructuring_default_value() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        for await (const { x = await Promise.resolve("ok") } of [ {} ]) {
          return x;
        }
        return "bad";
      }
      f().then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "ok");
  Ok(())
}

#[test]
fn await_in_for_await_of_lhs_destructuring_computed_key() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        for await (const { [await Promise.resolve("x")]: v } of [ { x: "ok" } ]) {
          return v;
        }
        return "bad";
      }
      f().then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "ok");
  Ok(())
}

#[test]
fn for_await_over_custom_async_iterable() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        let log = "";
        const iterable = {};
        iterable[Symbol.asyncIterator] = function () {
          let i = 0;
          return {
            next() {
              i++;
              if (i === 1) return Promise.resolve({ value: "a", done: false });
              if (i === 2) return Promise.resolve({ value: "b", done: false });
              return Promise.resolve({ value: undefined, done: true });
            },
          };
        };
        for await (const x of iterable) {
          log += x;
        }
        return log;
      }
      f().then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "ab");
  Ok(())
}

#[test]
fn for_await_break_closes_iterator() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        let log = "";
        const iterable = {};
        iterable[Symbol.asyncIterator] = function () {
          return {
            i: 0,
            next() {
              this.i++;
              if (this.i === 1) return Promise.resolve({ value: "a", done: false });
              return Promise.resolve({ value: "b", done: false });
            },
            return() {
              // Side effect happens asynchronously to ensure `for await..of` awaits `return()`.
              return Promise.resolve().then(function () {
                log += "R";
                return { done: true };
              });
            },
          };
        };
        for await (const x of iterable) {
          log += x;
          break;
        }
        return log;
      }
      f().then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "aR");
  Ok(())
}

#[test]
fn for_await_rejected_next_rejects_async_function_promise() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        const iterable = {};
        iterable[Symbol.asyncIterator] = function () {
          return {
            next() {
              return Promise.reject("boom");
            },
          };
        };
        for await (const x of iterable) {
          // Never reached.
          out = "bad";
        }
        return "ok";
      }
      f().then(function () { out = "bad"; }, function (e) { out = e; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "boom");
  Ok(())
}

#[test]
fn for_await_of_break_calls_iterator_return_on_custom_array_iterator() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var returnCalls = 0;

      const arr = [1, 2, 3];
      arr[Symbol.iterator] = function () {
        let i = 0;
        return {
          next() {
            if (i >= 3) return { value: undefined, done: true };
            return { value: i++, done: false };
          },
          return() {
            returnCalls++;
            return { done: true };
          },
        };
      };

      (async function () {
        for await (const x of arr) {
          break;
        }
      })();

      returnCalls;
    "#,
  )?;
  assert_eq!(value, Value::Number(0.0));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("returnCalls")?;
  assert_eq!(value, Value::Number(1.0));
  Ok(())
}

#[test]
fn for_await_of_throw_calls_iterator_return_on_custom_array_iterator() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      var returnCalls = 0;

      const arr = [1, 2, 3];
      arr[Symbol.iterator] = function () {
        let i = 0;
        return {
          next() {
            if (i >= 3) return { value: undefined, done: true };
            return { value: i++, done: false };
          },
          return() {
            returnCalls++;
            return { done: true };
          },
        };
      };

      async function f() {
        for await (const x of arr) {
          throw "boom";
        }
      }

      f().then(function () { out = "bad"; }, function (e) { out = e; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "boom");

  let return_calls = rt.exec_script("returnCalls")?;
  assert_eq!(return_calls, Value::Number(1.0));
  Ok(())
}

#[test]
fn for_await_of_break_awaits_sync_iterator_return_promise() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";

      async function f() {
        var log = "";
        var returnCalls = 0;

        const arr = [1, 2, 3];
        arr[Symbol.iterator] = function () {
          let i = 0;
          return {
            next() {
              i++;
              if (i === 1) return { value: "a", done: false };
              return { value: undefined, done: true };
            },
            return() {
              returnCalls++;
              return Promise.resolve().then(function () {
                log += "R";
                return { done: true };
              });
            },
          };
        };

        for await (const x of arr) {
          log += x;
          break;
        }
        return log + ":" + returnCalls;
      }

      f().then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "aR:1");
  Ok(())
}

#[test]
fn for_await_of_break_does_not_call_array_return_getter_with_default_iterator() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";

      async function f() {
        const arr = [1, 2, 3];
        Object.defineProperty(arr, "return", {
          get() { throw "wrong"; },
        });

        try {
          for await (const x of arr) {
            break;
          }
          out = "ok";
        } catch (e) {
          out = e;
        }
      }

      f();
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "ok");
  Ok(())
}

#[test]
fn for_await_of_throw_suppresses_iterator_return_throw() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      var returnCalls = 0;

      const iterable = {};
      iterable[Symbol.asyncIterator] = function () {
        return {
          next() {
            return Promise.resolve({ value: 1, done: false });
          },
          return() {
            returnCalls++;
            throw "close";
          },
        };
      };

      async function f() {
        for await (const x of iterable) {
          throw "body";
        }
      }

      f().then(function () { out = "bad"; }, function (e) { out = e; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "body");

  let return_calls = rt.exec_script("returnCalls")?;
  assert_eq!(return_calls, Value::Number(1.0));
  Ok(())
}

#[test]
fn for_await_of_break_propagates_iterator_return_throw() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      var returnCalls = 0;

      const iterable = {};
      iterable[Symbol.asyncIterator] = function () {
        return {
          next() {
            return Promise.resolve({ value: 1, done: false });
          },
          return() {
            returnCalls++;
            throw "close";
          },
        };
      };

      async function f() {
        for await (const x of iterable) {
          break;
        }
        return "ok";
      }

      f().then(function (v) { out = v; }, function (e) { out = e; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "close");

  let return_calls = rt.exec_script("returnCalls")?;
  assert_eq!(return_calls, Value::Number(1.0));
  Ok(())
}

#[test]
fn for_await_of_throw_suppresses_iterator_return_rejection() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      var returnCalls = 0;

      const iterable = {};
      iterable[Symbol.asyncIterator] = function () {
        return {
          next() {
            return Promise.resolve({ value: 1, done: false });
          },
          return() {
            returnCalls++;
            return Promise.reject("close");
          },
        };
      };

      async function f() {
        for await (const x of iterable) {
          throw "body";
        }
      }

      f().then(function () { out = "bad"; }, function (e) { out = e; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "body");

  let return_calls = rt.exec_script("returnCalls")?;
  assert_eq!(return_calls, Value::Number(1.0));
  Ok(())
}

#[test]
fn for_await_of_break_propagates_iterator_return_rejection() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      var returnCalls = 0;

      const iterable = {};
      iterable[Symbol.asyncIterator] = function () {
        return {
          next() {
            return Promise.resolve({ value: 1, done: false });
          },
          return() {
            returnCalls++;
            return Promise.reject("close");
          },
        };
      };

      async function f() {
        for await (const x of iterable) {
          break;
        }
        return "ok";
      }

      f().then(function (v) { out = v; }, function (e) { out = e; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "close");

  let return_calls = rt.exec_script("returnCalls")?;
  assert_eq!(return_calls, Value::Number(1.0));
  Ok(())
}
