#![cfg(feature = "typed")]

use effect_js::{
  load_default_api_database, recognize_semantic_pattern_tables, SemanticArrayOp, SemanticPattern,
};
use effect_js::typed::TypedProgram;
use hir_js::{ExprId, ExprKind, Literal, StmtKind};
use std::sync::Arc;
use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
use typecheck_ts::{FileKey, MemoryHost, Program};

fn es2015_host() -> MemoryHost {
  MemoryHost::with_options(TsCompilerOptions {
    libs: vec![LibName::parse("es2015").expect("LibName::parse(es2015)")],
    ..Default::default()
  })
}

fn typecheck_and_lower(
  source: &str,
) -> (
  Arc<Program>,
  Arc<hir_js::LowerResult>,
  hir_js::BodyId,
  typecheck_ts::FileId,
) {
  let index_key = FileKey::new("index.ts");
  let mut host = es2015_host();
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

fn assert_ident(body: &hir_js::Body, lowered: &hir_js::LowerResult, expr: ExprId, expected: &str) {
  let expr = &body.exprs[expr.0 as usize];
  match &expr.kind {
    ExprKind::Ident(name) => assert_eq!(
      lowered.names.resolve(*name),
      Some(expected),
      "expected Ident({expected})"
    ),
    other => panic!("expected Ident({expected}), got {other:?}"),
  }
}

fn root_expr_stmts(body: &hir_js::Body) -> Vec<ExprId> {
  body
    .root_stmts
    .iter()
    .filter_map(|stmt_id| match &body.stmts[stmt_id.0 as usize].kind {
      StmtKind::Expr(expr) => Some(*expr),
      _ => None,
    })
    .collect()
}

#[test]
fn semantic_patterns_array_chain_is_recognized() {
  let source = r#"
    const xs: number[] = [1, 2, 3];
    const f = (x: number) => x + 1;
    const g = (x: number) => x * 2;
    const h = (a: number, b: number) => a + b;

    xs.map(f).map(g);
    xs.map(f).filter(g).reduce(h, 0);
    xs.filter(g);
  "#;

  let (program, lowered, root_body, file) = typecheck_and_lower(source);
  let body = lowered.body(root_body).expect("root body exists");
  let types = TypedProgram::from_program(Arc::clone(&program), file);
  let db = load_default_api_database();

  let tables = recognize_semantic_pattern_tables(&lowered, root_body, body, &db, Some(&types));

  let expr_stmts = root_expr_stmts(body);
  assert_eq!(
    expr_stmts.len(),
    3,
    "expected 3 expression statements, got {expr_stmts:?}"
  );
  let map_map_expr = expr_stmts[0];
  let map_filter_reduce_expr = expr_stmts[1];
  let filter_only_expr = expr_stmts[2];

  // `xs.map(f).map(g)`.
  let map_map_chains: Vec<_> = tables.patterns[map_map_expr.0 as usize]
    .iter()
    .filter_map(|pat_id| match &tables.recognized[pat_id.0 as usize] {
      SemanticPattern::ArrayChain { array, ops } => Some((*array, ops)),
      _ => None,
    })
    .collect();
  assert_eq!(
    map_map_chains.len(),
    1,
    "expected one ArrayChain pattern on xs.map(f).map(g), got {map_map_chains:?}"
  );
  let (map_map_base, map_map_ops) = map_map_chains[0];
  assert_ident(body, &lowered, map_map_base, "xs");
  assert!(
    matches!(
      map_map_ops.as_slice(),
      [SemanticArrayOp::Map { .. }, SemanticArrayOp::Map { .. }]
    ),
    "expected ops=[Map, Map], got {map_map_ops:?}"
  );

  if let [SemanticArrayOp::Map { callback: cb0 }, SemanticArrayOp::Map { callback: cb1 }] =
    map_map_ops.as_slice()
  {
    assert_ident(body, &lowered, *cb0, "f");
    assert_ident(body, &lowered, *cb1, "g");
  }

  // `xs.map(f).filter(g).reduce(h, 0)`.
  let mfr_chains: Vec<_> = tables.patterns[map_filter_reduce_expr.0 as usize]
    .iter()
    .filter_map(|pat_id| match &tables.recognized[pat_id.0 as usize] {
      SemanticPattern::ArrayChain { array, ops } => Some((*array, ops)),
      _ => None,
    })
    .collect();
  assert_eq!(
    mfr_chains.len(),
    1,
    "expected one ArrayChain pattern on xs.map(f).filter(g).reduce(h, 0), got {mfr_chains:?}"
  );
  let (mfr_base, mfr_ops) = mfr_chains[0];
  assert_ident(body, &lowered, mfr_base, "xs");
  assert!(
    matches!(
      mfr_ops.as_slice(),
      [
        SemanticArrayOp::Map { .. },
        SemanticArrayOp::Filter { .. },
        SemanticArrayOp::Reduce { .. }
      ]
    ),
    "expected ops=[Map, Filter, Reduce], got {mfr_ops:?}"
  );

  if let [
    SemanticArrayOp::Map { callback: map_cb },
    SemanticArrayOp::Filter { callback: filter_cb },
    SemanticArrayOp::Reduce { callback: reduce_cb, init },
  ] = mfr_ops.as_slice()
  {
    assert_ident(body, &lowered, *map_cb, "f");
    assert_ident(body, &lowered, *filter_cb, "g");
    assert_ident(body, &lowered, *reduce_cb, "h");

    match &body.exprs[init.0 as usize].kind {
      ExprKind::Literal(Literal::Number(n)) => assert_eq!(n, "0"),
      other => panic!("expected init to be numeric literal, got {other:?}"),
    }
  }

  // `xs.filter(g)` is too short to form an ArrayChain.
  assert!(
    !tables.patterns[filter_only_expr.0 as usize]
      .iter()
      .any(|pat_id| matches!(tables.recognized[pat_id.0 as usize], SemanticPattern::ArrayChain { .. })),
    "expected xs.filter(g) to not be recognized as ArrayChain"
  );
}

