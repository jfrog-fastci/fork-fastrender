use effect_js::{analyze_string_encodings, StringEncoding};
use knowledge_base::KnowledgeBase;

fn find_first_expr(
  body: &hir_js::Body,
  pred: impl Fn(&hir_js::ExprKind) -> bool,
) -> hir_js::ExprId {
  body
    .exprs
    .iter()
    .enumerate()
    .find_map(|(idx, expr)| pred(&expr.kind).then_some(hir_js::ExprId(idx as u32)))
    .expect("expected to find matching expression in test body")
}

#[test]
fn ascii_string_literal_is_ascii() {
  let lower = hir_js::lower_from_source("\"hello\";").unwrap();
  let root_body_id = lower.hir.root_body;
  let root_body = &lower.bodies[*lower.body_index.get(&root_body_id).unwrap()];

  let expr_id = find_first_expr(root_body, |kind| {
    matches!(kind, hir_js::ExprKind::Literal(hir_js::Literal::String(_)))
  });

  let kb = KnowledgeBase::default();
  let results = analyze_string_encodings(&lower, &kb);
  let root = results.get(&root_body_id).unwrap();

  assert_eq!(root.encodings[expr_id.0 as usize], StringEncoding::Ascii);
}

#[test]
fn utf8_string_literal_is_utf8() {
  let lower = hir_js::lower_from_source("\"hé\";").unwrap();
  let root_body_id = lower.hir.root_body;
  let root_body = &lower.bodies[*lower.body_index.get(&root_body_id).unwrap()];

  let expr_id = find_first_expr(root_body, |kind| {
    matches!(kind, hir_js::ExprKind::Literal(hir_js::Literal::String(_)))
  });

  let kb = KnowledgeBase::default();
  let results = analyze_string_encodings(&lower, &kb);
  let root = results.get(&root_body_id).unwrap();

  assert_eq!(root.encodings[expr_id.0 as usize], StringEncoding::Utf8);
}

#[test]
fn template_literal_ascii_segments_and_ascii_expr_is_ascii() {
  let lower = hir_js::lower_from_source("`x${\"a\"}y`;").unwrap();
  let root_body_id = lower.hir.root_body;
  let root_body = &lower.bodies[*lower.body_index.get(&root_body_id).unwrap()];

  let expr_id = find_first_expr(root_body, |kind| matches!(kind, hir_js::ExprKind::Template(_)));

  let kb = KnowledgeBase::default();
  let results = analyze_string_encodings(&lower, &kb);
  let root = results.get(&root_body_id).unwrap();

  assert_eq!(root.encodings[expr_id.0 as usize], StringEncoding::Ascii);
}

#[cfg(feature = "typed")]
#[test]
fn to_lowercase_preserves_ascii() {
  use effect_js::typed::TypedProgram;
  use std::sync::Arc;
  use typecheck_ts::{FileKey, MemoryHost, Program};

  let key = FileKey::new("index.ts");
  let mut host = MemoryHost::new();
  host.insert(key.clone(), "\"ABC\".toLowerCase();");

  let program = Arc::new(Program::new(host, vec![key.clone()]));
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "typecheck diagnostics: {diagnostics:#?}"
  );

  let file = program.file_id(&key).expect("index.ts loaded");
  let lowered = program.hir_lowered(file).expect("HIR lowered");
  let root_body_id = lowered.root_body();
  let root_body = lowered.body(root_body_id).expect("root body exists");

  let expr_id = find_first_expr(root_body, |kind| matches!(kind, hir_js::ExprKind::Call(_)));

  let types = TypedProgram::from_program(Arc::clone(&program), file);
  let kb = KnowledgeBase::default();
  let results = effect_js::encoding::analyze_string_encodings_typed(lowered.as_ref(), &kb, &types);
  let root = results.get(&root_body_id).unwrap();

  assert_eq!(root.encodings[expr_id.0 as usize], StringEncoding::Ascii);
}
