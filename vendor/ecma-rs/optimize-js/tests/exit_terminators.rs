#[path = "common/mod.rs"]
mod common;

use common::compile_source;
use optimize_js::cfg::cfg::Cfg;
use optimize_js::il::inst::{Arg, Const, InstTyp};
use optimize_js::{compile_source as compile_source_result, TopLevelMode};
use parse_js::num::JsNumber;
use std::collections::BTreeMap;

fn all_insts(cfg: &Cfg) -> Vec<&optimize_js::il::inst::Inst> {
  let mut labels: Vec<_> = cfg.bblocks.all().map(|(label, _)| label).collect();
  labels.sort_unstable();
  labels
    .into_iter()
    .flat_map(|label| cfg.bblocks.get(label).iter())
    .collect()
}

#[test]
fn function_return_is_lowered_to_return_inst_with_value() {
  let src = r#"
    const make = () => {
      return 1;
    };
    make();
  "#;
  let program = compile_source(src, TopLevelMode::Module, false);
  assert_eq!(program.functions.len(), 1, "expected nested function to be compiled");

  let func = &program.functions[0].body;
  let insts = all_insts(func);

  let mut saw_return = false;
  let mut saw_return_1 = false;
  for inst in insts.iter() {
    if inst.t != InstTyp::Return {
      continue;
    }
    saw_return = true;
    let Some(value) = inst.as_return() else {
      continue;
    };
    match value {
      Arg::Const(Const::Num(n)) if *n == JsNumber(1.0) => {
        saw_return_1 = true;
      }
      Arg::Var(var) => {
        // Some lowering pipelines may materialize the return value into a temporary.
        let has_assign = insts.iter().any(|i| {
          i.t == InstTyp::VarAssign
            && i.tgts.get(0) == Some(var)
            && i.args.get(0) == Some(&Arg::Const(Const::Num(JsNumber(1.0))))
        });
        saw_return_1 |= has_assign;
      }
      _ => {}
    }
  }

  assert!(saw_return, "expected at least one Return terminator in nested function");
  assert!(
    saw_return_1,
    "expected Return terminator to preserve `1` as the return value"
  );
}

#[test]
fn function_throw_is_lowered_to_throw_inst() {
  let src = r#"
    const fail = () => {
      throw err;
    };
    fail();
  "#;
  let program = compile_source(src, TopLevelMode::Module, false);
  assert_eq!(program.functions.len(), 1, "expected nested function to be compiled");

  let func = &program.functions[0].body;
  let insts = all_insts(func);

  let throw_inst = insts
    .iter()
    .find(|inst| inst.t == InstTyp::Throw)
    .expect("expected Throw terminator in nested function");

  match throw_inst.as_throw() {
    Arg::Var(var) => {
      assert!(
        insts.iter().any(|inst| {
          inst.t == InstTyp::UnknownLoad && inst.tgts.get(0) == Some(var) && inst.unknown == "err"
        }),
        "expected thrown value to come from an UnknownLoad of `err`"
      );
    }
    other => panic!("expected thrown value to be a variable, got {other:?}"),
  }
}

#[test]
fn top_level_return_is_rejected() {
  let err = compile_source_result("return 1;", TopLevelMode::Module, false)
    .expect_err("return statement outside function should be rejected");
  assert!(
    err.iter().any(|diag| diag.code == "OPT0002"),
    "expected OPT0002 diagnostic, got {err:?}"
  );
}

fn serialize_cfg(cfg: &Cfg) -> String {
  let mut bblocks = BTreeMap::new();
  for (label, insts) in cfg.bblocks.all() {
    bblocks.insert(
      label,
      insts
        .iter()
        .map(|inst| format!("{inst:?}"))
        .collect::<Vec<_>>(),
    );
  }

  let mut edges = BTreeMap::new();
  for label in cfg.graph.labels_sorted() {
    edges.insert(label, cfg.graph.children_sorted(label));
  }

  format!(
    "entry={}\nbblocks={bblocks:?}\nedges={edges:?}\n",
    cfg.entry
  )
}

#[test]
fn return_and_throw_cfg_is_deterministic() {
  let src = r#"
    const make = () => { return 1; };
    const fail = () => { throw err; };
    make();
    fail();
  "#;

  let first = compile_source(src, TopLevelMode::Module, false);
  let second = compile_source(src, TopLevelMode::Module, false);

  let first_cfgs: Vec<_> = first
    .functions
    .iter()
    .map(|f| serialize_cfg(&f.body))
    .collect();
  let second_cfgs: Vec<_> = second
    .functions
    .iter()
    .map(|f| serialize_cfg(&f.body))
    .collect();

  assert_eq!(first_cfgs, second_cfgs, "expected deterministic function CFGs");
}
