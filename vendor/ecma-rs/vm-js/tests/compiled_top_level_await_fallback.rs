use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Top-level await execution allocates Promise/job machinery; use a slightly larger heap than
  // the minimal 1MiB used by some unit tests to avoid spurious OOMs as builtin surface area grows.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn compiled_script_falls_back_for_top_level_await() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "compiled_top_level_await_fallback.js",
    r#"
      var out = "";
      out = await Promise.resolve("ok");
      out
    "#,
  )?;

  rt.exec_compiled_script(script)?;

  // The assignment after `await` should not have executed yet.
  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "ok");
  Ok(())
}

#[test]
fn compiled_script_falls_back_for_top_level_for_await_of() -> Result<(), VmError> {
  let mut rt = new_runtime();

  // Use a manual async iterator (no `async function` / generators) so this tests the top-level
  // await fallback rather than the async-function AST fallback.
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "compiled_top_level_for_await_of_fallback.js",
    r#"
      var out = "";
      var iter = {
        i: 0,
        next: function () {
          if (this.i++ === 0) return Promise.resolve({ value: "ok", done: false });
          return Promise.resolve({ value: undefined, done: true });
        },
      };
      var iterable = {};
      iterable[Symbol.asyncIterator] = function () { return iter; };

      for await (var x of iterable) {
        out = x;
      }
      out
    "#,
  )?;

  rt.exec_compiled_script(script)?;

  // Loop body should not have executed until we run microtasks.
  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "ok");
  Ok(())
}
