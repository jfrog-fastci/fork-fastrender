use crate::analysis;
use crate::analysis::find_conds::find_conds;
use crate::cfg::cfg::Cfg;
use crate::dom::{Dom, PostDom};
use crate::il::inst::{Arg, BinOp, InstTyp, UnOp};
use crate::opt::PassResult;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug)]
struct DefLoc {
  block: u32,
  inst_idx: usize,
}

fn build_def_locs(cfg: &Cfg) -> BTreeMap<u32, DefLoc> {
  let mut out = BTreeMap::<u32, DefLoc>::new();
  for label in cfg.graph.labels_sorted() {
    let Some(block) = cfg.bblocks.maybe_get(label) else {
      continue;
    };
    for (inst_idx, inst) in block.iter().enumerate() {
      for &tgt in &inst.tgts {
        // SSA-form CFGs should only define each var once. Keep the first
        // deterministically if we ever see duplicates.
        out.entry(tgt).or_insert(DefLoc { block: label, inst_idx });
      }
    }
  }
  out
}

fn resolve_copy_var(cfg: &Cfg, defs: &BTreeMap<u32, DefLoc>, mut var: u32) -> u32 {
  for _ in 0..8 {
    let Some(loc) = defs.get(&var).copied() else {
      break;
    };
    let inst = &cfg.bblocks.get(loc.block)[loc.inst_idx];
    match inst.t {
      InstTyp::VarAssign => {
        let (_tgt, arg) = inst.as_var_assign();
        let Some(src) = arg.maybe_var() else {
          break;
        };
        var = src;
      }
      InstTyp::Phi => {
        let Some(first) = inst.args.get(0).and_then(|a| a.maybe_var()) else {
          break;
        };
        if inst.args.iter().all(|a| a.maybe_var() == Some(first)) {
          var = first;
        } else {
          break;
        }
      }
      _ => break,
    }
  }
  var
}

fn build_len_to_array(cfg: &Cfg, defs: &BTreeMap<u32, DefLoc>) -> BTreeMap<u32, u32> {
  let mut out = BTreeMap::<u32, u32>::new();
  for label in cfg.graph.labels_sorted() {
    let Some(block) = cfg.bblocks.maybe_get(label) else {
      continue;
    };
    for inst in block.iter() {
      if inst.t != InstTyp::ArrayLen {
        continue;
      }
      let (tgt, array, _elem_layout) = inst.as_array_len();
      let Some(array_var) = array.maybe_var() else {
        continue;
      };
      let array_var = resolve_copy_var(cfg, defs, array_var);
      out.insert(tgt, array_var);
    }
  }
  out
}

fn build_block_conditions(
  cfg: &Cfg,
  dom: &Dom,
  postdom: &PostDom,
) -> BTreeMap<u32, Vec<(u32, bool)>> {
  let conds = find_conds(cfg, dom, postdom);

  // Map from block -> list of (cond_label, is_true) pairs that hold when control
  // reaches that block.
  let mut out: BTreeMap<u32, Vec<(u32, bool)>> = BTreeMap::new();

  let mut cond_labels: Vec<u32> = conds.keys().copied().collect();
  cond_labels.sort_unstable();

  for cond_label in cond_labels {
    let region = &conds[&cond_label];
    let mut then_nodes: Vec<u32> = region.then_nodes.iter().copied().collect();
    then_nodes.sort_unstable();
    for node in then_nodes {
      out.entry(node).or_default().push((cond_label, true));
    }

    let mut else_nodes: Vec<u32> = region.else_nodes.iter().copied().collect();
    else_nodes.sort_unstable();
    for node in else_nodes {
      out.entry(node).or_default().push((cond_label, false));
    }
  }

  for conds in out.values_mut() {
    conds.sort_unstable();
    conds.dedup();
  }

  out
}

fn resolve_cond_compare(
  cfg: &Cfg,
  defs: &BTreeMap<u32, DefLoc>,
  mut cond_var: u32,
) -> Option<(Arg, BinOp, Arg, bool)> {
  // Returns (left, op, right, negated).
  let mut negated = false;
  for _ in 0..8 {
    let loc = defs.get(&cond_var).copied()?;
    let inst = &cfg.bblocks.get(loc.block)[loc.inst_idx];
    match inst.t {
      InstTyp::VarAssign => {
        let (_tgt, arg) = inst.as_var_assign();
        cond_var = arg.maybe_var()?;
      }
      InstTyp::Un => {
        let (_tgt, op, arg) = inst.as_un();
        if op != UnOp::Not {
          return None;
        }
        cond_var = arg.maybe_var()?;
        negated = !negated;
      }
      InstTyp::Bin => {
        let (_tgt, left, op, right) = inst.as_bin();
        return Some((left.clone(), op, right.clone(), negated));
      }
      _ => return None,
    }
  }
  None
}

fn arg_as_i64_const(arg: &Arg) -> Option<i64> {
  match arg {
    Arg::Const(crate::il::inst::Const::Num(n)) => {
      let value = n.0;
      if value.is_finite()
        && value.fract() == 0.0
        && value >= i64::MIN as f64
        && value <= i64::MAX as f64
        && (value as i64) as f64 == value
      {
        Some(value as i64)
      } else {
        None
      }
    }
    _ => None,
  }
}

fn idx_is_non_negative(cfg: &Cfg, ranges: &analysis::range::RangeResult, label: u32, inst_idx: usize, idx: &Arg) -> bool {
  let Some(idx_var) = idx.maybe_var() else {
    // Const index.
    return arg_as_i64_const(idx).is_some_and(|v| v >= 0);
  };
  let state = ranges.state_before_inst(cfg, label, inst_idx);
  match state.range_of_var(idx_var) {
    analysis::range::IntRange::Interval { lo, .. } => match lo {
      analysis::range::Bound::I64(n) => n >= 0,
      _ => false,
    },
    _ => false,
  }
}

