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
fn promise_all_close_error_is_suppressed_for_promise_resolve_throw() -> Result<(), VmError> {
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

  // Per ECMA-262 `IteratorClose`, errors thrown while getting/calling `iterator.return` are
  // suppressed for throw completions (the original throw is preserved).
  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "bad promise resolve");
  Ok(())
}
