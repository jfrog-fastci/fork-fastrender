use std::path::Path;
use xtask::lint_no_panics::{lint_source, ViolationKind};

#[test]
fn lint_no_panics_reports_violations_with_line_numbers() {
  let src = r#"
pub fn demo() {
  let _ = Some(1).unwrap();
}

#[cfg(test)]
mod tests {
  #[test]
  fn allows_panics_in_tests() {
    let _ = Some(1).unwrap();
    panic!("expected");
  }
}
"#;

  let violations = lint_source(Path::new("demo.rs"), src);
  assert_eq!(violations.len(), 1, "unexpected violations: {violations:#?}");
  assert_eq!(violations[0].kind, ViolationKind::Unwrap);
  assert_eq!(violations[0].line, 3);
}

#[test]
fn lint_no_panics_flags_assert_and_unreachable_in_production_code() {
  let src = r#"
pub fn demo() {
  assert!(1 == 2);
  assert_eq!(1, 2);
  assert_ne!(1, 1);
  unreachable!("boom");
}
"#;

  let violations = lint_source(Path::new("demo.rs"), src);
  assert_eq!(violations.len(), 4, "unexpected violations: {violations:#?}");
  assert_eq!(violations[0].kind, ViolationKind::Assert);
  assert_eq!(violations[1].kind, ViolationKind::AssertEq);
  assert_eq!(violations[2].kind, ViolationKind::AssertNe);
  assert_eq!(violations[3].kind, ViolationKind::Unreachable);
}

#[test]
fn lint_no_panics_allows_allow_panic_marker_for_asserts() {
  let src = r#"
pub fn demo() {
  assert!(1 == 2); // fastrender-allow-panic
  unreachable!("boom"); // fastrender-allow-panic
}
"#;

  let violations = lint_source(Path::new("demo.rs"), src);
  assert!(violations.is_empty(), "expected allow markers to suppress: {violations:#?}");
}

#[test]
fn lint_no_panics_ignores_cfg_test_asserts() {
  let src = r#"
pub fn demo() {
  #[cfg(test)]
  {
    assert!(1 == 2);
    unreachable!("boom");
  }
}
"#;

  let violations = lint_source(Path::new("demo.rs"), src);
  assert!(violations.is_empty(), "unexpected violations: {violations:#?}");
}
