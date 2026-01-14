use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, MicrotaskQueue, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Async function scripts allocate Promise/job machinery; use a slightly larger heap than the
  // minimal 1MiB used by some unit tests to avoid spurious OOMs as builtin surface area grows.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn compiled_script_with_host_and_hooks_does_not_fall_back_for_async_function_defs() -> Result<(), VmError> {
  // Regression test for `exec_compiled_script_with_host_and_hooks`: scripts which only *define*
  // async functions (no top-level await) should still execute via the compiled (HIR) executor.
  //
  // Async function bodies are executed later via the AST interpreter at call-time.
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "compiled_async_functions_fallback_with_hooks.js",
    r#"
      var result = 0;

      async function f() {
        return this === globalThis ? 1 : -100;
      }

      const g = async function () {
        return this === globalThis ? 2 : -100;
      };

      const h = async () => (this === globalThis ? 3 : -100);

      const obj = {
        async m() { return this === obj ? 4 : -100; }
      };

      class C {
        async m() { return this instanceof C ? 5 : -100; }
      }
      const inst = new C();

      f().then(v => { result += v; });
      g().then(v => { result += v; });
      h().then(v => { result += v; });
      obj.m().then(v => { result += v; });
      inst.m().then(v => { result += v; });
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

  let mut host = ();
  let mut hooks = MicrotaskQueue::new();

  // This exercises `JsRuntime::exec_compiled_script_with_host_and_hooks` and ensures Promise jobs
  // are enqueued via the provided `hooks`.
  rt.exec_compiled_script_with_host_and_hooks(&mut host, &mut hooks, script)?;

  // Promise jobs were enqueued via `hooks`; run a microtask checkpoint so the `.then` callback runs.
  let errors = hooks.perform_microtask_checkpoint(&mut rt);
  if let Some(err) = errors.into_iter().next() {
    return Err(err);
  }

  let value = rt.exec_script("result")?;
  assert_eq!(value, Value::Number(15.0));
  Ok(())
}
