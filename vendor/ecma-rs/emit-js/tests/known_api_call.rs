#![cfg(all(feature = "api-database", feature = "semantic-ops"))]

use emit_js::{emit_hir_file_to_string, emit_hir_file_to_string_with_api_database, EmitOptions};
use hir_js::{lower_from_source_with_kind, ExprKind, FileKind, StmtKind};
use knowledge_base::ApiDatabase;
use std::sync::Arc;

fn rewrite_first_expr_stmt_to_known_api(lowered: &hir_js::LowerResult, api_name: &str) -> hir_js::LowerResult {
  let body_id = lowered.root_body();
  let body = lowered.body(body_id).expect("root body");
  let stmt_id = *body.root_stmts.first().expect("root statement");
  let stmt = &body.stmts[stmt_id.0 as usize];
  let expr_id = match stmt.kind {
    StmtKind::Expr(expr) => expr,
    _ => panic!("expected expression statement"),
  };

  let ExprKind::Call(call) = &body.exprs[expr_id.0 as usize].kind else {
    panic!("expected Call expression");
  };
  let args = call.args.iter().map(|arg| arg.expr).collect();

  let mut rewritten = lowered.clone();
  let body_idx = *rewritten
    .body_index
    .get(&body_id)
    .expect("root body index");
  let mut new_body = rewritten.bodies[body_idx].as_ref().clone();
  new_body.exprs[expr_id.0 as usize].kind = ExprKind::KnownApiCall {
    api: hir_js::ApiId::from_name(api_name),
    args,
  };
  rewritten.bodies[body_idx] = Arc::new(new_body);

  rewritten
}

#[test]
fn emits_known_api_call_with_database() {
  let src = "JSON.parse(x);";
  let lowered = lower_from_source_with_kind(FileKind::Js, src).expect("lower");
  let rewritten = rewrite_first_expr_stmt_to_known_api(&lowered, "JSON.parse");

  let expected = emit_hir_file_to_string(&lowered, EmitOptions::minified()).expect("emit baseline");
  let db = ApiDatabase::from_embedded().expect("load knowledge base");
  let emitted = emit_hir_file_to_string_with_api_database(&rewritten, EmitOptions::minified(), &db)
    .expect("emit KnownApiCall");

  assert_eq!(emitted, expected);
}

#[test]
fn emits_non_identifier_known_api_callee_with_global_this() {
  let src = "fs.readFile(x);";
  let lowered = lower_from_source_with_kind(FileKind::Js, src).expect("lower");
  let rewritten = rewrite_first_expr_stmt_to_known_api(&lowered, "node:fs.readFile");

  let db = ApiDatabase::from_embedded().expect("load knowledge base");
  let emitted = emit_hir_file_to_string_with_api_database(&rewritten, EmitOptions::minified(), &db)
    .expect("emit KnownApiCall");

  assert_eq!(emitted, "globalThis[\"node:fs\"].readFile(x);");
}

