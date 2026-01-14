use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

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
