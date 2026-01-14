use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, MicrotaskQueue, Value, Vm, VmError, VmOptions};

mod _async_generator_support;

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Async generator scripts allocate Promise/job machinery; use a slightly larger heap than the
  // minimal 1MiB used by some unit tests to avoid spurious OOMs as builtin surface area grows.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn compiled_script_falls_back_for_async_generators() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !_async_generator_support::supports_async_generators(&mut rt)? {
    return Ok(());
  }

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "compiled_async_generators_fallback.js",
    r#"
      var result = 0;

      async function* g() {
        yield 1;
      }

      g().next().then(r => { result = r.value; });
    "#,
  )?;

  rt.exec_compiled_script(script)?;
  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("result")?;
  assert_eq!(value, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_script_with_host_and_hooks_falls_back_for_async_generators() -> Result<(), VmError> {
  // Regression test for `exec_compiled_script_with_host_and_hooks`: async generator scripts are
  // not yet supported by the compiled (HIR) executor and must fall back to the AST interpreter.
  let mut rt = new_runtime();
  if !_async_generator_support::supports_async_generators(&mut rt)? {
    return Ok(());
  }

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "compiled_async_generators_fallback_with_hooks.js",
    r#"
      var result = 0;

      async function* g() {
        yield 1;
      }

      g().next().then(r => { result = r.value; });
    "#,
  )?;

  assert!(
    !script.requires_ast_fallback,
    "scripts that define/call async generator functions should be able to execute via HIR (generator bodies execute via call-time AST evaluation)"
  );

  let mut host = ();
  let mut hooks = MicrotaskQueue::new();

  rt.exec_compiled_script_with_host_and_hooks(&mut host, &mut hooks, script)?;
  let errors = hooks.perform_microtask_checkpoint(&mut rt);
  if let Some(err) = errors.into_iter().next() {
    return Err(err);
  }

  let value = rt.exec_script("result")?;
  assert_eq!(value, Value::Number(1.0));
  Ok(())
}
