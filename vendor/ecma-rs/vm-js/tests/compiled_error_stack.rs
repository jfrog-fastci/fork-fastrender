use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

#[test]
fn compiled_script_catch_has_error_stack() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      try {
        throw new Error("boom");
      } catch (e) {
        e.stack;
      }
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;

  let Value::String(stack_s) = result else {
    panic!("expected script to return stack string, got {result:?}");
  };
  let stack = rt.heap().get_string(stack_s)?.to_utf8_lossy();
  assert!(!stack.is_empty(), "expected non-empty stack string");
  assert!(
    stack.contains("boom"),
    "expected stack string to contain error message, got {stack:?}"
  );
  Ok(())
}

