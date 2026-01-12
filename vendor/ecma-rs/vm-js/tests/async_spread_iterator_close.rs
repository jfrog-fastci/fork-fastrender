use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Async spread tests exercise Promise jobs + stack capture. Use a slightly larger heap than the
  // default 1MiB used by many unit tests to avoid spurious OOM failures when implementation details
  // change.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn async_spread_does_not_close_iterator_when_next_throws() -> Result<(), VmError> {
  let mut rt = new_runtime();

  // This must exercise the async evaluator's call/argument evaluation, so the call expression
  // contains an `await` even though the spread throws before the awaited argument is evaluated.
  let value = rt.exec_script(
    r#"
      var returnCalled = false;

      function f() {}

      var iter = {};
      iter[Symbol.iterator] = function () {
        return {
          next: function () { throw "boom"; },
          "return": function () { returnCalled = true; return {}; },
        };
      };

      var out = false;

      async function g() {
        await 0;
        try {
          f(...iter, await 0);
        } catch (e) {}
        return returnCalled;
      }

      g().then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value, Value::Bool(false));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value, Value::Bool(false));
  Ok(())
}
