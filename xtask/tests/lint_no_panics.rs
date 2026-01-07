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

