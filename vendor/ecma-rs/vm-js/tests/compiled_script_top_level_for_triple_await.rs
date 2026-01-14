use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, PromiseState, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Promise + microtask machinery needs a bit of heap headroom.
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
fn compiled_script_top_level_for_triple_await_in_init_suspends_and_resumes() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      var log = [];
      var x = 0;
      for (x = await Promise.resolve(1); x < 3; x++) {
        log.push(x);
      }
      log.push("done");
      log.join(",")
    "#,
  )?;
  assert!(script.contains_top_level_await);
  assert!(
    !script.top_level_await_requires_ast_fallback,
    "top-level await in for-loop init should be supported by the HIR async classic-script executor"
  );
  assert!(
    !script.requires_ast_fallback,
    "supported top-level await scripts should not trigger the general compiled-script AST fallback"
  );

  let result = rt.exec_compiled_script(script)?;
  let result_root = rt.heap_mut().add_root(result)?;

  let Value::Object(promise_obj) = result else {
    panic!("expected Promise object, got {result:?}");
  };
  assert!(rt.heap().is_promise_object(promise_obj));
  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Pending);

  // The init assignment after `await` should not have executed yet.
  assert_eq!(rt.exec_script("x")?, Value::Number(0.0));
  let before_log = rt.exec_script("log.join(',')")?;
  assert_eq!(value_to_utf8(&rt, before_log), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);
  let resolved = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("fulfilled promise should have a result");
  assert_eq!(value_to_utf8(&rt, resolved), "1,2,done");

  rt.heap_mut().remove_root(result_root);
  Ok(())
}

#[test]
fn compiled_script_top_level_for_triple_await_in_init_var_decl_suspends_and_resumes() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      var log = [];
      var x;
      for (var x = await Promise.resolve(1); x < 3; x++) {
        log.push(x);
      }
      log.push("done");
      log.join(",")
    "#,
  )?;
  assert!(script.contains_top_level_await);
  assert!(
    !script.top_level_await_requires_ast_fallback,
    "top-level await in for-loop init var declaration should be supported by the HIR async classic-script executor"
  );
  assert!(
    !script.requires_ast_fallback,
    "supported top-level await scripts should not trigger the general compiled-script AST fallback"
  );

  let result = rt.exec_compiled_script(script)?;
  let result_root = rt.heap_mut().add_root(result)?;

  let Value::Object(promise_obj) = result else {
    panic!("expected Promise object, got {result:?}");
  };
  assert!(rt.heap().is_promise_object(promise_obj));
  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Pending);

  // The init declaration after `await` should not have executed yet.
  assert_eq!(rt.exec_script("x")?, Value::Undefined);
  let before_log = rt.exec_script("log.join(',')")?;
  assert_eq!(value_to_utf8(&rt, before_log), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);
  let resolved = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("fulfilled promise should have a result");
  assert_eq!(value_to_utf8(&rt, resolved), "1,2,done");

  rt.heap_mut().remove_root(result_root);
  Ok(())
}

#[test]
fn compiled_script_top_level_for_triple_await_in_init_let_decl_suspends_and_resumes() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      var log = [];
      for (let x = await Promise.resolve(1); x < 3; x++) {
        log.push(x);
      }
      log.push("done");
      log.join(",")
    "#,
  )?;
  assert!(script.contains_top_level_await);
  assert!(
    !script.top_level_await_requires_ast_fallback,
    "top-level await in for-loop init let declaration should be supported by the HIR async classic-script executor"
  );
  assert!(
    !script.requires_ast_fallback,
    "supported top-level await scripts should not trigger the general compiled-script AST fallback"
  );

  let result = rt.exec_compiled_script(script)?;
  let result_root = rt.heap_mut().add_root(result)?;

  let Value::Object(promise_obj) = result else {
    panic!("expected Promise object, got {result:?}");
  };
  assert!(rt.heap().is_promise_object(promise_obj));
  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Pending);

  // The init declaration after `await` should not have executed yet.
  let before_log = rt.exec_script("log.join(',')")?;
  assert_eq!(value_to_utf8(&rt, before_log), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);
  let resolved = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("fulfilled promise should have a result");
  assert_eq!(value_to_utf8(&rt, resolved), "1,2,done");

  rt.heap_mut().remove_root(result_root);
  Ok(())
}

