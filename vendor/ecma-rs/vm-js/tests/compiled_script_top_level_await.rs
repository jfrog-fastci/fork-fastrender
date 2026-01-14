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

fn value_to_number(value: Value) -> f64 {
  let Value::Number(n) = value else {
    panic!("expected number, got {value:?}");
  };
  n
}

#[test]
fn compiled_script_top_level_await_executes_via_hir_and_resumes_in_microtasks() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      var actual = [];
      actual.push("pre");
      await Promise.resolve(0);
      actual.push("post");
    "#,
  )?;
  assert!(script.contains_top_level_await);
  assert!(
    !script.top_level_await_requires_ast_fallback,
    "simple top-level await should be supported by the HIR async classic-script executor"
  );

  let result = rt.exec_compiled_script(script)?;
  let Value::Object(promise_obj) = result else {
    panic!("expected Promise object, got {result:?}");
  };
  assert!(
    rt.heap().is_promise_object(promise_obj),
    "expected Promise return value from async classic script"
  );

  let before = rt.exec_script("JSON.stringify(actual)")?;
  assert_eq!(value_to_utf8(&rt, before), r#"["pre"]"#);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let after = rt.exec_script("JSON.stringify(actual)")?;
  assert_eq!(value_to_utf8(&rt, after), r#"["pre","post"]"#);

  Ok(())
}

#[test]
fn compiled_script_top_level_await_in_var_initializer_suspends_and_resumes() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      var actual = [];
      const x = await Promise.resolve("ok");
      actual.push(x);
    "#,
  )?;
  assert!(script.contains_top_level_await);
  assert!(
    !script.top_level_await_requires_ast_fallback,
    "top-level await in a var/let/const initializer should be supported by the HIR async classic-script executor"
  );

  let result = rt.exec_compiled_script(script)?;
  let Value::Object(promise_obj) = result else {
    panic!("expected Promise object, got {result:?}");
  };
  assert!(
    rt.heap().is_promise_object(promise_obj),
    "expected Promise return value from async classic script"
  );

  let before = rt.exec_script("JSON.stringify(actual)")?;
  assert_eq!(value_to_utf8(&rt, before), r#"[]"#);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let after = rt.exec_script("JSON.stringify(actual)")?;
  assert_eq!(value_to_utf8(&rt, after), r#"["ok"]"#);

  Ok(())
}

#[test]
fn compiled_script_top_level_await_in_assignment_suspends_and_resumes() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      var out = "";
      out = await Promise.resolve("ok");
      out
    "#,
  )?;
  assert!(script.contains_top_level_await);
  assert!(
    !script.top_level_await_requires_ast_fallback,
    "top-level await in a simple assignment should be supported by the HIR async classic-script executor"
  );

  let result = rt.exec_compiled_script(script)?;
  let result_root = rt.heap_mut().add_root(result)?;

  let Value::Object(promise_obj) = result else {
    panic!("expected Promise object, got {result:?}");
  };
  assert!(
    rt.heap().is_promise_object(promise_obj),
    "expected Promise return value from async classic script"
  );

  // The assignment after `await` should not have executed yet.
  let before = rt.exec_script("out")?;
  assert_eq!(value_to_utf8(&rt, before), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let after = rt.exec_script("out")?;
  assert_eq!(value_to_utf8(&rt, after), "ok");

  rt.heap_mut().remove_root(result_root);
  Ok(())
}

#[test]
fn compiled_script_top_level_await_in_computed_member_assignment_preserves_eval_order() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      var log = [];
      var obj = {};
      obj[(log.push("key"), "k")] = await Promise.resolve((log.push("rhs"), "v"));
      log.push("after");
      obj.k
    "#,
  )?;
  assert!(script.contains_top_level_await);
  assert!(
    !script.top_level_await_requires_ast_fallback,
    "top-level await in a computed member assignment should be supported by the HIR async classic-script executor"
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
  assert!(
    rt.heap().is_promise_object(promise_obj),
    "expected Promise return value from async classic script"
  );

  // The assignment should not have executed yet, but the assignment *reference* (including the
  // computed key) and the await argument should have been evaluated in order.
  let before_log = rt.exec_script("log.join(',')")?;
  assert_eq!(value_to_utf8(&rt, before_log), "key,rhs");

  let before_k = rt.exec_script("obj.k")?;
  assert_eq!(before_k, Value::Undefined);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let after_log = rt.exec_script("log.join(',')")?;
  assert_eq!(value_to_utf8(&rt, after_log), "key,rhs,after");

  let after_k = rt.exec_script("obj.k")?;
  assert_eq!(value_to_utf8(&rt, after_k), "v");

  rt.heap_mut().remove_root(result_root);
  Ok(())
}

