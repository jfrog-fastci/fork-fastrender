#![cfg(feature = "typed")]

#[path = "common/mod.rs"]
mod common;

use common::compile_source_typed;
use optimize_js::analysis::{validate_layouts, LayoutValidationMode};
use optimize_js::TopLevelMode;

#[test]
fn layout_map_smoke_every_value_def_has_layout() {
  let program = compile_source_typed(
    r#"
      let x: number = 1;
      let y: number = x + 2;
      console.log(y);
    "#,
    TopLevelMode::Module,
    false,
  );

  let cfg = program
    .top_level
    .cfg_ssa()
    .expect("expected SSA CFG to be preserved on ProgramFunction::ssa_body");

  let layouts = validate_layouts(cfg, LayoutValidationMode::Strict).expect("layout validation");

  for label in cfg.graph.labels_sorted() {
    let Some(block) = cfg.bblocks.maybe_get(label) else {
      continue;
    };
    for inst in block.iter() {
      let Some(&tgt) = inst.tgts.get(0) else {
        continue;
      };
      assert!(
        layouts.contains_key(&tgt),
        "missing layout for SSA value %{tgt} defined by {inst:?}"
      );
    }
  }
}

