use vm_js::{
  CompiledScript, Heap, HeapLimits, JsRuntime, MicrotaskQueue, PromiseState, Value, Vm, VmError,
  VmOptions,
};

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
      out += await Promise.resolve("ok");
      out
    "#,
  )?;
  assert!(script.contains_top_level_await);
  assert!(
    script.top_level_await_requires_ast_fallback,
    "compound assignment with await is not supported by the HIR async classic-script executor"
  );

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
fn compiled_script_with_host_and_hooks_falls_back_for_top_level_await() -> Result<(), VmError> {
  // Regression test for `exec_compiled_script_with_host_and_hooks`: unsupported top-level await
  // patterns must execute via the AST interpreter and enqueue Promise jobs via the provided
  // `hooks`.
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "compiled_top_level_await_fallback_with_hooks.js",
    r#"
      var out = "";
      out += await Promise.resolve("ok");
      out
    "#,
  )?;
  assert!(script.contains_top_level_await);
  assert!(
    script.top_level_await_requires_ast_fallback,
    "compound assignment with await is not supported by the HIR async classic-script executor"
  );
  assert!(
    script.requires_ast_fallback,
    "unsupported top-level await patterns should set `requires_ast_fallback` for compiled-script execution"
  );

  let mut host = ();
  let mut hooks = MicrotaskQueue::new();
  let completion = rt.exec_compiled_script_with_host_and_hooks(&mut host, &mut hooks, script)?;
  let completion_root = rt.heap_mut().add_root(completion)?;

  let Value::Object(promise_obj) = completion else {
    panic!("expected Promise object from top-level await script, got {completion:?}");
  };
  assert!(rt.heap().is_promise_object(promise_obj));
  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Pending);

  // The assignment after `await` should not have executed yet.
  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  let errors = hooks.perform_microtask_checkpoint(&mut rt);
  if let Some(err) = errors.into_iter().next() {
    return Err(err);
  }

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
fn compiled_script_falls_back_for_top_level_for_await_of_with_nested_await() -> Result<(), VmError> {
  let mut rt = new_runtime();

  // Use a manual async iterator (no `async function` / generators) so this tests the top-level
  // await fallback rather than the async-function AST fallback.
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "compiled_top_level_for_await_of_nested_await_fallback.js",
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
        // Nested awaits inside the loop body are not yet supported by the compiled (HIR) async
        // classic-script executor.
        out = await Promise.resolve(x);
      }
      out
    "#,
  )?;
  assert!(script.contains_top_level_await);
  assert!(
    script.top_level_await_requires_ast_fallback,
    "top-level for-await-of loops with nested await are not supported by the HIR async classic-script executor"
  );

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
fn compiled_script_executes_top_level_for_await_of_with_await_rhs() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "compiled_top_level_for_await_of_await_rhs.js",
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

      for await (var x of await Promise.resolve(iterable)) {
        out = x;
      }
      out
    "#,
  )?;
  assert!(script.contains_top_level_await);
  assert!(
    !script.top_level_await_requires_ast_fallback,
    "for-await-of should be supported by the HIR async classic-script executor even when the RHS is a direct await expression"
  );
  assert!(
    !script.requires_ast_fallback,
    "for-await-of with direct await RHS should not trigger a full AST fallback"
  );

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
fn compiled_script_top_level_for_await_of_break_invokes_iterator_return() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "compiled_top_level_for_await_of_break_invokes_return.js",
    r#"
      var out = "";
      var return_calls = 0;
      var iter = {
        i: 0,
        next: function () {
          if (this.i++ === 0) return Promise.resolve({ value: "ok", done: false });
          return Promise.resolve({ value: undefined, done: true });
        },
        return: function () {
          return_calls++;
          return Promise.resolve({ done: true });
        },
      };
      var iterable = {};
      iterable[Symbol.asyncIterator] = function () { return iter; };

      for await (var x of iterable) {
        out = x;
        break;
      }
     out
    "#,
  )?;
  assert!(script.contains_top_level_await);
  assert!(
    !script.top_level_await_requires_ast_fallback,
    "top-level for-await-of should be supported by the HIR async classic-script executor"
  );
  assert!(
    !script.requires_ast_fallback,
    "top-level for-await-of should not trigger a full AST fallback"
  );

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
  let return_calls = rt.exec_script("return_calls")?;
  assert_eq!(return_calls, Value::Number(0.0));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);
  let result = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("fulfilled promise should have a result");
  assert_eq!(value_to_string(&rt, result), "ok");

  let return_calls = rt.exec_script("return_calls")?;
  assert_eq!(
    return_calls,
    Value::Number(1.0),
    "breaking out of for-await-of must call iterator.return()"
  );

  rt.heap_mut().remove_root(completion_root);
  Ok(())
}

