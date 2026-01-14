use vm_js::{
  CallHandler, CompiledScript, FunctionData, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions,
};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Async function scripts allocate Promise/job machinery; use a slightly larger heap than the
  // minimal 1MiB used by some unit tests to avoid spurious OOMs as builtin surface area grows.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn assert_compiled_async_fn(rt: &JsRuntime, value: Value, name: &str) -> Result<(), VmError> {
  let Value::Object(func_obj) = value else {
    panic!("expected {name} to evaluate to a function object, got {value:?}");
  };
  let call_handler = rt.heap.get_function_call_handler(func_obj)?;
  assert!(
    matches!(call_handler, CallHandler::User(_)),
    "expected {name} to use the compiled (HIR) call handler; got {call_handler:?}"
  );
  let data = rt.heap.get_function_data(func_obj)?;
  assert!(
    !matches!(
      data,
      FunctionData::EcmaFallback { .. } | FunctionData::AsyncEcmaFallback { .. }
    ),
    "expected {name} to execute via the compiled async evaluator (no AST fallback tag); got {data:?}"
  );
  Ok(())
}

#[test]
fn compiled_script_does_not_fall_back_for_async_function_defs() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "compiled_async_functions_fallback.js",
    r#"
      var result = 0;
      const thisObj = {};

      async function f() {
        return this === globalThis ? 1 : (this === thisObj ? 6 : -100);
      }

      const g = async function () {
        return this === globalThis ? 2 : (this === thisObj ? 7 : -100);
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
      // Ensure explicit receiver binding is preserved when async function bodies execute via
      // the compiled async evaluator.
      f.call(thisObj).then(v => { result += v; });
      g().then(v => { result += v; });
      g.call(thisObj).then(v => { result += v; });
      // Async arrow functions execute via the compiled async evaluator; ensure they still use
      // *lexical* `this` rather than the call-site receiver (even when invoked via `.call`).
      h.call({}).then(v => { result += v; });
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

  rt.exec_compiled_script(script)?;

  // Prove that async functions allocated during compiled script execution are backed by compiled
  // HIR (CallHandler::User) and do not opt into per-function AST fallback.
  let f = rt.exec_script("f")?;
  assert_compiled_async_fn(&rt, f, "f")?;
  let g = rt.exec_script("g")?;
  assert_compiled_async_fn(&rt, g, "g")?;
  let h = rt.exec_script("h")?;
  assert_compiled_async_fn(&rt, h, "h")?;

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("result")?;
  assert_eq!(value, Value::Number(28.0));
  Ok(())
}

#[test]
fn compiled_script_async_function_fallback_preserves_closure_env() -> Result<(), VmError> {
  // Async function bodies execute via call-time AST fallback; ensure that fallback still captures
  // and reads from the correct outer lexical environment.
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "compiled_async_functions_fallback_closure.js",
    r#"
      var result = 0;
      (function () {
        let x = 21;
        const f = async function () {
          await Promise.resolve(0);
          return x;
        };
        const g = async () => {
          await Promise.resolve(0);
          return x + 1;
        };
        f().then(v => { result += v; });
        g().then(v => { result += v; });
      })();
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
  assert_eq!(value, Value::Number(43.0));
  Ok(())
}
