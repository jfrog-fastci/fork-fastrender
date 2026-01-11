#![cfg(feature = "hir-semantic-ops")]

use effect_js::hir_rewrite::annotate_known_api_calls;
use effect_js::ApiDatabase;
use hir_js::Expr;
use hir_js::{ExprId, ExprKind, FileKind, LowerResult};

fn static_callee_path(lowered: &LowerResult, body: &hir_js::Body, expr_id: ExprId) -> Option<String> {
  let expr = body.exprs.get(expr_id.0 as usize)?;
  match &expr.kind {
    ExprKind::Ident(name) => Some(lowered.names.resolve(*name)?.to_string()),
    ExprKind::Member(mem) => {
      if mem.optional {
        return None;
      }
      let hir_js::ObjectKey::Ident(prop) = mem.property else {
        return None;
      };
      let base = static_callee_path(lowered, body, mem.object)?;
      let prop = lowered.names.resolve(prop)?;
      Some(format!("{base}.{prop}"))
    }
    ExprKind::TypeAssertion { expr: inner, .. }
    | ExprKind::NonNull { expr: inner }
    | ExprKind::Satisfies { expr: inner, .. } => static_callee_path(lowered, body, *inner),
    _ => None,
  }
}

fn find_call(body: &hir_js::Body, lowered: &LowerResult, name: &str) -> (ExprId, Vec<ExprId>) {
  for (idx, expr) in body.exprs.iter().enumerate() {
    let ExprKind::Call(call) = &expr.kind else {
      continue;
    };
    if call.optional || call.is_new {
      continue;
    }
    if static_callee_path(lowered, body, call.callee).as_deref() != Some(name) {
      continue;
    }
    let args = call.args.iter().map(|a| a.expr).collect();
    return (ExprId(idx as u32), args);
  }

  panic!("missing call expression for {name}");
}

fn hir_api_id_from_kb_id(id: knowledge_base::ApiId) -> hir_js::ApiId {
  hir_js::ApiId(id.raw())
}

fn range_of(source: &str, needle: &str) -> diagnostics::TextRange {
  let start = source.find(needle).expect("needle not found") as u32;
  diagnostics::TextRange::new(start, start + needle.len() as u32)
}

fn find_expr_by_span(body: &hir_js::Body, span: diagnostics::TextRange) -> (ExprId, &Expr) {
  body
    .exprs
    .iter()
    .enumerate()
    .find_map(|(idx, expr)| (expr.span == span).then_some((ExprId(idx as u32), expr)))
    .expect("expression not found for span")
}

#[test]
fn rewrites_known_api_calls() {
  let src = r#"
    const s = '{"x": 1}';
    const x = 9;
    JSON.parse(s);
    Math.sqrt(x);
  "#;

  let lowered = hir_js::lower_from_source_with_kind(FileKind::Ts, src).unwrap();
  let body_id = lowered.hir.root_body;
  let body = lowered.body(body_id).unwrap();

  let (json_call, json_args) = find_call(body, &lowered, "JSON.parse");
  let (sqrt_call, sqrt_args) = find_call(body, &lowered, "Math.sqrt");

  let db = ApiDatabase::from_embedded().unwrap();
  let rewritten = annotate_known_api_calls(&lowered, &db, None);
  let rewritten_body = rewritten.body(body_id).unwrap();

  let expected_json = hir_api_id_from_kb_id(db.id_of("JSON.parse").unwrap());
  let expected_sqrt = hir_api_id_from_kb_id(db.id_of("Math.sqrt").unwrap());

  match &rewritten_body.exprs[json_call.0 as usize].kind {
    ExprKind::KnownApiCall { api, args } => {
      assert_eq!(*api, expected_json);
      assert_eq!(args, &json_args);
    }
    other => panic!("expected KnownApiCall at {json_call:?}, got {other:?}"),
  }

  match &rewritten_body.exprs[sqrt_call.0 as usize].kind {
    ExprKind::KnownApiCall { api, args } => {
      assert_eq!(*api, expected_sqrt);
      assert_eq!(args, &sqrt_args);
    }
    other => panic!("expected KnownApiCall at {sqrt_call:?}, got {other:?}"),
  }
}

#[test]
fn does_not_rewrite_require_member_calls() {
  let src = "require('node:fs').readFile('x', () => {});";
  let lowered = hir_js::lower_from_source_with_kind(FileKind::Ts, src).unwrap();
  let body_id = lowered.hir.root_body;
  let body = lowered.body(body_id).unwrap();
  let span = range_of(src, "require('node:fs').readFile('x', () => {})");
  let (call_expr, _) = find_expr_by_span(body, span);

  let db = ApiDatabase::from_embedded().unwrap();
  let rewritten = annotate_known_api_calls(&lowered, &db, None);
  let rewritten_body = rewritten.body(body_id).unwrap();

  match &rewritten_body.exprs[call_expr.0 as usize].kind {
    ExprKind::KnownApiCall { .. } => panic!(
      "expected call to preserve callee evaluation (require(...) base); got KnownApiCall at {call_expr:?}"
    ),
    ExprKind::Call(_) => {}
    other => panic!("expected Call at {call_expr:?}, got {other:?}"),
  }
}

#[test]
fn does_not_rewrite_member_calls_with_call_base() {
  let src = r#"
    function getStr() { return 'x'; }
    getStr().toLowerCase();
  "#;
  let lowered = hir_js::lower_from_source_with_kind(FileKind::Ts, src).unwrap();
  let body_id = lowered.hir.root_body;
  let body = lowered.body(body_id).unwrap();
  let span = range_of(src, "getStr().toLowerCase()");
  let (call_expr, _) = find_expr_by_span(body, span);

  let db = ApiDatabase::from_embedded().unwrap();
  let rewritten = annotate_known_api_calls(&lowered, &db, None);
  let rewritten_body = rewritten.body(body_id).unwrap();

  assert!(
    !matches!(
      rewritten_body.exprs[call_expr.0 as usize].kind,
      ExprKind::KnownApiCall { .. }
    ),
    "expected member-call with Call base to remain a Call; got KnownApiCall at {call_expr:?}"
  );
}

#[test]
fn does_not_rewrite_instance_methods() {
  let src = r#"
    const s = 'x';
    s.toLowerCase();
  "#;
  let lowered = hir_js::lower_from_source_with_kind(FileKind::Ts, src).unwrap();
  let body_id = lowered.hir.root_body;
  let body = lowered.body(body_id).unwrap();
  let span = range_of(src, "s.toLowerCase()");
  let (call_expr, _) = find_expr_by_span(body, span);

  let db = ApiDatabase::from_embedded().unwrap();
  let rewritten = annotate_known_api_calls(&lowered, &db, None);
  let rewritten_body = rewritten.body(body_id).unwrap();

  assert!(
    !matches!(
      rewritten_body.exprs[call_expr.0 as usize].kind,
      ExprKind::KnownApiCall { .. }
    ),
    "expected instance-method call to remain a Call; got KnownApiCall at {call_expr:?}"
  );
}
