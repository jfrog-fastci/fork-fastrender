#![cfg(feature = "typed")]

#[path = "common/mod.rs"]
mod common;

use common::compile_source_typed;
use optimize_js::cfg::cfg::Cfg;
use optimize_js::il::inst::{Arg, BinOp, Const, Inst, InstTyp};
use optimize_js::TopLevelMode;

fn collect_insts(cfg: &Cfg) -> Vec<Inst> {
  cfg
    .graph
    .labels_sorted()
    .into_iter()
    .flat_map(|label| cfg.bblocks.get(label).iter().cloned())
    .collect()
}

fn is_canonical_u32_string(s: &str) -> bool {
  let Ok(idx) = s.parse::<u32>() else {
    return false;
  };
  idx != u32::MAX && idx.to_string() == s
}

fn is_numeric_prop(arg: &Arg) -> bool {
  match arg {
    Arg::Const(Const::Num(n)) => {
      let value = n.0;
      value.is_finite()
        && value.fract() == 0.0
        && value >= 0.0
        && value < (u32::MAX as f64)
        && (value as u32) as f64 == value
    }
    Arg::Const(Const::Str(s)) => is_canonical_u32_string(s),
    _ => false,
  }
}

#[test]
fn typed_arrays_lower_to_first_class_array_insts() {
  let program = compile_source_typed(
    r#"
      let a: number[] = [1, 2, 3];
      let i: number = 1;
      let len1 = a.length;
      let len2 = a["length"];
      let x = a[i];
      a[i] = x + 1;
      console.log(len1, len2, x, a[i]);
    "#,
    TopLevelMode::Module,
    false,
  );

  let insts = collect_insts(program.top_level.analyzed_cfg());

  assert!(
    insts.iter().any(|inst| inst.t == InstTyp::ArrayLen),
    "expected ArrayLen"
  );
  assert!(
    insts.iter().any(|inst| inst.t == InstTyp::ArrayLoad),
    "expected ArrayLoad"
  );
  assert!(
    insts.iter().any(|inst| inst.t == InstTyp::ArrayStore),
    "expected ArrayStore"
  );

  for inst in &insts {
    if inst.t == InstTyp::Bin && inst.bin_op == BinOp::GetProp {
      let (_tgt, _obj, _op, prop) = inst.as_bin();
      assert!(
        !matches!(prop, Arg::Const(Const::Str(s)) if s == "length"),
        "expected `a.length` to lower to ArrayLen, not GetProp"
      );
      assert!(
        !is_numeric_prop(prop),
        "expected `a[i]` to lower to ArrayLoad, not GetProp"
      );
    }
    if inst.t == InstTyp::PropAssign {
      let (_obj, prop, _value) = inst.as_prop_assign();
      assert!(
        !is_numeric_prop(prop),
        "expected `a[i] = v` to lower to ArrayStore, not PropAssign"
      );
    }
  }
}

