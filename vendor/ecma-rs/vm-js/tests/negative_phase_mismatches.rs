use vm_js::{Heap, HeapLimits, JsRuntime, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn await_in_non_async_function_is_parse_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script("function f() { await 0; }").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)), "expected VmError::Syntax, got {err:?}");
}

#[test]
fn for_await_of_in_non_async_function_is_parse_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("function f() { for await (const x of []) { void x; } }")
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)), "expected VmError::Syntax, got {err:?}");
}

#[test]
fn await_in_class_field_initializer_is_parse_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script("class C { x = await 0; }").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)), "expected VmError::Syntax, got {err:?}");
}

#[test]
fn await_in_class_static_field_initializer_is_parse_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("class C { static x = await 0; }")
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)), "expected VmError::Syntax, got {err:?}");
}

#[test]
fn await_in_object_literal_expression_in_non_async_function_is_parse_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("function f() { ({ x: await 0 }); }")
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)), "expected VmError::Syntax, got {err:?}");
}

#[test]
fn super_property_at_top_level_is_parse_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script("super.x;").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)), "expected VmError::Syntax, got {err:?}");
}

#[test]
fn super_computed_property_at_top_level_is_parse_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script("super[0];").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)), "expected VmError::Syntax, got {err:?}");
}

#[test]
fn super_property_in_non_method_function_is_parse_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script("function f() { super.x; }").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)), "expected VmError::Syntax, got {err:?}");
}

#[test]
fn super_property_in_nested_non_method_function_is_parse_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("class C { m() { function f() { super.x; } } }")
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)), "expected VmError::Syntax, got {err:?}");
}

#[test]
fn arguments_assignment_in_class_static_block_is_parse_syntax_error() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script("class C { static { arguments = 1; } }")
    .unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)), "expected VmError::Syntax, got {err:?}");
}

#[test]
fn return_at_top_level_is_parse_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script("return 0;").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)), "expected VmError::Syntax, got {err:?}");
}

#[test]
fn break_at_top_level_is_parse_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script("break;").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)), "expected VmError::Syntax, got {err:?}");
}

#[test]
fn continue_at_top_level_is_parse_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script("continue;").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)), "expected VmError::Syntax, got {err:?}");
}

#[test]
fn duplicate_lexical_binding_is_parse_syntax_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script("let a; let a;").unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)), "expected VmError::Syntax, got {err:?}");
}
