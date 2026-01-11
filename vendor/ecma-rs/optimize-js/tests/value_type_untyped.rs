#[path = "common/mod.rs"]
mod common;

use common::compile_source;
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
fn untyped_literal_var_assign_seeds_value_type() {
  let program = compile_source(
    r#"
      let x = 123;
      console.log(x);
    "#,
    TopLevelMode::Module,
    true,
  );

  // Use the CFG immediately after SSA renaming (before DVN) so the literal assignment still
  // appears explicitly.
  let step = find_step(&program.top_level, "ssa_rename_targets");
  let insts = collect_insts(step);

  let assign = insts
    .iter()
    .copied()
    .find(|inst| {
      inst.t == InstTyp::VarAssign
        && matches!(
          inst.args.get(0),
          Some(Arg::Const(Const::Num(JsNumber(123.0))))
        )
    })
    .expect("expected VarAssign of numeric literal 123");

  assert_eq!(assign.value_type, ValueTypeSummary::NUMBER);
}

#[test]
fn untyped_dvn_consteval_preserves_value_type_for_folded_constant() {
  let program = compile_source(
    r#"
      let x = 1 + 2;
      console.log(x);
    "#,
    TopLevelMode::Module,
    true,
  );

  let step = find_step(&program.top_level, "opt1_dvn");
  let insts = collect_insts(step);

  let folded = insts
    .iter()
    .copied()
    .find(|inst| {
      inst.t == InstTyp::VarAssign
        && matches!(inst.args.get(0), Some(Arg::Const(Const::Num(JsNumber(3.0)))))
    })
    .expect("expected DVN to const-fold 1 + 2 into VarAssign 3");

  assert_eq!(folded.value_type, ValueTypeSummary::NUMBER);
}

#[test]
fn untyped_dvn_constant_propagation_updates_value_type() {
  let program = compile_source(
    r#"
      let a = 1;
      let b = a;
      console.log(b);
    "#,
    TopLevelMode::Module,
    true,
  );

  let step = find_step(&program.top_level, "opt1_dvn");
  let insts = collect_insts(step);

  let assigns: Vec<_> = insts
    .iter()
    .copied()
    .filter(|inst| {
      inst.t == InstTyp::VarAssign
        && matches!(inst.args.get(0), Some(Arg::Const(Const::Num(JsNumber(1.0)))))
    })
    .collect();

  assert!(
    assigns.len() >= 2,
    "expected at least two VarAssign sites for the constant-propagated `1`, got {assigns:?}"
  );
  assert!(
    assigns
      .iter()
      .all(|inst| inst.value_type == ValueTypeSummary::NUMBER),
    "expected DVN to keep VarAssign.value_type consistent after constant propagation, got {assigns:?}"
  );
}

#[test]
fn untyped_dvn_const_builtin_undefined_sets_value_type() {
  let program = compile_source(
    r#"
      let x = undefined;
      console.log(x);
    "#,
    TopLevelMode::Module,
    true,
  );

  let step = find_step(&program.top_level, "opt1_dvn");
  let insts = collect_insts(step);

  let assign = insts
    .iter()
    .copied()
    .find(|inst| inst.t == InstTyp::VarAssign && matches!(inst.args.get(0), Some(Arg::Const(Const::Undefined))))
    .expect("expected DVN to canonicalize builtin undefined into Const::Undefined");

  assert_eq!(
    assign.value_type,
    ValueTypeSummary::UNDEFINED,
    "expected value_type to be updated when DVN canonicalizes builtin undefined"
  );
}
