use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, MicrotaskQueue, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Async function scripts allocate Promise/job machinery; use a slightly larger heap than the
  // minimal 1MiB used by some unit tests to avoid spurious OOMs as builtin surface area grows.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn compiled_script_with_host_and_hooks_falls_back_for_async_functions() -> Result<(), VmError> {
  // Regression test for a previously-broken `exec_compiled_script_with_hooks` gate: it must fall
  // back to the AST interpreter for async function scripts (not just async generators).
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "compiled_async_functions_fallback_with_hooks.js",
    r#"
      var result = 0;

      async function f() {
        return 1;
      }

      f().then(v => { result = v; });
    "#,
  )?;

  let mut host = ();
  let mut hooks = MicrotaskQueue::new();

  // This exercises `JsRuntime::exec_compiled_script_with_hooks` (via the public alias) and ensures
  // it uses `script.requires_ast_fallback` rather than only checking `contains_async_generators`.
  rt.exec_compiled_script_with_host_and_hooks(&mut host, &mut hooks, script)?;

  // Promise jobs were enqueued via `hooks`; run a microtask checkpoint so the `.then` callback runs.
  let errors = hooks.perform_microtask_checkpoint(&mut rt);
  if let Some(err) = errors.into_iter().next() {
    return Err(err);
  }

  let value = rt.exec_script("result")?;
  assert_eq!(value, Value::Number(1.0));
  Ok(())
}

