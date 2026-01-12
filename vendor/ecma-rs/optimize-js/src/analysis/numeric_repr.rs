use crate::analysis::range;
use crate::analysis::value_types::ValueTypeSummaries;
use crate::cfg::cfg::Cfg;
use crate::il::meta::NumericRepr;

/// Per-variable numeric representation inference results.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct NumericReprResult {
  reprs: Vec<NumericRepr>,
}

impl NumericReprResult {
  pub fn repr_of_var(&self, var: u32) -> NumericRepr {
    self
      .reprs
      .get(var as usize)
      .copied()
      .unwrap_or(NumericRepr::Unknown)
  }
}

fn cfg_var_count(cfg: &Cfg) -> usize {
  let mut max: Option<u32> = None;
  for (_, block) in cfg.bblocks.all() {
    for inst in block.iter() {
      for &tgt in inst.tgts.iter() {
        max = Some(max.map_or(tgt, |m| m.max(tgt)));
      }
      for arg in inst.args.iter() {
        if let crate::il::inst::Arg::Var(v) = arg {
          max = Some(max.map_or(*v, |m| m.max(*v)));
        }
      }
    }
  }
  max.map(|m| m as usize + 1).unwrap_or(0)
}

fn repr_from_range(r: range::IntRange) -> Option<NumericRepr> {
  let range::IntRange::Interval { lo, hi } = r else {
    return None;
  };
  let (range::Bound::I64(lo), range::Bound::I64(hi)) = (lo, hi) else {
    return None;
  };

  if lo >= i32::MIN as i64 && hi <= i32::MAX as i64 {
    return Some(NumericRepr::I32);
  }

  Some(NumericRepr::I64)
}

/// Infer numeric representations for SSA values in `cfg`.
///
/// The analysis is intentionally conservative:
/// - Only values proven to be numbers (via `ValueTypeSummary`) participate.
/// - Only values with a bounded integer range are classified as `I32`/`I64`.
/// - Other numeric values fall back to `F64`.
pub fn analyze_cfg_numeric_repr(cfg: &Cfg, ranges: &range::RangeResult) -> NumericReprResult {
  let var_count = cfg_var_count(cfg);
  let mut reprs = vec![NumericRepr::Unknown; var_count];
  let types = ValueTypeSummaries::new(cfg);

  for label in cfg.graph.labels_sorted() {
    let exit_state = ranges.exit(label);
    let block = cfg.bblocks.get(label);
    for inst in block.iter() {
      let Some(&tgt) = inst.tgts.get(0) else {
        continue;
      };

      // Representation inference is intended for typed pipelines; skip values
      // that do not carry a `type_id` from lowering/typechecking.
      if inst.meta.type_id.is_none() {
        continue;
      }

      // Only attempt to classify values that are statically known to be numbers.
      let is_number = types
        .var(tgt)
        .is_some_and(|ty| ty.is_definitely_number());
      if !is_number {
        continue;
      }

      // Default numeric representation is `F64` unless proven integral.
      let mut repr = NumericRepr::F64;

      if let Some(state) = exit_state {
        let r = state.range_of_var(tgt);
        if let Some(int_repr) = repr_from_range(r) {
          repr = int_repr;
        }
      }

      if let Some(slot) = reprs.get_mut(tgt as usize) {
        *slot = repr;
      }
    }
  }

  NumericReprResult { reprs }
}

/// Attach [`NumericRepr`] decisions to `Inst.meta.numeric_repr` for each value-defining instruction.
pub fn annotate_cfg_numeric_repr(cfg: &mut Cfg, result: &NumericReprResult) {
  for label in cfg.graph.labels_sorted() {
    let block = cfg.bblocks.get_mut(label);
    for inst in block.iter_mut() {
      let Some(&tgt) = inst.tgts.get(0) else {
        continue;
      };
      let repr = result.repr_of_var(tgt);
      if matches!(repr, NumericRepr::Unknown) {
        continue;
      }
      inst.meta.numeric_repr = repr;
    }
  }
}
