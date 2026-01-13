use crate::function::CallHandler;
use crate::{CompiledScript, GcObject, Heap, HeapLimits, JsRuntime, PromiseState, Value, Vm, VmError, VmOptions};

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

fn assert_compiled_hir_async_function(rt: &JsRuntime, func_obj: GcObject) -> Result<(), VmError> {
  let call_handler = rt.heap.get_function_call_handler(func_obj)?;
  assert!(
    matches!(call_handler, CallHandler::User(_)),
    "expected async function to be allocated as a compiled user function, got {call_handler:?}"
  );
  // Async function bodies may still execute via the AST interpreter at call-time
  // (`FunctionData::AsyncEcmaFallback`). Once compiled async/await execution is enabled, this test will
  // automatically exercise the compiled async evaluator instead.
  let _func_data = rt.heap.get_function_data(func_obj)?;
  Ok(())
}

fn call_and_await_promise(rt: &mut JsRuntime, func_obj: GcObject) -> Result<Value, VmError> {
  let promise = {
    let mut scope = rt.heap.scope();
    rt.vm
      .call_without_host(&mut scope, Value::Object(func_obj), Value::Undefined, &[])?
  };
  let promise_root = rt.heap.add_root(promise)?;

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let Some(Value::Object(promise_obj)) = rt.heap.get_root(promise_root) else {
    panic!("expected async function call to return a Promise object");
  };
  let state = rt.heap.promise_state(promise_obj)?;
  assert_eq!(
    state,
    PromiseState::Fulfilled,
    "expected Promise to be fulfilled, got {state:?} with result {:?}",
    rt.heap.promise_result(promise_obj)?
  );
  let result = rt
    .heap
    .promise_result(promise_obj)?
    .expect("fulfilled promise missing result");

  rt.heap.remove_root(promise_root);
  Ok(result)
}

#[test]
fn hir_async_class_static_block_await_is_syntax_error() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let err = CompiledScript::compile_script(
    &mut rt.heap,
    "<inline>",
    r#"
      class C {
        static {
          await 0;
        }
      }
    "#,
  )
  .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
  Ok(())
}

#[test]
fn hir_async_class_extends_expression_can_await() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let func_obj = compile_and_get_function(
    &mut rt,
    r#"
      async function f(){
        class B {}
        class C extends (await Promise.resolve(B)) {}
        return new C() instanceof B;
      }
      f
    "#,
  )?;
  assert_compiled_hir_async_function(&rt, func_obj)?;

  let result = call_and_await_promise(&mut rt, func_obj)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn hir_async_class_computed_method_name_can_await() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let func_obj = compile_and_get_function(
    &mut rt,
    r#"
      async function f(){
        class C { [await Promise.resolve('m')](){ return 1; } }
        return new C().m();
      }
      f
    "#,
  )?;
  assert_compiled_hir_async_function(&rt, func_obj)?;

  let result = call_and_await_promise(&mut rt, func_obj)?;
  assert_eq!(result, Value::Number(1.0));
  Ok(())
}
