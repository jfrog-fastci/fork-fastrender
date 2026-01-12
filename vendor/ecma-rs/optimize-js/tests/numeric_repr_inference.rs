#![cfg(feature = "typed")]

use optimize_js::analysis::annotate_program;
use optimize_js::il::inst::{Arg, BinOp, Const, InstTyp, NumericRepr};
use optimize_js::{compile_source_typed_cfg_options, CompileCfgOptions, InlineOptions, TopLevelMode};
use parse_js::num::JsNumber;

fn compile_typed_no_opt(source: &str) -> optimize_js::Program {
  compile_source_typed_cfg_options(
    source,
    TopLevelMode::Module,
    false,
    CompileCfgOptions {
      keep_ssa: true,
      run_opt_passes: false,
      inline: InlineOptions::default(),
      ..CompileCfgOptions::default()
    },
  )
  .expect("compile typed source")
}

#[test]
fn numeric_repr_proves_i32_for_small_integer_add() {
  let mut program = compile_typed_no_opt(
    r#"
      const x: number = 100000 + 7;
      console.log(x);
    "#,
  );
  annotate_program(&mut program);

  let cfg = program.top_level.analyzed_cfg();
  let add = cfg
    .graph
    .labels_sorted()
    .into_iter()
    .flat_map(|label| cfg.bblocks.get(label).iter())
    .find(|inst| {
      inst.t == InstTyp::Bin
        && inst.bin_op == BinOp::Add
        && matches!(
          (&inst.args[0], &inst.args[1]),
          (
            Arg::Const(Const::Num(JsNumber(100000.0))),
            Arg::Const(Const::Num(JsNumber(7.0)))
          )
            | (
              Arg::Const(Const::Num(JsNumber(7.0))),
              Arg::Const(Const::Num(JsNumber(100000.0)))
            )
        )
    })
    .expect("expected to find Bin(Add) for 100000 + 7");

  assert_eq!(add.meta.numeric_repr, NumericRepr::I32);
}

#[test]
fn numeric_repr_proves_i64_for_large_integer_literal() {
  let mut program = compile_typed_no_opt(
    r#"
      const x: number = 5000000000;
      console.log(x);
    "#,
  );
  annotate_program(&mut program);

  let cfg = program.top_level.analyzed_cfg();
  let assign = cfg
    .graph
    .labels_sorted()
    .into_iter()
    .flat_map(|label| cfg.bblocks.get(label).iter())
    .find(|inst| {
      inst.t == InstTyp::VarAssign
        && matches!(
          inst.args.get(0),
          Some(Arg::Const(Const::Num(JsNumber(5000000000.0))))
        )
    })
    .expect("expected to find VarAssign for 5000000000");

  assert_eq!(assign.meta.numeric_repr, NumericRepr::I64);
}

#[test]
fn numeric_repr_falls_back_to_f64_for_non_integer_literal() {
  let mut program = compile_typed_no_opt(
    r#"
      const x: number = 1.5;
      console.log(x);
    "#,
  );
  annotate_program(&mut program);

  let cfg = program.top_level.analyzed_cfg();
  let assign = cfg
    .graph
    .labels_sorted()
    .into_iter()
    .flat_map(|label| cfg.bblocks.get(label).iter())
    .find(|inst| {
      inst.t == InstTyp::VarAssign
        && matches!(
          inst.args.get(0),
          Some(Arg::Const(Const::Num(JsNumber(1.5))))
        )
    })
    .expect("expected to find VarAssign for 1.5");

  assert_eq!(assign.meta.numeric_repr, NumericRepr::F64);
}

#[test]
fn numeric_repr_is_unknown_for_non_number_values() {
  let mut program = compile_typed_no_opt(
    r#"
      const s: string = "hello";
      console.log(s);
    "#,
  );
  annotate_program(&mut program);

  let cfg = program.top_level.analyzed_cfg();
  let assign = cfg
    .graph
    .labels_sorted()
    .into_iter()
    .flat_map(|label| cfg.bblocks.get(label).iter())
    .find(|inst| {
      inst.t == InstTyp::VarAssign
        && matches!(
          inst.args.get(0),
          Some(Arg::Const(Const::Str(s))) if s == "hello"
        )
    })
    .expect("expected to find VarAssign for \"hello\"");

  assert_eq!(assign.meta.numeric_repr, NumericRepr::Unknown);
}
