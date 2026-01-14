use vm_js::{
  CompiledScript, GcObject, Heap, HeapLimits, JsRuntime, PromiseState, PropertyKey, PropertyKind, Value, Vm, VmError,
  VmOptions,
};

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

fn get_data_property(rt: &mut JsRuntime, obj: GcObject, name: &str) -> Result<Value, VmError> {
  // Root the object while allocating the property key string in case the allocation triggers GC.
  let mut scope = rt.heap_mut().scope();
  scope.push_root(Value::Object(obj))?;
  let key = PropertyKey::from_string(scope.alloc_string(name)?);
  let Some(desc) = scope.heap().get_property(obj, &key)? else {
    return Err(VmError::InvariantViolation("expected property on object"));
  };
  match desc.kind {
    PropertyKind::Data { value, .. } => Ok(value),
    PropertyKind::Accessor { .. } => Err(VmError::InvariantViolation("expected data property")),
  }
}

fn error_name(rt: &mut JsRuntime, err: Value) -> Result<String, VmError> {
  let Value::Object(obj) = err else {
    panic!("expected error object, got {err:?}");
  };
  let name = get_data_property(rt, obj, "name")?;
  Ok(value_to_utf8(rt, name))
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
fn compiled_script_top_level_await_assignment_infers_function_name_for_binding() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      var f;
      f = await Promise.resolve(function(){});
      f.name
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

  // The assignment should not have executed yet.
  let before = rt.exec_script("f")?;
  assert_eq!(before, Value::Undefined);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);
  let promise_result = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("fulfilled promise should have a result");
  assert_eq!(value_to_utf8(&rt, promise_result), "f");

  rt.heap_mut().remove_root(result_root);
  Ok(())
}

#[test]
fn compiled_script_top_level_await_assignment_infers_function_name_for_property() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      var o = {};
      o.m = await Promise.resolve(function(){});
      o.m.name
    "#,
  )?;
  assert!(script.contains_top_level_await);
  assert!(
    !script.top_level_await_requires_ast_fallback,
    "top-level await member assignment should be supported by the HIR async classic-script executor"
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

  // The assignment should not have executed yet.
  let before_keys = rt.exec_script("Object.keys(o).join(',')")?;
  assert_eq!(value_to_utf8(&rt, before_keys), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);
  let promise_result = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("fulfilled promise should have a result");
  assert_eq!(value_to_utf8(&rt, promise_result), "m");

  rt.heap_mut().remove_root(result_root);
  Ok(())
}

#[test]
fn compiled_script_top_level_await_assignment_infers_function_name_for_computed_property() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      var o = {};
      o["m"] = await Promise.resolve(function(){});
      o.m.name
    "#,
  )?;
  assert!(script.contains_top_level_await);
  assert!(
    !script.top_level_await_requires_ast_fallback,
    "top-level await computed member assignment should be supported by the HIR async classic-script executor"
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

  // The assignment should not have executed yet.
  let before_keys = rt.exec_script("Object.keys(o).join(',')")?;
  assert_eq!(value_to_utf8(&rt, before_keys), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);
  let promise_result = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("fulfilled promise should have a result");
  assert_eq!(value_to_utf8(&rt, promise_result), "m");

  rt.heap_mut().remove_root(result_root);
  Ok(())
}

