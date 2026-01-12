use vm_js::{Heap, HeapLimits, JsRuntime, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn duplicate_labels_are_syntax_error_in_scripts() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script(
      r#"
        label: {
          label: 0;
        }
      "#,
    )
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}
