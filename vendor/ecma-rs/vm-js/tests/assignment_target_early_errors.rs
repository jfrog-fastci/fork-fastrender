use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

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

fn assert_execs_to_number(src: &str, expected: f64) {
  let mut rt = new_runtime();
  let value = rt.exec_script(src).unwrap();
  assert!(
    matches!(value, Value::Number(n) if n == expected),
    "expected number {expected}, got {value:?}"
  );
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

#[test]
fn parenthesized_identifier_is_valid_assignment_target() {
  assert_execs_to_number("var a = 0; (a) = 1; a", 1.0);
}

#[test]
fn parenthesized_identifier_is_valid_prefix_update_target() {
  assert_execs_to_number("var a = 0; ++(a); a", 1.0);
}

#[test]
fn parenthesized_identifier_is_valid_postfix_update_target() {
  assert_execs_to_number("var a = 0; (a)++; a", 1.0);
}

#[test]
fn parenthesized_member_is_valid_postfix_update_target() {
  assert_execs_to_number("var o = { x: 0 }; (o.x)++; o.x", 1.0);
}

#[test]
fn parenthesized_optional_chain_base_is_valid_assignment_target() {
  assert_execs_to_number("var o = { x: { y: 0 } }; (o?.x).y = 1; o.x.y", 1.0);
}

#[test]
fn parenthesized_optional_chain_base_is_valid_update_target() {
  assert_execs_to_number("var o = { x: { y: 0 } }; (o?.x).y++; o.x.y", 1.0);
}

#[test]
fn parenthesized_optional_chain_base_is_valid_parenthesized_update_target() {
  assert_execs_to_number("var o = { x: { y: 0 } }; ((o?.x).y)++; o.x.y", 1.0);
}

#[test]
fn parenthesized_optional_chain_base_is_valid_for_of_lhs_target() {
  assert_execs_to_number("var o = { x: { y: 0 } }; for ((o?.x).y of [1]) {} o.x.y", 1.0);
}

#[test]
fn unparenthesized_optional_chain_is_invalid_assignment_target() {
  assert_syntax_error("var o = { x: { y: 0 } }; o?.x.y = 1;");
}
