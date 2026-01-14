use vm_js::{
  CompiledScript, Heap, HeapLimits, JsRuntime, MicrotaskQueue, SourceText, SourceTextModuleRecord,
  Value, Vm, VmError, VmOptions,
};

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

#[test]
fn compiled_module_catch_has_error_stack() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  // Store the captured stack on the global object so we can assert on it after module evaluation.
  // This avoids needing to plumb module namespace exports into the test.
  // Avoid `Arc::new`, which can abort the process on allocator OOM.
  let source = SourceText::new_charged_arc(
    rt.heap_mut(),
    "m.js",
    r#"
      globalThis.__stack = (function () {
        try {
          throw new Error("boom");
        } catch (e) {
          return e.stack;
        }
      })();

      export {};
    "#,
  )?;

  let record = SourceTextModuleRecord::compile_source(rt.heap_mut(), source)?;
  let module_id = rt.modules_mut().add_module(record)?;

  let global_object = rt.realm().global_object();
  let realm_id = rt.realm().id();

  // Evaluate the module via the module graph so it executes through the compiled (HIR) executor
  // when `compiled` is present.
  let mut host = ();
  let mut hooks = MicrotaskQueue::new();
  {
    let (vm, modules, heap) = rt.vm_modules_and_heap_mut();
    modules.evaluate_sync(vm, heap, global_object, realm_id, module_id, &mut host, &mut hooks)?;
  }

  let result = rt.exec_script("globalThis.__stack")?;
  let Value::String(stack_s) = result else {
    panic!("expected module to set stack string, got {result:?}");
  };
  let stack = rt.heap().get_string(stack_s)?.to_utf8_lossy();
  assert!(!stack.is_empty(), "expected non-empty stack string");
  assert!(
    stack.contains("boom"),
    "expected stack string to contain error message, got {stack:?}"
  );
  Ok(())
}
