use vm_js::{Heap, HeapLimits, JsRuntime, PromiseState, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // `for await...of` exercises async iteration + Promise/job queuing. With ongoing vm-js builtin
  // growth, a 1MiB heap can be too tight and cause spurious `VmError::OutOfMemory` failures that
  // are not relevant to the semantics being tested here.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
}

fn assert_actual_join(rt: &mut JsRuntime, expected: &str) -> Result<(), VmError> {
  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;
  let value = rt.exec_script("actual.join(',')")?;
  assert_eq!(value_to_string(rt, value), expected);
  Ok(())
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
fn top_level_for_await_of_script_returns_promise_and_resolves() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var sum = 0;
      for await (const x of [Promise.resolve(1), 2]) {
        sum += x;
      }
      sum
    "#,
  )?;

  let Value::Object(promise) = value else {
    panic!("expected Promise, got {value:?}");
  };
  assert_eq!(rt.heap.promise_state(promise)?, PromiseState::Pending);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.heap.promise_state(promise)?, PromiseState::Fulfilled);
  assert_eq!(rt.heap.promise_result(promise)?, Some(Value::Number(3.0)));
  Ok(())
}

#[test]
fn top_level_for_await_break_awaits_iterator_return() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
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
              return { done: true };
            });
          },
        };
      };

      for await (const x of iterable) {
        break;
      }
      closed
    "#,
  )?;

  let Value::Object(promise) = value else {
    panic!("expected Promise, got {value:?}");
  };

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.heap.promise_state(promise)?, PromiseState::Fulfilled);
  assert_eq!(rt.heap.promise_result(promise)?, Some(Value::Bool(true)));
  Ok(())
}

#[test]
fn top_level_for_await_throw_awaits_iterator_close_before_catch() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
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
              return { done: true };
            });
          },
        };
      };

      var out = "unset";
      try {
        for await (const x of iterable) {
          throw "boom";
        }
        out = "bad";
      } catch (e) {
        out = closed ? "closed" : "not closed";
      }

      out
    "#,
  )?;

  let Value::Object(promise) = value else {
    panic!("expected Promise, got {value:?}");
  };

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.heap.promise_state(promise)?, PromiseState::Fulfilled);
  let out = rt
    .heap
    .promise_result(promise)?
    .expect("expected promise result");
  assert_eq!(value_to_string(&rt, out), "closed");
  Ok(())
}

