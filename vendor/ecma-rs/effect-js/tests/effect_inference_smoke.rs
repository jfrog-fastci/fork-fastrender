use effect_js::{analyze_effects_untyped, load_default_api_database, EffectSet, Purity};
#[cfg(feature = "hir-semantic-ops")]
use effect_js::callsite_info_for_args;
use hir_js::{BodyId, ExprId, FileKind, StmtKind};

fn first_stmt_expr(lowered: &hir_js::LowerResult) -> (BodyId, ExprId) {
  let root = lowered.root_body();
  let root_body = lowered.body(root).expect("root body");
  for stmt_id in &root_body.root_stmts {
    let stmt = &root_body.stmts[stmt_id.0 as usize];
    if let StmtKind::Expr(expr) = stmt.kind {
      return (root, expr);
    }
  }
  panic!("expected expression statement");
}

#[test]
fn math_sqrt_call_is_pure_and_may_throw() {
  let kb = load_default_api_database();
  let lowered = hir_js::lower_from_source_with_kind(FileKind::Js, "Math.sqrt(x);").unwrap();
  let (body, expr) = first_stmt_expr(&lowered);

  let tables = analyze_effects_untyped(&kb, &lowered);
  let tables = tables.get(&body).expect("tables");
  let effects = tables.effects_by_expr[expr.0 as usize];
  let purity = tables.purity_by_expr[expr.0 as usize];

  assert_eq!(purity, Purity::Pure);
  assert!(effects.contains(EffectSet::MAY_THROW));
  assert!(!effects.contains(EffectSet::IO));
  assert!(!effects.contains(EffectSet::NETWORK));
}

#[test]
fn fetch_call_is_io_network_and_impure() {
  let kb = load_default_api_database();
  let lowered = hir_js::lower_from_source_with_kind(FileKind::Js, r#"fetch("https://e");"#).unwrap();
  let (body, expr) = first_stmt_expr(&lowered);

  let tables = analyze_effects_untyped(&kb, &lowered);
  let tables = tables.get(&body).expect("tables");
  let effects = tables.effects_by_expr[expr.0 as usize];
  let purity = tables.purity_by_expr[expr.0 as usize];

  assert_eq!(purity, Purity::Impure);
  assert!(effects.contains(EffectSet::IO));
  assert!(effects.contains(EffectSet::NETWORK));
}

#[cfg(feature = "hir-semantic-ops")]
#[test]
fn array_map_is_allocating() {
  let kb = load_default_api_database();
  let lowered = hir_js::lower_from_source_with_kind(FileKind::Js, "arr.map(x => x + 1);").unwrap();
  let (body, expr) = first_stmt_expr(&lowered);

  let tables = analyze_effects_untyped(&kb, &lowered);
  let tables = tables.get(&body).expect("tables");
  let effects = tables.effects_by_expr[expr.0 as usize];
  let purity = tables.purity_by_expr[expr.0 as usize];

  assert_eq!(purity, Purity::Allocating);
  assert!(effects.contains(EffectSet::ALLOCATES));
  assert!(!effects.contains(EffectSet::IO));
}

#[cfg(feature = "hir-semantic-ops")]
#[test]
fn array_reduce_bigint_add_is_pureish_and_associative() {
  let kb = load_default_api_database();
  let lowered = hir_js::lower_from_source_with_kind(
    FileKind::Ts,
    "arr.reduce((a: bigint, b: bigint) => a + b, 0n);",
  )
  .unwrap();
  let (body, expr) = first_stmt_expr(&lowered);

  // Callback associativity is inferred by the existing callback analysis.
  let callsite = callsite_info_for_args(&lowered, body, expr, &kb);
  assert_eq!(callsite.callback_is_associative, Some(true));

  let tables = analyze_effects_untyped(&kb, &lowered);
  let tables = tables.get(&body).expect("tables");
  let effects = tables.effects_by_expr[expr.0 as usize];
  let purity = tables.purity_by_expr[expr.0 as usize];

  assert!(
    matches!(purity, Purity::Pure | Purity::Allocating),
    "expected reduce to be Pure-ish, got {purity:?}"
  );
  assert!(!effects.contains(EffectSet::IO));
  assert!(!effects.contains(EffectSet::NETWORK));
}

#[cfg(feature = "hir-semantic-ops")]
#[test]
fn promise_all_semantic_op_is_handled() {
  let kb = load_default_api_database();
  let lowered = hir_js::lower_from_source_with_kind(FileKind::Js, "Promise.all([a, b]);").unwrap();
  let (body, expr) = first_stmt_expr(&lowered);

  let tables = analyze_effects_untyped(&kb, &lowered);
  let tables = tables.get(&body).expect("tables");
  let effects = tables.effects_by_expr[expr.0 as usize];
  assert!(
    effects.contains(EffectSet::NONDETERMINISTIC),
    "expected Promise.all to be conservatively nondeterministic"
  );
}

#[cfg(feature = "typed")]
#[test]
fn typed_array_map_resolves_instance_call() {
  use effect_js::analyze_effects_typed;
  use effect_js::typed::TypedProgram;
  use std::sync::Arc;
  use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
  use typecheck_ts::{FileKey, MemoryHost, Program};

  let index_key = FileKey::new("index.ts");
  let mut host = MemoryHost::with_options(TsCompilerOptions {
    libs: vec![LibName::parse("es2015").expect("LibName::parse(es2015)")],
    ..Default::default()
  });
  host.insert(index_key.clone(), r#"const arr: number[] = [1, 2, 3]; arr.map(x => x + 1);"#);

  let program = Arc::new(Program::new(host, vec![index_key.clone()]));
  let diagnostics = program.check();
  assert!(diagnostics.is_empty(), "typecheck diagnostics: {diagnostics:#?}");

  let file = program.file_id(&index_key).expect("index.ts is loaded");
  let lowered = program.hir_lowered(file).expect("HIR lowered");
  let types = TypedProgram::from_program(Arc::clone(&program), file);
  let kb = load_default_api_database();

  let (body, expr) = first_stmt_expr(&lowered);
  let tables = analyze_effects_typed(&kb, &lowered, &types);
  let tables = tables.get(&body).expect("tables");
  let effects = tables.effects_by_expr[expr.0 as usize];
  let purity = tables.purity_by_expr[expr.0 as usize];

  assert_eq!(purity, Purity::Allocating);
  assert!(effects.contains(EffectSet::ALLOCATES));
  assert!(!effects.contains(EffectSet::IO));
}
