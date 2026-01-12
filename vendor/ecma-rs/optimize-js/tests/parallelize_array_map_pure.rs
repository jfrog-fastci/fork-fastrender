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
fn parallelize_array_map_pure() {
  let mut program = compile_source_typed(
    r#"
      const arr = [1, 2, 3];
      const out = arr.map((x) => x + 1);
      console.log(out);
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

  let map_call = insts
    .iter()
    .find(|inst| {
      inst.t == InstTyp::Call
        && matches!(inst.args.get(0), Some(Arg::Builtin(path)) if path == "Array.prototype.map")
    })
    .expect("expected Array.prototype.map call");

  assert_eq!(map_call.meta.parallel, Some(ParallelPlan::Parallelizable));
}
