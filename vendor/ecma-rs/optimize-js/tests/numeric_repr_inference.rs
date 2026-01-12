#![cfg(feature = "typed")]

use optimize_js::analysis::annotate_program;
use optimize_js::cfg::cfg::Cfg;
use optimize_js::il::inst::{Arg, BinOp, Const, InstTyp, NumericRepr};
use optimize_js::{compile_source_typed_cfg_options, CompileCfgOptions, TopLevelMode};
use parse_js::num::JsNumber;

fn compile_typed_no_opt(source: &str) -> optimize_js::Program {
  compile_source_typed_cfg_options(
    source,
    TopLevelMode::Module,
    false,
    CompileCfgOptions {
      keep_ssa: true,
      run_opt_passes: false,
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

#[test]
fn numeric_repr_infers_i32_for_phi_merge() {
  let mut program = compile_typed_no_opt(
    r#"
      let x: 1 | 2;
      if (Math.random() < 0.5) {
        x = 1;
      } else {
        x = 2;
      }
      console.log(x);
    "#,
  );
  annotate_program(&mut program);

  let cfg = program.top_level.analyzed_cfg();

  fn find_def_inst<'a>(cfg: &'a Cfg, var: u32) -> Option<&'a optimize_js::il::inst::Inst> {
    cfg
      .graph
      .labels_sorted()
      .into_iter()
      .flat_map(|label| cfg.bblocks.get(label).iter())
      .find(|inst| inst.tgts.iter().any(|tgt| *tgt == var))
  }

  fn is_num_const(arg: &Arg, n: f64) -> bool {
    matches!(arg, Arg::Const(Const::Num(JsNumber(v))) if *v == n)
  }

  let phi = cfg
    .graph
    .labels_sorted()
    .into_iter()
    .flat_map(|label| cfg.bblocks.get(label).iter())
    .find(|inst| {
      if inst.t != InstTyp::Phi || inst.args.len() != 2 {
        return false;
      }

      let mut has_one = false;
      let mut has_two = false;
      for arg in inst.args.iter() {
        match arg {
          Arg::Const(_) => {
            has_one |= is_num_const(arg, 1.0);
            has_two |= is_num_const(arg, 2.0);
          }
          Arg::Var(v) => {
            let Some(def) = find_def_inst(cfg, *v) else {
              continue;
            };
            if def.t != InstTyp::VarAssign {
              continue;
            }
            let Some(arg0) = def.args.get(0) else {
              continue;
            };
            has_one |= is_num_const(arg0, 1.0);
            has_two |= is_num_const(arg0, 2.0);
          }
          _ => {}
        }
      }
      has_one && has_two
    })
    .expect("expected to find Phi merging 1 and 2");

  assert_eq!(phi.meta.numeric_repr, NumericRepr::I32);
}

#[test]
fn numeric_repr_does_not_require_type_id_metadata() {
  use optimize_js::analysis::{numeric_repr, range};
  use optimize_js::il::inst::Inst;
  use ahash::HashMap;

  let mut bblocks = HashMap::default();
  bblocks.insert(
    0,
    vec![Inst::var_assign(
      0,
      Arg::Const(Const::Num(JsNumber(7.0))),
    )],
  );
  let cfg = Cfg::from_bblocks(bblocks, vec![0]);

  let range_result = range::analyze_ranges(&cfg);
  let numeric_repr_result = numeric_repr::analyze_cfg_numeric_repr(&cfg, &range_result);
  assert_eq!(numeric_repr_result.repr_of_var(0), NumericRepr::I32);

  let mut annotated_cfg = cfg.clone();
  numeric_repr::annotate_cfg_numeric_repr(&mut annotated_cfg, &numeric_repr_result);
  let inst = annotated_cfg
    .bblocks
    .get(0)
    .iter()
    .find(|inst| inst.t == InstTyp::VarAssign)
    .expect("expected VarAssign in synthetic CFG");
  assert!(inst.meta.type_id.is_none(), "expected no type_id metadata on synthetic inst");
  assert_eq!(inst.meta.numeric_repr, NumericRepr::I32);
}
