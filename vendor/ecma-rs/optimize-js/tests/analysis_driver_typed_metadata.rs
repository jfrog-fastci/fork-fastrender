#![cfg(feature = "typed")]

#[path = "common/mod.rs"]
mod common;

use common::compile_source_typed;
use optimize_js::analysis::annotate_program;
use optimize_js::cfg::cfg::Cfg;
use optimize_js::il::inst::Inst;
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
fn annotate_program_preserves_typed_il_metadata() {
  let mut program = compile_source_typed(
    r#"
      let num: number = 123;
      // Keep the value live so the optimizer emits at least one typed instruction.
      console.log(num);
    "#,
    TopLevelMode::Module,
    false,
  );

  let insts_before = collect_insts(program.top_level.analyzed_cfg());
  let (idx, before) = insts_before
    .iter()
    .enumerate()
    .find(|(_, inst)| inst.meta.hir_expr.is_some())
    .map(|(idx, inst)| (idx, inst.meta.clone()))
    .expect("expected at least one instruction to carry typed metadata before annotation");

  annotate_program(&mut program);

  let insts_after = collect_insts(program.top_level.analyzed_cfg());
  assert_eq!(
    insts_before.len(),
    insts_after.len(),
    "annotate_program should not change instruction count"
  );

  let after = &insts_after[idx].meta;
  assert_eq!(after.type_id, before.type_id, "expected type_id to be preserved");
  assert_eq!(
    after.native_layout, before.native_layout,
    "expected native_layout to be preserved"
  );
  assert_eq!(after.hir_expr, before.hir_expr, "expected hir_expr to be preserved");
  assert_eq!(
    after.type_summary, before.type_summary,
    "expected type_summary to be preserved"
  );
  assert_eq!(
    after.excludes_nullish, before.excludes_nullish,
    "expected excludes_nullish to be preserved"
  );
}
