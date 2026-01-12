#![cfg(feature = "typed")]

#[path = "common/mod.rs"]
mod common;

use common::compile_source_typed;
use optimize_js::analysis::annotate_program;
use optimize_js::cfg::cfg::Cfg;
use optimize_js::il::inst::{Inst, InstTyp, VectorizeHint};
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
fn vectorize_hint_simple_loop() {
  let mut program = compile_source_typed(
    r#"
      let a: number[] = [1, 2, 3, 4, 5, 6, 7, 8];
      let b: number[] = [9, 10, 11, 12, 13, 14, 15, 16];
      let sum: number = 0;
      for (let i: number = 0; i < a.length; i = i + 1) {
        sum = sum + a[i] * b[i];
      }
      console.log(sum);
    "#,
    TopLevelMode::Module,
    false,
  );

  annotate_program(&mut program);

  let insts = collect_insts(program.top_level.analyzed_cfg());
  assert!(
    insts.iter().any(|inst| inst.t == InstTyp::CondGoto && inst.meta.vectorize_hint == Some(VectorizeHint::Yes)),
    "expected at least one loop header CondGoto to have VectorizeHint::Yes"
  );
}

