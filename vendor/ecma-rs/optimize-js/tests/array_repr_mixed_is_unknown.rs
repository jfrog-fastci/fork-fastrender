#![cfg(feature = "typed")]

#[path = "common/mod.rs"]
mod common;

use common::compile_source_typed;
use optimize_js::analysis::annotate_program;
use optimize_js::cfg::cfg::Cfg;
use optimize_js::il::inst::{ArrayElemRepr, Inst, InstTyp};
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
fn array_repr_mixed_is_unknown() {
  let mut program = compile_source_typed(
    r#"
      let a: (number | string)[] = [1, "x"];
      let x = a[0];
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
      inst.t == InstTyp::ArrayLoad && inst.meta.array_elem_repr == Some(ArrayElemRepr::Unknown)
    })
    .expect("expected at least one array element load annotated as Unknown");

  assert_eq!(load.meta.array_elem_repr, Some(ArrayElemRepr::Unknown));
}
