use crate::analysis::dataflow::BlockState;
use crate::analysis::dataflow_edge::{ForwardEdgeDataFlowAnalysis, ForwardEdgeDataFlowResult};
use crate::cfg::cfg::Cfg;
use crate::dom::Dom;
use crate::il::inst::{Arg, BinOp, Const, Inst, InstTyp, UnOp};
use ahash::{HashMap, HashSet};
use num_traits::ToPrimitive;
use parse_js::num::JsNumber;
use std::cmp::Ordering;
 
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub enum Bound {
  NegInf,
  Finite(i64),
  PosInf,
}
 
impl Bound {
  fn min(self, other: Self) -> Self {
    if self <= other { self } else { other }
  }
 
  fn max(self, other: Self) -> Self {
    if self >= other { self } else { other }
  }
}
 
impl PartialOrd for Bound {
  fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
    Some(self.cmp(other))
  }
}
 
impl Ord for Bound {
  fn cmp(&self, other: &Self) -> Ordering {
    use Bound::*;
    match (self, other) {
      (NegInf, NegInf) => Ordering::Equal,
      (NegInf, _) => Ordering::Less,
      (_, NegInf) => Ordering::Greater,
      (PosInf, PosInf) => Ordering::Equal,
      (PosInf, _) => Ordering::Greater,
      (_, PosInf) => Ordering::Less,
      (Finite(a), Finite(b)) => a.cmp(b),
    }
  }
}
 
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub enum IntRange {
  Bottom,
  Range { lo: Bound, hi: Bound },
}
 
impl IntRange {
  pub fn top() -> Self {
    Self::Range {
      lo: Bound::NegInf,
      hi: Bound::PosInf,
    }
  }
 
  pub fn const_(value: i64) -> Self {
    Self::Range {
      lo: Bound::Finite(value),
      hi: Bound::Finite(value),
    }
  }
 
  fn new(lo: Bound, hi: Bound) -> Self {
    if lo > hi {
      Self::Bottom
    } else {
      Self::Range { lo, hi }
    }
  }
 
  pub fn union(self, other: Self) -> Self {
    match (self, other) {
      (Self::Bottom, x) | (x, Self::Bottom) => x,
      (
        Self::Range { lo: lo1, hi: hi1 },
        Self::Range { lo: lo2, hi: hi2 },
      ) => Self::Range {
        lo: lo1.min(lo2),
        hi: hi1.max(hi2),
      },
    }
  }
 
  pub fn intersect(self, other: Self) -> Self {
    match (self, other) {
      (Self::Bottom, _) | (_, Self::Bottom) => Self::Bottom,
      (
        Self::Range { lo: lo1, hi: hi1 },
        Self::Range { lo: lo2, hi: hi2 },
      ) => Self::new(lo1.max(lo2), hi1.min(hi2)),
    }
  }
 
  /// Interval widening used to enforce convergence on loops.
  ///
  /// This is the classic interval widening:
  ///
  /// ```text
  /// [a, b] ▽ [c, d] = [ if c < a then -∞ else a,
  ///                    if d > b then +∞ else b ]
  /// ```
  pub fn widen(self, next: Self) -> Self {
    match (self, next) {
      (Self::Bottom, x) => x,
      (x, Self::Bottom) => x,
      (
        Self::Range { lo: old_lo, hi: old_hi },
        Self::Range { lo: new_lo, hi: new_hi },
      ) => Self::Range {
        lo: if new_lo < old_lo { Bound::NegInf } else { old_lo },
        hi: if new_hi > old_hi { Bound::PosInf } else { old_hi },
      },
    }
  }
}
 
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub enum RangeState {
  Unreachable,
  Vars(HashMap<u32, IntRange>),
}
 
