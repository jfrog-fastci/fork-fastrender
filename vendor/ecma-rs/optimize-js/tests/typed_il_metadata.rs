#![cfg(feature = "typed")]

#[path = "common/mod.rs"]
mod common;

use common::compile_source_typed;
use optimize_js::il::inst::{Arg, Const, Inst, InstTyp};
use optimize_js::types::ValueTypeSummary;
use optimize_js::util::debug::OptimizerDebugStep;
use optimize_js::{ProgramFunction, TopLevelMode};
use parse_js::num::JsNumber;

fn find_step<'a>(func: &'a ProgramFunction, name: &str) -> &'a OptimizerDebugStep {
  func
    .debug
    .as_ref()
    .expect("debug output should be enabled")
    .steps()
    .iter()
    .find(|step| step.name == name)
    .unwrap_or_else(|| panic!("missing debug step {name}"))
}

fn collect_insts(step: &OptimizerDebugStep) -> Vec<&Inst> {
  step
    .bblocks
    .values()
    .flat_map(|block| block.iter())
    .collect()
}

#[test]
fn typed_il_records_value_type_metadata() {
  let program = compile_source_typed(
    r#"
      let num: number = 123;
      let str: string = "hello";
      let maybe: string | null = Math.random() ? "a" : null;
      // Prevent trivial dead code elimination of the declarations.
      console.log(num, str, maybe);
    "#,
    TopLevelMode::Module,
    true,
  );

  // Inspect the IR after SSA construction + at least one optimization pass (DVN).
  let step = find_step(&program.top_level, "opt1_dvn");
  let insts = collect_insts(step);

  let num_inst = insts
    .iter()
    .copied()
    .find(|inst| {
      inst.t == InstTyp::VarAssign
        && matches!(
          inst.args.get(0),
          Some(Arg::Const(Const::Num(JsNumber(123.0))))
        )
    })
    .expect("expected VarAssign for numeric literal 123");
  let num_meta = &num_inst.meta;
  assert_eq!(num_meta.type_summary, Some(ValueTypeSummary::Number));
  assert!(num_meta.excludes_nullish);
  assert!(num_meta.hir_expr.is_some());

  let str_inst = insts
    .iter()
    .copied()
    .find(|inst| {
      inst.t == InstTyp::VarAssign
        && matches!(
          inst.args.get(0),
          Some(Arg::Const(Const::Str(value))) if value == "hello"
        )
    })
    .expect("expected VarAssign for string literal \"hello\"");
  let str_meta = &str_inst.meta;
  assert_eq!(str_meta.type_summary, Some(ValueTypeSummary::String));
  assert!(str_meta.excludes_nullish);
  assert!(str_meta.hir_expr.is_some());

  // The conditional expression `Math.random() ? "a" : null` has type `string | null`,
  // so we record an unknown runtime kind and that it does *not* exclude nullish.
  let union_inst = insts
    .iter()
    .copied()
    .find(|inst| inst.t == InstTyp::VarAssign && matches!(inst.args.get(0), Some(Arg::Const(Const::Null))))
    .expect("expected VarAssign for union branch assigning null");
  let union_meta = &union_inst.meta;
  assert_eq!(union_meta.type_summary, Some(ValueTypeSummary::Unknown));
  assert!(!union_meta.excludes_nullish);
  assert!(union_meta.hir_expr.is_some());
}

#[test]
fn dvn_var_assign_rewrite_preserves_value_type_metadata() {
  let program = compile_source_typed("console.log(1 + 2);", TopLevelMode::Module, true);
  let step = find_step(&program.top_level, "opt1_dvn");
  let insts = collect_insts(step);

  let folded = insts
    .iter()
    .copied()
    .find(|inst| {
      inst.t == InstTyp::VarAssign
        && matches!(
          inst.args.get(0),
          Some(Arg::Const(Const::Num(JsNumber(3.0))))
        )
    })
    .expect("expected DVN to const-fold 1 + 2 into VarAssign 3");
  let meta = &folded.meta;
  assert_eq!(meta.type_summary, Some(ValueTypeSummary::Number));
  assert!(meta.excludes_nullish);
  assert!(meta.hir_expr.is_some());
}
