use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

const HEAP_MAX_BYTES: usize = 4 * 1024 * 1024;
const HEAP_GC_THRESHOLD_BYTES: usize = 2 * 1024 * 1024;

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // This test focuses on iterator-closing semantics, not OOM behavior. Use a moderate heap size so
  // baseline builtins + `Object.defineProperty` don't trip spurious `OutOfMemory` failures.
  let heap = Heap::new(HeapLimits::new(HEAP_MAX_BYTES, HEAP_GC_THRESHOLD_BYTES));
  JsRuntime::new(vm, heap).unwrap()
}

const ITER_STEP_ERR_TEMPLATE: &str = r#"
  var returnCount = 0;
  var error = {};
  var poisonedDone = {};
  Object.defineProperty(poisonedDone, 'done', {
    get: function () { throw error; },
  });
  Object.defineProperty(poisonedDone, 'value', {
    get: function () { throw 'value should not be accessed'; },
  });

  var iterStepThrows = {};
  iterStepThrows[Symbol.iterator] = function () {
    return {
      next: function () { return poisonedDone; },
      return: function () { returnCount += 1; return {}; },
    };
  };

  var outcome = 0; // 0 = pending, 1 = fulfilled, 2 = rejected w/ expected reason, 3 = rejected w/ wrong reason
  var p = Promise.__METHOD__(iterStepThrows);
  p.then(
    function () { outcome = 1; },
    function (reason) { outcome = (reason === error) ? 2 : 3; },
  );
"#;

const ITER_NEXT_VAL_ERR_TEMPLATE: &str = r#"
  var returnCount = 0;
  var error = {};
  var poisonedVal = { done: false };
  Object.defineProperty(poisonedVal, 'value', {
    get: function () { throw error; },
  });

  var iterNextValThrows = {};
  iterNextValThrows[Symbol.iterator] = function () {
    return {
      next: function () { return poisonedVal; },
      return: function () { returnCount += 1; return {}; },
    };
  };

  var outcome = 0; // 0 = pending, 1 = fulfilled, 2 = rejected w/ expected reason, 3 = rejected w/ wrong reason
  var p = Promise.__METHOD__(iterNextValThrows);
  p.then(
    function () { outcome = 1; },
    function (reason) { outcome = (reason === error) ? 2 : 3; },
  );
"#;

fn assert_promise_iterator_error_does_not_close_iterator(method: &str, template: &str) {
  let mut rt = new_runtime();
  rt.exec_script(&template.replace("__METHOD__", method)).unwrap();

  // If the iterator is (incorrectly) closed, IteratorClose will synchronously call `return()`.
  assert_eq!(
    rt.exec_script("returnCount").unwrap(),
    Value::Number(0.0),
    "Promise.{method} unexpectedly called iterator.return() on iterator protocol error",
  );

  rt.vm.perform_microtask_checkpoint(&mut rt.heap).unwrap();

  assert_eq!(
    rt.exec_script("outcome").unwrap(),
    Value::Number(2.0),
    "Promise.{method} did not reject with the iterator protocol error",
  );

  assert_eq!(
    rt.exec_script("returnCount").unwrap(),
    Value::Number(0.0),
    "Promise.{method} unexpectedly called iterator.return() after the initial call",
  );
}

#[test]
fn promise_all_iter_step_err_does_not_close_iterator() {
  assert_promise_iterator_error_does_not_close_iterator("all", ITER_STEP_ERR_TEMPLATE);
}

#[test]
fn promise_all_iter_next_val_err_does_not_close_iterator() {
  assert_promise_iterator_error_does_not_close_iterator("all", ITER_NEXT_VAL_ERR_TEMPLATE);
}

#[test]
fn promise_race_iter_step_err_does_not_close_iterator() {
  assert_promise_iterator_error_does_not_close_iterator("race", ITER_STEP_ERR_TEMPLATE);
}

#[test]
fn promise_race_iter_next_val_err_does_not_close_iterator() {
  assert_promise_iterator_error_does_not_close_iterator("race", ITER_NEXT_VAL_ERR_TEMPLATE);
}

#[test]
fn promise_all_settled_iter_step_err_does_not_close_iterator() {
  assert_promise_iterator_error_does_not_close_iterator("allSettled", ITER_STEP_ERR_TEMPLATE);
}

#[test]
fn promise_all_settled_iter_next_val_err_does_not_close_iterator() {
  assert_promise_iterator_error_does_not_close_iterator("allSettled", ITER_NEXT_VAL_ERR_TEMPLATE);
}

#[test]
fn promise_any_iter_step_err_does_not_close_iterator() {
  assert_promise_iterator_error_does_not_close_iterator("any", ITER_STEP_ERR_TEMPLATE);
}

#[test]
fn promise_any_iter_next_val_err_does_not_close_iterator() {
  assert_promise_iterator_error_does_not_close_iterator("any", ITER_NEXT_VAL_ERR_TEMPLATE);
}