impl RangeState {
  /// Lookup used during analysis. Missing variables are treated as bottom so
  /// that values from currently-unreachable predecessors don't pollute `Phi`
  /// ranges (e.g. during the first loop iteration before the back edge is
  /// reached).
  fn get_var(&self, var: u32) -> IntRange {
    match self {
      RangeState::Unreachable => IntRange::Bottom,
      RangeState::Vars(map) => map.get(&var).copied().unwrap_or(IntRange::Bottom),
    }
  }

  fn get_var_query(&self, var: u32) -> IntRange {
    match self {
      RangeState::Unreachable => IntRange::Bottom,
      RangeState::Vars(map) => map.get(&var).copied().unwrap_or(IntRange::top()),
    }
  }
 
  fn set_var(&mut self, var: u32, range: IntRange) {
    let RangeState::Vars(map) = self else {
      return;
    };
    map.insert(var, range);
  }
 
  fn union_with(&mut self, other: &Self) {
    match other {
      RangeState::Unreachable => {}
      RangeState::Vars(right) => match self {
        RangeState::Unreachable => {
          *self = RangeState::Vars(right.clone());
        }
        RangeState::Vars(left) => {
          for (var, right_range) in right.iter() {
            let merged = left
              .get(var)
              .copied()
              .unwrap_or(IntRange::Bottom)
              .union(*right_range);
            left.insert(*var, merged);
          }
        }
      },
    }
  }
 
  fn widen_from(&self, incoming: &Self) -> Self {
    match (self, incoming) {
      (RangeState::Unreachable, x) => x.clone(),
      (_, RangeState::Unreachable) => RangeState::Unreachable,
      (RangeState::Vars(old), RangeState::Vars(new)) => {
        let mut widened = HashMap::<u32, IntRange>::default();
        for (&var, &new_range) in new.iter() {
          let old_range = old.get(&var).copied().unwrap_or(IntRange::Bottom);
          widened.insert(var, old_range.widen(new_range));
        }
        RangeState::Vars(widened)
      }
    }
  }
}
 
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct RangeResult {
  pub blocks: HashMap<u32, BlockState<RangeState>>,
}
 
impl RangeResult {
  pub fn entry(&self, label: u32) -> Option<&RangeState> {
    self.blocks.get(&label).map(|b| &b.entry)
  }
 
  pub fn exit(&self, label: u32) -> Option<&RangeState> {
    self.blocks.get(&label).map(|b| &b.exit)
  }
 
  pub fn var_at_entry(&self, label: u32, var: u32) -> Option<IntRange> {
    self.entry(label).map(|s| s.get_var_query(var))
  }
 
  pub fn var_at_exit(&self, label: u32, var: u32) -> Option<IntRange> {
    self.exit(label).map(|s| s.get_var_query(var))
  }
}
 
pub fn analyze_ranges(cfg: &Cfg) -> RangeResult {
  let mut analysis = RangeAnalysis::new(cfg);
  let ForwardEdgeDataFlowResult {
    block_entry,
    block_exit,
    ..
  } = analysis.analyze(cfg, cfg.entry);
  let mut blocks = HashMap::<u32, BlockState<RangeState>>::default();
  for (label, entry) in block_entry {
    let exit = block_exit.get(&label).cloned().unwrap_or(RangeState::Unreachable);
    blocks.insert(label, BlockState { entry, exit });
  }
  RangeResult { blocks }
}
 
struct RangeAnalysis {
  // header -> latch preds (back edges) for widening.
  back_preds: HashMap<u32, HashSet<u32>>,
  // Vars used but never defined; treat as params/externals at entry.
  entry_vars: Vec<u32>,
}
 
