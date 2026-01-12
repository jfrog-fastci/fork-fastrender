use hir_js::ExprKind;
use hir_js::FileKind;
use hir_js::lower_from_source_with_kind;

fn assert_no_missing_exprs(lowered: &hir_js::LowerResult) {
  for body in lowered.bodies.iter() {
    for expr in body.exprs.iter() {
      assert!(
        !matches!(expr.kind, ExprKind::Missing),
        "found ExprKind::Missing at span {:?}",
        expr.span
      );
    }
  }
}

#[test]
fn traverses_throw_statement_children() {
  let source = r#"
    function outer() {
      throw (function inner() { return 1; });
    }
  "#;

  let lowered = lower_from_source_with_kind(FileKind::Ts, source).expect("lower");
  assert_no_missing_exprs(&lowered);
}

#[test]
fn traverses_labeled_statement_children() {
  let source = r#"
    function outer() {
      label: (function inner() { return 1; });
    }
  "#;

  let lowered = lower_from_source_with_kind(FileKind::Ts, source).expect("lower");
  assert_no_missing_exprs(&lowered);
}

