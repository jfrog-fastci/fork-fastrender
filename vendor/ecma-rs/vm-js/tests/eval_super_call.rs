use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn exec_compiled(rt: &mut JsRuntime, source: &str) -> Result<Value, VmError> {
  let script = CompiledScript::compile_script(rt.heap_mut(), "<eval_super_call>", source)?;
  rt.exec_compiled_script(script)
}

// Mirrors test262 `language/eval-code/direct/super-call-fn.js`.
const SUPER_CALL_IN_PLAIN_FUNCTION: &str = r#"
  var executed = false;
  var caught = null;

  function f() {
    try {
      eval('executed = true; super();');
    } catch (e) {
      caught = e;
    }
  }

  f();

  caught && caught.name === 'SyntaxError' && executed === false;
"#;

// Mirrors test262 `language/eval-code/direct/super-call-method.js`.
const SUPER_CALL_IN_OBJECT_METHOD: &str = r#"
  var evaluatedArg = false;
  var caught = null;

  var obj = {
    method() {
      try {
        eval('super(evaluatedArg = true);');
      } catch (e) {
        caught = e;
      }
    }
  };

  obj.method();

  caught && caught.name === 'SyntaxError' && evaluatedArg === false;
"#;

#[test]
fn direct_eval_rejects_super_call_in_plain_function_without_running_side_effects() -> Result<(), VmError>
{
  let mut rt = new_runtime();
  let value = rt.exec_script(SUPER_CALL_IN_PLAIN_FUNCTION)?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn direct_eval_rejects_super_call_in_plain_function_without_running_side_effects_compiled(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(&mut rt, SUPER_CALL_IN_PLAIN_FUNCTION)?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn direct_eval_rejects_super_call_in_object_method_without_evaluating_arguments() -> Result<(), VmError>
{
  let mut rt = new_runtime();
  let value = rt.exec_script(SUPER_CALL_IN_OBJECT_METHOD)?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn direct_eval_rejects_super_call_in_object_method_without_evaluating_arguments_compiled(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(&mut rt, SUPER_CALL_IN_OBJECT_METHOD)?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

