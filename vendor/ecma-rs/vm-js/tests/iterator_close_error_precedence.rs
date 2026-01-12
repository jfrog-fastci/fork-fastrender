use vm_js::{Heap, HeapLimits, JsRuntime, PromiseState, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // These tests intentionally use iterables with accessors that throw; this can allocate enough
  // bytecode / objects that 1MiB heaps become flaky as the engine evolves.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn object_from_entries_iterator_close_return_getter_throw_overrides_original_throw(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

  // When the iterator value access throws, `Object.fromEntries` performs `IteratorClose`. Per
  // ECMA-262 `IteratorClose`, errors produced while getting/calling `iterator.return` override the
  // incoming completion (even when the incoming completion is a throw completion).
  let value = rt.exec_script(
    r#"
      (function () {
        var original = "original";
        var close = "close";

        var iter = {};
        iter[Symbol.iterator] = function () {
          return {
            next: function () {
              return {
                done: false,
                value: {
                  get 0() { throw original; },
                  1: 1,
                },
              };
            },
            get return() { throw close; },
          };
        };

        try {
          Object.fromEntries(iter);
        } catch (e) {
          return e === close;
        }
        return false;
      })()
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn object_from_entries_iterator_close_return_not_callable_overrides_original_throw(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      (function () {
        var original = "original";

        var iter = {};
        iter[Symbol.iterator] = function () {
          return {
            next: function () {
              return {
                done: false,
                value: {
                  get 0() { throw original; },
                  1: 1,
                },
              };
            },
            return: 1,
          };
        };

        try {
          Object.fromEntries(iter);
        } catch (e) {
          return e && e.name === "TypeError";
        }
        return false;
      })()
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn promise_all_step_error_does_not_invoke_iterator_close() -> Result<(), VmError> {
  let mut rt = new_runtime();

  // If `Promise.all`'s iterator step throws, `PerformPromiseAll` does not perform `IteratorClose`
  // (the iterator protocol itself failed, so `return()` must not be invoked).
  let promise_value = rt.exec_script(
    r#"
      var original = "original";
      var close = "close";

      var iter = {};
      iter[Symbol.iterator] = function () {
        return {
          next: function () { throw original; },
          "return": function () { throw close; },
        };
      };

      Promise.all(iter)
    "#,
  )?;
  let Value::Object(promise_obj) = promise_value else {
    panic!("Promise.all should return a Promise object");
  };
  assert_eq!(rt.heap.promise_state(promise_obj)?, PromiseState::Rejected);

  let reason = rt
    .heap
    .promise_result(promise_obj)?
    .expect("rejected promise should have a rejection reason");
  let Value::String(s) = reason else {
    panic!("expected Promise.all rejection reason to be a string, got {reason:?}");
  };
  assert_eq!(rt.heap.get_string(s)?.to_utf8_lossy(), "original");
  Ok(())
}

#[test]
fn promise_race_step_error_does_not_invoke_iterator_close() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let promise_value = rt.exec_script(
    r#"
      var original = "original";
      var close = "close";

      var iter = {};
      iter[Symbol.iterator] = function () {
        return {
          next: function () { throw original; },
          "return": function () { throw close; },
        };
      };

      Promise.race(iter)
    "#,
  )?;
  let Value::Object(promise_obj) = promise_value else {
    panic!("Promise.race should return a Promise object");
  };
  assert_eq!(rt.heap.promise_state(promise_obj)?, PromiseState::Rejected);

  let reason = rt
    .heap
    .promise_result(promise_obj)?
    .expect("rejected promise should have a rejection reason");
  let Value::String(s) = reason else {
    panic!("expected Promise.race rejection reason to be a string, got {reason:?}");
  };
  assert_eq!(rt.heap.get_string(s)?.to_utf8_lossy(), "original");
  Ok(())
}

#[test]
fn weak_map_constructor_iterator_close_return_getter_throw_overrides_original_throw(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      (function () {
        var original = "original";
        var close = "close";

        var iter = {};
        iter[Symbol.iterator] = function () {
          return {
            next: function () {
              return {
                done: false,
                value: {
                  get 0() { throw original; },
                  get 1() { return {}; },
                },
              };
            },
            get return() { throw close; },
          };
        };

        try {
          new WeakMap(iter);
        } catch (e) {
          return e === close;
        }
        return false;
      })()
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn weak_set_constructor_iterator_close_return_getter_throw_overrides_original_throw(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      (function () {
        var original = "original";
        var close = "close";

        // Force the per-element `adder` call to throw a predictable value.
        WeakSet.prototype.add = function () { throw original; };

        var iter = {};
        iter[Symbol.iterator] = function () {
          return {
            next: function () { return { done: false, value: {} }; },
            get return() { throw close; },
          };
        };

        try {
          new WeakSet(iter);
        } catch (e) {
          return e === close;
        }
        return false;
      })()
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}
