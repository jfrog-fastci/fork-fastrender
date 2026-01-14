use crate::function::{CallHandler, FunctionData};
use crate::property::{PropertyKey, PropertyKind};
use crate::{
  CompiledScript, GcObject, Heap, HeapLimits, JsRuntime, PromiseState, Value, Vm, VmError, VmOptions,
};

fn new_runtime() -> Result<JsRuntime, VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap)
}

fn compile_and_get_function(rt: &mut JsRuntime, source: &str) -> Result<GcObject, VmError> {
  let script = CompiledScript::compile_script(&mut rt.heap, "<inline>", source)?;
  assert!(
    !script.requires_ast_fallback,
    "expected script to run via compiled HIR path (requires_ast_fallback=false)"
  );
  let value = rt.exec_compiled_script(script)?;
  let Value::Object(func_obj) = value else {
    panic!("expected script to evaluate to a function object, got {value:?}");
  };
  Ok(func_obj)
}

fn assert_compiled_hir_async_function(rt: &mut JsRuntime, func_obj: GcObject) -> Result<(), VmError> {
  let call_handler = rt.heap.get_function_call_handler(func_obj)?;
  let CallHandler::User(func_ref) = call_handler else {
    panic!("expected async function to be allocated as a compiled user function, got {call_handler:?}");
  };
  assert!(
    func_ref.ast_fallback.is_none(),
    "expected async function to have no call-time AST fallback, got ast_fallback={:?}",
    func_ref.ast_fallback
  );

  // Async functions may still be tagged for call-time AST fallback; clear that marker so this test
  // exercises the compiled async/await evaluator.
  //
  // When compiled async function execution is enabled by default, this becomes a no-op.
  match rt.heap.get_function_data(func_obj)? {
    FunctionData::EcmaFallback { .. } | FunctionData::AsyncEcmaFallback { .. } => {
      rt.heap.set_function_data(func_obj, FunctionData::None)?;
    }
    _ => {}
  }

  let func_data = rt.heap.get_function_data(func_obj)?;
  assert!(
    !matches!(
      func_data,
      FunctionData::EcmaFallback { .. } | FunctionData::AsyncEcmaFallback { .. }
    ),
    "expected async function to execute via the compiled/HIR async path after clearing fallback marker, but it was still tagged for AST fallback: {func_data:?}"
  );
  Ok(())
}

fn call_and_await_promise(
  rt: &mut JsRuntime,
  func_obj: GcObject,
) -> Result<(PromiseState, Value), VmError> {
  let promise = {
    let mut scope = rt.heap.scope();
    rt.vm
      .call_without_host(&mut scope, Value::Object(func_obj), Value::Undefined, &[])?
  };
  let promise_root = rt.heap.add_root(promise)?;

  let Some(Value::Object(promise_obj)) = rt.heap.get_root(promise_root) else {
    panic!("expected async function call to return a Promise object");
  };

  // Await should always suspend and resume via microtasks (even for already-settled Promises), so
  // the returned Promise must not have settled yet.
  let initial_state = rt.heap.promise_state(promise_obj)?;
  assert_eq!(
    initial_state,
    PromiseState::Pending,
    "expected Promise returned from async function call to be pending before microtask checkpoint, got {initial_state:?}"
  );

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let state = rt.heap.promise_state(promise_obj)?;
  assert!(
    matches!(state, PromiseState::Fulfilled | PromiseState::Rejected),
    "expected Promise to settle after microtask checkpoint, got {state:?} with result {:?}",
    rt.heap.promise_result(promise_obj)?
  );
  let result = rt.heap.promise_result(promise_obj)?.expect("settled promise missing result");

  rt.heap.remove_root(promise_root);
  Ok((state, result))
}

fn assert_value_utf8_string_eq(rt: &JsRuntime, value: Value, expected: &str) -> Result<(), VmError> {
  let Value::String(s) = value else {
    panic!("expected String result, got {value:?}");
  };
  let actual = rt.heap.get_string(s)?.to_utf8_lossy();
  assert_eq!(actual, expected);
  Ok(())
}

fn get_global_data_property(rt: &mut JsRuntime, name: &str) -> Result<Value, VmError> {
  let global = rt.realm().global_object();
  let mut scope = rt.heap.scope();
  let key_s = scope.alloc_string(name)?;
  let key = PropertyKey::from_string(key_s);
  let desc = scope
    .heap()
    .get_own_property(global, key)?
    .unwrap_or_else(|| panic!("expected global property {name}"));
  let PropertyKind::Data { value, .. } = desc.kind else {
    panic!("expected global property {name} to be a data property");
  };
  Ok(value)
}

