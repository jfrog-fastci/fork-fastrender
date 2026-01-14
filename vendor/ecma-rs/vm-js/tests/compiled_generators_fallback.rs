use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, MicrotaskQueue, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn compiled_script_supports_generators_via_call_time_ast_evaluation() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "compiled_generators_fallback.js",
    r#"
      var result = 0;

      function* g() {
        yield 1;
      }

      result = g().next().value;
    "#,
  )?;

  assert!(
    !script.requires_ast_fallback,
    "scripts that define/call generator functions should be able to execute via HIR (generator bodies execute via call-time AST evaluation)"
  );

  rt.exec_compiled_script(script)?;

  let value = rt.exec_script("result")?;
  assert_eq!(value, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_script_with_host_and_hooks_supports_generators_via_call_time_ast_evaluation(
) -> Result<(), VmError> {
  // Regression test for `exec_compiled_script_with_host_and_hooks`: generator bodies execute via
  // call-time AST evaluation, but the surrounding script can remain on the compiled (HIR) path.
  //
  // Ensure Promise jobs are enqueued via the provided `hooks` implementation.
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "compiled_generators_fallback_with_hooks.js",
    r#"
      var result = 0;

      function* g() {
        yield 1;
      }

      Promise.resolve(2).then(v => { result = g().next().value + v; });
    "#,
  )?;

  assert!(
    !script.requires_ast_fallback,
    "scripts that define/call generator functions should be able to execute via HIR (generator bodies execute via call-time AST evaluation)"
  );

  let mut host = ();
  let mut hooks = MicrotaskQueue::new();

  rt.exec_compiled_script_with_host_and_hooks(&mut host, &mut hooks, script)?;
  assert!(
    !hooks.is_empty(),
    "expected Promise jobs to be enqueued via the provided host hook implementation"
  );

  let errors = hooks.perform_microtask_checkpoint(&mut rt);
  if let Some(err) = errors.into_iter().next() {
    return Err(err);
  }

  let value = rt.exec_script("result")?;
  assert_eq!(value, Value::Number(3.0));
  Ok(())
}