impl RangeAnalysis {
  fn new(cfg: &Cfg) -> Self {
    let dom = Dom::<false>::calculate(cfg);
    let dominates = dom.dominates_graph();
    let mut back_preds = HashMap::<u32, HashSet<u32>>::default();
    for header in cfg.graph.labels_sorted() {
      let latches: HashSet<u32> = cfg
        .graph
        .parents(header)
        .filter(|&pred| dominates.dominates(header, pred))
        .collect();
      if !latches.is_empty() {
        back_preds.insert(header, latches);
      }
    }
 
    let mut used = HashSet::<u32>::default();
    let mut defined = HashSet::<u32>::default();
    for (_, block) in cfg.bblocks.all() {
      for inst in block.iter() {
        for &tgt in inst.tgts.iter() {
          defined.insert(tgt);
        }
        for arg in inst.args.iter() {
          if let Arg::Var(v) = arg {
            used.insert(*v);
          }
        }
      }
    }
    let mut entry_vars = used.difference(&defined).copied().collect::<Vec<_>>();
    entry_vars.sort_unstable();
 
    Self {
      back_preds,
      entry_vars,
    }
  }
 
  fn arg_to_i64(arg: &Arg) -> Option<i64> {
    match arg {
      Arg::Const(Const::Num(JsNumber(n))) => {
        if !n.is_finite() {
          return None;
        }
        if n.trunc() != *n {
          return None;
        }
        if *n < i64::MIN as f64 || *n > i64::MAX as f64 {
          return None;
        }
        Some(*n as i64)
      }
      Arg::Const(Const::BigInt(v)) => v.to_i64(),
      _ => None,
    }
  }
 
  fn const_range(c: &Const) -> IntRange {
    match c {
      Const::Num(JsNumber(n)) => {
        if !n.is_finite() || n.trunc() != *n || *n < i64::MIN as f64 || *n > i64::MAX as f64 {
          IntRange::top()
        } else {
          IntRange::const_(*n as i64)
        }
      }
      Const::BigInt(v) => v.to_i64().map(IntRange::const_).unwrap_or(IntRange::top()),
      _ => IntRange::top(),
    }
  }
 
  fn arg_range(state: &RangeState, arg: &Arg) -> IntRange {
    match arg {
      Arg::Const(c) => Self::const_range(c),
      Arg::Var(v) => state.get_var(*v),
      _ => IntRange::top(),
    }
  }
 
  fn add(a: IntRange, b: IntRange) -> IntRange {
    let (a_lo, a_hi) = match a {
      IntRange::Bottom => return IntRange::Bottom,
      IntRange::Range { lo, hi } => (lo, hi),
    };
    let (b_lo, b_hi) = match b {
      IntRange::Bottom => return IntRange::Bottom,
      IntRange::Range { lo, hi } => (lo, hi),
    };

    let lo = match (a_lo, b_lo) {
      (Bound::NegInf, _) | (_, Bound::NegInf) => Bound::NegInf,
      (Bound::PosInf, _) | (_, Bound::PosInf) => Bound::PosInf,
      (Bound::Finite(x), Bound::Finite(y)) => {
        let sum = x as i128 + y as i128;
        if sum < i64::MIN as i128 {
          Bound::NegInf
        } else if sum > i64::MAX as i128 {
          Bound::Finite(i64::MAX)
        } else {
          Bound::Finite(sum as i64)
        }
      }
    };
    let hi = match (a_hi, b_hi) {
      (Bound::NegInf, _) | (_, Bound::NegInf) => Bound::NegInf,
      (Bound::PosInf, _) | (_, Bound::PosInf) => Bound::PosInf,
      (Bound::Finite(x), Bound::Finite(y)) => {
        let sum = x as i128 + y as i128;
        if sum < i64::MIN as i128 {
          Bound::Finite(i64::MIN)
        } else if sum > i64::MAX as i128 {
          Bound::PosInf
        } else {
          Bound::Finite(sum as i64)
        }
      }
    };
    IntRange::new(lo, hi)
  }
 
