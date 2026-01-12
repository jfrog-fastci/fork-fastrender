#![cfg(feature = "typed")]

use optimize_js::cfg::cfg::Cfg;
use optimize_js::il::inst::{BinOp, InstTyp};
use optimize_js::{compile_source_typed_cfg_options, CompileCfgOptions, InlineOptions, TopLevelMode};

fn add_bin_meta(cfg: &Cfg) -> (Option<optimize_js::types::TypeId>, Option<hir_js::ExprId>) {
  let mut metas = Vec::new();
  for label in cfg.reverse_postorder() {
    for inst in cfg.bblocks.get(label) {
      if inst.t == InstTyp::Bin && inst.bin_op == BinOp::Add {
        metas.push((inst.meta.type_id, inst.meta.hir_expr));
      }
    }
  }
  assert_eq!(
    metas.len(),
    1,
    "expected exactly one Add binop in cfg, got {metas:?}"
  );
  metas[0]
}

#[test]
fn inlining_preserves_typed_metadata_on_cloned_insts() {
  let src = r#"
    function inc(x: number): number {
      return x + 1;
    }
    const y = inc(41);
    void y;
  "#;

  let base_options = CompileCfgOptions {
    keep_ssa: true,
    run_opt_passes: true,
    ..CompileCfgOptions::default()
  };

  let program_no_inline =
    compile_source_typed_cfg_options(src, TopLevelMode::Module, false, base_options)
      .expect("typed compile");
  assert_eq!(program_no_inline.functions.len(), 1);
  let callee_cfg = program_no_inline.functions[0].ssa_body.as_ref().expect("ssa cfg");
  let callee_meta = add_bin_meta(callee_cfg);
  assert!(
    callee_meta.0.is_some() && callee_meta.1.is_some(),
    "expected typed metadata to be present on callee Add instruction, got {callee_meta:?}"
  );

  let inline_options = CompileCfgOptions {
    inline: InlineOptions {
      enabled: true,
      threshold: 16,
      max_depth: 8,
    },
    ..base_options
  };
  let program_inline =
    compile_source_typed_cfg_options(src, TopLevelMode::Module, false, inline_options)
      .expect("typed compile with inliner");
  let top_cfg = program_inline.top_level.ssa_body.as_ref().expect("ssa cfg");
  let inlined_meta = add_bin_meta(top_cfg);

  assert_eq!(
    inlined_meta, callee_meta,
    "expected inlined Add instruction to preserve typed metadata from callee"
  );
}

