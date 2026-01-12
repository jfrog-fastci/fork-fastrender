use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

const HEAP_MAX_BYTES: usize = 8 * 1024 * 1024;
const HEAP_GC_THRESHOLD_BYTES: usize = 8 * 1024 * 1024;

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Promise combinators allocate a fair amount of internal machinery (capabilities, reactions,
  // microtask jobs, etc). Use a slightly larger heap so these tests focus on iterator-close
  // semantics rather than baseline heap pressure.
  let heap = Heap::new(HeapLimits::new(HEAP_MAX_BYTES, HEAP_GC_THRESHOLD_BYTES));
  JsRuntime::new(vm, heap).unwrap()
}

fn assert_return_not_called(script: &str) -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(script)?;
  assert_eq!(value, Value::Number(0.0));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;
  let value = rt.exec_script("returnCount")?;
  assert_eq!(value, Value::Number(0.0));

  Ok(())
}

// Promise.all

#[test]
fn promise_all_iter_step_err_no_close() -> Result<(), VmError> {
  assert_return_not_called(
    r#"
      var returnCount = 0;
      var iterable = {};
      iterable[Symbol.iterator] = function() {
        return {
          next: function() { throw "next"; },
          return: function() { returnCount++; return {}; }
        };
      };
      Promise.all(iterable);
      returnCount;
    "#,
  )
}

#[test]
fn promise_all_iter_next_val_err_no_close() -> Result<(), VmError> {
  assert_return_not_called(
    r#"
      var returnCount = 0;
      var iterable = {};
      iterable[Symbol.iterator] = function() {
        return {
          next: function() {
            return { done: false, get value() { throw "value"; } };
          },
          return: function() { returnCount++; return {}; }
        };
      };
      Promise.all(iterable);
      returnCount;
    "#,
  )
}

// Promise.race

#[test]
fn promise_race_iter_step_err_no_close() -> Result<(), VmError> {
  assert_return_not_called(
    r#"
      var returnCount = 0;
      var iterable = {};
      iterable[Symbol.iterator] = function() {
        return {
          next: function() { throw "next"; },
          return: function() { returnCount++; return {}; }
        };
      };
      Promise.race(iterable);
      returnCount;
    "#,
  )
}

#[test]
fn promise_race_iter_next_val_err_no_close() -> Result<(), VmError> {
  assert_return_not_called(
    r#"
      var returnCount = 0;
      var iterable = {};
      iterable[Symbol.iterator] = function() {
        return {
          next: function() {
            return { done: false, get value() { throw "value"; } };
          },
          return: function() { returnCount++; return {}; }
        };
      };
      Promise.race(iterable);
      returnCount;
    "#,
  )
}

// Promise.allSettled

#[test]
fn promise_all_settled_iter_step_err_no_close() -> Result<(), VmError> {
  assert_return_not_called(
    r#"
      var returnCount = 0;
      var iterable = {};
      iterable[Symbol.iterator] = function() {
        return {
          next: function() { throw "next"; },
          return: function() { returnCount++; return {}; }
        };
      };
      Promise.allSettled(iterable);
      returnCount;
    "#,
  )
}

#[test]
fn promise_all_settled_iter_next_val_err_no_close() -> Result<(), VmError> {
  assert_return_not_called(
    r#"
      var returnCount = 0;
      var iterable = {};
      iterable[Symbol.iterator] = function() {
        return {
          next: function() {
            return { done: false, get value() { throw "value"; } };
          },
          return: function() { returnCount++; return {}; }
        };
      };
      Promise.allSettled(iterable);
      returnCount;
    "#,
  )
}

// Promise.any

#[test]
fn promise_any_iter_step_err_no_close() -> Result<(), VmError> {
  assert_return_not_called(
    r#"
      var returnCount = 0;
      var iterable = {};
      iterable[Symbol.iterator] = function() {
        return {
          next: function() { throw "next"; },
          return: function() { returnCount++; return {}; }
        };
      };
      Promise.any(iterable);
      returnCount;
    "#,
  )
}

#[test]
fn promise_any_iter_next_val_err_no_close() -> Result<(), VmError> {
  assert_return_not_called(
    r#"
      var returnCount = 0;
      var iterable = {};
      iterable[Symbol.iterator] = function() {
        return {
          next: function() {
            return { done: false, get value() { throw "value"; } };
          },
          return: function() { returnCount++; return {}; }
        };
      };
      Promise.any(iterable);
      returnCount;
    "#,
  )
}