#[test]
fn for_await_of_array_destructuring_closes_inner_iterator() -> Result<(), VmError> {
  let mut rt = new_runtime();

  // This exercises `BindingInitialization` for an ArrayBindingPattern inside `for await...of`.
  // The loop yields a *custom iterable object* and the LHS destructuring pattern `[x]` must use
  // `@@iterator` + `IteratorClose` (not array-like indexing) to bind `x`.
  //
  // In particular, the iterator returned by `iter[Symbol.iterator]()` is not exhausted after
  // binding `x`, so `IteratorClose` must be performed and call `return()`.
  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        var returnCalls = 0;
        var iter = {};
        iter[Symbol.iterator] = function () {
          return {
            next() { return { value: 1, done: false }; },
            return() { returnCalls++; return {}; },
          };
        };

        for await (const [x] of [iter]) {
          return returnCalls + ":" + x;
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
  assert_eq!(value_to_string(&rt, value), "1:1");
  Ok(())
}

#[test]
fn for_await_of_array_destructuring_yield_in_default_closes_on_return_non_object() -> Result<(), VmError> {
  let mut rt = new_runtime();

  // Regression test for async generator `return` completion propagating through `for await..of`
  // destructuring binding initialization.
  //
  // When the async generator is closed while suspended in a destructuring default initializer,
  // the inner iterator must be closed via `IteratorClose` with a *non-throw* completion. This means
  // that a non-object `iterator.return()` result must throw a TypeError and override the return.
  let value = rt.exec_script(
    r#"
      var out = "";
      var closed = false;

      async function* gen() {
        var iter = {};
        iter[Symbol.iterator] = function () {
          return {
            next() { return { value: undefined, done: false }; },
            return() { closed = true; return null; },
          };
        };

        for await (const [x = yield "Y"] of [iter]) {
          // Never reached: the generator is closed while suspended at `yield`.
        }
      }

      (async function () {
        var it = gen();
        var r1 = await it.next();
        out += "next:" + r1.value + ":" + r1.done;

        try {
          await it.return("done");
          out += "|return:ok";
        } catch (e) {
          out += "|return:" + (e && e.name);
        }
        out += "|closed:" + closed;
      })();

      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(
    value_to_string(&rt, value),
    "next:Y:false|return:TypeError|closed:true"
  );
  Ok(())
}

#[test]
fn for_await_of_array_destructuring_assignment_yield_in_target_precedes_iterator_next() -> Result<(), VmError> {
  let mut rt = new_runtime();

  // Regression test for `IteratorDestructuringAssignmentEvaluation` order in `for await..of`:
  // when the LHS is a destructuring *assignment* pattern, assignment targets (including computed
  // member keys) must be evaluated before consuming iterator values.
  let value = rt.exec_script(
    r#"
      var out = "";

      async function* gen() {
        var log = "";
        var obj = {};
        var inner = {};
        inner[Symbol.iterator] = function () {
          return {
            next() { log += "N"; return { value: 1, done: false }; },
            return() { log += "R"; return {}; },
          };
        };

        // Destructuring *assignment* in a `for await..of` LHS.
        // The computed key contains `yield` so we can observe ordering:
        // the yielded value is `log` before `inner.next()` runs.
        for await ([obj[yield log]] of [inner]) {
          break;
        }

        yield log + ":" + obj.k;
      }

      (async function () {
        var it = gen();
        var r1 = await it.next(); // yields `log` from the computed key
        var r2 = await it.next("k");
        out = r1.value + "|" + r2.value;
      })();

      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  // If `inner.next()` ran before evaluating the computed key, the first yield would observe `log = "N"`.
  assert_eq!(value_to_string(&rt, value), "|NR:1");
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
      var returnCalls = 0;
      async function f() {
        const iterable = {};
        iterable[Symbol.asyncIterator] = function () {
          return {
            next() {
              return Promise.reject("boom");
            },
            return() {
              returnCalls++;
              return {};
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

  let return_calls = rt.exec_script("returnCalls")?;
  assert_eq!(return_calls, Value::Number(0.0));
  Ok(())
}

#[test]
fn for_await_of_next_throw_does_not_close_iterator() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      var returnCalls = 0;

      const iterable = {};
      iterable[Symbol.asyncIterator] = function () {
        return {
          next() {
            throw "boom";
          },
          return() {
            returnCalls++;
            return {};
          },
        };
      };

      async function f() {
        for await (const x of iterable) {
          out = "bad";
        }
        out = "bad";
      }

      f().then(
        function () { out = "resolved"; },
        function (e) { out = e; }
      );
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "boom");

  let return_calls = rt.exec_script("returnCalls")?;
  assert_eq!(return_calls, Value::Number(0.0));
  Ok(())
}

#[test]
fn for_await_of_done_getter_throw_does_not_close_iterator() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      var returnCalls = 0;

      const iterable = {};
      iterable[Symbol.asyncIterator] = function () {
        const resultObj = {
          get done() { throw "boom"; },
          value: 1,
        };
        return {
          next() {
            return Promise.resolve(resultObj);
          },
          return() {
            returnCalls++;
            return {};
          },
        };
      };

      async function f() {
        for await (const x of iterable) {
          out = "bad";
        }
        out = "bad";
      }

      f().then(
        function () { out = "resolved"; },
        function (e) { out = e; }
      );
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "boom");

  let return_calls = rt.exec_script("returnCalls")?;
  assert_eq!(return_calls, Value::Number(0.0));
  Ok(())
}

#[test]
fn for_await_of_value_getter_throw_does_not_close_iterator() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      var returnCalls = 0;

      const iterable = {};
      iterable[Symbol.asyncIterator] = function () {
        const resultObj = {
          get done() { return false; },
          get value() { throw "boom"; },
        };
        return {
          next() {
            return Promise.resolve(resultObj);
          },
          return() {
            returnCalls++;
            return {};
          },
        };
      };

      async function f() {
        for await (const x of iterable) {
          out = "bad";
        }
        out = "bad";
      }

      f().then(
        function () { out = "resolved"; },
        function (e) { out = e; }
      );
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "boom");

  let return_calls = rt.exec_script("returnCalls")?;
  assert_eq!(return_calls, Value::Number(0.0));
  Ok(())
}

#[test]
fn for_await_of_next_result_non_object_does_not_close_iterator() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      var returnCalls = 0;

      const iterable = {};
      iterable[Symbol.asyncIterator] = function () {
        return {
          next() {
            return Promise.resolve(1);
          },
          return() {
            returnCalls++;
            return {};
          },
        };
      };

      async function f() {
        for await (const x of iterable) {
          out = "bad";
        }
        out = "bad";
      }

      f().then(
        function () { out = "resolved"; },
        function (e) { out = e.constructor.name; }
      );
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "TypeError");

  let return_calls = rt.exec_script("returnCalls")?;
  assert_eq!(return_calls, Value::Number(0.0));
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