#[test]
fn compiled_script_top_level_await_in_compound_assignment_reads_lhs_before_await() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      var x = 1;
      Promise.resolve().then(() => { x = 100; });
      x += await Promise.resolve(2);
      x
    "#,
  )?;
  assert!(script.contains_top_level_await);
  assert!(
    !script.top_level_await_requires_ast_fallback,
    "top-level await in a compound assignment should be supported by the HIR async classic-script executor"
  );

  let result = rt.exec_compiled_script(script)?;
  let result_root = rt.heap_mut().add_root(result)?;

  let Value::Object(promise_obj) = result else {
    panic!("expected Promise object, got {result:?}");
  };
  assert!(
    rt.heap().is_promise_object(promise_obj),
    "expected Promise return value from async classic script"
  );

  // The compound assignment should not have executed yet (and the microtask that mutates `x` should
  // not have run yet either).
  let before = rt.exec_script("x")?;
  assert_eq!(value_to_number(before), 1.0);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  // Spec: compound assignment reads the LHS value before evaluating the RHS. The microtask that
  // mutates `x` runs before the await continuation resumes, but `x += await ...` must still use the
  // original LHS value (1) rather than the updated value (100).
  let after = rt.exec_script("x")?;
  assert_eq!(value_to_number(after), 3.0);

  rt.heap_mut().remove_root(result_root);
  Ok(())
}

#[test]
fn compiled_script_top_level_await_assignment_roots_lhs_reference_across_gc() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      var log = [];
      function makeObj() { return { set x(v) { log.push("set:" + v); } }; }
      makeObj().x = await Promise.resolve("ok");
      log.join(",")
    "#,
  )?;
  assert!(script.contains_top_level_await);
  assert!(
    !script.top_level_await_requires_ast_fallback,
    "top-level await assignment should be supported by the HIR async classic-script executor"
  );

  let result = rt.exec_compiled_script(script)?;
  let result_root = rt.heap_mut().add_root(result)?;

  let Value::Object(promise_obj) = result else {
    panic!("expected Promise object, got {result:?}");
  };
  assert!(
    rt.heap().is_promise_object(promise_obj),
    "expected Promise return value from async classic script"
  );
  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Pending);

  // Force a full GC while the async classic script is suspended. The assignment target object is
  // not referenced from user code after LHS evaluation, so the compiled executor must keep the
  // assignment reference alive via persistent roots.
  rt.heap.collect_garbage();

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);
  let promise_result = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("fulfilled promise should have a result");
  assert_eq!(value_to_utf8(&rt, promise_result), "set:ok");

  let log = rt.exec_script("log.join(',')")?;
  assert_eq!(value_to_utf8(&rt, log), "set:ok");

  rt.heap_mut().remove_root(result_root);
  Ok(())
}

#[test]
fn compiled_script_top_level_await_in_member_compound_assignment_reads_lhs_before_await() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      var obj = { x: 1 };
      Promise.resolve().then(() => { obj.x = 100; });
      obj.x += await Promise.resolve(2);
      obj.x
    "#,
  )?;
  assert!(script.contains_top_level_await);
  assert!(
    !script.top_level_await_requires_ast_fallback,
    "top-level await in a member compound assignment should be supported by the HIR async classic-script executor"
  );

  let result = rt.exec_compiled_script(script)?;
  let result_root = rt.heap_mut().add_root(result)?;

  let Value::Object(promise_obj) = result else {
    panic!("expected Promise object, got {result:?}");
  };
  assert!(
    rt.heap().is_promise_object(promise_obj),
    "expected Promise return value from async classic script"
  );

  // The compound assignment should not have executed yet.
  let before = rt.exec_script("obj.x")?;
  assert_eq!(value_to_number(before), 1.0);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  // `obj.x` is mutated by an earlier microtask before the await continuation resumes; the compound
  // assignment must still use the original LHS value (1).
  let after = rt.exec_script("obj.x")?;
  assert_eq!(value_to_number(after), 3.0);

  rt.heap_mut().remove_root(result_root);
  Ok(())
}