#[test]
fn compiled_script_top_level_await_assignment_infers_class_name_for_binding() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      var C;
      C = await Promise.resolve(class{});
      C.name
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

  // The assignment should not have executed yet.
  let before = rt.exec_script("C")?;
  assert_eq!(before, Value::Undefined);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);
  let promise_result = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("fulfilled promise should have a result");
  assert_eq!(value_to_utf8(&rt, promise_result), "C");

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
fn compiled_script_top_level_await_computed_member_assignment_to_property_key_is_not_recomputed_after_await(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      var log = [];
      var obj = {};
      var key = { toString() { log.push("toString"); return "k"; } };
      Promise.resolve().then(() => {
        key.toString = function() { log.push("toString2"); return "x"; };
        log.push("mt");
      });
      obj[key] = await Promise.resolve((log.push("rhs"), "v"));
      log.push("after");
      Object.keys(obj).join(",")
    "#,
  )?;
  assert!(script.contains_top_level_await);
  assert!(
    !script.top_level_await_requires_ast_fallback,
    "top-level await in a computed member assignment should be supported by the HIR async classic-script executor"
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

  // `ToPropertyKey(key)` must happen before evaluating the await operand and must not be repeated
  // after resumption.
  let before_log = rt.exec_script("log.join(',')")?;
  assert_eq!(value_to_utf8(&rt, before_log), "toString,rhs");
  let before_keys = rt.exec_script("Object.keys(obj).join(',')")?;
  assert_eq!(value_to_utf8(&rt, before_keys), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);
  let promise_result = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("fulfilled promise should have a result");
  assert_eq!(value_to_utf8(&rt, promise_result), "k");

  let after_log = rt.exec_script("log.join(',')")?;
  assert_eq!(value_to_utf8(&rt, after_log), "toString,rhs,mt,after");
  let v = rt.exec_script("obj.k")?;
  assert_eq!(value_to_utf8(&rt, v), "v");

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
fn compiled_script_top_level_await_computed_member_assignment_roots_key_across_gc() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      var log = [];
      function makeObj() { return { set x1(v) { log.push("set:" + v); } }; }
      makeObj()[("x" + 1)] = await Promise.resolve("ok");
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

  // Force a full GC while the async classic script is suspended. The computed property key is not
  // referenced by user code after LHS evaluation, so the compiled executor must keep both the base
  // and key alive via persistent roots.
  rt.heap.collect_garbage();

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);
  let promise_result = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("fulfilled promise should have a result");
  assert_eq!(value_to_utf8(&rt, promise_result), "set:ok");

  rt.heap_mut().remove_root(result_root);
  Ok(())
}

#[test]
fn compiled_script_top_level_await_member_assignment_on_primitive_base_strict_mode_throws_after_await(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      "use strict";
      var log = [];
      Promise.resolve().then(() => { log.push("mt"); });
      ("s").x = await Promise.resolve((log.push("rhs"), 1));
      log.push("after");
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

  // The await argument should have been evaluated, but the assignment should not have completed yet.
  let log = rt.exec_script("log.join(',')")?;
  assert_eq!(value_to_utf8(&rt, log), "rhs");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  // Strict-mode assignment to a property on a primitive base throws TypeError at PutValue time.
  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Rejected);
  let reason = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("rejected promise should have a reason");
  assert_eq!(error_name(&mut rt, reason)?, "TypeError");

  // The pre-scheduled microtask runs before await resumption; ensure it ran and that the statement
  // after the failing assignment did not.
  let log = rt.exec_script("log.join(',')")?;
  assert_eq!(value_to_utf8(&rt, log), "rhs,mt");

  rt.heap_mut().remove_root(result_root);
  Ok(())
}

#[test]
fn compiled_script_top_level_await_member_assignment_to_read_only_property_sloppy_mode_does_not_put_value(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      var log = [];
      var obj = {};
      Object.defineProperty(obj, "x", { value: 1, writable: false });
      obj.x = await Promise.resolve((log.push("rhs"), 2));
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

  // The await argument should have been evaluated, but the PutValue step should not have happened yet.
  let log = rt.exec_script("log.join(',')")?;
  assert_eq!(value_to_utf8(&rt, log), "rhs");
  let x = rt.exec_script("obj.x")?;
  assert_eq!(value_to_number(x), 1.0);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  // Sloppy-mode assignment to a non-writable property fails silently, but the assignment expression
  // still evaluates to the RHS value.
  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);
  let promise_result = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("fulfilled promise should have a result");
  assert_eq!(value_to_number(promise_result), 2.0);

  let x = rt.exec_script("obj.x")?;
  assert_eq!(value_to_number(x), 1.0);

  rt.heap_mut().remove_root(result_root);
  Ok(())
}

#[test]
fn compiled_script_top_level_await_member_assignment_to_read_only_property_strict_mode_throws_after_await(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      "use strict";
      var log = [];
      var obj = {};
      Object.defineProperty(obj, "x", { value: 1, writable: false });
      Promise.resolve().then(() => { log.push("mt"); });
      obj.x = await Promise.resolve((log.push("rhs"), 2));
      log.push("after");
      obj.x
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

  let log = rt.exec_script("log.join(',')")?;
  assert_eq!(value_to_utf8(&rt, log), "rhs");
  let x = rt.exec_script("obj.x")?;
  assert_eq!(value_to_number(x), 1.0);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  // Strict-mode assignment to a non-writable property throws TypeError at PutValue time.
  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Rejected);
  let reason = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("rejected promise should have a reason");
  assert_eq!(error_name(&mut rt, reason)?, "TypeError");

  // The pre-scheduled microtask runs before await resumption; ensure it ran and that the statement
  // after the failing assignment did not.
  let log = rt.exec_script("log.join(',')")?;
  assert_eq!(value_to_utf8(&rt, log), "rhs,mt");
  let x = rt.exec_script("obj.x")?;
  assert_eq!(value_to_number(x), 1.0);

  rt.heap_mut().remove_root(result_root);
  Ok(())
}

