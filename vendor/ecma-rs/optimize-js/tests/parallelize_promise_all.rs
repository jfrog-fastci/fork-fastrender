#![cfg(all(feature = "semantic-ops", feature = "typed"))]

#[path = "common/mod.rs"]
mod common;

use common::compile_source_typed;
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
fn parallelize_promise_all() {
  let mut program = compile_source_typed(
    r#"
      async function a(): Promise<number> { return 1; }
      async function b(): Promise<number> { return 2; }
      const p = Promise.all([a(), b()]);
      console.log(p);
    "#,
    TopLevelMode::Module,
    false,
  );

  annotate_program(&mut program);

  let insts = collect_insts(program.top_level.analyzed_cfg());
  let all = insts
    .iter()
    .find(|inst| inst.t == InstTyp::Call && matches!(inst.args.get(0), Some(Arg::Builtin(path)) if path == "Promise.all"))
    .expect("expected Promise.all call");

  assert_eq!(all.meta.parallel, Some(ParallelPlan::SpawnAll));
}

