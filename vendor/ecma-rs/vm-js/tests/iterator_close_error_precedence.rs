use vm_js::{Heap, HeapLimits, JsRuntime, PromiseState, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn object_from_entries_suppresses_iterator_close_errors_on_throw_completion() -> Result<(), VmError> {
  let mut rt = new_runtime();

  // When the iterator value access throws, `Object.fromEntries` performs `IteratorClose`. Per
  // ECMA-262 `IteratorClose`, if the original completion is a throw completion, errors produced
  // while getting/calling `iterator.return` must be suppressed.
  let value = rt.exec_script(
    r#"
      (function () {
        // Case 1: `iterator.return` getter throws.
        var original1 = "original1";
        var close1 = "close1";

        var iter1 = {};
        iter1[Symbol.iterator] = function () {
          return {
            next: function () {
              return {
                done: false,
                value: {
                  get 0() { throw original1; },
                  get 1() { return 1; },
                },
              };
            },
            get return() { throw close1; },
          };
        };

        var r1 = null;
        try {
          Object.fromEntries(iter1);
        } catch (e) {
          r1 = e;
        }

        // Case 2: `iterator.return` is non-callable.
        var original2 = "original2";

        var iter2 = {};
        iter2[Symbol.iterator] = function () {
          return {
            next: function () {
              return {
                done: false,
                value: {
                  get 0() { throw original2; },
                  get 1() { return 1; },
                },
              };
            },
            return: 1,
          };
        };

        var r2 = null;
        try {
          Object.fromEntries(iter2);
        } catch (e) {
          r2 = e;
        }

        return r1 === original1 && r2 === original2;
      })()
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn promise_all_suppresses_iterator_close_errors_on_throw_completion() -> Result<(), VmError> {
  let mut rt = new_runtime();

  // If `Promise.all`'s iterator step throws, `IteratorClose` is performed. If the close operation
  // throws, it must not replace the original throw completion.
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
fn promise_race_suppresses_iterator_close_errors_on_throw_completion() -> Result<(), VmError> {
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