#[test]
fn compiled_script_falls_back_for_await_in_nested_stmt_list() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "compiled_top_level_await_in_if_block_fallback.js",
    r#"
      var out = "";
      if (true) {
        out = await Promise.resolve("ok");
      }
      out
    "#,
  )?;
  assert!(script.contains_top_level_await);
  assert!(
    script.top_level_await_requires_ast_fallback,
    "await inside nested statement lists is not supported by the HIR async classic-script executor"
  );

  let completion = rt.exec_compiled_script(script)?;
  let completion_root = rt.heap_mut().add_root(completion)?;

  let Value::Object(promise_obj) = completion else {
    panic!("expected Promise object from top-level await script, got {completion:?}");
  };
  assert!(rt.heap().is_promise_object(promise_obj));
  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Pending);

  // Await in the nested statement list should not have executed until we run microtasks.
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
fn compiled_script_falls_back_for_nested_await_in_expression() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "compiled_top_level_nested_await_fallback.js",
    r#"
      var out = "";
      function set(x) { out = x; }
      set(await Promise.resolve("ok"));
      out
    "#,
  )?;
  assert!(script.contains_top_level_await);
  assert!(
    script.top_level_await_requires_ast_fallback,
    "nested await inside an expression is not supported by the HIR async classic-script executor"
  );

  let completion = rt.exec_compiled_script(script)?;
  let completion_root = rt.heap_mut().add_root(completion)?;

  let Value::Object(promise_obj) = completion else {
    panic!("expected Promise object from top-level await script, got {completion:?}");
  };
  assert!(rt.heap().is_promise_object(promise_obj));
  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Pending);

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
fn compiled_script_falls_back_for_await_in_computed_member_key() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "compiled_top_level_await_in_computed_member_key_fallback.js",
    r#"
      var log = [];
      var obj = {};
      obj[await Promise.resolve((log.push("key"), "k"))] = (log.push("rhs"), "v");
      log.push("after");
      obj.k
    "#,
  )?;
  assert!(script.contains_top_level_await);
  assert!(
    script.top_level_await_requires_ast_fallback,
    "await in a computed member key is not supported by the HIR async classic-script executor"
  );

  let completion = rt.exec_compiled_script(script)?;
  let completion_root = rt.heap_mut().add_root(completion)?;

  let Value::Object(promise_obj) = completion else {
    panic!("expected Promise object from top-level await script, got {completion:?}");
  };
  assert!(rt.heap().is_promise_object(promise_obj));
  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Pending);

  // Only the computed member key evaluation should have occurred so far.
  let log = rt.exec_script("log.join(',')")?;
  assert_eq!(value_to_string(&rt, log), "key");
  assert_eq!(rt.exec_script("obj.k")?, Value::Undefined);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);
  let result = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("fulfilled promise should have a result");
  assert_eq!(value_to_string(&rt, result), "v");

  let log = rt.exec_script("log.join(',')")?;
  assert_eq!(value_to_string(&rt, log), "key,rhs,after");
  let obj_k = rt.exec_script("obj.k")?;
  assert_eq!(value_to_string(&rt, obj_k), "v");

  rt.heap_mut().remove_root(completion_root);
  Ok(())
}

#[test]
fn compiled_script_top_level_await_assignment_suspends_and_resumes() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "compiled_top_level_await_assignment.js",
    r#"
      var out = "";
      out = await Promise.resolve("ok");
      out
    "#,
  )?;
  assert!(script.contains_top_level_await);
  assert!(
    !script.top_level_await_requires_ast_fallback,
    "assignment-await should be supported by the HIR async classic-script executor"
  );
  assert!(
    !script.requires_ast_fallback,
    "assignment-await should not trigger the general compiled-script AST fallback"
  );

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
fn compiled_script_top_level_await_member_assignment_suspends_and_resumes() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "compiled_top_level_await_member_assignment.js",
    r#"
      var obj = { x: "" };
      obj.x = await Promise.resolve("ok");
      obj.x
    "#,
  )?;
  assert!(script.contains_top_level_await);
  assert!(
    !script.top_level_await_requires_ast_fallback,
    "member assignment-await should be supported by the HIR async classic-script executor"
  );
  assert!(
    !script.requires_ast_fallback,
    "member assignment-await should not trigger the general compiled-script AST fallback"
  );

  let completion = rt.exec_compiled_script(script)?;
  let completion_root = rt.heap_mut().add_root(completion)?;

  let Value::Object(promise_obj) = completion else {
    panic!("expected Promise object from top-level await script, got {completion:?}");
  };
  assert!(rt.heap().is_promise_object(promise_obj));
  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Pending);

  // The assignment after `await` should not have executed yet.
  let obj_x = rt.exec_script("obj.x")?;
  assert_eq!(value_to_string(&rt, obj_x), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);
  let result = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("fulfilled promise should have a result");
  assert_eq!(value_to_string(&rt, result), "ok");

  let obj_x = rt.exec_script("obj.x")?;
  assert_eq!(value_to_string(&rt, obj_x), "ok");

  rt.heap_mut().remove_root(completion_root);
  Ok(())
}
