#![cfg(all(feature = "semantic-ops", feature = "typed"))]

#[path = "common/mod.rs"]
mod common;

use common::compile_source_typed;
use optimize_js::analysis::annotate_program;
use optimize_js::cfg::cfg::Cfg;
use optimize_js::il::inst::{Arg, Inst, InstTyp, ParallelPlan, ParallelReason};
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
fn parallelize_array_map_impure() {
  let mut program = compile_source_typed(
    r#"
      let total = 0;
      const out = [1, 2, 3].map((x) => {
        total = total + x;
        return x + 1;
      });
      console.log(out, total);
    "#,
    TopLevelMode::Module,
    false,
  );

  annotate_program(&mut program);

  let insts = collect_insts(program.top_level.analyzed_cfg());
  let map = insts
    .iter()
    .find(|inst| {
      inst.t == InstTyp::Call
        && matches!(inst.args.get(0), Some(Arg::Builtin(path)) if path == "Array.prototype.map")
    })
    .expect("expected Array.prototype.map call");

  assert_eq!(
    map.meta.parallel,
    Some(ParallelPlan::NotParallelizable(ParallelReason::ImpureCallback))
  );
}