#[test]
fn compiled_script_top_level_for_triple_await_in_init_destructuring_assignment_suspends_and_resumes(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      var log = [];
      var x = 0;
      for (({ x } = await Promise.resolve({ x: 1 })); x < 3; x++) {
        log.push(x);
      }
      log.push("done");
      log.join(",")
    "#,
  )?;
  assert!(script.contains_top_level_await);
  assert!(
    !script.top_level_await_requires_ast_fallback,
    "top-level await in for-loop init destructuring assignment should be supported by the HIR async classic-script executor"
  );
  assert!(
    !script.requires_ast_fallback,
    "supported top-level await scripts should not trigger the general compiled-script AST fallback"
  );

  let result = rt.exec_compiled_script(script)?;
  let result_root = rt.heap_mut().add_root(result)?;

  let Value::Object(promise_obj) = result else {
    panic!("expected Promise object, got {result:?}");
  };
  assert!(rt.heap().is_promise_object(promise_obj));
  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Pending);

  // The init destructuring assignment after `await` should not have executed yet.
  assert_eq!(rt.exec_script("x")?, Value::Number(0.0));
  let before_log = rt.exec_script("log.join(',')")?;
  assert_eq!(value_to_utf8(&rt, before_log), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);
  let resolved = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("fulfilled promise should have a result");
  assert_eq!(value_to_utf8(&rt, resolved), "1,2,done");

  rt.heap_mut().remove_root(result_root);
  Ok(())
}

#[test]
fn compiled_script_top_level_for_triple_logical_assignment_in_init_roots_lhs_reference_across_gc(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      var log = [];
      function makeObj() {
        return {
          get x() { log.push("get"); return 0; },
          set x(v) { log.push("set:" + v); },
        };
      }

      for (makeObj().x ||= await Promise.resolve("ok"); false; ) {}
      log.join(",")
    "#,
  )?;
  assert!(script.contains_top_level_await);
  assert!(
    !script.top_level_await_requires_ast_fallback,
    "top-level await in a for-loop init logical assignment should be supported by the HIR async classic-script executor"
  );

  let result = rt.exec_compiled_script(script)?;
  let result_root = rt.heap_mut().add_root(result)?;

  let Value::Object(promise_obj) = result else {
    panic!("expected Promise object, got {result:?}");
  };
  assert!(rt.heap().is_promise_object(promise_obj));
  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Pending);

  // The loop init has suspended; force a full GC while the loop state machine holds the pending
  // assignment reference.
  rt.heap.collect_garbage();

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);
  let resolved = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("fulfilled promise should have a result");
  assert_eq!(value_to_utf8(&rt, resolved), "get,set:ok");

  rt.heap_mut().remove_root(result_root);
  Ok(())
}

#[test]
fn compiled_script_top_level_for_triple_logical_assignment_in_init_short_circuits_without_awaiting(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      var log = [];
      var x = 1;
      for (x ||= await Promise.resolve((log.push("rhs"), 2)); false; ) {}
      log.push("after");
      log.join(",")
    "#,
  )?;
  assert!(script.contains_top_level_await);
  assert!(
    !script.top_level_await_requires_ast_fallback,
    "top-level await in a for-loop init logical assignment should be supported by the HIR async classic-script executor"
  );

  let result = rt.exec_compiled_script(script)?;
  let Value::Object(promise_obj) = result else {
    panic!("expected Promise object, got {result:?}");
  };
  assert!(rt.heap().is_promise_object(promise_obj));
  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);

  let resolved = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("fulfilled promise should have a result");
  assert_eq!(value_to_utf8(&rt, resolved), "after");

  assert_eq!(rt.exec_script("x")?, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_script_top_level_for_triple_await_in_test_suspends_and_resumes() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      var log = [];
      var x = 0;
      for (; await Promise.resolve(x < 3); x++) {
        log.push(x);
      }
      log.join(",")
    "#,
  )?;
  assert!(script.contains_top_level_await);
  assert!(
    !script.top_level_await_requires_ast_fallback,
    "top-level await in for-loop test should be supported by the HIR async classic-script executor"
  );

  let result = rt.exec_compiled_script(script)?;
  let result_root = rt.heap_mut().add_root(result)?;

  let Value::Object(promise_obj) = result else {
    panic!("expected Promise object, got {result:?}");
  };
  assert!(rt.heap().is_promise_object(promise_obj));
  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Pending);

  // The loop body should not have executed yet (the loop suspends while evaluating the test).
  let before_log = rt.exec_script("log.join(',')")?;
  assert_eq!(value_to_utf8(&rt, before_log), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);
  let resolved = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("fulfilled promise should have a result");
  assert_eq!(value_to_utf8(&rt, resolved), "0,1,2");

  rt.heap_mut().remove_root(result_root);
  Ok(())
}

