#![cfg(feature = "typed")]

use effect_js::properties::OutputLengthRelation;
use effect_js::typed::TypedProgram;
use effect_js::{load_default_api_database, plan_array_chains_typed, ArrayPipelinePlan, ArrayStageKind};
use hir_js::{ExprId, ExprKind};
use std::sync::Arc;
use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
use typecheck_ts::{FileKey, MemoryHost, Program};

const INDEX_TS: &str = r#"
const xs: number[] = [1, 2, 3];
const f = (x: number) => x + 1;
const g = (x: number) => x * 2;

const h_add = (a: number, b: number) => a + b;
const h_or = (a: number, b: number) => a | b;

xs.map(f).filter(g).reduce(h_add, 0);
xs.map(f).filter(g).reduce(h_or, 0);
xs.map(f).map(g);
"#;

fn es2015_host() -> MemoryHost {
  MemoryHost::with_options(TsCompilerOptions {
    libs: vec![LibName::parse("es2015").expect("LibName::parse(es2015)")],
    ..Default::default()
  })
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

fn match_plan<'a>(plans: &'a [ArrayPipelinePlan], kinds: &[ArrayStageKind]) -> &'a ArrayPipelinePlan {
  plans
    .iter()
    .find(|plan| plan.stages.iter().map(|s| s.kind).collect::<Vec<_>>() == kinds)
    .unwrap_or_else(|| panic!("expected plan with stage kinds {kinds:?}, got {plans:#?}"))
}

#[test]
fn plans_array_chains_typed() {
  let index_key = FileKey::new("index.ts");
  let mut host = es2015_host();
  host.insert(index_key.clone(), INDEX_TS);

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

  let plans = plan_array_chains_typed(&kb, &lowered, root_body, &types);

  // `xs.map(f).filter(g).reduce(h_*, 0)` appears twice + `xs.map(f).map(g)`.
  assert_eq!(plans.len(), 3, "expected exactly three pipeline plans");

  // map→filter→reduce(h_add)
  let map_filter_reduce = match_plan(
    &plans,
    &[
      ArrayStageKind::Map,
      ArrayStageKind::Filter,
      ArrayStageKind::Reduce,
    ],
  );
  assert_ident(body, &lowered, map_filter_reduce.base, "xs");
  assert_eq!(map_filter_reduce.stages.len(), 3);

  let s0 = &map_filter_reduce.stages[0];
  assert_ident(body, &lowered, s0.callback, "f");
  assert_eq!(s0.meta.output_len, OutputLengthRelation::SameAsInput);
  assert!(s0.meta.fusable_with_next);

  let s1 = &map_filter_reduce.stages[1];
  assert_ident(body, &lowered, s1.callback, "g");
  assert_eq!(s1.meta.output_len, OutputLengthRelation::LeInput);
  assert!(s1.meta.fusable_with_next);

  let s2 = &map_filter_reduce.stages[2];
  // We don't know whether this is h_add or h_or yet; check both below.
  assert_eq!(s2.kind, ArrayStageKind::Reduce);
  assert_eq!(s2.meta.output_len, OutputLengthRelation::Unknown);
  assert!(!s2.meta.fusable_with_next);

  // One reduce should be parallelizable (bitwise OR), the other should not (number add).
  let reduce_parallelizable: Vec<_> = plans
    .iter()
    .filter(|plan| {
      plan.stages.len() == 3
        && plan.stages[0].kind == ArrayStageKind::Map
        && plan.stages[1].kind == ArrayStageKind::Filter
        && plan.stages[2].kind == ArrayStageKind::Reduce
    })
    .map(|plan| {
      let cb = plan.stages[2].callback;
      let parallel = plan.stages[2].meta.parallelizable;
      (cb, parallel)
    })
    .collect();

  assert_eq!(
    reduce_parallelizable.len(),
    2,
    "expected two map→filter→reduce chains"
  );
  let mut by_name = reduce_parallelizable
    .into_iter()
    .map(|(cb, parallel)| {
      let ExprKind::Ident(name) = &body.exprs[cb.0 as usize].kind else {
        panic!("expected reduce callback to be ident");
      };
      let name = lowered.names.resolve(*name).expect("callback ident name").to_string();
      (name, parallel)
    })
    .collect::<std::collections::BTreeMap<_, _>>();

  assert_eq!(
    by_name.remove("h_add"),
    Some(false),
    "expected + reduce to be non-parallelizable"
  );
  assert_eq!(
    by_name.remove("h_or"),
    Some(true),
    "expected | reduce to be parallelizable"
  );

  // map→map
  let map_map = match_plan(&plans, &[ArrayStageKind::Map, ArrayStageKind::Map]);
  assert_ident(body, &lowered, map_map.base, "xs");
  assert_eq!(map_map.stages.len(), 2);
  assert_ident(body, &lowered, map_map.stages[0].callback, "f");
  assert_ident(body, &lowered, map_map.stages[1].callback, "g");
  assert!(map_map.stages[0].meta.fusable_with_next);
  assert!(!map_map.stages[1].meta.fusable_with_next);
}
