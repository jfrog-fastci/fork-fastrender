use vm_js::{
  CallHandler, CompiledScript, FunctionData, Heap, HeapLimits, JsRuntime, MicrotaskQueue, Value, Vm,
  VmError, VmOptions,
};

mod _async_generator_support;

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Async generator scripts allocate Promise/job machinery; use a slightly larger heap than the
  // minimal 1MiB used by some unit tests to avoid spurious OOMs as builtin surface area grows.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn compiled_script_executes_async_generators_via_hir() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !_async_generator_support::supports_async_generators(&mut rt)? {
    return Ok(());
  }

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "compiled_async_generators_fallback.js",
    r#"
      var result = 0;
      // Keep a reference to the function so Rust can inspect it.
      var f;

      async function* g() { yield 1; }
      f = g;

      g().next().then(r => { result = r.value; });
    "#,
  )?;

  assert!(
    !script.requires_ast_fallback,
    "scripts that define/call async generator functions should be able to execute via HIR (no full-script AST fallback)"
  );

  rt.exec_compiled_script(script)?;

  // The function object should be a compiled user function without per-function AST fallback.
  let func_value = rt.exec_script("f")?;
  let Value::Object(func_obj) = func_value else {
    panic!("expected function object in global `f`, got {func_value:?}");
  };
  let call_handler = rt.heap.get_function_call_handler(func_obj)?;
  let CallHandler::User(func_ref) = call_handler else {
    panic!("expected async generator function to be allocated as a compiled user function, got {call_handler:?}");
  };
  assert!(
    func_ref.ast_fallback.is_none(),
    "expected async generator function to have no call-time AST fallback metadata, got ast_fallback={:?}",
    func_ref.ast_fallback
  );
  let func_data = rt.heap.get_function_data(func_obj)?;
  assert!(
    matches!(func_data, FunctionData::None),
    "expected async generator function to execute via HIR (no per-function AST fallback), got {func_data:?}"
  );

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("result")?;
  assert_eq!(value, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_script_async_generator_supports_return_and_throw() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !_async_generator_support::supports_async_generators(&mut rt)? {
    return Ok(());
  }

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "compiled_async_generators_return_throw.js",
    r#"
      var v1 = 0;
      var threw = false;
      var vret = 0;
      var done_ret = false;
      var f;

      async function* g() { yield 1; yield 2; }
      f = g;

      var it1 = g();
      it1.next()
        .then(r => { v1 = r.value; return it1.throw("boom"); })
        .catch(e => { threw = (e === "boom"); });

      var it2 = g();
      it2.next()
        .then(_ => it2.return(42))
        .then(r => { vret = r.value; done_ret = r.done; });
    "#,
  )?;

  rt.exec_compiled_script(script)?;

  // The function object should be a compiled user function without per-function AST fallback.
  let func_value = rt.exec_script("f")?;
  let Value::Object(func_obj) = func_value else {
    panic!("expected function object in global `f`, got {func_value:?}");
  };
  let call_handler = rt.heap.get_function_call_handler(func_obj)?;
  let CallHandler::User(func_ref) = call_handler else {
    panic!("expected async generator function to be allocated as a compiled user function, got {call_handler:?}");
  };
  assert!(
    func_ref.ast_fallback.is_none(),
    "expected async generator function to have no call-time AST fallback metadata, got ast_fallback={:?}",
    func_ref.ast_fallback
  );
  let func_data = rt.heap.get_function_data(func_obj)?;
  assert!(
    matches!(func_data, FunctionData::None),
    "expected async generator function to execute via HIR (no per-function AST fallback), got {func_data:?}"
  );

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.exec_script("v1")?, Value::Number(1.0));
  assert_eq!(rt.exec_script("threw")?, Value::Bool(true));
  assert_eq!(rt.exec_script("vret")?, Value::Number(42.0));
  assert_eq!(rt.exec_script("done_ret")?, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_script_with_host_and_hooks_executes_async_generators_via_hir() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !_async_generator_support::supports_async_generators(&mut rt)? {
    return Ok(());
  }

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "compiled_async_generators_fallback_with_hooks.js",
    r#"
      var result = 0;
      var f;
      async function* g() { yield 1; }
      f = g;
      g().next().then(r => { result = r.value; });
    "#,
  )?;

  assert!(
    !script.requires_ast_fallback,
    "scripts that define/call async generator functions should be able to execute via HIR (no full-script AST fallback)"
  );

  let mut host = ();
  let mut hooks = MicrotaskQueue::new();

  rt.exec_compiled_script_with_host_and_hooks(&mut host, &mut hooks, script)?;

  // The function object should be a compiled user function without per-function AST fallback.
  let func_value = rt.exec_script("f")?;
  let Value::Object(func_obj) = func_value else {
    panic!("expected function object in global `f`, got {func_value:?}");
  };
  let call_handler = rt.heap.get_function_call_handler(func_obj)?;
  let CallHandler::User(func_ref) = call_handler else {
    panic!("expected async generator function to be allocated as a compiled user function, got {call_handler:?}");
  };
  assert!(
    func_ref.ast_fallback.is_none(),
    "expected async generator function to have no call-time AST fallback metadata, got ast_fallback={:?}",
    func_ref.ast_fallback
  );
  let func_data = rt.heap.get_function_data(func_obj)?;
  assert!(
    matches!(func_data, FunctionData::None),
    "expected async generator function to execute via HIR (no per-function AST fallback), got {func_data:?}"
  );

  let errors = hooks.perform_microtask_checkpoint(&mut rt);
  if let Some(err) = errors.into_iter().next() {
    return Err(err);
  }

  let value = rt.exec_script("result")?;
  assert_eq!(value, Value::Number(1.0));
  Ok(())
}