#[test]
fn compiled_script_top_level_for_triple_await_in_update_suspends_and_resumes() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      var log = [];
      var x = 0;
      for (; x < 3; x = await Promise.resolve(x + 1)) {
        log.push(x);
      }
      log.join(",")
    "#,
  )?;
  assert!(script.contains_top_level_await);
  assert!(
    !script.top_level_await_requires_ast_fallback,
    "top-level await in for-loop update should be supported by the HIR async classic-script executor"
  );

  let result = rt.exec_compiled_script(script)?;
  let result_root = rt.heap_mut().add_root(result)?;

  let Value::Object(promise_obj) = result else {
    panic!("expected Promise object, got {result:?}");
  };
  assert!(rt.heap().is_promise_object(promise_obj));
  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Pending);

  // The first loop iteration runs synchronously before we suspend at the update `await`.
  let before_log = rt.exec_script("log.join(',')")?;
  assert_eq!(value_to_utf8(&rt, before_log), "0");
  assert_eq!(rt.exec_script("x")?, Value::Number(0.0));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);
  let resolved = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("fulfilled promise should have a result");
  assert_eq!(value_to_utf8(&rt, resolved), "0,1,2");

  rt.heap_mut().remove_root(result_root);
  Ok(())
}

#[test]
fn compiled_script_top_level_for_triple_await_in_update_destructuring_assignment_suspends_and_resumes(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      var log = [];
      var x = 0;
      for (; x < 3; ([x] = await Promise.resolve([x + 1]))) {
        log.push(x);
      }
      log.join(",")
    "#,
  )?;
  assert!(script.contains_top_level_await);
  assert!(
    !script.top_level_await_requires_ast_fallback,
    "top-level await in for-loop update destructuring assignment should be supported by the HIR async classic-script executor"
  );

  let result = rt.exec_compiled_script(script)?;
  let result_root = rt.heap_mut().add_root(result)?;

  let Value::Object(promise_obj) = result else {
    panic!("expected Promise object, got {result:?}");
  };
  assert!(rt.heap().is_promise_object(promise_obj));
  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Pending);

  // The first loop iteration runs synchronously before we suspend at the update `await`.
  let before_log = rt.exec_script("log.join(',')")?;
  assert_eq!(value_to_utf8(&rt, before_log), "0");
  assert_eq!(rt.exec_script("x")?, Value::Number(0.0));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);
  let resolved = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("fulfilled promise should have a result");
  assert_eq!(value_to_utf8(&rt, resolved), "0,1,2");

  rt.heap_mut().remove_root(result_root);
  Ok(())
}

#[test]
fn compiled_script_top_level_labeled_for_triple_with_await_in_head_is_supported() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      var log = [];
      outer: for (; await Promise.resolve(true); ) {
        log.push("body");
        break outer;
      }
      log.join(",")
    "#,
  )?;
  assert!(script.contains_top_level_await);
  assert!(
    !script.top_level_await_requires_ast_fallback,
    "top-level await in a labeled for-loop head should be supported by the HIR async classic-script executor"
  );

  let result = rt.exec_compiled_script(script)?;
  let result_root = rt.heap_mut().add_root(result)?;

  let Value::Object(promise_obj) = result else {
    panic!("expected Promise object, got {result:?}");
  };
  assert!(rt.heap().is_promise_object(promise_obj));
  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Pending);

  let before_log = rt.exec_script("log.join(',')")?;
  assert_eq!(value_to_utf8(&rt, before_log), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);
  let resolved = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("fulfilled promise should have a result");
  assert_eq!(value_to_utf8(&rt, resolved), "body");

  rt.heap_mut().remove_root(result_root);
  Ok(())
}