  fn sub(a: IntRange, b: IntRange) -> IntRange {
    let (a_lo, a_hi, b_lo, b_hi) = match (a, b) {
      (IntRange::Bottom, _) | (_, IntRange::Bottom) => return IntRange::Bottom,
      (IntRange::Range { lo: a_lo, hi: a_hi }, IntRange::Range { lo: b_lo, hi: b_hi }) => {
        (a_lo, a_hi, b_lo, b_hi)
      }
    };
 
    let lo = match (a_lo, b_hi) {
      (Bound::NegInf, _) | (_, Bound::PosInf) => Bound::NegInf,
      (Bound::PosInf, Bound::NegInf) => Bound::PosInf,
      (Bound::PosInf, _) => Bound::PosInf,
      (_, Bound::NegInf) => Bound::PosInf,
      (Bound::Finite(x), Bound::Finite(y)) => {
        let diff = x as i128 - y as i128;
        if diff < i64::MIN as i128 {
          Bound::NegInf
        } else if diff > i64::MAX as i128 {
          Bound::Finite(i64::MAX)
        } else {
          Bound::Finite(diff as i64)
        }
      }
    };
    let hi = match (a_hi, b_lo) {
      (Bound::PosInf, _) | (_, Bound::NegInf) => Bound::PosInf,
      (Bound::NegInf, Bound::PosInf) => Bound::NegInf,
      (Bound::NegInf, _) => Bound::NegInf,
      (_, Bound::PosInf) => Bound::NegInf,
      (Bound::Finite(x), Bound::Finite(y)) => {
        let diff = x as i128 - y as i128;
        if diff < i64::MIN as i128 {
          Bound::Finite(i64::MIN)
        } else if diff > i64::MAX as i128 {
          Bound::PosInf
        } else {
          Bound::Finite(diff as i64)
        }
      }
    };
    IntRange::new(lo, hi)
  }
 
  fn mul(a: IntRange, b: IntRange) -> IntRange {
    let (a_lo, a_hi, b_lo, b_hi) = match (a, b) {
      (IntRange::Bottom, _) | (_, IntRange::Bottom) => return IntRange::Bottom,
      (IntRange::Range { lo: a_lo, hi: a_hi }, IntRange::Range { lo: b_lo, hi: b_hi }) => {
        (a_lo, a_hi, b_lo, b_hi)
      }
    };
 
    // If either range is unbounded, be conservative.
    if matches!(a_lo, Bound::NegInf | Bound::PosInf)
      || matches!(a_hi, Bound::NegInf | Bound::PosInf)
      || matches!(b_lo, Bound::NegInf | Bound::PosInf)
      || matches!(b_hi, Bound::NegInf | Bound::PosInf)
    {
      return IntRange::top();
    }
 
    let (Bound::Finite(a_lo), Bound::Finite(a_hi), Bound::Finite(b_lo), Bound::Finite(b_hi)) =
      (a_lo, a_hi, b_lo, b_hi)
    else {
      return IntRange::top();
    };
 
    let candidates = [
      a_lo as i128 * b_lo as i128,
      a_lo as i128 * b_hi as i128,
      a_hi as i128 * b_lo as i128,
      a_hi as i128 * b_hi as i128,
    ];
    let min = candidates.into_iter().min().unwrap();
    let max = candidates.into_iter().max().unwrap();
 
    let lo = if min < i64::MIN as i128 {
      Bound::NegInf
    } else if min > i64::MAX as i128 {
      Bound::Finite(i64::MAX)
    } else {
      Bound::Finite(min as i64)
    };
    let hi = if max < i64::MIN as i128 {
      Bound::Finite(i64::MIN)
    } else if max > i64::MAX as i128 {
      Bound::PosInf
    } else {
      Bound::Finite(max as i64)
    };
    IntRange::new(lo, hi)
  }
 
  fn i32_range() -> IntRange {
    IntRange::Range {
      lo: Bound::Finite(i32::MIN as i64),
      hi: Bound::Finite(i32::MAX as i64),
    }
  }
 
  fn u32_range() -> IntRange {
    IntRange::Range {
      lo: Bound::Finite(0),
      hi: Bound::Finite(u32::MAX as i64),
    }
  }
 
