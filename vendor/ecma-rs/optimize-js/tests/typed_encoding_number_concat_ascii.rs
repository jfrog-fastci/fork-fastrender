#![cfg(feature = "typed")]

#[path = "common/mod.rs"]
mod common;

use common::compile_source_typed;
use optimize_js::analysis::driver::annotate_program;
use optimize_js::cfg::cfg::Cfg;
use optimize_js::il::inst::{BinOp, Inst, InstTyp, StringEncoding};
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

#[test]
fn typed_encoding_number_concat_ascii() {
  let mut program = compile_source_typed(
    r#"
      let n: number = Math.random();
      let s = "a" + n;
      console.log(s);
    "#,
    TopLevelMode::Module,
    false,
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
    "expected string concatenation (`+`) with a typed number RHS to be annotated as ASCII"
  );
}