fn cond_proves_idx_lt_len(
  cfg: &Cfg,
  defs: &BTreeMap<u32, DefLoc>,
  len_to_array: &BTreeMap<u32, u32>,
  cond_label: u32,
  mut cond_is_true: bool,
  array_var: u32,
  idx_var: u32,
) -> bool {
  let Some(block) = cfg.bblocks.maybe_get(cond_label) else {
    return false;
  };
  let Some(term) = block.last() else {
    return false;
  };
  if term.t != InstTyp::CondGoto {
    return false;
  }
  let (cond, _then_label, _else_label) = term.as_cond_goto();
  let Some(cond_var) = cond.maybe_var() else {
    return false;
  };
  let Some((left, op, right, negated)) = resolve_cond_compare(cfg, defs, cond_var) else {
    return false;
  };
  if negated {
    cond_is_true = !cond_is_true;
  }

  // We only remove bounds checks when we can prove a *strict* upper bound.
  //
  // Accepted patterns:
  // - (idx < len) is true
  // - (idx >= len) is false  => idx < len
  // - (len > idx) is true    => idx < len
  // - (len <= idx) is false  => idx < len
  let array_var = resolve_copy_var(cfg, defs, array_var);
  let idx_var = resolve_copy_var(cfg, defs, idx_var);

  let match_idx_len = |idx_side: &Arg, len_side: &Arg| {
    let Some(idx_side) = idx_side.maybe_var() else {
      return false;
    };
    let Some(len_side) = len_side.maybe_var() else {
      return false;
    };
    let idx_side = resolve_copy_var(cfg, defs, idx_side);
    let len_side = resolve_copy_var(cfg, defs, len_side);
    if idx_side != idx_var {
      return false;
    }
    len_to_array
      .get(&len_side)
      .is_some_and(|&arr| resolve_copy_var(cfg, defs, arr) == array_var)
  };

  if cond_is_true && op == BinOp::Lt {
    return match_idx_len(&left, &right);
  }
  if !cond_is_true && op == BinOp::Geq {
    return match_idx_len(&left, &right);
  }
  if cond_is_true && op == BinOp::Gt {
    return match_idx_len(&right, &left);
  }
  if !cond_is_true && op == BinOp::Leq {
    return match_idx_len(&right, &left);
  }

  false
}

pub fn optpass_bounds_check_elim(cfg: &mut Cfg) -> PassResult {
  let mut result = PassResult::default();

  // Only do work when the CFG contains checked array accesses.
  let mut has_checked = false;
  for label in cfg.graph.labels_sorted() {
    let Some(block) = cfg.bblocks.maybe_get(label) else {
      continue;
    };
    if block.iter().any(|inst| {
      matches!(inst.t, InstTyp::ArrayLoad | InstTyp::ArrayStore) && inst.checked
    }) {
      has_checked = true;
      break;
    }
  }
  if !has_checked {
    return result;
  }

  let ranges = analysis::range::analyze_ranges(cfg);
  let dom = Dom::calculate(cfg);
  let postdom = PostDom::calculate(cfg);

  let defs = build_def_locs(cfg);
  let len_to_array = build_len_to_array(cfg, &defs);
  let block_conds = build_block_conditions(cfg, &dom, &postdom);

  let mut to_uncheck: BTreeSet<(u32, usize)> = BTreeSet::new();

  for label in cfg.graph.labels_sorted() {
    let Some(block) = cfg.bblocks.maybe_get(label) else {
      continue;
    };

    for (inst_idx, inst) in block.iter().enumerate() {
      let (array, idx) = match inst.t {
        InstTyp::ArrayLoad => {
          if !inst.checked {
            continue;
          }
          let (_tgt, array, idx, _elem_layout, _checked) = inst.as_array_load();
          (array, idx)
        }
        InstTyp::ArrayStore => {
          if !inst.checked {
            continue;
          }
          let (array, idx, _val, _elem_layout, _checked) = inst.as_array_store();
          (array, idx)
        }
        _ => continue,
      };

      let Some(array_var) = array.maybe_var() else {
        continue;
      };

      // Lower bound: prove `idx >= 0`.
      if !idx_is_non_negative(cfg, &ranges, label, inst_idx, idx) {
        continue;
      }

      // Upper bound: try to find a dominating condition that proves `idx < len`.
      let Some(idx_var) = idx.maybe_var() else {
        // Constant indices are handled via dedicated range proofs later (not yet).
        continue;
      };

      let mut proven = false;

      // 1) Dominating CondGoto conditions (if/loop headers).
      if let Some(conds) = block_conds.get(&label) {
        for &(cond_label, cond_is_true) in conds {
          if cond_proves_idx_lt_len(
            cfg,
            &defs,
            &len_to_array,
            cond_label,
            cond_is_true,
            array_var,
            idx_var,
          ) {
            proven = true;
            break;
          }
        }
      }

      if !proven {
        continue;
      }

      to_uncheck.insert((label, inst_idx));
    }
  }

  if to_uncheck.is_empty() {
    return result;
  }

  for (label, inst_idx) in to_uncheck.into_iter() {
    let inst = &mut cfg.bblocks.get_mut(label)[inst_idx];
    if matches!(inst.t, InstTyp::ArrayLoad | InstTyp::ArrayStore) && inst.checked {
      inst.checked = false;
      result.mark_changed();
    }
  }

  result
}

