#![cfg(feature = "semantic-ops")]

#[path = "common/mod.rs"]
mod common;

use common::compile_source;
use optimize_js::analysis::annotate_program;
use optimize_js::cfg::cfg::Cfg;
use optimize_js::il::inst::{Arg, Inst, InstTyp, ParallelPlan};
use optimize_js::TopLevelMode;

fn collect_insts(cfg: &Cfg) -> Vec<Inst> {
  cfg
    .graph
    .labels_sorted()
    .into_iter()
    .flat_map(|label| cfg.bblocks.get(label).iter().cloned())
    .collect()
}

#[test]
fn parallelize_promise_all_spawns_all() {
  let mut program = compile_source(
    r#"
      function a() { return 1; }
      function b() { return 2; }
      const p = Promise.all([a(), b()]);
      console.log(p);
    "#,
    TopLevelMode::Module,
    false,
  );

  annotate_program(&mut program);

  let insts = collect_insts(program.top_level.analyzed_cfg());

  #[cfg(feature = "native-async-ops")]
  {
    let op = insts
      .iter()
      .find(|inst| inst.t == InstTyp::PromiseAll)
      .expect("expected PromiseAll inst");
    assert_eq!(op.meta.parallel, Some(ParallelPlan::SpawnAll));
  }

  #[cfg(not(feature = "native-async-ops"))]
  {
    let call = insts
      .iter()
      .find(|inst| inst.t == InstTyp::Call && matches!(inst.args.get(0), Some(Arg::Builtin(path)) if path == "Promise.all"))
      .expect("expected Promise.all call");
    assert_eq!(call.meta.parallel, Some(ParallelPlan::SpawnAll));
  }
}

#[test]
fn parallelize_promise_race_spawns_all_but_races() {
  let mut program = compile_source(
    r#"
      function a() { return 1; }
      function b() { return 2; }
      const p = Promise.race([a(), b()]);
      console.log(p);
    "#,
    TopLevelMode::Module,
    false,
  );

  annotate_program(&mut program);

  let insts = collect_insts(program.top_level.analyzed_cfg());

  #[cfg(feature = "native-async-ops")]
  {
    let op = insts
      .iter()
      .find(|inst| inst.t == InstTyp::PromiseRace)
      .expect("expected PromiseRace inst");
    assert_eq!(op.meta.parallel, Some(ParallelPlan::SpawnAllButRaceResult));
  }

  #[cfg(not(feature = "native-async-ops"))]
  {
    let call = insts
      .iter()
      .find(|inst| inst.t == InstTyp::Call && matches!(inst.args.get(0), Some(Arg::Builtin(path)) if path == "Promise.race"))
      .expect("expected Promise.race call");
    assert_eq!(call.meta.parallel, Some(ParallelPlan::SpawnAllButRaceResult));
  }
}

