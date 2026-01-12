#![cfg(all(feature = "native-async-ops", feature = "semantic-ops"))]

use optimize_js::cfg::cfg::Cfg;
use optimize_js::il::inst::InstTyp;
use optimize_js::{compile_source_with_cfg_options, CompileCfgOptions, TopLevelMode};

fn count_awaits(cfg: &Cfg) -> usize {
  cfg
    .graph
    .labels_sorted()
    .into_iter()
    .flat_map(|label| cfg.bblocks.get(label).iter())
    .filter(|inst| inst.t == InstTyp::Await)
    .count()
}

fn count_known_resolved_awaits(cfg: &Cfg) -> usize {
  cfg
    .graph
    .labels_sorted()
    .into_iter()
    .flat_map(|label| cfg.bblocks.get(label).iter())
    .filter(|inst| inst.t == InstTyp::Await && inst.meta.await_known_resolved)
    .count()
}

#[test]
fn known_resolved_await_elision_can_be_disabled() {
  let src = r#"
    async function f() {
      const x = await 1;
      return x;
    }
  "#;

  let program_elide = compile_source_with_cfg_options(
    src,
    TopLevelMode::Module,
    false,
    CompileCfgOptions {
      keep_ssa: true,
      // `elide_known_resolved_awaits` defaults to true (existing behaviour).
      ..Default::default()
    },
  )
  .expect("compile source with await elision enabled");
  let cfg_elide = program_elide
    .functions
    .iter()
    .filter_map(|func| func.cfg_ssa())
    .next()
    .expect("expected SSA cfg");
  assert_eq!(
    count_awaits(cfg_elide),
    0,
    "expected known-resolved await to be elided by default"
  );

  let program_no_elide = compile_source_with_cfg_options(
    src,
    TopLevelMode::Module,
    false,
    CompileCfgOptions {
      keep_ssa: true,
      elide_known_resolved_awaits: false,
      ..Default::default()
    },
  )
  .expect("compile source with await elision disabled");
  let cfg_no_elide = program_no_elide
    .functions
    .iter()
    .filter_map(|func| func.cfg_ssa())
    .next()
    .expect("expected SSA cfg");
  assert!(
    count_awaits(cfg_no_elide) > 0,
    "expected await to remain when elide_known_resolved_awaits=false"
  );
  assert!(
    count_known_resolved_awaits(cfg_no_elide) > 0,
    "expected at least one await to remain marked await_known_resolved"
  );
}

