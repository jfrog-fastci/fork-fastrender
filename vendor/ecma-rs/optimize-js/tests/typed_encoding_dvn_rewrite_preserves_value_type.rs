#![cfg(feature = "typed")]

#[path = "common/mod.rs"]
mod common;

use common::compile_source_typed;
use optimize_js::analysis::driver::annotate_program;
use optimize_js::cfg::cfg::Cfg;
use optimize_js::il::inst::{BinOp, Inst, InstTyp, StringEncoding, ValueTypeSummary};
use optimize_js::TopLevelMode;

fn any_inst(cfg: &Cfg, pred: impl Fn(&Inst) -> bool) -> bool {
  for label in cfg.graph.labels_sorted() {
    for inst in cfg.bblocks.get(label).iter() {
      if pred(inst) {
        return true;
      }
    }
  }
  false
}

fn count_numeric_adds(cfg: &Cfg) -> usize {
  let mut count = 0;
  for label in cfg.graph.labels_sorted() {
    for inst in cfg.bblocks.get(label).iter() {
      if inst.t != InstTyp::Bin {
        continue;
      }
      let (_tgt, _left, op, _right) = inst.as_bin();
      if op == BinOp::Add && inst.value_type == ValueTypeSummary::NUMBER {
        count += 1;
      }
    }
  }
  count
}

#[test]
fn typed_encoding_uses_value_type_after_dvn_rewrite() {
  let mut program = compile_source_typed(
    r#"
      let a: number = Math.random();
      let b: number = Math.random();
      let n = a + b;
      console.log(n);
      let s = "a" + (a + b);
      console.log(s);
    "#,
    TopLevelMode::Module,
    false,
  );

  let numeric_adds = {
    let ssa_cfg = program.top_level.analyzed_cfg();
    count_numeric_adds(ssa_cfg)
  };
  assert_eq!(
    numeric_adds, 1,
    "expected DVN to CSE the repeated numeric `a + b` expression"
  );

  annotate_program(&mut program);

  assert!(
    any_inst(&program.top_level.body, |inst| {
      if inst.t != InstTyp::Bin {
        return false;
      }
      let (_tgt, _left, op, _right) = inst.as_bin();
      op == BinOp::Add && inst.meta.result_type.string_encoding == Some(StringEncoding::Ascii)
    }),
    "expected string concatenation to be annotated as ASCII even when its numeric operand is DVN-rewritten"
  );
}

