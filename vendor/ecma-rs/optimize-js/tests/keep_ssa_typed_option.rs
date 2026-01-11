#![cfg(feature = "typed")]

use optimize_js::cfg::cfg::Cfg;
use optimize_js::il::inst::{Inst, InstTyp};
use optimize_js::types::ValueTypeSummary;
use optimize_js::{
  compile_source_typed, compile_source_typed_cfg_options, CompileCfgOptions, Program, TopLevelMode,
};

fn collect_insts(cfg: &Cfg) -> Vec<Inst> {
  let mut blocks: Vec<_> = cfg.bblocks.all().collect();
  blocks.sort_by_key(|(label, _)| *label);
  let mut insts = Vec::new();
  for (_, block) in blocks {
    insts.extend(block.iter().cloned());
  }
  insts
}

fn collect_all_insts(program: &Program) -> Vec<Inst> {
  let mut insts = collect_insts(&program.top_level.body);
  for func in &program.functions {
    insts.extend(collect_insts(&func.body));
  }
  insts
}

fn collect_program_insts(program: &Program) -> Vec<Vec<Inst>> {
  let mut out = Vec::new();
  out.push(collect_insts(&program.top_level.body));
  for func in &program.functions {
    out.push(collect_insts(&func.body));
  }
  out
}

fn join_program_source() -> &'static str {
  r#"
    /// <reference no-default-lib="true" />
    declare function unknown_cond(): boolean;
    declare function side_effect_true(): void;
    declare function side_effect_false(): void;
    declare function unknown_func(x: number): void;

    let x = 0;
    if (unknown_cond()) {
      side_effect_true();
      x = 1;
    } else {
      side_effect_false();
      x = 2;
    }
    unknown_func(x);
  "#
}

#[test]
fn typed_default_compile_output_has_no_phi_nodes() {
  let program =
    compile_source_typed(join_program_source(), TopLevelMode::Module, false).expect("compile");
  let insts = collect_all_insts(&program);
  assert!(
    insts.iter().all(|inst| inst.t != InstTyp::Phi),
    "expected default compilation to deconstruct SSA, found Phi insts: {insts:?}"
  );
}

#[test]
fn typed_keep_ssa_compile_retains_phi_nodes() {
  let options = CompileCfgOptions {
    keep_ssa: true,
    ..CompileCfgOptions::default()
  };
  let program =
    compile_source_typed_cfg_options(join_program_source(), TopLevelMode::Module, false, options)
      .expect("compile");
  let insts = collect_all_insts(&program);
  assert!(
    insts.iter().any(|inst| inst.t == InstTyp::Phi),
    "expected SSA-retaining compilation to keep Phi insts, got: {insts:?}"
  );
}

#[test]
fn typed_keep_ssa_compile_is_deterministic() {
  let options = CompileCfgOptions {
    keep_ssa: true,
    ..CompileCfgOptions::default()
  };
  let first =
    compile_source_typed_cfg_options(join_program_source(), TopLevelMode::Module, false, options)
      .expect("first compile");
  let second =
    compile_source_typed_cfg_options(join_program_source(), TopLevelMode::Module, false, options)
      .expect("second compile");

  assert_eq!(
    collect_program_insts(&first),
    collect_program_insts(&second),
    "SSA output should be deterministic across runs"
  );
}

#[test]
fn typed_keep_ssa_phi_nodes_carry_type_metadata() {
  let source = r#"
    /// <reference no-default-lib="true" />
    declare function unknown_cond(): boolean;
    declare function unknown_num(): number;

    const sink = (x: number) => x;

    let x = 0;
    if (unknown_cond()) {
      x = unknown_num();
    } else {
      x = unknown_num();
    }
    sink(x);
  "#;

  let options = CompileCfgOptions {
    keep_ssa: true,
    ..CompileCfgOptions::default()
  };
  let program =
    compile_source_typed_cfg_options(source, TopLevelMode::Module, false, options).expect("compile");

  let insts = collect_insts(&program.top_level.body);
  let phi = insts
    .iter()
    .find(|inst| {
      inst.t == InstTyp::Phi
        && inst.meta.type_id.is_some()
        && inst.meta.type_summary == Some(ValueTypeSummary::Number)
        && inst.meta.excludes_nullish
    })
    .expect("expected at least one Phi node to carry typed metadata");

  assert!(
    phi.meta.hir_expr.is_none(),
    "phi nodes inserted for statement-level assignments should not have a single canonical hir_expr, got {phi:?}"
  );
}
