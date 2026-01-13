use vm_js::{Heap, HeapLimits, JsRuntime, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn assert_syntax_error(src: &str) {
  let mut rt = new_runtime();
  let err = rt.exec_script(src).unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)), "expected syntax error, got {err:?}");
}

#[test]
fn invalid_assignment_target_in_assignment_expression() {
  assert_syntax_error("1 = 2;");
}

#[test]
fn invalid_assignment_target_in_prefix_update_expression() {
  assert_syntax_error("++1;");
}

#[test]
fn invalid_assignment_target_in_postfix_update_expression() {
  assert_syntax_error("1++;");
}

#[test]
fn destructuring_pattern_is_invalid_in_compound_assignment() {
  assert_syntax_error("({a} += 1);");
}

#[test]
fn invalid_destructuring_assignment_target() {
  assert_syntax_error("({a: 1} = {a: 2});");
}

#[test]
fn invalid_for_in_lhs_assignment_target() {
  assert_syntax_error("for (1 in {a: 1}) {}");
}

#[test]
fn invalid_for_of_lhs_assignment_target() {
  assert_syntax_error("for (1 of [1]) {}");
}

