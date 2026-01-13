use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Async for-of tests exercise Promise jobs + stack capture. Use a slightly larger heap than the
  // default 1MiB used by many unit tests to avoid spurious OOM failures when implementation details
  // change.
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
fn iterator_close_get_method_throw_takes_precedence_over_throw_completion_in_async_for_of_before_await(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

  // Ensure we don't leak queued microtasks even if this test fails.
  let result: Result<(), VmError> = (|| {
    // The `for..of` statement contains `await` so it runs through the async AST evaluator, but the
    // loop body throws before reaching the `await`.
    let value = rt.exec_script(
      r#"
        var out = "";
        var closed = false;

        var iterable = {};
        iterable[Symbol.iterator] = function () {
          return {
            next: function () { return { value: 1, done: false }; },
            get "return"() { closed = true; throw "getter1"; }
          };
        };

        async function f() {
          for (const _ of iterable) {
            throw "body1";
            await 0;
          }
        }

        f().catch(e => { out = e; });

        out
      "#,
    )?;
    assert_eq!(value_to_string(&rt, value), "");

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let out = rt.exec_script("out")?;
    // Per ECMA-262 `IteratorClose(iteratorRecord, completion)`, `GetMethod(iterator, "return")` is
    // still performed, but when the incoming completion is a throw completion, errors thrown while
    // getting/calling `iterator.return` are ignored and the original throw is preserved.
    assert_eq!(value_to_string(&rt, out), "body1");

    let closed = rt.exec_script("closed")?;
    assert_eq!(closed, Value::Bool(true));

    Ok(())
  })();

  rt.teardown_microtasks();
  result
}

#[test]
fn iterator_close_get_method_throw_takes_precedence_over_throw_completion_in_async_for_of_after_await(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

  let result: Result<(), VmError> = (|| {
    let value = rt.exec_script(
      r#"
        var out = "";
        var closed = false;

        var iterable = {};
        iterable[Symbol.iterator] = function () {
          return {
            next: function () { return { value: 1, done: false }; },
            get "return"() { closed = true; throw "getter2"; }
          };
        };

        async function f() {
          for (const _ of iterable) {
            await 0;
            throw "body2";
          }
        }

        f().catch(e => { out = e; });

        out
      "#,
    )?;
    assert_eq!(value_to_string(&rt, value), "");

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let out = rt.exec_script("out")?;
    assert_eq!(value_to_string(&rt, out), "body2");

    let closed = rt.exec_script("closed")?;
    assert_eq!(closed, Value::Bool(true));

    Ok(())
  })();

  rt.teardown_microtasks();
  result
}

#[test]
fn iterator_close_get_method_throw_takes_precedence_over_throw_completion_in_async_for_of_binding_error(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

  let result: Result<(), VmError> = (|| {
    // Ensure the `for..of` statement goes through the async AST evaluator by including `await` in the
    // loop body. The binding error (unresolvable reference in strict mode) happens before the body
    // executes, so the iterator must be closed using the error-based IteratorClose path.
    let value = rt.exec_script(
      r#"
        "use strict";

        var out = "";
        var closed = false;

        var iterable = {};
        iterable[Symbol.iterator] = function () {
          return {
            next: function () { return { value: 1, done: false }; },
            get "return"() { closed = true; throw "close"; }
          };
        };

        async function f() {
          for (x of iterable) {
            await 0;
          }
        }

        f().catch(e => { out = (e && e.name) || e; });

        out
      "#,
    )?;
    assert_eq!(value_to_string(&rt, value), "");

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let out = rt.exec_script("out")?;
    assert_eq!(value_to_string(&rt, out), "ReferenceError");

    let closed = rt.exec_script("closed")?;
    assert_eq!(closed, Value::Bool(true));

    Ok(())
  })();

  rt.teardown_microtasks();
  result
}