#[test]
fn compiled_script_top_level_await_assignment_rejection_does_not_put_value() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      var log = [];
      function getObj() { log.push("target"); return { set x(v) { log.push("set:" + v); } }; }
      getObj().x = await Promise.reject((log.push("rhs"), "boom"));
      log.push("after");
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

  // The LHS assignment reference and await argument should have executed, but the setter and
  // following statements should not have.
  let log = rt.exec_script("log.join(',')")?;
  assert_eq!(value_to_utf8(&rt, log), "target,rhs");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Rejected);
  let reason = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("rejected promise should have a reason");
  assert_eq!(value_to_utf8(&rt, reason), "boom");

  let log = rt.exec_script("log.join(',')")?;
  assert_eq!(value_to_utf8(&rt, log), "target,rhs");

  rt.heap_mut().remove_root(result_root);
  Ok(())
}

#[test]
fn compiled_script_top_level_await_compound_assignment_rejection_does_not_put_value() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      var log = [];
      Object.defineProperty(globalThis, "x", {
        get() { log.push("get"); return 1; },
        set(v) { log.push("set:" + v); }
      });
      x += await Promise.reject((log.push("rhs"), "boom"));
      log.push("after");
      log.join(",")
    "#,
  )?;
  assert!(script.contains_top_level_await);
  assert!(
    !script.top_level_await_requires_ast_fallback,
    "top-level await compound assignment should be supported by the HIR async classic-script executor"
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

  // The LHS value and RHS expression should have been evaluated, but the assignment should not
  // have completed yet (and the setter + subsequent statements should not have run).
  let log = rt.exec_script("log.join(',')")?;
  assert_eq!(value_to_utf8(&rt, log), "get,rhs");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Rejected);
  let reason = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("rejected promise should have a reason");
  assert_eq!(value_to_utf8(&rt, reason), "boom");

  let log = rt.exec_script("log.join(',')")?;
  assert_eq!(value_to_utf8(&rt, log), "get,rhs");

  rt.heap_mut().remove_root(result_root);
  Ok(())
}

#[test]
fn compiled_script_top_level_await_computed_member_assignment_rejection_does_not_put_value() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      var log = [];
      var obj = {};
      obj[(log.push("key"), "k")] = await Promise.reject((log.push("rhs"), "boom"));
      log.push("after");
      log.join(",")
    "#,
  )?;
  assert!(script.contains_top_level_await);
  assert!(
    !script.top_level_await_requires_ast_fallback,
    "top-level await in a computed member assignment should be supported by the HIR async classic-script executor"
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

  // The assignment reference (including the computed key) and await argument should have been
  // evaluated, but the assignment and subsequent statements should not have.
  let log = rt.exec_script("log.join(',')")?;
  assert_eq!(value_to_utf8(&rt, log), "key,rhs");

  let before_k = rt.exec_script("obj.k")?;
  assert_eq!(before_k, Value::Undefined);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Rejected);
  let reason = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("rejected promise should have a reason");
  assert_eq!(value_to_utf8(&rt, reason), "boom");

  let log = rt.exec_script("log.join(',')")?;
  assert_eq!(value_to_utf8(&rt, log), "key,rhs");

  let after_k = rt.exec_script("obj.k")?;
  assert_eq!(after_k, Value::Undefined);

  rt.heap_mut().remove_root(result_root);
  Ok(())
}

#[test]
fn compiled_script_top_level_await_computed_member_assignment_null_base_throws_before_await_rhs() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      var log = [];
      null[(log.push("key"), "k")] = await Promise.resolve((log.push("rhs"), "v"));
      log.push("after");
      log.join(",")
    "#,
  )?;
  assert!(script.contains_top_level_await);
  assert!(
    !script.top_level_await_requires_ast_fallback,
    "top-level await in a computed member assignment should be supported by the HIR async classic-script executor"
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

  // `RequireObjectCoercible(null)` throws *after* evaluating the computed key (per spec), but
  // before evaluating the RHS/await operand.
  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Rejected);
  let reason = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("rejected promise should have a reason");
  assert_eq!(error_name(&mut rt, reason)?, "TypeError");

  let log = rt.exec_script("log.join(',')")?;
  assert_eq!(value_to_utf8(&rt, log), "key");

  rt.heap_mut().remove_root(result_root);
  Ok(())
}

