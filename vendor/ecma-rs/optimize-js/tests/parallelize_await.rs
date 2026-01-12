#![cfg(feature = "typed")]

#[path = "common/mod.rs"]
mod common;

use common::compile_source_typed;
use optimize_js::analysis::annotate_program;
use optimize_js::cfg::cfg::Cfg;
use optimize_js::il::inst::{Inst, InstTyp, ParallelPlan, ParallelReason};
#[cfg(not(feature = "native-async-ops"))]
use optimize_js::il::inst::Arg;
use optimize_js::TopLevelMode;

fn collect_insts(cfg: &Cfg) -> Vec<Inst> {
  cfg
    .graph
    .labels_sorted()
    .into_iter()
    .flat_map(|label| cfg.bblocks.get(label).iter().cloned())
    .collect()
}

fn collect_all_insts(program: &optimize_js::Program) -> Vec<Inst> {
  let mut insts = collect_insts(program.top_level.analyzed_cfg());
  for func in &program.functions {
    insts.extend(collect_insts(func.analyzed_cfg()));
  }
  insts
}

#[test]
fn parallelize_await_is_not_parallelizable() {
  let mut program = compile_source_typed(
    r#"
      async function a(): Promise<number> { return 1; }
      async function main(): Promise<void> {
        const x = await a();
        console.log(x);
      }
      main();
    "#,
    TopLevelMode::Module,
    false,
  );

  annotate_program(&mut program);

  let insts = collect_all_insts(&program);

  #[cfg(feature = "native-async-ops")]
  {
    let await_inst = insts
      .iter()
      .find(|inst| inst.t == InstTyp::Await)
      .expect("expected Await semantic op");
    assert_eq!(
      await_inst.meta.parallel,
      Some(ParallelPlan::NotParallelizable(ParallelReason::Await))
    );
  }

  #[cfg(not(feature = "native-async-ops"))]
  {
    let await_inst = insts
      .iter()
      .find(|inst| {
        inst.t == InstTyp::Call
          && matches!(inst.args.get(0), Some(Arg::Builtin(path)) if path == "__optimize_js_await")
      })
      .expect("expected __optimize_js_await call");
    assert_eq!(
      await_inst.meta.parallel,
      Some(ParallelPlan::NotParallelizable(ParallelReason::Await))
    );
  }
}
