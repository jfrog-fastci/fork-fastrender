#![cfg(all(feature = "semantic-ops", feature = "typed", feature = "native-fusion"))]

#[path = "common/mod.rs"]
mod common;

use common::compile_source_typed;
use optimize_js::analysis::annotate_program;
use optimize_js::cfg::cfg::Cfg;
use optimize_js::il::inst::{Inst, InstTyp, ParallelPlan, ParallelReason};
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
fn parallelize_array_chain_map_filter_pure() {
  let mut program = compile_source_typed(
    r#"
      const arr = [1, 2, 3];
      const out = arr.map((x) => x + 1).filter((x) => x > 1);
      console.log(out);
    "#,
    TopLevelMode::Module,
    false,
  );

  annotate_program(&mut program);

  let insts = collect_insts(program.top_level.analyzed_cfg());
  let chain = insts
    .iter()
    .find(|inst| inst.t == InstTyp::ArrayChain)
    .expect("expected ArrayChain inst");

  assert_eq!(chain.meta.parallel, Some(ParallelPlan::Parallelizable));
}

#[test]
fn parallelize_array_chain_disallows_index() {
  let mut program = compile_source_typed(
    r#"
      const arr = [1, 2, 3];
      const out = arr.map((x, i) => x + i).filter((x) => x > 1);
      console.log(out);
    "#,
    TopLevelMode::Module,
    false,
  );

  annotate_program(&mut program);

  let insts = collect_insts(program.top_level.analyzed_cfg());
  let chain = insts
    .iter()
    .find(|inst| inst.t == InstTyp::ArrayChain)
    .expect("expected ArrayChain inst");

  assert_eq!(
    chain.meta.parallel,
    Some(ParallelPlan::NotParallelizable(
      ParallelReason::CallbackUsesIndex
    ))
  );
}

#[test]
fn parallelize_array_chain_disallows_impure_callback() {
  let mut program = compile_source_typed(
    r#"
      const arr = [1, 2, 3];
      const box = { x: 0 };
      const out = arr
        .map((x) => {
          box.x = box.x + x;
          return x;
        })
        .filter((x) => x > 1);
      console.log(out);
    "#,
    TopLevelMode::Module,
    false,
  );

  annotate_program(&mut program);

  let insts = collect_insts(program.top_level.analyzed_cfg());
  let chain = insts
    .iter()
    .find(|inst| inst.t == InstTyp::ArrayChain)
    .expect("expected ArrayChain inst");

  assert_eq!(
    chain.meta.parallel,
    Some(ParallelPlan::NotParallelizable(
      ParallelReason::ImpureCallback
    ))
  );
}
