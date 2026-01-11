use optimize_js::analysis::analyze_cfg;
use optimize_js::il::inst::InstTyp;
use optimize_js::{compile_source_with_cfg_options, CompileCfgOptions, TopLevelMode};

#[test]
fn analyses_driver_is_deterministic_on_ssa_cfg() {
  let source = r#"
    let x = 0;
    if (unknown_cond()) {
      side_effect_true();
      x = 1;
    } else {
      side_effect_false();
      x = 2;
    }
    sink(x);
  "#;

  let options = CompileCfgOptions {
    keep_ssa: true,
    ..CompileCfgOptions::default()
  };

  let program =
    compile_source_with_cfg_options(source, TopLevelMode::Module, false, options).expect("compile");
  let cfg = &program.top_level.body;

  let has_phi = cfg
    .bblocks
    .all()
    .any(|(_, insts)| insts.iter().any(|inst| inst.t == InstTyp::Phi));
  assert!(has_phi, "expected SSA CFG to contain at least one Phi node");

  let first = analyze_cfg(cfg);
  let second = analyze_cfg(cfg);
  assert_eq!(
    first, second,
    "analysis results should be stable across invocations (SSA cfg)"
  );
}

