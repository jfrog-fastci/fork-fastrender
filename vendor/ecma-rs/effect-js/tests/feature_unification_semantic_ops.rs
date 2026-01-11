#![cfg(feature = "hir-semantic-ops")]

use effect_js::{callsite_info_for_args, load_default_api_database};
use hir_js::{lower_from_source_with_kind, BodyId, ExprId, ExprKind, FileKind, StmtKind};

fn first_stmt_expr(lowered: &hir_js::LowerResult) -> (BodyId, ExprId) {
  let root = lowered.root_body();
  let root_body = lowered.body(root).expect("root body");
  let first_stmt = *root_body.root_stmts.first().expect("root stmt");
  let stmt = &root_body.stmts[first_stmt.0 as usize];
  match stmt.kind {
    StmtKind::Expr(expr) => (root, expr),
    _ => panic!("expected expression statement"),
  }
}

#[test]
fn feature_unification_semantic_ops_compiles() {
  let kb = load_default_api_database();
  let lowered = lower_from_source_with_kind(FileKind::Js, "arr.map(x => x + 1);").unwrap();
  let (body, expr) = first_stmt_expr(&lowered);
  let body_ref = lowered.body(body).expect("body");

  assert!(
    matches!(&body_ref.exprs[expr.0 as usize].kind, ExprKind::ArrayMap { .. }),
    "expected `hir-js` to lower Array.prototype.map as a semantic op when `hir-semantic-ops` is enabled"
  );

  let callsite = callsite_info_for_args(&lowered, body, expr, &kb);
  assert_eq!(callsite.callback_is_pure, Some(true));
  assert_eq!(callsite.callback_uses_index, Some(false));
}
