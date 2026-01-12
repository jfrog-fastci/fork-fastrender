use vm_js::{Heap, HeapLimits, JsRuntime, SourceTextModuleRecord, Vm, VmError, VmOptions};

#[test]
fn heap_limits_account_for_source_text_when_executing_scripts() {
  // `SourceText` is stored outside the GC heap. Ensure `SourceText::new_charged` enforces
  // `HeapLimits::max_bytes` so hostile scripts cannot bypass heap accounting via huge sources.
  let max_bytes = 1024 * 1024; // 1 MiB
  let heap = Heap::new(HeapLimits::new(max_bytes, max_bytes));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap).expect("JsRuntime::new");

  let src = ";".repeat(max_bytes * 2);
  let err = rt.exec_script(&src).unwrap_err();
  assert!(matches!(err, VmError::OutOfMemory));
}

#[test]
fn heap_limits_account_for_source_text_when_parsing_modules() {
  // Module record parsing should also charge `SourceText` against heap limits.
  let max_bytes = 1024 * 1024; // 1 MiB
  let mut heap = Heap::new(HeapLimits::new(max_bytes, max_bytes));

  let src = ";".repeat(max_bytes * 2);
  let err = SourceTextModuleRecord::parse(&mut heap, &src).unwrap_err();
  assert!(matches!(err, VmError::OutOfMemory));
}

