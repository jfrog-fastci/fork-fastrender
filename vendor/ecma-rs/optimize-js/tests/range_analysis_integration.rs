#[path = "common/mod.rs"]
mod common;

use common::compile_source;
use optimize_js::analysis::range::{analyze_ranges, Bound, IntRange};
use optimize_js::il::inst::{Arg, BinOp, Const, InstTyp};
use optimize_js::TopLevelMode;
use parse_js::num::JsNumber;

fn find_cond_lt_const_10(
  cfg: &optimize_js::cfg::cfg::Cfg,
) -> Option<(u32 /* x temp */, u32 /* then label */, u32 /* else label */)> {
  for (_label, block) in cfg.bblocks.all() {
    // Scan for a conditional branch.
    for inst in block.iter() {
      if inst.t != InstTyp::CondGoto {
        continue;
      }
      let (cond, then_label, else_label) = inst.as_cond_goto();
      let Arg::Var(cond_var) = cond else {
        continue;
      };

      // Find the defining `tmp = x < 10` in the same block.
      for def in block.iter() {
        if def.t != InstTyp::Bin {
          continue;
        }
        if def.tgts.first() != Some(cond_var) {
          continue;
        }

        let (_tgt, left, op, right) = def.as_bin();
        if op != BinOp::Lt {
          continue;
        }

        if !matches!(right, Arg::Const(Const::Num(JsNumber(n))) if *n == 10.0) {
          continue;
        }

        let Arg::Var(x_temp) = left else {
          continue;
        };
        return Some((*x_temp, then_label, else_label));
      }
    }
  }
  None
}

fn assert_hi_leq(range: IntRange, max: i64) {
  match range {
    IntRange::Bottom => panic!("expected reachable range, got Bottom"),
    IntRange::Range { hi, .. } => match hi {
      Bound::Finite(v) => assert!(
        v <= max,
        "expected upper bound <= {max}, got {v} ({range:?})"
      ),
      Bound::PosInf => panic!("expected upper bound <= {max}, got +inf ({range:?})"),
      Bound::NegInf => panic!("unexpected upper bound -inf ({range:?})"),
    },
  }
}

fn assert_lo_geq(range: IntRange, min: i64) {
  match range {
    IntRange::Bottom => panic!("expected reachable range, got Bottom"),
    IntRange::Range { lo, .. } => match lo {
      Bound::Finite(v) => assert!(
        v >= min,
        "expected lower bound >= {min}, got {v} ({range:?})"
      ),
      Bound::NegInf => panic!("expected lower bound >= {min}, got -inf ({range:?})"),
      Bound::PosInf => panic!("unexpected lower bound +inf ({range:?})"),
    },
  }
}

#[test]
fn range_analysis_matches_real_lowering_lt() {
  let program = compile_source(
    r#"
      let x = foo;
      if (x < 10) {
        bar(x);
      } else {
        baz(x);
      }
    "#,
    TopLevelMode::Module,
    false,
  );

  let cfg = &program.top_level.body;
  let (x_temp, then_label, else_label) =
    find_cond_lt_const_10(cfg).expect("expected `tmp = x < 10; CondGoto tmp` pattern");

  let ranges = analyze_ranges(cfg);

  let then_range = ranges
    .var_at_entry(then_label, x_temp)
    .expect("range results should contain then block entry");
  let else_range = ranges
    .var_at_entry(else_label, x_temp)
    .expect("range results should contain else block entry");

  // Tolerant checks: only assert the bound that should be narrowed by the branch.
  assert_hi_leq(then_range, 9);
  assert_lo_geq(else_range, 10);
}

