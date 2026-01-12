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
fn parallelize_array_reduce_associative_add() {
  let mut program = compile_source_typed(
    r#"
      const arr: number[] = [1, 2, 3];
      const sum: number = arr.reduce((a: number, b: number) => a + b, 0);
      console.log(sum);
    "#,
    TopLevelMode::Module,
    false,
  );

  annotate_program(&mut program);

  let insts = collect_insts(program.top_level.analyzed_cfg());
  #[cfg(any(feature = "native-fusion", feature = "native-array-ops"))]
  if let Some(chain) = insts.iter().find(|inst| inst.t == InstTyp::ArrayChain) {
    assert_eq!(chain.meta.parallel, Some(ParallelPlan::Parallelizable));
    return;
  }

  let reduce_call = insts
    .iter()
    .find(|inst| {
      inst.t == InstTyp::Call
        && matches!(inst.args.get(0), Some(Arg::Builtin(path)) if path == "Array.prototype.reduce")
    })
    .expect("expected Array.prototype.reduce call");

  assert_eq!(reduce_call.meta.parallel, Some(ParallelPlan::Parallelizable));
}
