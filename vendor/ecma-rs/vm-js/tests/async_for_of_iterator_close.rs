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
fn iterator_close_get_method_throw_suppressed_on_throw_completion_in_async_for_of() -> Result<(), VmError> {
  let mut rt = new_runtime();

  // The `for..of` statement contains `await` so it runs through the async AST evaluator, but the
  // loop body throws before reaching the `await` in `f1`.
  let value = rt.exec_script(
    r#"
      var out1 = "";
      var out2 = "";
      var closed1 = false;
      var closed2 = false;

      var iterable1 = {};
      iterable1[Symbol.iterator] = function () {
        return {
          next: function () { return { value: 1, done: false }; },
          get "return"() { closed1 = true; throw "getter1"; }
        };
      };

      var iterable2 = {};
      iterable2[Symbol.iterator] = function () {
        return {
          next: function () { return { value: 1, done: false }; },
          get "return"() { closed2 = true; throw "getter2"; }
        };
      };

      async function f1() {
        for (const _ of iterable1) {
          throw "body1";
          await 0;
        }
      }

      async function f2() {
        for (const _ of iterable2) {
          await 0;
          throw "body2";
        }
      }

      f1().catch(e => { out1 = e; });
      f2().catch(e => { out2 = e; });

      out1
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out1 = rt.exec_script("out1")?;
  assert_eq!(value_to_string(&rt, out1), "body1");
  let out2 = rt.exec_script("out2")?;
  assert_eq!(value_to_string(&rt, out2), "body2");

  let closed1 = rt.exec_script("closed1")?;
  assert_eq!(closed1, Value::Bool(true));
  let closed2 = rt.exec_script("closed2")?;
  assert_eq!(closed2, Value::Bool(true));

  Ok(())
}

#[test]
fn iterator_close_get_method_throw_suppressed_on_throw_completion_in_async_for_of_binding_error(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

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
}
