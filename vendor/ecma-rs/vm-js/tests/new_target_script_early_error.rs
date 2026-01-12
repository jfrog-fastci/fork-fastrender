use vm_js::{Heap, HeapLimits, JsRuntime, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
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
