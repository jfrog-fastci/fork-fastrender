#![cfg(feature = "typed")]

use optimize_js::analysis::annotate_program;
use optimize_js::cfg::cfg::Cfg;
use optimize_js::il::inst::Inst;
use optimize_js::{compile_source_typed, TopLevelMode};

fn collect_insts(cfg: &Cfg) -> Vec<Inst> {
  let mut blocks: Vec<_> = cfg.bblocks.all().collect();
  blocks.sort_by_key(|(label, _)| *label);
  let mut insts = Vec::new();
  for (_, block) in blocks {
    insts.extend(block.iter().cloned());
  }
  insts
}

#[test]
fn annotate_program_preserves_typed_lowering_meta() {
  let mut program = compile_source_typed(
    r#"
      let x: number = 1;
      console.log(x);
    "#,
    TopLevelMode::Module,
    false,
  )
  .expect("compile");

  let before = collect_insts(&program.top_level.body);
  let had_type_summary = before.iter().any(|inst| inst.meta.type_summary.is_some());
  let had_hir_expr = before.iter().any(|inst| inst.meta.hir_expr.is_some());
  let had_excludes_nullish = before.iter().any(|inst| inst.meta.excludes_nullish);
  assert!(
    had_type_summary || had_hir_expr || had_excludes_nullish,
    "expected typed lowering to populate InstMeta.type_summary/hir_expr/excludes_nullish"
  );

  annotate_program(&mut program);

  let after = collect_insts(&program.top_level.body);
  if had_type_summary {
    assert!(
      after.iter().any(|inst| inst.meta.type_summary.is_some()),
      "expected annotate_program to preserve InstMeta.type_summary"
    );
  }
  if had_hir_expr {
    assert!(
      after.iter().any(|inst| inst.meta.hir_expr.is_some()),
      "expected annotate_program to preserve InstMeta.hir_expr"
    );
  }
  if had_excludes_nullish {
    assert!(
      after.iter().any(|inst| inst.meta.excludes_nullish),
      "expected annotate_program to preserve InstMeta.excludes_nullish"
    );
  }
}

