use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn promise_all_suppresses_iterator_close_errors_when_resolve_throws() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var returnGetterCalls = 0;

      var iter = {
        [Symbol.iterator]: function () { return this; },
        next: function () { return { done: false, value: 0 }; },
        get return() { returnGetterCalls += 1; return 0; },
      };

      var out = "pending";

      Promise.resolve = function () {
        throw "bad promise resolve";
      };

      Promise.all(iter).then(
        function () { out = "fulfilled"; },
        function (e) { out = e; },
      );

      returnGetterCalls
    "#,
  )?;
  assert_eq!(value, Value::Number(1.0));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out === 'bad promise resolve'")?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