  fn find_cond_compare(block: &[Inst], cond_var: u32) -> Option<(u32, BinOp, i64)> {
    // Find the defining instruction for the condition variable in this block.
    let def = block.iter().rev().find(|inst| inst.tgts.first() == Some(&cond_var))?;
    if def.t != InstTyp::Bin {
      return None;
    }
    let (_, left, op, right) = def.as_bin();
    let Arg::Var(x) = left else {
      return None;
    };
    let c = Self::arg_to_i64(right)?;
    Some((*x, op, c))
  }
}
 
impl ForwardEdgeDataFlowAnalysis for RangeAnalysis {
  type State = RangeState;
 
  fn bottom(&self, _cfg: &Cfg) -> Self::State {
    RangeState::Unreachable
  }
 
  fn boundary_state(&self, _entry: u32, _cfg: &Cfg) -> Self::State {
    let mut vars = HashMap::<u32, IntRange>::default();
    for &var in self.entry_vars.iter() {
      vars.insert(var, IntRange::top());
    }
    RangeState::Vars(vars)
  }
 
  fn meet(&mut self, inputs: &[(u32, &Self::State)]) -> Self::State {
    let mut merged = RangeState::Unreachable;
    for (_, state) in inputs.iter() {
      merged.union_with(state);
    }
    merged
  }
 
  fn meet_block(
    &mut self,
    label: u32,
    prev_entry: &Self::State,
    inputs: &[(u32, &Self::State)],
  ) -> Self::State {
    let mut merged = RangeState::Unreachable;
    for (pred, state) in inputs.iter() {
      let incoming = if self
        .back_preds
        .get(&label)
        .is_some_and(|latches| latches.contains(pred))
      {
        prev_entry.widen_from(state)
      } else {
        (*state).clone()
      };
      merged.union_with(&incoming);
    }
    merged
  }
 
  fn apply_to_instruction(
    &mut self,
    _label: u32,
    _inst_idx: usize,
    inst: &Inst,
    state: &mut Self::State,
  ) {
    let RangeState::Vars(_) = state else {
      return;
    };
 
    let define_top = |state: &mut RangeState, var: u32| state.set_var(var, IntRange::top());
 
    match inst.t {
      InstTyp::VarAssign => {
        let (tgt, arg) = inst.as_var_assign();
        let r = Self::arg_range(state, arg);
        state.set_var(tgt, r);
      }
      InstTyp::Phi => {
        let tgt = inst.tgts[0];
        let mut acc = None::<IntRange>;
        for arg in inst.args.iter() {
          let r = Self::arg_range(state, arg);
          acc = Some(acc.map_or(r, |a| a.union(r)));
        }
        state.set_var(tgt, acc.unwrap_or_else(IntRange::top));
      }
      InstTyp::Bin => {
        let (tgt, left, op, right) = inst.as_bin();
        let l = Self::arg_range(state, left);
        let r = Self::arg_range(state, right);
        let out = match op {
          BinOp::Add => Self::add(l, r),
          BinOp::Sub => Self::sub(l, r),
          BinOp::Mul => Self::mul(l, r),
          BinOp::Shl | BinOp::Shr | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => {
            Self::i32_range()
          }
          BinOp::UShr => Self::u32_range(),
          _ => IntRange::top(),
        };
        state.set_var(tgt, out);
      }
      // These define values, but we don't attempt to track their integer range.
      InstTyp::Un
      | InstTyp::Call
      | InstTyp::ForeignLoad
      | InstTyp::UnknownLoad => {
        for &tgt in inst.tgts.iter() {
          define_top(state, tgt);
        }
      }
      // No defining targets.
      InstTyp::PropAssign
      | InstTyp::CondGoto
      | InstTyp::Return
      | InstTyp::Throw
      | InstTyp::ForeignStore
      | InstTyp::UnknownStore
      | InstTyp::_Label
      | InstTyp::_Goto
      | InstTyp::_Dummy => {}
    }
  }
 
