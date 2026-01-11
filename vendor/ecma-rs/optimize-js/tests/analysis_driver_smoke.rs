#[path = "common/mod.rs"]
mod common;

use common::compile_source;
use optimize_js::analysis::analyze_program_function;
use optimize_js::analysis::driver::{analyze_program, annotate_program, FunctionKey};
use optimize_js::cfg::cfg::Cfg;
use optimize_js::il::inst::{Arg, Const, Inst, InstTyp, StringEncoding};
use optimize_js::TopLevelMode;
use optimize_js::{OptimizationStats, ProgramFunction};

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
  let top_cfg = program.top_level.analyzed_cfg();

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
  let cfg = program.top_level.analyzed_cfg();

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

#[test]
fn analyze_program_function_uses_analyzed_cfg() {
  use optimize_js::cfg::cfg::{CfgBBlocks, CfgGraph};

  fn cfg_with_string(s: &str) -> Cfg {
    let mut graph = CfgGraph::default();
    // Ensure the node exists even though the CFG has no edges.
    graph.connect(0, 0);
    graph.disconnect(0, 0);
    let mut bblocks = CfgBBlocks::default();
    bblocks.add(
      0,
      vec![Inst::var_assign(
        0,
        Arg::Const(Const::Str(s.to_string())),
      )],
    );
    Cfg {
      graph,
      bblocks,
      entry: 0,
    }
  }

  // Make the deconstructed `body` and SSA `ssa_body` intentionally disagree so
  // we can assert the wrapper uses `ProgramFunction::analyzed_cfg()`.
  let func = ProgramFunction {
    debug: None,
    body: cfg_with_string("hello"),
    params: Vec::new(),
    ssa_body: Some(cfg_with_string("π")),
    stats: OptimizationStats::default(),
  };

  let analyses = analyze_program_function(&func);
  assert_eq!(
    analyses.encoding.encoding_at_exit(0, 0),
    StringEncoding::Utf8,
    "expected analyze_program_function to analyze the SSA cfg when present"
  );
}
