use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn exec_compiled(rt: &mut JsRuntime, source: &str) -> Result<Value, VmError> {
  let script = CompiledScript::compile_script(rt.heap_mut(), "<eval_super_call_in_class_fields>", source)?;
  assert!(
    !script.requires_ast_fallback,
    "test script should execute via compiled (HIR) script executor"
  );
  rt.exec_compiled_script(script)
}

// Mirrors test262 `language/statements/class/elements/derived-cls-direct-eval-err-contains-supercall.js`.
const DIRECT_EVAL_SUPER_CALL_IN_FIELD_INIT: &str = r#"
  (() => {
    var executed = false;
    var out = "no error";

    class B {}
    class D extends B {
      x = eval("executed = true; super();");
    }

    try { new D(); } catch (e) { out = e.name; }
    return out === "SyntaxError" && executed === false;
  })()
"#;

// Mirrors test262 `language/statements/class/elements/arrow-body-derived-cls-direct-eval-err-contains-supercall.js`.
const DIRECT_EVAL_SUPER_CALL_IN_ARROW_FIELD_INIT: &str = r#"
  (() => {
    var executed = false;
    var out = "no error";

    class B {}
    class D extends B {
      f = () => eval("executed = true; super();");
    }

    try { (new D()).f(); } catch (e) { out = e.name; }
    return out === "SyntaxError" && executed === false;
  })()
"#;

#[test]
fn direct_eval_in_field_initializer_rejects_super_call_without_running_side_effects() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(DIRECT_EVAL_SUPER_CALL_IN_FIELD_INIT)?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn direct_eval_in_field_initializer_rejects_super_call_without_running_side_effects_compiled(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(&mut rt, DIRECT_EVAL_SUPER_CALL_IN_FIELD_INIT)?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn direct_eval_in_arrow_field_initializer_rejects_super_call_without_running_side_effects(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(DIRECT_EVAL_SUPER_CALL_IN_ARROW_FIELD_INIT)?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn direct_eval_in_arrow_field_initializer_rejects_super_call_without_running_side_effects_compiled(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(&mut rt, DIRECT_EVAL_SUPER_CALL_IN_ARROW_FIELD_INIT)?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

