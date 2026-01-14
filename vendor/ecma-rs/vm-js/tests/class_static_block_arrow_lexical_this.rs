use vm_js::{
  CallHandler, CompiledScript, FunctionData, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions,
};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Promise + microtask machinery needs a bit of heap headroom.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn assert_compiled_async_fn(rt: &JsRuntime, value: Value, name: &str) -> Result<(), VmError> {
  let Value::Object(func_obj) = value else {
    panic!("expected {name} to evaluate to a function object, got {value:?}");
  };
  let call_handler = rt.heap.get_function_call_handler(func_obj)?;
  let CallHandler::User(func_ref) = call_handler else {
    panic!("expected {name} to use the compiled (HIR) call handler; got {call_handler:?}");
  };
  assert!(
    func_ref.ast_fallback.is_none(),
    "expected {name} to have no call-time AST fallback, got ast_fallback={:?}",
    func_ref.ast_fallback
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
fn arrow_this_in_class_static_block_uses_class_constructor_ast() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      class C {
        static {
          this.f = () => this;
        }
      }
      C.f() === C
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn arrow_this_in_class_static_block_uses_class_constructor_hir() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      class C {
        static {
          this.f = () => this;
        }
      }
      C.f() === C
    "#,
  )?;
  assert!(
    !script.requires_ast_fallback,
    "test script should execute via compiled (HIR) script executor"
  );

  let value = rt.exec_compiled_script(script)?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn arrow_this_in_async_class_static_block_uses_class_constructor_ast() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var ok = false;
      async function f() {
        class C {
          static {
            await Promise.resolve(0);
            this.f = () => this;
          }
        }
        return C.f() === C;
      }
      f().then(v => ok = v);
    "#,
  )?;

  assert_eq!(rt.exec_script("ok")?, Value::Bool(false));
  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;
  assert_eq!(rt.exec_script("ok")?, Value::Bool(true));
  Ok(())
}

#[test]
fn arrow_this_in_async_class_static_block_uses_class_constructor_hir() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      var ok = false;
      async function f() {
        class B {}
        class C extends (await Promise.resolve(B)) {
          static {
            this.f = () => this;
          }
        }
        return C.f() === C;
      }
      f().then(v => ok = v);
    "#,
  )?;
  assert!(
    !script.requires_ast_fallback,
    "test script should execute via compiled (HIR) script executor"
  );

  rt.exec_compiled_script(script)?;

  // Prove the async function body executes via the compiled async evaluator (no AST fallback tag).
  let f = rt.exec_script("f")?;
  assert_compiled_async_fn(&rt, f, "f")?;

  assert_eq!(rt.exec_script("ok")?, Value::Bool(false));
  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;
  assert_eq!(rt.exec_script("ok")?, Value::Bool(true));
  Ok(())
}
