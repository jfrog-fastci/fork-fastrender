use effect_js::{
  load_default_api_database, recognize_semantic_pattern_tables, SemanticPattern,
  SemanticPromiseCombinatorKind, SemanticPromiseInputPattern,
};
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

fn promise_combinators_for_expr(
  tables: &effect_js::SemanticPatternTables,
  expr: ExprId,
) -> Vec<(SemanticPromiseCombinatorKind, &SemanticPromiseInputPattern)> {
  tables.patterns[expr.0 as usize]
    .iter()
    .filter_map(|pat_id| match &tables.recognized[pat_id.0 as usize] {
      SemanticPattern::PromiseCombinator { kind, input } => Some((*kind, input)),
      _ => None,
    })
    .collect()
}

#[test]
fn promise_all_array_literal_is_recognized_as_promise_combinator() {
  let db = load_default_api_database();
  let lowered = lower_from_source_with_kind(FileKind::Js, "Promise.all([a(), b()]);").unwrap();
  let (body_id, expr_id) = first_stmt_expr(&lowered);
  let body = lowered.body(body_id).expect("body");

  let tables = recognize_semantic_pattern_tables(&lowered, body_id, body, &db, None);
  let matches = promise_combinators_for_expr(&tables, expr_id);

  assert_eq!(
    matches.len(),
    1,
    "expected one PromiseCombinator pattern on expr {}, got {matches:#?}",
    expr_id.0
  );

  let (kind, input) = matches[0];
  assert_eq!(kind, SemanticPromiseCombinatorKind::All);
  let SemanticPromiseInputPattern::ArrayLiteral {
    array_expr,
    elements,
  } = input
  else {
    panic!("expected ArrayLiteral input, got {input:?}");
  };

  assert_eq!(elements.len(), 2);
  assert!(
    matches!(&body.exprs[array_expr.0 as usize].kind, ExprKind::Array(_)),
    "expected array_expr to point at ExprKind::Array"
  );
}

#[test]
fn promise_race_array_literal_is_recognized_as_promise_combinator() {
  let db = load_default_api_database();
  let lowered = lower_from_source_with_kind(FileKind::Js, "Promise.race([a(), b()]);").unwrap();
  let (body_id, expr_id) = first_stmt_expr(&lowered);
  let body = lowered.body(body_id).expect("body");

  let tables = recognize_semantic_pattern_tables(&lowered, body_id, body, &db, None);
  let matches = promise_combinators_for_expr(&tables, expr_id);

  assert_eq!(
    matches.len(),
    1,
    "expected one PromiseCombinator pattern on expr {}, got {matches:#?}",
    expr_id.0
  );

  let (kind, input) = matches[0];
  assert_eq!(kind, SemanticPromiseCombinatorKind::Race);
  let SemanticPromiseInputPattern::ArrayLiteral {
    array_expr,
    elements,
  } = input
  else {
    panic!("expected ArrayLiteral input, got {input:?}");
  };

  assert_eq!(elements.len(), 2);
  assert!(
    matches!(&body.exprs[array_expr.0 as usize].kind, ExprKind::Array(_)),
    "expected array_expr to point at ExprKind::Array"
  );
}

#[cfg(feature = "hir-semantic-ops")]
#[test]
fn promise_all_array_map_is_recognized_as_promise_combinator() {
  let db = load_default_api_database();
  let lowered = lower_from_source_with_kind(FileKind::Js, "Promise.all(urls.map(fetch));").unwrap();
  let (body_id, expr_id) = first_stmt_expr(&lowered);
  let body = lowered.body(body_id).expect("body");

  let tables = recognize_semantic_pattern_tables(&lowered, body_id, body, &db, None);
  let matches = promise_combinators_for_expr(&tables, expr_id);

  assert_eq!(matches.len(), 1, "expected one PromiseCombinator pattern");
  let (kind, input) = matches[0];
  assert_eq!(kind, SemanticPromiseCombinatorKind::All);

  let SemanticPromiseInputPattern::ArrayMap {
    base,
    map_expr,
    callback,
  } = input
  else {
    panic!("expected ArrayMap input, got {input:?}");
  };

  assert!(
    matches!(
      &body.exprs[map_expr.0 as usize].kind,
      ExprKind::ArrayMap { .. }
    ),
    "expected map_expr to be lowered as ExprKind::ArrayMap when `hir-semantic-ops` is enabled"
  );

  let ExprKind::Ident(base_name) = &body.exprs[base.0 as usize].kind else {
    panic!("expected base to be an identifier expression");
  };
  assert_eq!(lowered.names.resolve(*base_name), Some("urls"));

  let ExprKind::Ident(callback_name) = &body.exprs[callback.0 as usize].kind else {
    panic!("expected callback to be an identifier expression");
  };
  assert_eq!(lowered.names.resolve(*callback_name), Some("fetch"));
}

#[cfg(feature = "hir-semantic-ops")]
#[test]
fn promise_all_semantic_op_is_recognized_as_promise_combinator() {
  let db = load_default_api_database();
  let lowered = lower_from_source_with_kind(FileKind::Js, "Promise.all([a(), b()]);").unwrap();
  let (body_id, expr_id) = first_stmt_expr(&lowered);
  let body = lowered.body(body_id).expect("body");

  let ExprKind::PromiseAll { promises } = &body.exprs[expr_id.0 as usize].kind else {
    panic!("expected ExprKind::PromiseAll when `hir-semantic-ops` is enabled");
  };

  let tables = recognize_semantic_pattern_tables(&lowered, body_id, body, &db, None);
  let matches = promise_combinators_for_expr(&tables, expr_id);

  assert_eq!(matches.len(), 1, "expected one PromiseCombinator pattern");
  let (kind, input) = matches[0];
  assert_eq!(kind, SemanticPromiseCombinatorKind::All);

  let SemanticPromiseInputPattern::ArrayLiteral { elements, .. } = input else {
    panic!("expected ArrayLiteral input, got {input:?}");
  };
  assert_eq!(elements.as_slice(), promises.as_slice());
}
