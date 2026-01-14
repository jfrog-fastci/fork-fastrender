use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Async function scripts allocate Promise/job machinery; use a slightly larger heap than the
  // minimal 1MiB used by some unit tests to avoid spurious OOMs as builtin surface area grows.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn compiled_script_does_not_fall_back_for_async_function_decls() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "compiled_async_functions_fallback.js",
    r#"
      var result = 0;

      async function f() {
        return 1;
      }

      f().then(v => { result = v; });
    "#,
  )?;

  assert!(
    script.contains_async_functions,
    "test script should contain at least one async function"
  );
  assert!(
    !script.requires_ast_fallback,
    "scripts that only *define* async functions should be able to execute via the compiled (HIR) executor"
  );

  rt.exec_compiled_script(script)?;
  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("result")?;
  assert_eq!(value, Value::Number(1.0));
  Ok(())
}
