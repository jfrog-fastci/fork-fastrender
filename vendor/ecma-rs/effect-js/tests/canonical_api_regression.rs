#![cfg(feature = "typed")]

use effect_js::typed::TypedProgram;
use effect_js::{
  analyze_patterns, load_default_api_database, recognize_patterns_typed, SemanticPattern,
};
use hir_js::ExprId;
use std::sync::Arc;
use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
use typecheck_ts::{FileKey, MemoryHost, Program};

fn es2015_host() -> MemoryHost {
  MemoryHost::with_options(TsCompilerOptions {
    libs: vec![LibName::parse("es2015").expect("LibName::parse(es2015)")],
    ..Default::default()
  })
}

#[test]
fn canonical_engine_reports_same_map_get_or_default_as_legacy_typed_entrypoint() {
  let source = r#"
    const m: Map<string, number> = new Map();
    const k = "x";
    const v = m.has(k) ? m.get(k) : 123;
  "#;

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
  let body = lowered.body(root_body).expect("root body exists");

  let types = TypedProgram::from_program(Arc::clone(&program), file);
  let kb = load_default_api_database();

  let canonical = analyze_patterns(&lowered, root_body, body, &kb, Some(&types));
  let canonical_matches: Vec<_> = canonical
    .tables
    .recognized
    .iter()
    .filter_map(|pat| match pat {
      SemanticPattern::MapGetOrDefault {
        conditional,
        map,
        key,
        default,
      } => Some((*conditional, *map, *key, *default)),
      _ => None,
    })
    .collect();
  assert_eq!(
    canonical_matches.len(),
    1,
    "expected exactly one canonical MapGetOrDefault pattern, got {canonical_matches:#?}"
  );

  let legacy = recognize_patterns_typed(&kb, &lowered, root_body, &types);
  let legacy_matches: Vec<_> = legacy
    .iter()
    .filter_map(|pat| match pat {
      effect_js::RecognizedPattern::MapGetOrDefault {
        conditional,
        map,
        key,
        default,
      } => Some((*conditional, *map, *key, *default)),
      _ => None,
    })
    .collect();
  assert_eq!(
    legacy_matches.len(),
    1,
    "expected exactly one legacy MapGetOrDefault pattern, got {legacy_matches:#?}"
  );

  // Ensure the canonical and legacy entry points point at the same HIR nodes.
  let (canon_cond, canon_map, canon_key, canon_default) = canonical_matches[0];
  let (legacy_cond, legacy_map, legacy_key, legacy_default) = legacy_matches[0];
  assert_eq!(canon_cond, legacy_cond);
  assert_eq!(canon_map, legacy_map);
  assert_eq!(canon_key, legacy_key);
  assert_eq!(canon_default, legacy_default);

  // Sanity-check: the conditional expression should actually exist in the body arena.
  assert!(
    body.exprs.get(canon_cond.0 as usize).is_some(),
    "expected conditional ExprId({}) to exist in body.exprs",
    canon_cond.0
  );

  // Prevent regressions where the pattern accidentally points at a different expression.
  let _ = ExprId(canon_cond.0);
}