  fn apply_edge(
    &mut self,
    from: u32,
    to: u32,
    from_block: &[Inst],
    state: &Self::State,
    cfg: &Cfg,
  ) -> Self::State {
    let RangeState::Vars(_) = state else {
      return RangeState::Unreachable;
    };
 
    let term = cfg.terminator(from);
    let crate::cfg::cfg::Terminator::CondGoto { cond, t, f } = term else {
      return state.clone();
    };
 
    let Arg::Var(cond_var) = cond else {
      return state.clone();
    };

    // Support `if (!(x < c))` by looking through a chain of `!` operators on the
    // condition variable.
    let mut probe_var = cond_var;
    let mut negate = false;
    let mut resolved: Option<(u32, BinOp, i64)> = None;
    for _ in 0..4 {
      if let Some((x, op, c)) = Self::find_cond_compare(from_block, probe_var) {
        resolved = Some((x, op, c));
        break;
      }
      let Some(def) = from_block
        .iter()
        .rev()
        .find(|inst| inst.tgts.first() == Some(&probe_var))
      else {
        break;
      };
      if def.t != InstTyp::Un {
        break;
      }
      let (_tgt, op, arg) = def.as_un();
      if op != UnOp::Not {
        break;
      }
      let Arg::Var(inner) = arg else {
        break;
      };
      probe_var = *inner;
      negate = !negate;
    }

    let Some((x, op, c)) = resolved else {
      return state.clone();
    };

    let is_true_edge = to == t;
    let is_false_edge = to == f;
    if !is_true_edge && !is_false_edge {
      return state.clone();
    }
    let is_true_edge = if negate { !is_true_edge } else { is_true_edge };

    let mut next = state.clone();
    let cur = next.get_var(x);

    let constraint = match op {
      BinOp::Lt => {
        if is_true_edge {
          match c.checked_sub(1) {
            Some(hi) => IntRange::Range {
              lo: Bound::NegInf,
              hi: Bound::Finite(hi),
            },
            None => IntRange::Bottom,
          }
        } else {
          IntRange::Range {
            lo: Bound::Finite(c),
            hi: Bound::PosInf,
          }
        }
      }
      BinOp::Leq => {
        if is_true_edge {
          IntRange::Range {
            lo: Bound::NegInf,
            hi: Bound::Finite(c),
          }
        } else {
          match c.checked_add(1) {
            Some(lo) => IntRange::Range {
              lo: Bound::Finite(lo),
              hi: Bound::PosInf,
            },
            None => IntRange::Bottom,
          }
        }
      }
      BinOp::Gt => {
        if is_true_edge {
          match c.checked_add(1) {
            Some(lo) => IntRange::Range {
              lo: Bound::Finite(lo),
              hi: Bound::PosInf,
            },
            None => IntRange::Bottom,
          }
        } else {
          IntRange::Range {
            lo: Bound::NegInf,
            hi: Bound::Finite(c),
          }
        }
      }
      BinOp::Geq => {
        if is_true_edge {
          IntRange::Range {
            lo: Bound::Finite(c),
            hi: Bound::PosInf,
          }
        } else {
          match c.checked_sub(1) {
            Some(hi) => IntRange::Range {
              lo: Bound::NegInf,
              hi: Bound::Finite(hi),
            },
            None => IntRange::Bottom,
          }
        }
      }
      BinOp::StrictEq => {
        if is_true_edge {
          IntRange::const_(c)
        } else {
          // Excluding a single value is optional; skip.
          return next;
        }
      }
      _ => return next,
    };
 
    let refined = cur.intersect(constraint);
    if refined == IntRange::Bottom {
      return RangeState::Unreachable;
    }
    next.set_var(x, refined);
    next
  }
}
 
#[cfg(test)]
mod tests {
  use super::*;
  use crate::cfg::cfg::{Cfg, CfgBBlocks, CfgGraph};
 
