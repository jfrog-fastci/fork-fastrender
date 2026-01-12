#![cfg(feature = "typed")]

#[path = "common/mod.rs"]
mod common;

use optimize_js::analysis::{encoding, nullability, range};
use optimize_js::cfg::cfg::Cfg;
use optimize_js::il::inst::{Arg, BinOp, Const, InstTyp, StringEncoding};
use optimize_js::TopLevelMode;
use parse_js::num::JsNumber;

fn find_bin<'a>(
  cfg: &'a Cfg,
  predicate: impl Fn(u32, &'a optimize_js::il::inst::Inst) -> bool,
) -> Option<(u32, &'a optimize_js::il::inst::Inst)> {
  let mut labels: Vec<u32> = cfg.bblocks.all().map(|(label, _)| label).collect();
  labels.sort_unstable();
  for label in labels {
    for inst in cfg.bblocks.get(label).iter() {
      if predicate(label, inst) {
        return Some((label, inst));
      }
    }
  }
  None
}

#[test]
fn typed_range_narrows_number_on_lt_true_edge() {
  let src = r#"
    function foo(): number {
      return Math.round(Math.random() * 100);
    }

    let x: number = foo();
    if (x + 1 < 10) {
      // Keep `x` live.
      console.log(x);
    }
  "#;

  let program = common::compile_source_typed(src, TopLevelMode::Module, false);
  let cfg = &program.top_level.body;

  let (cond_block, lt_inst) = find_bin(cfg, |_, inst| {
    if inst.t != InstTyp::Bin || inst.bin_op != BinOp::Lt {
      return false;
    }
    matches!(inst.args.as_slice(), [Arg::Var(_), Arg::Const(Const::Num(JsNumber(10.0)))])
  })
  .expect("expected `< 10` compare in CFG");

  let (cond_tgt, left, _, _) = lt_inst.as_bin();
  let Arg::Var(add_tgt) = left else {
    panic!("expected var lhs for `< 10` compare");
  };
  let add_tgt = *add_tgt;

  let x_var = cfg.bblocks.get(cond_block)
    .iter()
    .find_map(|inst| {
      if inst.t != InstTyp::Bin || inst.bin_op != BinOp::Add {
        return None;
      }
      if inst.tgts.first().copied() != Some(add_tgt) {
        return None;
      }
      match inst.args.as_slice() {
        [Arg::Var(x), Arg::Const(Const::Num(JsNumber(1.0)))]
        | [Arg::Const(Const::Num(JsNumber(1.0))), Arg::Var(x)] => Some(*x),
        _ => None,
      }
    })
    .expect("expected `tmp = x + 1` defining the compare lhs");

  let (then_label, _) = cfg.bblocks.get(cond_block)
    .iter()
    .rev()
    .find(|inst| inst.t == InstTyp::CondGoto && inst.args.get(0) == Some(&Arg::Var(cond_tgt)))
    .map(|inst| {
      let (_cond, t, f) = inst.as_cond_goto();
      (t, f)
    })
    .expect("expected conditional branch on compare result");

  let result = range::analyze_ranges(cfg);
  let narrowed = result
    .var_at_entry(then_label, x_var)
    .expect("then block should have entry state");

  match narrowed {
    range::IntRange::Bottom => panic!("expected reachable range, got Bottom"),
    range::IntRange::Unknown => panic!("expected upper bound <= 8, got ⊤ ({narrowed:?})"),
    range::IntRange::Interval { hi, .. } => match hi {
      range::Bound::I64(v) => assert!(
        v <= 8,
        "expected upper bound <= 8 for `x + 1 < 10`, got {v} ({narrowed:?})"
      ),
      range::Bound::PosInf => panic!("expected upper bound <= 8, got +inf ({narrowed:?})"),
      range::Bound::NegInf => panic!("unexpected upper bound -inf ({narrowed:?})"),
    },
  }
}

#[test]
fn typed_encoding_propagates_through_string_concat_with_number_operand() {
  let src = r#"
    function foo(): number {
      return Math.round(Math.random() * 100);
    }

    let n: number = foo();
    let s: string = "ENC" + n;
    console.log(s);
  "#;

  let program = common::compile_source_typed(src, TopLevelMode::Module, false);
  let cfg = &program.top_level.body;

  let (label, concat_inst) = find_bin(cfg, |_, inst| {
    if inst.t != InstTyp::StringConcat {
      return false;
    }
    matches!(
      inst.args.as_slice(),
      [Arg::Const(Const::Str(s)), Arg::Var(_)] if s == "ENC"
    )
  })
  .expect("expected StringConcat lowering for `\"ENC\" + n`");

  let (tgt, _parts) = concat_inst.as_string_concat();
  let enc = encoding::analyze_cfg_encoding(cfg).encoding_at_exit(label, tgt);
  assert_eq!(enc, StringEncoding::Ascii);
}

#[test]
fn typed_nullability_marks_string_temps_non_nullish() {
  let src = r#"
    function foo(): string {
      return Math.random() > 0.5 ? "a" : "b";
    }

    let x: string = foo();
    if (x.length > 0) {
      console.log(x);
    }
  "#;

  let program = common::compile_source_typed(src, TopLevelMode::Module, false);
  let cfg = &program.top_level.body;

  let (_label, getprop) = find_bin(cfg, |_, inst| {
    if inst.t != InstTyp::Bin || inst.bin_op != BinOp::GetProp {
      return false;
    }
    matches!(
      inst.args.as_slice(),
      [Arg::Var(_), Arg::Const(Const::Str(prop))] if prop == "length"
    )
  })
  .expect("expected `x.length` property access");

  let (_tgt, left, _op, _right) = getprop.as_bin();
  let Arg::Var(x_var) = left else {
    panic!("expected var receiver for `x.length`");
  };

  let nulls = nullability::calculate_nullability(cfg);
  let entry = cfg.entry;
  let succ = cfg
    .graph
    .children_sorted(entry)
    .into_iter()
    .next()
    .expect("expected at least one successor edge");

  let state = nulls
    .edge_state(entry, succ)
    .expect("expected nullability edge state");
  assert!(
    state.mask_of_var(*x_var).is_non_nullish(),
    "expected `x` to be non-nullish after initialization, got {:?}",
    state.mask_of_var(*x_var)
  );
}
