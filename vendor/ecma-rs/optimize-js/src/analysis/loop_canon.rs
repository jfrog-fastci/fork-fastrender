use crate::analysis::loop_info::LoopInfo;
use crate::cfg::cfg::{Cfg, Terminator};
use crate::dom::Dom;
use crate::il::inst::{Arg, BinOp, Const, InstTyp};
use ahash::{HashMap, HashMapExt};
use parse_js::num::JsNumber;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopPhi {
  pub tgt: u32,
  pub preheader_arg: Arg,
  pub latch_arg: Arg,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CountedLoop {
  /// All CFG block labels that belong to the natural loop (including header/latch).
  ///
  /// This list is sorted so downstream passes can traverse deterministically.
  pub nodes: Vec<u32>,
  pub header: u32,
  pub latch: u32,
  pub preheader: u32,
  pub body: u32,
  pub exit: u32,
  /// Phi nodes in the header that only merge values from {preheader, latch}.
  ///
  /// This list is sorted by `tgt` so downstream passes can process deterministically.
  pub header_phis: Vec<LoopPhi>,
  /// Header phi target for the canonical induction variable.
  pub indvar: u32,
  /// Initial value flowing into `indvar` from the preheader.
  pub indvar_init: Arg,
  /// SSA variable flowing into `indvar` from the latch.
  pub indvar_latch_var: u32,
  /// Compare op used in the loop header condition.
  pub cmp_op: BinOp,
  /// Invariant loop bound used in the header compare.
  pub bound: Arg,
  /// Compile-time trip count when it can be proven.
  pub trip_count: Option<u64>,
}

fn cfg_defs(cfg: &Cfg) -> HashMap<u32, (u32, usize)> {
  let mut defs = HashMap::<u32, (u32, usize)>::new();
  for (label, block) in cfg.bblocks.all() {
    for (idx, inst) in block.iter().enumerate() {
      for &tgt in &inst.tgts {
        defs.insert(tgt, (label, idx));
      }
    }
  }
  defs
}

fn maybe_i64_from_const(c: &Const) -> Option<i64> {
  let Const::Num(JsNumber(n)) = c else {
    return None;
  };
  if !n.is_finite() || n.trunc() != *n {
    return None;
  }
  if *n < i64::MIN as f64 || *n > i64::MAX as f64 {
    return None;
  }
  let as_i64 = *n as i64;
  if as_i64 as f64 != *n {
    return None;
  }
  Some(as_i64)
}

fn maybe_i64_from_arg(cfg: &Cfg, defs: &HashMap<u32, (u32, usize)>, arg: &Arg) -> Option<i64> {
  let mut cur = arg.clone();
  for _ in 0..8 {
    match cur {
      Arg::Const(c) => return maybe_i64_from_const(&c),
      Arg::Var(v) => {
        let Some(&(label, idx)) = defs.get(&v) else {
          return None;
        };
        let inst = &cfg.bblocks.get(label)[idx];
        if inst.t != InstTyp::VarAssign {
          return None;
        }
        cur = inst.args[0].clone();
      }
      _ => return None,
    }
  }
  None
}

fn is_latch_add_one(
  cfg: &Cfg,
  defs: &HashMap<u32, (u32, usize)>,
  latch: u32,
  tgt: u32,
  ind: u32,
) -> bool {
  let Some(&(label, idx)) = defs.get(&tgt) else {
    return false;
  };
  if label != latch {
    return false;
  }
  let inst = &cfg.bblocks.get(label)[idx];
  if inst.t != InstTyp::Bin || inst.bin_op != BinOp::Add {
    return false;
  }
  let left = &inst.args[0];
  let right = &inst.args[1];
  let one = Arg::Const(Const::Num(JsNumber(1.0)));
  (left == &Arg::Var(ind) && right == &one) || (left == &one && right == &Arg::Var(ind))
}

fn compute_trip_count(init: i64, bound: i64, op: BinOp) -> Option<u64> {
  let tc_i128: i128 = match op {
    BinOp::Lt => {
      if init >= bound {
        0
      } else {
        (bound as i128) - (init as i128)
      }
    }
    BinOp::Leq => {
      if init > bound {
        0
      } else {
        (bound as i128) - (init as i128) + 1
      }
    }
    _ => return None,
  };
  if tc_i128 < 0 || tc_i128 > (u64::MAX as i128) {
    return None;
  }
  Some(tc_i128 as u64)
}

/// Find simple counted loops in SSA form.
///
/// This only recognizes loops that already have a preheader and a single latch. The result is
/// intended for downstream optimization passes (strength reduction, unrolling, LICM).
pub fn find_counted_loops(cfg: &Cfg, dom: &Dom) -> Vec<CountedLoop> {
  let loop_info = LoopInfo::compute(cfg, dom);
  let defs = cfg_defs(cfg);

  let mut loops = loop_info.loops.iter().collect::<Vec<_>>();
  loops.sort_by_key(|l| l.header);

  let mut out = Vec::<CountedLoop>::new();

  for l in loops {
    if l.latches.len() != 1 {
      continue;
    }
    let header = l.header;
    let latch = l.latches[0];

    // Preheader is the unique predecessor of the header that is outside the loop.
    let header_parents = cfg.graph.parents_sorted(header);
    let mut preheader_candidates = header_parents
      .iter()
      .copied()
      .filter(|p| !l.nodes.contains(p))
      .collect::<Vec<_>>();
    preheader_candidates.sort_unstable();
    preheader_candidates.dedup();
    let [preheader] = preheader_candidates.as_slice() else {
      continue;
    };
    let preheader = *preheader;

    // Ensure the header only has {preheader, latch} predecessors.
    let mut expected_preds = vec![preheader, latch];
    expected_preds.sort_unstable();
    let mut actual_preds = header_parents.clone();
    actual_preds.sort_unstable();
    actual_preds.dedup();
    if actual_preds != expected_preds {
      continue;
    }

    // Ensure latch is a simple backedge.
    if cfg.terminator(latch) != Terminator::Goto(header) {
      continue;
    }

    // Ensure preheader flows exclusively into the header.
    if cfg.graph.children_sorted(preheader) != vec![header] {
      continue;
    }

    // Identify body/exit successors from the header conditional.
    let Terminator::CondGoto { cond: _, t, f } = cfg.terminator(header) else {
      continue;
    };
    let (body, exit) = match (l.nodes.contains(&t), l.nodes.contains(&f)) {
      (true, false) => (t, f),
      (false, true) => (f, t),
      _ => continue,
    };

    // Parse header phi nodes (SSA requirement).
    let header_block = cfg.bblocks.get(header);
    let mut header_phis = Vec::<LoopPhi>::new();
    let mut phi_count = 0usize;
    let mut valid_phis = true;
    for inst in header_block.iter() {
      if inst.t != InstTyp::Phi {
        break;
      }
      phi_count += 1;
      if inst.labels.len() != 2 || inst.args.len() != 2 {
        valid_phis = false;
        break;
      }
      let mut pre_arg: Option<Arg> = None;
      let mut latch_arg: Option<Arg> = None;
      for (&lbl, arg) in inst.labels.iter().zip(inst.args.iter()) {
        if lbl == preheader {
          pre_arg = Some(arg.clone());
        } else if lbl == latch {
          latch_arg = Some(arg.clone());
        }
      }
      let (Some(pre_arg), Some(latch_arg)) = (pre_arg, latch_arg) else {
        valid_phis = false;
        break;
      };
      header_phis.push(LoopPhi {
        tgt: inst.tgts[0],
        preheader_arg: pre_arg,
        latch_arg,
      });
    }
    // Downstream passes assume that *all* header phi nodes are canonical and represented in
    // `header_phis`.
    if !valid_phis || header_phis.len() != phi_count || header_phis.is_empty() {
      continue;
    }
    header_phis.sort_by_key(|phi| phi.tgt);

    // Identify induction variable phi.
    let mut indvar: Option<(u32, Arg, u32)> = None;
    for phi in &header_phis {
      let Arg::Var(latch_var) = phi.latch_arg else {
        continue;
      };
      if !is_latch_add_one(cfg, &defs, latch, latch_var, phi.tgt) {
        continue;
      }
      if indvar.is_some() {
        // Multiple candidates; skip.
        indvar = None;
        break;
      }
      indvar = Some((phi.tgt, phi.preheader_arg.clone(), latch_var));
    }
    let Some((indvar_tgt, indvar_init, indvar_latch_var)) = indvar else {
      continue;
    };

    // Match the loop header compare that feeds the CondGoto.
    let Some(last) = header_block.last() else {
      continue;
    };
    if last.t != InstTyp::CondGoto {
      continue;
    }
    let Some(cond_var) = last.args[0].maybe_var() else {
      continue;
    };
    let mut cmp_op: Option<BinOp> = None;
    let mut bound: Option<Arg> = None;
    for inst in header_block.iter().rev() {
      if inst.tgts.get(0).copied() != Some(cond_var) {
        continue;
      }
      if inst.t != InstTyp::Bin {
        break;
      }
      let (_tgt, left, op, right) = inst.as_bin();
      if !matches!(op, BinOp::Lt | BinOp::Leq) {
        break;
      }
      if left != &Arg::Var(indvar_tgt) {
        break;
      }
      // Bound must be invariant: either not defined in the CFG (entry var) or defined outside the loop.
      if let Some(v) = right.maybe_var() {
        if let Some(&(def_label, _)) = defs.get(&v) {
          if l.nodes.contains(&def_label) {
            break;
          }
        }
      }
      cmp_op = Some(op);
      bound = Some(right.clone());
      break;
    }
    let (Some(cmp_op), Some(bound)) = (cmp_op, bound) else {
      continue;
    };

    let trip_count = match (
      maybe_i64_from_arg(cfg, &defs, &indvar_init),
      maybe_i64_from_arg(cfg, &defs, &bound),
    ) {
      (Some(init), Some(bound)) => compute_trip_count(init, bound, cmp_op),
      _ => None,
    };

    out.push(CountedLoop {
      nodes: {
        let mut nodes = l.nodes.iter().copied().collect::<Vec<_>>();
        nodes.sort_unstable();
        nodes
      },
      header,
      latch,
      preheader,
      body,
      exit,
      header_phis,
      indvar: indvar_tgt,
      indvar_init,
      indvar_latch_var,
      cmp_op,
      bound,
      trip_count,
    });
  }

  out
}