  fn cfg_with_blocks(blocks: &[(u32, Vec<Inst>)], edges: &[(u32, u32)]) -> Cfg {
    let labels: Vec<u32> = blocks.iter().map(|(label, _)| *label).collect();
    let mut graph = CfgGraph::default();
    for &(from, to) in edges {
      graph.connect(from, to);
    }
    // Ensure all labels exist in the graph even if they have no edges.
    for &label in &labels {
      if !graph.contains(label) {
        graph.connect(label, label);
        graph.disconnect(label, label);
      }
    }
    let mut bblocks = CfgBBlocks::default();
    for (label, insts) in blocks.iter() {
      bblocks.add(*label, insts.clone());
    }
    Cfg {
      graph,
      bblocks,
      entry: 0,
    }
  }
 
  #[test]
  fn narrows_on_lt_branch_edges() {
    let cfg = cfg_with_blocks(
      &[
        (
          0,
          vec![
            Inst::bin(
              1,
              Arg::Var(0),
              BinOp::Lt,
              Arg::Const(Const::Num(JsNumber(10.0))),
            ),
            Inst::cond_goto(Arg::Var(1), 1, 2),
          ],
        ),
        (1, vec![]),
        (2, vec![]),
      ],
      &[(0, 1), (0, 2)],
    );
 
    let result = analyze_ranges(&cfg);
    assert_eq!(
      result.var_at_entry(1, 0),
      Some(IntRange::Range {
        lo: Bound::NegInf,
        hi: Bound::Finite(9)
      })
    );
    assert_eq!(
      result.var_at_entry(2, 0),
      Some(IntRange::Range {
        lo: Bound::Finite(10),
        hi: Bound::PosInf
      })
    );
  }

  #[test]
  fn narrows_on_lt_branch_edges_through_not() {
    let cfg = cfg_with_blocks(
      &[
        (
          0,
          vec![
            Inst::bin(
              1,
              Arg::Var(0),
              BinOp::Lt,
              Arg::Const(Const::Num(JsNumber(10.0))),
            ),
            Inst::un(2, crate::il::inst::UnOp::Not, Arg::Var(1)),
            Inst::cond_goto(Arg::Var(2), 1, 2),
          ],
        ),
        (1, vec![]),
        (2, vec![]),
      ],
      &[(0, 1), (0, 2)],
    );

    let result = analyze_ranges(&cfg);
    assert_eq!(
      result.var_at_entry(1, 0),
      Some(IntRange::Range {
        lo: Bound::Finite(10),
        hi: Bound::PosInf
      })
    );
    assert_eq!(
      result.var_at_entry(2, 0),
      Some(IntRange::Range {
        lo: Bound::NegInf,
        hi: Bound::Finite(9)
      })
    );
  }

  #[test]
  fn loop_widening_converges() {
    // i0 = 0
    // header:
    //   i = phi { pre: i0, latch: i_next }
    //   goto latch
    // latch:
    //   i_next = i + 1
    //   goto header
    let mut phi = Inst::phi_empty(1);
    phi.insert_phi(0, Arg::Var(0));
    phi.insert_phi(2, Arg::Var(2));
 
    let cfg = cfg_with_blocks(
      &[
        (0, vec![Inst::var_assign(0, Arg::Const(Const::Num(JsNumber(0.0))))]),
        (1, vec![phi]),
        (
          2,
          vec![Inst::bin(
            2,
            Arg::Var(1),
            BinOp::Add,
            Arg::Const(Const::Num(JsNumber(1.0))),
          )],
        ),
      ],
      &[(0, 1), (1, 2), (2, 1)],
    );
 
    let result = analyze_ranges(&cfg);
    assert_eq!(
      result.var_at_entry(1, 2),
      Some(IntRange::Range {
        lo: Bound::Finite(1),
        hi: Bound::PosInf
      })
    );
  }
}
