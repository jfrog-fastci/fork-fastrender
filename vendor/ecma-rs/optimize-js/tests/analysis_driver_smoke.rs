#[path = "common/mod.rs"]
mod common;

use common::compile_source;
use optimize_js::analysis::driver::{analyze_program, annotate_program, FunctionKey};
use optimize_js::cfg::cfg::Cfg;
use optimize_js::il::inst::{Inst, InstTyp, StringEncoding};
use optimize_js::TopLevelMode;

fn any_inst(cfg: &Cfg, pred: impl Fn(&Inst) -> bool) -> bool {
  for label in cfg.graph.labels_sorted() {
    for inst in cfg.bblocks.get(label).iter() {
      if pred(inst) {
        return true;
      }
    }
  }
  false
}

#[test]
fn analysis_driver_smoke() {
  let source = r#"
    let x = foo;
    if (x == null) { bar(); } else { baz(); }
    let s = `π`;
    sink(s);
  "#;

  let program = compile_source(source, TopLevelMode::Module, false);
  let top_cfg = &program.top_level.body;

  // Snapshot one instruction's metadata to ensure `analyze_program` is non-mutating.
  let meta_before = top_cfg
    .graph
    .labels_sorted()
    .into_iter()
    .find_map(|label| top_cfg.bblocks.get(label).first().map(|inst| inst.meta.clone()));

  let analyses = analyze_program(&program);
  assert!(
    analyses.nullability.contains_key(&FunctionKey::TopLevel),
    "expected `analyze_program` to produce top-level nullability results"
  );
  assert!(
    analyses.encoding.contains_key(&FunctionKey::TopLevel),
    "expected `analyze_program` to produce top-level string encoding results"
  );

  assert!(
    analyses
      .nullability
      .get(&FunctionKey::TopLevel)
      .is_some_and(|r| r.entry_state(top_cfg.entry).is_reachable()),
    "expected nullability analysis results for top-level entry block"
  );
  assert!(
    analyses
      .encoding
      .get(&FunctionKey::TopLevel)
      .is_some_and(|r| r.block_entry(top_cfg.entry).is_some()),
    "expected encoding analysis results for top-level entry block"
  );

  if let Some(meta_before) = meta_before {
    let meta_after = top_cfg
      .graph
      .labels_sorted()
      .into_iter()
      .find_map(|label| top_cfg.bblocks.get(label).first().map(|inst| inst.meta.clone()))
      .expect("expected program to contain at least one instruction");
    assert_eq!(
      meta_before, meta_after,
      "`analyze_program` should not mutate instruction metadata"
    );
  }

  let mut program = compile_source(source, TopLevelMode::Module, false);
  let _analyses = annotate_program(&mut program);
  let cfg = &program.top_level.body;

  assert!(
    any_inst(cfg, |inst| inst.t == InstTyp::CondGoto
      && inst.meta.nullability_narrowing.is_some()),
    "expected `annotate_program` to set `InstMeta.nullability_narrowing` on at least one CondGoto"
  );

  assert!(
    any_inst(cfg, |inst| inst.meta.result_type.string_encoding == Some(StringEncoding::Utf8)),
    "expected `annotate_program` to annotate at least one Utf8 string result (from `π` template literal)"
  );
}