#[test]
fn for_await_of_break_forwards_completion_when_iterator_return_getter_is_null() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      var iterationCount = 0;
      var returnGets = 0;

      const iterable = {};
      iterable[Symbol.asyncIterator] = function () {
        return {
          next() {
            return Promise.resolve({ value: 1, done: false });
          },
          get return() {
            returnGets++;
            return null;
          },
        };
      };

      async function f() {
        for await (const x of iterable) {
          iterationCount++;
          break;
        }
        return "ok";
      }

      f().then(function (v) { out = v; }, function () { out = "bad"; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "ok");

  let iteration_count = rt.exec_script("iterationCount")?;
  assert_eq!(iteration_count, Value::Number(1.0));

  let return_gets = rt.exec_script("returnGets")?;
  assert_eq!(return_gets, Value::Number(1.0));

  Ok(())
}

#[test]
fn for_await_of_break_propagates_getmethod_typeerror_when_iterator_return_is_non_callable() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      var iterationCount = 0;

      const iterable = {};
      iterable[Symbol.asyncIterator] = function () {
        return {
          next() {
            return Promise.resolve({ value: 1, done: false });
          },
          return: Symbol(),
        };
      };

      async function f() {
        for await (const x of iterable) {
          iterationCount++;
          break;
        }
      }

      f().then(
        function () { out = "resolved"; },
        function (e) { out = e.constructor.name; }
      );
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "TypeError");

  let iteration_count = rt.exec_script("iterationCount")?;
  assert_eq!(iteration_count, Value::Number(1.0));

  Ok(())
}

#[test]
fn for_await_of_break_propagates_getmethod_abrupt_when_iterator_return_getter_throws() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      var iterationCount = 0;
      var returnGets = 0;
      const innerError = { name: "inner error" };

      const iterable = {};
      iterable[Symbol.asyncIterator] = function () {
        return {
          next() {
            return Promise.resolve({ value: 1, done: false });
          },
          get return() {
            returnGets++;
            throw innerError;
          },
        };
      };

      async function f() {
        for await (const x of iterable) {
          iterationCount++;
          break;
        }
      }

      f().then(
        function () { out = "resolved"; },
        function (e) { out = (e === innerError) ? "inner" : "other"; }
      );
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "inner");

  let iteration_count = rt.exec_script("iterationCount")?;
  assert_eq!(iteration_count, Value::Number(1.0));

  let return_gets = rt.exec_script("returnGets")?;
  assert_eq!(return_gets, Value::Number(1.0));

  Ok(())
}

#[test]
fn for_await_of_throw_suppresses_getmethod_typeerror_when_iterator_return_is_non_callable() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      var iterationCount = 0;
      var returnGets = 0;

      const iterable = {};
      iterable[Symbol.asyncIterator] = function () {
        return {
          next() {
            return Promise.resolve({ value: 1, done: false });
          },
          get return() {
            returnGets++;
            return true;
          },
        };
      };

      async function f() {
        for await (const x of iterable) {
          iterationCount++;
          throw "body";
        }
      }

      f().then(
        function () { out = "resolved"; },
        function (e) { out = (typeof e === "string") ? e : e.constructor.name; }
      );
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "body");

  let iteration_count = rt.exec_script("iterationCount")?;
  assert_eq!(iteration_count, Value::Number(1.0));

  let return_gets = rt.exec_script("returnGets")?;
  assert_eq!(return_gets, Value::Number(1.0));

  Ok(())
}

#[test]
fn for_await_of_throw_suppresses_getmethod_abrupt_when_iterator_return_getter_throws() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      var iterationCount = 0;
      var returnGets = 0;
      const innerError = { name: "inner error" };

      const iterable = {};
      iterable[Symbol.asyncIterator] = function () {
        return {
          next() {
            return Promise.resolve({ value: 1, done: false });
          },
          get return() {
            returnGets++;
            throw innerError;
          },
        };
      };

      async function f() {
        for await (const x of iterable) {
          iterationCount++;
          throw "body";
        }
      }

      f().then(
        function () { out = "resolved"; },
        function (e) { out = (typeof e === "string") ? e : e.name; }
      );
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "body");

  let iteration_count = rt.exec_script("iterationCount")?;
  assert_eq!(iteration_count, Value::Number(1.0));

  let return_gets = rt.exec_script("returnGets")?;
  assert_eq!(return_gets, Value::Number(1.0));
  Ok(())
}

