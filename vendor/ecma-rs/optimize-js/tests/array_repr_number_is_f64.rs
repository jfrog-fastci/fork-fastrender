#![cfg(feature = "typed")]

#[path = "common/mod.rs"]
mod common;

use common::compile_source_typed;
use optimize_js::analysis::annotate_program;
use optimize_js::cfg::cfg::Cfg;
use optimize_js::il::inst::{ArrayElemRepr, BinOp, Const, Inst, InstTyp};
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
fn array_repr_number_is_f64() {
  let mut program = compile_source_typed(
    r#"
      let a: number[] = [1, 2, 3];
      let i: number = 1;
      let x = a[i];
      console.log(x);
    "#,
    TopLevelMode::Module,
    false,
  );

  annotate_program(&mut program);

  let insts = collect_insts(program.top_level.analyzed_cfg());
  let load = insts
    .iter()
    .find(|inst| {
      inst.t == InstTyp::Bin
        && inst.bin_op == BinOp::GetProp
        && inst.meta.array_elem_repr == Some(ArrayElemRepr::F64)
        && !matches!(
          inst.args.get(1),
          Some(optimize_js::il::inst::Arg::Const(Const::Str(s))) if s == "length"
        )
    })
    .expect("expected at least one array element load annotated as F64");

  assert_eq!(load.meta.array_elem_repr, Some(ArrayElemRepr::F64));
}

