use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn assert_value_is_bool(value: Value, expected: bool) {
  let Value::Bool(actual) = value else {
    panic!("expected bool, got {value:?}");
  };
  assert_eq!(actual, expected);
}

fn exec_compiled(rt: &mut JsRuntime, source: &str) -> Result<Value, VmError> {
  let script = CompiledScript::compile_script(rt.heap_mut(), "<eval_in_class_field_init_args>", source)?;
  assert!(
    !script.requires_ast_fallback,
    "test script should execute via compiled (HIR) script executor"
  );
  rt.exec_compiled_script(script)
}

#[test]
fn direct_eval_in_public_field_initializer_rejects_arguments() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var executed = false;
      class C {
        x = eval('executed = true; arguments;');
      }
      var err;
      try { new C(); } catch (e) { err = e; }
      err instanceof SyntaxError && executed === false;
    "#,
  )?;
  assert_value_is_bool(value, true);
  Ok(())
}

#[test]
fn direct_eval_in_public_field_initializer_rejects_arguments_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      var executed = false;
      class C {
        x = eval('executed = true; arguments;');
      }
      var err;
      try { new C(); } catch (e) { err = e; }
      err instanceof SyntaxError && executed === false;
    "#,
  )?;
  assert_value_is_bool(value, true);
  Ok(())
}

#[test]
fn direct_eval_in_arrow_in_public_field_initializer_rejects_arguments() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var executed = false;
      class C {
        x = () => eval('executed = true; arguments;');
      }
      var err;
      try { new C().x(); } catch (e) { err = e; }
      err instanceof SyntaxError && executed === false;
    "#,
  )?;
  assert_value_is_bool(value, true);
  Ok(())
}

#[test]
fn direct_eval_in_public_field_initializer_rejects_arguments_in_arrow_body() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var executed = false;
      class C {
        x = eval('executed = true; () => arguments;');
      }
      var err;
      try { new C().x(); } catch (e) { err = e; }
      err instanceof SyntaxError && executed === false;
    "#,
  )?;
  assert_value_is_bool(value, true);
  Ok(())
}

#[test]
fn indirect_eval_in_public_field_initializer_allows_arguments() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var arguments = 123;
      class C {
        x = (0, eval)('arguments;');
      }
      new C().x === arguments;
    "#,
  )?;
  assert_value_is_bool(value, true);
  Ok(())
}

#[test]
fn indirect_eval_in_public_field_initializer_allows_arguments_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      var arguments = 123;
      class C {
        x = (0, eval)('arguments;');
      }
      new C().x === arguments;
    "#,
  )?;
  assert_value_is_bool(value, true);
  Ok(())
}