#[test]
fn for_await_over_sync_iterator_rejected_value_close_error_is_suppressed_for_value_rejection_and_closes_iterator(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      var returnCalls = 0;

      const iterable = {};
      iterable[Symbol.iterator] = function () {
        let i = 0;
        return {
          next() {
            i++;
            if (i === 1) return { value: Promise.reject("boom"), done: false };
            return { value: undefined, done: true };
          },
          return() {
            returnCalls++;
            throw "close";
          },
        };
      };

      async function f() {
        for await (const x of iterable) {
          // Never reached: awaiting the `value` promise rejects.
          out = "bad";
        }
      }

      f().then(function () { out = "bad"; }, function (e) { out = e; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  // Per ECMA-262 `IteratorClose`, errors thrown while getting/calling `iterator.return` are
  // suppressed for throw completions (the original rejection reason is preserved).
  assert_eq!(value_to_string(&rt, value), "boom");

  let value = rt.exec_script("returnCalls")?;
  let Value::Number(return_calls) = value else {
    panic!("expected number, got {value:?}");
  };
  assert!(
    return_calls >= 1.0,
    "expected iterator.return to be called at least once, got {return_calls}"
  );
  Ok(())
}

// test262:
// - language/statements/for-await-of/ticks-with-sync-iter-resolved-promise-and-constructor-lookup.js
// - language/statements/for-await-of/ticks-with-async-iter-resolved-promise-and-constructor-lookup.js
// - language/statements/for-await-of/ticks-with-async-iter-resolved-promise-and-constructor-lookup-two.js

#[test]
fn ticks_with_sync_iter_resolved_promise_and_constructor_lookup() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var actual = [];
      var value = Promise.resolve("a");
      Object.defineProperty(value, "constructor", { get() { actual.push("constructor"); return Promise; } });

      async function f() {
        for await (var x of [value]) {
          actual.push(x);
        }
        actual.push("done");
      }

      f();
      actual.push("sync");
      Promise.resolve().then(function () { actual.push("tick"); });
    "#,
  )?;

  assert_actual_join(&mut rt, "constructor,sync,tick,a,done")
}

#[test]
fn ticks_with_async_iter_resolved_promise_and_constructor_lookup() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var actual = [];
      var value = Promise.resolve("a");
      Object.defineProperty(value, "constructor", { get() { actual.push("value constructor"); return Promise; } });

      var nextResult = Promise.resolve({ value: value, done: false });
      Object.defineProperty(nextResult, "constructor", { get() { actual.push("next constructor"); return Promise; } });

      var iterable = {};
      iterable[Symbol.asyncIterator] = function () {
        var i = 0;
        return {
          next() {
            i++;
            if (i === 1) return nextResult;
            return Promise.resolve({ value: undefined, done: true });
          }
        };
      };

      async function f() {
        for await (var x of iterable) {
          actual.push(x === value ? "same" : "diff");
        }
        actual.push("done");
      }

      f();
      actual.push("sync");
      Promise.resolve().then(function () { actual.push("tick"); });
    "#,
  )?;

  // For protocol async iterators, the loop awaits `nextResult` but does *not* await `value`.
  // This must observe `nextResult.constructor` but must not touch `value.constructor`.
  assert_actual_join(&mut rt, "next constructor,sync,same,tick,done")
}

#[test]
fn ticks_with_async_iter_resolved_promise_and_constructor_lookup_two() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var actual = [];
      var value1 = Promise.resolve("a");
      var value2 = Promise.resolve("b");
      Object.defineProperty(value1, "constructor", { get() { actual.push("value1 constructor"); return Promise; } });
      Object.defineProperty(value2, "constructor", { get() { actual.push("value2 constructor"); return Promise; } });

      var nextResult1 = Promise.resolve({ value: value1, done: false });
      var nextResult2 = Promise.resolve({ value: value2, done: false });
      Object.defineProperty(nextResult1, "constructor", { get() { actual.push("next1 constructor"); return Promise; } });
      Object.defineProperty(nextResult2, "constructor", { get() { actual.push("next2 constructor"); return Promise; } });

      var iterable = {};
      iterable[Symbol.asyncIterator] = function () {
        var i = 0;
        return {
          next() {
            i++;
            if (i === 1) return nextResult1;
            if (i === 2) return nextResult2;
            return Promise.resolve({ value: undefined, done: true });
          }
        };
      };

      async function f() {
        for await (var x of iterable) {
          actual.push(x === value1 ? "same1" : x === value2 ? "same2" : "diff");
        }
        actual.push("done");
      }

      f();
      actual.push("sync");
      Promise.resolve().then(function () { actual.push("tick"); });
    "#,
  )?;

  assert_actual_join(&mut rt, "next1 constructor,sync,same1,next2 constructor,tick,same2,done")
}