#[test]
fn compiled_script_top_level_await_computed_member_assignment_key_to_property_key_throw_happens_before_await_rhs(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      var log = [];
      var obj = {};
      var key = { toString() { log.push("toString"); throw "boom"; } };
      obj[key] = await Promise.resolve((log.push("rhs"), "v"));
      log.push("after");
      log.join(",")
    "#,
  )?;
  assert!(script.contains_top_level_await);
  assert!(
    !script.top_level_await_requires_ast_fallback,
    "top-level await in a computed member assignment should be supported by the HIR async classic-script executor"
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

  // Key conversion (`ToPropertyKey`) throws before the RHS/await operand is evaluated, so the async
  // classic-script should reject synchronously without suspension.
  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Rejected);
  let reason = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("rejected promise should have a reason");
  assert_eq!(value_to_utf8(&rt, reason), "boom");

  let log = rt.exec_script("log.join(',')")?;
  assert_eq!(value_to_utf8(&rt, log), "toString");

  let keys = rt.exec_script("Object.keys(obj).join(',')")?;
  assert_eq!(value_to_utf8(&rt, keys), "");

  rt.heap_mut().remove_root(result_root);
  Ok(())
}

#[test]
fn compiled_script_top_level_await_unresolvable_binding_assignment_strict_mode_throws_after_await() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      "use strict";
      var log = [];
      Promise.resolve().then(() => { globalThis.x = 123; log.push("mt"); });
      x = await Promise.resolve((log.push("rhs"), 1));
      log.push("after");
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

  // The await argument should have been evaluated, but microtasks should not have executed yet.
  let log = rt.exec_script("log.join(',')")?;
  assert_eq!(value_to_utf8(&rt, log), "rhs");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  // `x` was unresolvable in strict mode at reference-evaluation time, so the assignment must throw
  // a ReferenceError even if a global property named `x` is created while suspended.
  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Rejected);
  let reason = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("rejected promise should have a reason");
  assert_eq!(error_name(&mut rt, reason)?, "ReferenceError");

  // The pre-scheduled microtask runs before await resumption; ensure it ran and that the await
  // assignment did not overwrite the global property.
  let x = rt.exec_script("globalThis.x")?;
  assert_eq!(value_to_number(x), 123.0);

  let log = rt.exec_script("log.join(',')")?;
  assert_eq!(value_to_utf8(&rt, log), "rhs,mt");

  rt.heap_mut().remove_root(result_root);
  Ok(())
}

#[test]
fn compiled_script_top_level_await_global_property_binding_assignment_strict_mode_does_not_become_unresolvable_after_await(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      "use strict";
      Object.defineProperty(globalThis, "x", { value: 0, writable: true, configurable: true });
      var log = [];
      Promise.resolve().then(() => { delete globalThis.x; log.push("mt1"); });
      Promise.resolve().then(() => {
        log.push(Object.prototype.hasOwnProperty.call(globalThis, "x") ? "still" : "deleted");
        log.push("mt2");
      });
      x = await Promise.resolve((log.push("rhs"), 1));
      log.push("after");
      globalThis.x
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

  // The await argument should have been evaluated, but microtasks should not have executed yet.
  let log = rt.exec_script("log.join(',')")?;
  assert_eq!(value_to_utf8(&rt, log), "rhs");
  let x = rt.exec_script("globalThis.x")?;
  assert_eq!(value_to_number(x), 0.0);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  // `x` resolved to a global property binding at reference-evaluation time, so the assignment must
  // still succeed even if the property is deleted while suspended.
  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);
  let promise_result = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("fulfilled promise should have a result");
  assert_eq!(value_to_number(promise_result), 1.0);

  let log = rt.exec_script("log.join(',')")?;
  assert_eq!(value_to_utf8(&rt, log), "rhs,mt1,deleted,mt2,after");

  let x = rt.exec_script("globalThis.x")?;
  assert_eq!(value_to_number(x), 1.0);

  rt.heap_mut().remove_root(result_root);
  Ok(())
}