#[test]
fn hir_async_await_expr_statement_suspends_and_resumes() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let func_obj = compile_and_get_function(
    &mut rt,
    r#"
      async function f(){
        let x = 0;
        await Promise.resolve(0);
        x = 1;
        return x;
      }
      f
    "#,
  )?;
  assert_compiled_hir_async_function(&mut rt, func_obj)?;

  let (state, result) = call_and_await_promise(&mut rt, func_obj)?;
  assert_eq!(state, PromiseState::Fulfilled);
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}

#[test]
fn hir_async_return_await_resolved() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let func_obj = compile_and_get_function(
    &mut rt,
    r#"
      async function f(){
        return await Promise.resolve(2);
      }
      f
    "#,
  )?;
  assert_compiled_hir_async_function(&mut rt, func_obj)?;

  let (state, result) = call_and_await_promise(&mut rt, func_obj)?;
  assert_eq!(state, PromiseState::Fulfilled);
  assert_eq!(result, Value::Number(2.0));
  Ok(())
}

#[test]
fn hir_async_return_await_rejected_is_catchable() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let func_obj = compile_and_get_function(
    &mut rt,
    r#"
      async function f(){
        return await Promise.reject('x');
      }
      f
    "#,
  )?;
  assert_compiled_hir_async_function(&mut rt, func_obj)?;

  let (state, result) = call_and_await_promise(&mut rt, func_obj)?;
  assert_eq!(state, PromiseState::Rejected);
  assert_value_utf8_string_eq(&rt, result, "x")?;
  Ok(())
}

#[test]
fn hir_async_throw_await_resolved_is_catchable() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let func_obj = compile_and_get_function(
    &mut rt,
    r#"
      async function f(){
        throw await Promise.resolve('y');
      }
      f
    "#,
  )?;
  assert_compiled_hir_async_function(&mut rt, func_obj)?;

  let (state, result) = call_and_await_promise(&mut rt, func_obj)?;
  assert_eq!(state, PromiseState::Rejected);
  assert_value_utf8_string_eq(&rt, result, "y")?;
  Ok(())
}

#[test]
fn hir_async_throw_await_rejected_is_catchable() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let func_obj = compile_and_get_function(
    &mut rt,
    r#"
      async function f(){
        throw await Promise.reject('z');
      }
      f
    "#,
  )?;
  assert_compiled_hir_async_function(&mut rt, func_obj)?;

  let (state, result) = call_and_await_promise(&mut rt, func_obj)?;
  assert_eq!(state, PromiseState::Rejected);
  assert_value_utf8_string_eq(&rt, result, "z")?;
  Ok(())
}

#[test]
fn hir_async_arrow_expr_body_direct_await() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let func_obj = compile_and_get_function(
    &mut rt,
    r#"
      let f = async () => await Promise.resolve(3);
      f
    "#,
  )?;
  assert_compiled_hir_async_function(&mut rt, func_obj)?;

  let (state, result) = call_and_await_promise(&mut rt, func_obj)?;
  assert_eq!(state, PromiseState::Fulfilled);
  assert_eq!(result, Value::Number(3.0));
  Ok(())
}

#[test]
fn hir_async_nested_await_rejection_stack_is_attributed_to_await_site() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let outer_obj = compile_and_get_function(
    &mut rt,
    r#"async function inner(){
  ({[await Promise.reject({})]: x} = {});
}
async function outer(){
  try { await inner(); } catch (e) { return e.stack; }
}
globalThis.__inner = inner;
outer"#,
  )?;

  // Force `inner` to execute via the compiled async evaluator. `outer` may remain on the AST fallback
  // path (it uses `try/catch` around `await`).
  let inner_value = get_global_data_property(&mut rt, "__inner")?;
  let Value::Object(inner_obj) = inner_value else {
    panic!("expected global __inner to be a function object, got {inner_value:?}");
  };
  assert_compiled_hir_async_function(&mut rt, inner_obj)?;

  let (state, result) = call_and_await_promise(&mut rt, outer_obj)?;
  assert_eq!(state, PromiseState::Fulfilled);
  let Value::String(stack_s) = result else {
    panic!("expected outer() to fulfill with a String stack, got {result:?}");
  };
  let stack = rt.heap.get_string(stack_s)?.to_utf8_lossy();
  assert!(!stack.is_empty(), "expected stack string to be non-empty");
  // The computed key statement line is 2. Best-effort column attribution depends on span fidelity
  // for nested expressions, but the top frame should still point at this statement (not internal
  // Promise machinery).
  assert!(
    stack.lines().next().is_some_and(|line| line.contains("<inline>:2:")),
    "expected stack top frame to point at await site on line 2 (<inline>:2:*), got:\n{stack}"
  );
  Ok(())
}
