use vm_js::{Heap, HeapLimits, JsRuntime, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn duplicate_private_field_and_method_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script("class C { #x; #x(){} }").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn duplicate_private_field_and_field_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script("class C { #x; #x; }").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn bare_private_identifier_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script("class C { #x; m(){ #x; } }").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn super_dot_private_name_is_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("class B{} class C extends B { #x; m(){ super.#x; } }")
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn private_getter_setter_pair_is_allowed() {
  let mut rt = new_runtime();
  rt.exec_script("class C { get #x(){return 1} set #x(v){} }")
    .unwrap();
}

