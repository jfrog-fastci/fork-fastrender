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
fn for_await_of_sync_iterator_suppresses_iterator_close_error_when_promise_resolve_throws() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";

      var thenable = {};
      Object.defineProperty(thenable, "then", {
        get: function () { throw "then"; },
      });

      var iterable = {};
      iterable[Symbol.iterator] = function () {
        return {
          next: function () { return { value: thenable, done: false }; },
          get "return"() { throw "close"; },
        };
      };

      (async function () {
        try {
          for await (const _ of iterable) {}
        } catch (e) {
          out = e;
        }
      })();

      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "then");
  Ok(())
}

#[test]
fn for_await_of_sync_iterator_suppresses_iterator_close_error_when_awaited_value_rejects() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";

      var iterable = {};
      iterable[Symbol.iterator] = function () {
        return {
          next: function () { return { value: Promise.reject("reason"), done: false }; },
          return: function () { throw "close"; },
        };
      };

      (async function () {
        try {
          for await (const _ of iterable) {}
        } catch (e) {
          out = e;
        }
      })();

      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "reason");
  Ok(())
}

