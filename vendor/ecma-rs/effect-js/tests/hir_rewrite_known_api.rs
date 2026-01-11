#![cfg(feature = "hir-semantic-ops")]

use effect_js::hir_rewrite::annotate_known_api_calls;
use effect_js::ApiDatabase;
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
