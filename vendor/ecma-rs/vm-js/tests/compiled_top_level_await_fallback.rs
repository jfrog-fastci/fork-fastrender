use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, PromiseState, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Top-level await execution allocates Promise/job machinery; use a slightly larger heap than
  // the minimal 1MiB used by some unit tests to avoid spurious OOMs as builtin surface area grows.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn compiled_script_falls_back_for_top_level_await() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "compiled_top_level_await_fallback.js",
    r#"
      var out = "";
      out = await Promise.resolve("ok");
      out
    "#,
  )?;

  let completion = rt.exec_compiled_script(script)?;
  let completion_root = rt.heap_mut().add_root(completion)?;

  let Value::Object(promise_obj) = completion else {
    panic!("expected Promise object from top-level await script, got {completion:?}");
  };
  assert!(rt.heap().is_promise_object(promise_obj));
  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Pending);

  // The assignment after `await` should not have executed yet.
  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);
  let result = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("fulfilled promise should have a result");
  assert_eq!(value_to_string(&rt, result), "ok");

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "ok");

  rt.heap_mut().remove_root(completion_root);
  Ok(())
}

#[test]
fn compiled_script_falls_back_for_top_level_for_await_of() -> Result<(), VmError> {
  let mut rt = new_runtime();

  // Use a manual async iterator (no `async function` / generators) so this tests the top-level
  // await fallback rather than the async-function AST fallback.
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "compiled_top_level_for_await_of_fallback.js",
    r#"
      var out = "";
      var iter = {
        i: 0,
        next: function () {
          if (this.i++ === 0) return Promise.resolve({ value: "ok", done: false });
          return Promise.resolve({ value: undefined, done: true });
        },
      };
      var iterable = {};
      iterable[Symbol.asyncIterator] = function () { return iter; };

      for await (var x of iterable) {
        out = x;
      }
      out
    "#,
  )?;

  let completion = rt.exec_compiled_script(script)?;
  let completion_root = rt.heap_mut().add_root(completion)?;

  let Value::Object(promise_obj) = completion else {
    panic!("expected Promise object from top-level await script, got {completion:?}");
  };
  assert!(rt.heap().is_promise_object(promise_obj));
  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Pending);

  // Loop body should not have executed until we run microtasks.
  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);
  let result = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("fulfilled promise should have a result");
  assert_eq!(value_to_string(&rt, result), "ok");

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "ok");

  rt.heap_mut().remove_root(completion_root);
  Ok(())
}

#[test]
fn compiled_script_falls_back_for_await_in_class_static_block() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "compiled_top_level_await_in_class_static_block_fallback.js",
    r#"
      var out = "";
      class C {
        static {
          out = await Promise.resolve("ok");
        }
      }
      out
    "#,
  )?;

  let completion = rt.exec_compiled_script(script)?;
  let completion_root = rt.heap_mut().add_root(completion)?;

  let Value::Object(promise_obj) = completion else {
    panic!("expected Promise object from top-level await script, got {completion:?}");
  };
  assert!(rt.heap().is_promise_object(promise_obj));
  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Pending);

  // Await in the class static block should not have executed until we run microtasks.
  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);
  let result = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("fulfilled promise should have a result");
  assert_eq!(value_to_string(&rt, result), "ok");

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "ok");

  rt.heap_mut().remove_root(completion_root);
  Ok(())
}