#[test]
fn compiled_script_top_level_await_global_property_binding_compound_assignment_strict_mode_succeeds_even_if_deleted_while_suspended(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      "use strict";
      Object.defineProperty(globalThis, "x", { value: 1, writable: true, configurable: true });
      var log = [];
      Promise.resolve().then(() => { delete globalThis.x; log.push("mt"); });
      x += await Promise.resolve((log.push("rhs"), 2));
      log.push("after");
      globalThis.x
    "#,
  )?;
  assert!(script.contains_top_level_await);
  assert!(
    !script.top_level_await_requires_ast_fallback,
    "top-level await compound assignment should be supported by the HIR async classic-script executor"
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

  // The await argument should have been evaluated, but microtasks should not have executed yet.
  let log = rt.exec_script("log.join(',')")?;
  assert_eq!(value_to_utf8(&rt, log), "rhs");
  let x = rt.exec_script("globalThis.x")?;
  assert_eq!(value_to_number(x), 1.0);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);
  let promise_result = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("fulfilled promise should have a result");

  // The global property is deleted by a microtask before resumption, but compound assignment should
  // still succeed using the original LHS value (1) and recreate the property.
  assert_eq!(value_to_number(promise_result), 3.0);

  let log = rt.exec_script("log.join(',')")?;
  assert_eq!(value_to_utf8(&rt, log), "rhs,mt,after");
  let x = rt.exec_script("globalThis.x")?;
  assert_eq!(value_to_number(x), 3.0);

  rt.heap_mut().remove_root(result_root);
  Ok(())
}

#[test]
fn compiled_script_top_level_await_unresolvable_binding_assignment_sloppy_mode_puts_value_after_await() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      var log = [];
      Promise.resolve().then(() => { globalThis.x = 123; log.push("mt"); });
      x = await Promise.resolve((log.push("rhs"), 1));
      log.push("after");
      globalThis.x
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

  // The await argument should have been evaluated, but microtasks should not have executed yet.
  let log = rt.exec_script("log.join(',')")?;
  assert_eq!(value_to_utf8(&rt, log), "rhs");
  let x = rt.exec_script("globalThis.x")?;
  assert_eq!(x, Value::Undefined);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);
  let promise_result = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("fulfilled promise should have a result");

  // Sloppy-mode unresolvable binding assignment performs `Set(globalThis, name, value, false)` at
  // PutValue time, so it overwrites any global property created while the script was suspended.
  assert_eq!(value_to_number(promise_result), 1.0);

  let log = rt.exec_script("log.join(',')")?;
  assert_eq!(value_to_utf8(&rt, log), "rhs,mt,after");

  rt.heap_mut().remove_root(result_root);
  Ok(())
}

#[test]
fn compiled_script_top_level_await_assignment_to_const_binding_throws_after_await() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      const x = 0;
      var log = [];
      Promise.resolve().then(() => { log.push("mt"); });
      x = await Promise.resolve((log.push("rhs"), 1));
      log.push("after");
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

  let log = rt.exec_script("log.join(',')")?;
  assert_eq!(value_to_utf8(&rt, log), "rhs");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Rejected);
  let reason = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("rejected promise should have a reason");
  assert_eq!(error_name(&mut rt, reason)?, "TypeError");

  // The pre-scheduled microtask runs before await resumption; ensure it ran and that the statement
  // after the failing assignment did not.
  let log = rt.exec_script("log.join(',')")?;
  assert_eq!(value_to_utf8(&rt, log), "rhs,mt");

  let x = rt.exec_script("x")?;
  assert_eq!(value_to_number(x), 0.0);

  rt.heap_mut().remove_root(result_root);
  Ok(())
}

#[test]
fn compiled_script_top_level_await_assignment_to_tdz_lexical_binding_throws_after_await() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      var log = [];
      Promise.resolve().then(() => { log.push("mt"); });
      x = await Promise.resolve((log.push("rhs"), 1));
      log.push("after");
      let x;
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

  // Await argument evaluation happens before suspension.
  let log = rt.exec_script("log.join(',')")?;
  assert_eq!(value_to_utf8(&rt, log), "rhs");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Rejected);
  let reason = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("rejected promise should have a reason");
  assert_eq!(error_name(&mut rt, reason)?, "ReferenceError");

  // The pre-scheduled microtask runs before await resumption; ensure it ran, and that the failing
  // PutValue did not create a global property.
  let log = rt.exec_script("log.join(',')")?;
  assert_eq!(value_to_utf8(&rt, log), "rhs,mt");
  let has_global = rt.exec_script("Object.prototype.hasOwnProperty.call(globalThis, 'x')")?;
  assert_eq!(has_global, Value::Bool(false));

  rt.heap_mut().remove_root(result_root);
  Ok(())
}

