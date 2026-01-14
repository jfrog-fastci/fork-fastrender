use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string value, got {value:?}");
  };
  rt.heap().get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn new_target_outside_function_is_syntax_error_in_scripts() {
  let mut rt = new_runtime();
  let err = rt.exec_script("new.target;").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn import_meta_is_syntax_error_in_scripts() {
  let mut rt = new_runtime();
  let err = rt.exec_script("import.meta;").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn import_meta_is_syntax_error_in_eval() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let result = rt.exec_script(r#"try { eval("import.meta"); "no"; } catch (e) { e.name }"#)?;
  assert_eq!(value_to_string(&rt, result), "SyntaxError");
  Ok(())
}

#[test]
fn import_meta_is_syntax_error_in_function_constructor() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let result = rt.exec_script(r#"try { new Function("return import.meta"); "no"; } catch (e) { e.name }"#)?;
  assert_eq!(value_to_string(&rt, result), "SyntaxError");
  Ok(())
}
