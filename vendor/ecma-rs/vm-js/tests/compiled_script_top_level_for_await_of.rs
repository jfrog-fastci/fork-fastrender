use vm_js::{
  CompiledScript, Heap, HeapLimits, JsRuntime, PromiseState, Value, Vm, VmError, VmOptions,
};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Promise + async iterator machinery needs a bit of heap headroom.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_utf8(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn compiled_script_top_level_for_await_of_executes_via_hir_and_resumes_in_microtasks(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      var actual = [];
      for await (const x of [Promise.resolve("a"), "b"]) {
        actual.push(x);
      }
      actual.push("done");
    "#,
  )?;
  assert!(
    !script.requires_ast_fallback,
    "simple top-level for-await-of loops should execute via the compiled (HIR) async script path"
  );

  let result = rt.exec_compiled_script(script)?;
  let Value::Object(promise_obj) = result else {
    panic!("expected Promise object, got {result:?}");
  };
  assert!(
    rt.heap().is_promise_object(promise_obj),
    "expected Promise return value from async classic script"
  );
  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Pending);

  let before = rt.exec_script("JSON.stringify(actual)")?;
  assert_eq!(value_to_utf8(&rt, before), r#"[]"#);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);
  let after = rt.exec_script("JSON.stringify(actual)")?;
  assert_eq!(value_to_utf8(&rt, after), r#"["a","b","done"]"#);

  Ok(())
}

#[test]
fn compiled_script_top_level_for_await_of_throw_suppresses_iterator_return_rejection(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      var returnCalls = 0;
      const iterable = {};
      iterable[Symbol.asyncIterator] = function () {
        return {
          next() {
            return Promise.resolve({ value: 1, done: false });
          },
          return() {
            returnCalls++;
            return Promise.reject("close");
          },
        };
      };

      for await (const x of iterable) {
        throw "body";
      }
    "#,
  )?;
  assert!(
    !script.requires_ast_fallback,
    "top-level for-await-of loops with synchronous bodies should execute via the compiled (HIR) async script path"
  );

  let result = rt.exec_compiled_script(script)?;
  let Value::Object(promise_obj) = result else {
    panic!("expected Promise object, got {result:?}");
  };
  assert!(rt.heap().is_promise_object(promise_obj));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  // Per ECMA-262 `AsyncIteratorClose`, errors from awaiting `iterator.return()` are suppressed for
  // throw completions (the original throw must be preserved).
  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Rejected);
  let reason = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("rejected promise should have a reason");
  assert_eq!(value_to_utf8(&rt, reason), "body");

  let return_calls = rt.exec_script("returnCalls")?;
  assert_eq!(return_calls, Value::Number(1.0));
  Ok(())
}