#[test]
fn compiled_script_top_level_await_unresolvable_binding_compound_assignment_throws_before_await() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      "use strict";
      var log = [];
      Promise.resolve().then(() => { log.push("mt"); });
      x += await Promise.resolve((log.push("rhs"), 1));
      log.push("after");
      log.join(",")
    "#,
  )?;
  assert!(script.contains_top_level_await);
  assert!(
    !script.top_level_await_requires_ast_fallback,
    "top-level await compound assignment should be supported by the HIR async classic-script executor"
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

  // Spec: compound assignment reads the LHS value before evaluating the RHS. For an unresolvable
  // binding, `GetValue` throws ReferenceError, so the await expression must not be evaluated and
  // the async script should reject synchronously (no suspension).
  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Rejected);
  let reason = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("rejected promise should have a reason");
  assert_eq!(error_name(&mut rt, reason)?, "ReferenceError");

  // RHS should not have run yet (no `rhs` log entry), and microtasks should not have executed yet.
  let log = rt.exec_script("log.join(',')")?;
  assert_eq!(value_to_utf8(&rt, log), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  // The earlier scheduled microtask should still run.
  let log = rt.exec_script("log.join(',')")?;
  assert_eq!(value_to_utf8(&rt, log), "mt");

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

#[test]
fn compiled_script_top_level_await_in_computed_member_compound_assignment_reads_lhs_before_await() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      var log = [];
      var store = 1;
      var obj = {};
      Object.defineProperty(obj, "k", {
        get() { log.push("get"); return store; },
        set(v) { log.push("set:" + v); store = v; }
      });
      Promise.resolve().then(() => { obj.k = 100; });
      obj[(log.push("key"), "k")] += await Promise.resolve((log.push("rhs"), 2));
      log.push("after");
      store
    "#,
  )?;
  assert!(script.contains_top_level_await);
  assert!(
    !script.top_level_await_requires_ast_fallback,
    "top-level await in a computed member compound assignment should be supported by the HIR async classic-script executor"
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

  // The compound assignment should not have completed yet, but the LHS reference (including the
  // computed key), `GetValue` on the LHS, and the await argument should have evaluated in order.
  let log = rt.exec_script("log.join(',')")?;
  assert_eq!(value_to_utf8(&rt, log), "key,get,rhs");
  let before = rt.exec_script("store")?;
  assert_eq!(value_to_number(before), 1.0);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  // The microtask that mutates `obj.k` runs before await resumption, but `obj[key] += await ...`
  // must still use the original LHS value (1) rather than the updated value (100).
  let after = rt.exec_script("store")?;
  assert_eq!(value_to_number(after), 3.0);
  let log = rt.exec_script("log.join(',')")?;
  assert_eq!(
    value_to_utf8(&rt, log),
    "key,get,rhs,set:100,set:3,after"
  );

  rt.heap_mut().remove_root(result_root);
  Ok(())
}

#[test]
fn compiled_script_top_level_await_in_computed_member_compound_assignment_to_property_key_is_not_recomputed_after_await(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      var log = [];
      var store = 1;
      var obj = {};
      Object.defineProperty(obj, "k", {
        get() { log.push("get"); return store; },
        set(v) { log.push("set:" + v); store = v; }
      });
      var key = { toString() { log.push("toString"); return "k"; } };
      Promise.resolve().then(() => {
        key.toString = function() { log.push("toString2"); return "x"; };
        log.push("mt");
      });
      obj[key] += await Promise.resolve((log.push("rhs"), 2));
      log.push("after");
      log.join(",")
    "#,
  )?;
  assert!(script.contains_top_level_await);
  assert!(
    !script.top_level_await_requires_ast_fallback,
    "top-level await in a computed member compound assignment should be supported by the HIR async classic-script executor"
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

  // `ToPropertyKey(key)` must happen before evaluating the await operand and must not be repeated
  // after resumption.
  let log = rt.exec_script("log.join(',')")?;
  assert_eq!(value_to_utf8(&rt, log), "toString,get,rhs");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);
  let promise_result = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("fulfilled promise should have a result");
  assert_eq!(
    value_to_utf8(&rt, promise_result),
    "toString,get,rhs,mt,set:3,after"
  );

  let store = rt.exec_script("store")?;
  assert_eq!(value_to_number(store), 3.0);
  let v = rt.exec_script("obj.k")?;
  assert_eq!(value_to_number(v), 3.0);

  rt.heap_mut().remove_root(result_root);
  Ok(())
}
