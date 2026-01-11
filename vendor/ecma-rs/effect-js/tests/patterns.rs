#![cfg(feature = "typed")]

use effect_js::{
  load_default_api_database, recognize_semantic_pattern_tables, SemanticArrayOp, SemanticPattern,
};
use effect_js::typed::TypedProgram;
use effect_js::types::{TypeKindSummary, TypeProvider};
use hir_js::{Body, ExprId, ExprKind};
use std::sync::Arc;
use typecheck_ts::{FileKey, MemoryHost, Program};

fn typecheck_and_lower(
  source: &str,
) -> (
  Arc<Program>,
  Arc<hir_js::LowerResult>,
  hir_js::BodyId,
  typecheck_ts::FileId,
) {
  let index_key = FileKey::new("index.ts");
  let mut host = MemoryHost::new();
  host.insert(index_key.clone(), source);

  let program = Arc::new(Program::new(host, vec![index_key.clone()]));
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "typecheck diagnostics: {diagnostics:#?}"
  );

  let file = program.file_id(&index_key).expect("index.ts is loaded");
  let lowered = program.hir_lowered(file).expect("HIR lowered");
  let root_body = lowered.root_body();
  (program, lowered, root_body, file)
}

fn find_ident_expr(body: &Body, lower: &hir_js::LowerResult, name: &str) -> ExprId {
  body
    .exprs
    .iter()
    .enumerate()
    .find_map(|(idx, expr)| match expr.kind {
      ExprKind::Ident(id) if lower.names.resolve(id) == Some(name) => Some(ExprId(idx as u32)),
      _ => None,
    })
    .unwrap_or_else(|| panic!("expected to find ident `{name}` in body"))
}

#[test]
fn typed_map_filter_reduce_chain_is_recognized() {
  let source = r#"
     const xs: number[] = [1,2,3];
     const r = xs.map(x=>x+1).filter(x=>x>1).reduce((a,b)=>a+b,0);
   "#;

  let (program, lowered, root_body, file) = typecheck_and_lower(source);
  let body = lowered.body(root_body).expect("root body exists");
  let types = TypedProgram::from_program(Arc::clone(&program), file);
  let db = load_default_api_database();

  let tables = recognize_semantic_pattern_tables(&lowered, root_body, body, &db, Some(&types));
  assert_eq!(tables.resolved_call.len(), body.exprs.len());
  assert_eq!(tables.patterns.len(), body.exprs.len());

  let patterns: Vec<_> = tables
    .recognized
    .iter()
    .filter_map(|pat| match pat {
      SemanticPattern::MapFilterReduce { array, ops } => Some((*array, ops)),
      _ => None,
    })
    .collect();
  assert_eq!(patterns.len(), 1);

  let xs_expr = find_ident_expr(body, &lowered, "xs");
  assert_eq!(patterns[0].0, xs_expr);
  assert!(matches!(
    patterns[0].1.as_slice(),
    [
      SemanticArrayOp::Map { .. },
      SemanticArrayOp::Filter { .. },
      SemanticArrayOp::Reduce { .. }
    ]
  ));
}

#[test]
fn promise_all_fetch_is_recognized() {
  let source = r#"
     export {};
     declare function fetch(x: string): unknown;
     const urls: string[] = ["a"];
     const Promise = { all: <T>(values: T) => values };
     Promise.all(urls.map((url: string) => fetch(url)));
    "#;

  let (program, lowered, root_body, file) = typecheck_and_lower(source);
  let body = lowered.body(root_body).expect("root body exists");
  let types = TypedProgram::from_program(Arc::clone(&program), file);
  let db = load_default_api_database();

  let patterns =
    recognize_semantic_pattern_tables(&lowered, root_body, body, &db, Some(&types)).recognized;
  let urls_expr = find_ident_expr(body, &lowered, "urls");

  let pats: Vec<_> = patterns
    .iter()
    .filter_map(|pat| match pat {
      SemanticPattern::PromiseAllFetch { urls } => Some(*urls),
      _ => None,
    })
    .collect();

  assert_eq!(pats, vec![urls_expr]);
}

#[test]
fn typed_json_parse_is_recognized() {
  let source = r#"
     type T = { a: number };
     declare const s: string;
     const x: T = JSON.parse(s);
   "#;

  let (program, lowered, root_body, file) = typecheck_and_lower(source);
  let body = lowered.body(root_body).expect("root body exists");
  let types = TypedProgram::from_program(Arc::clone(&program), file);
  let db = load_default_api_database();

  let patterns =
    recognize_semantic_pattern_tables(&lowered, root_body, body, &db, Some(&types)).recognized;

  let s_expr = find_ident_expr(body, &lowered, "s");

  let pats: Vec<_> = patterns
    .iter()
    .filter_map(|pat| match pat {
      SemanticPattern::TypedJsonParse { input, target } => Some((*input, *target)),
      _ => None,
    })
    .collect();

  assert_eq!(pats.len(), 1);
  assert_eq!(pats[0].0, s_expr);
  assert!(!matches!(
    types.type_kind(pats[0].1),
    Some(TypeKindSummary::Unknown | TypeKindSummary::Any) | None
  ));
}
