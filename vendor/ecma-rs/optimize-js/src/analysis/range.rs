use crate::analysis::dataflow_edge::{ForwardEdgeDataFlowAnalysis, ForwardEdgeDataFlowResult};
use crate::analysis::facts::{Edge, InstLoc};
use crate::analysis::loop_info::LoopInfo;
use crate::analysis::value_types::ValueTypeSummaries;
use crate::cfg::cfg::Cfg;
use crate::dom::Dom;
use crate::il::inst::{Arg, BinOp, Const, Inst, InstTyp, UnOp};
use ahash::{HashMap, HashSet};
use parse_js::num::JsNumber;
use std::cmp::Ordering;
use std::fmt;
use std::fmt::Formatter;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub enum Bound {
  NegInf,
  I64(i64),
  PosInf,
}

impl fmt::Debug for Bound {
  fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
    match self {
      Bound::NegInf => write!(f, "-inf"),
      Bound::PosInf => write!(f, "+inf"),
      Bound::I64(n) => write!(f, "{n}"),
    }
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
      (I64(a), I64(b)) => a.cmp(b),
    }
  }
}

impl Bound {
  fn min(self, other: Self) -> Self {
    if self <= other { self } else { other }
  }

  fn max(self, other: Self) -> Self {
    if self >= other { self } else { other }
  }

  fn checked_neg(self) -> Option<Self> {
    match self {
      Bound::NegInf => Some(Bound::PosInf),
      Bound::PosInf => Some(Bound::NegInf),
      Bound::I64(n) => Some(Bound::I64(n.checked_neg()?)),
    }
  }

  fn as_i64(self) -> Option<i64> {
    match self {
      Bound::I64(v) => Some(v),
      _ => None,
    }
  }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub enum IntRange {
  Bottom,
  Interval { lo: Bound, hi: Bound },
  Unknown,
}

impl fmt::Debug for IntRange {
  fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
    match self {
      IntRange::Bottom => write!(f, "⊥"),
      IntRange::Unknown => write!(f, "⊤"),
      IntRange::Interval { lo, hi } => write!(f, "[{lo:?},{hi:?}]"),
    }
  }
}

impl IntRange {
  pub fn interval(lo: Bound, hi: Bound) -> Self {
    if lo > hi {
      return IntRange::Bottom;
    }
    if lo == Bound::NegInf && hi == Bound::PosInf {
      return IntRange::Unknown;
    }
    IntRange::Interval { lo, hi }
  }

  pub fn const_i64(n: i64) -> Self {
    IntRange::Interval {
      lo: Bound::I64(n),
      hi: Bound::I64(n),
    }
  }

  pub fn join(self, other: Self) -> Self {
    use IntRange::*;
    match (self, other) {
      (Unknown, _) | (_, Unknown) => Unknown,
      (Bottom, x) | (x, Bottom) => x,
      (Interval { lo: lo1, hi: hi1 }, Interval { lo: lo2, hi: hi2 }) => {
        IntRange::interval(lo1.min(lo2), hi1.max(hi2))
      }
    }
  }

  pub fn intersect(self, other: Self) -> Self {
    use IntRange::*;
    match (self, other) {
      (Bottom, _) | (_, Bottom) => Bottom,
      (Unknown, x) | (x, Unknown) => x,
      (Interval { lo: lo1, hi: hi1 }, Interval { lo: lo2, hi: hi2 }) => {
        IntRange::interval(lo1.max(lo2), hi1.min(hi2))
      }
    }
  }

  pub fn widen_backedge(self, next: Self) -> Self {
    use IntRange::*;
    match (self, next) {
      (Unknown, _) | (_, Unknown) => Unknown,
      (Bottom, x) => x,
      (x, Bottom) => x,
      (Interval { lo: old_lo, hi: old_hi }, Interval { lo: new_lo, hi: new_hi }) => {
        IntRange::interval(
          if new_lo < old_lo { Bound::NegInf } else { old_lo },
          if new_hi > old_hi { Bound::PosInf } else { old_hi },
        )
      }
    }
  }

  fn bounds(self) -> Option<(Bound, Bound)> {
    match self {
      IntRange::Bottom => None,
      IntRange::Unknown => Some((Bound::NegInf, Bound::PosInf)),
      IntRange::Interval { lo, hi } => Some((lo, hi)),
    }
  }

  fn singleton_i64(self) -> Option<i64> {
    match self {
      IntRange::Interval {
        lo: Bound::I64(a),
        hi: Bound::I64(b),
      } if a == b => Some(a),
      _ => None,
    }
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct State {
  reachable: bool,
  ranges: Vec<IntRange>,
}

impl State {
  fn bottom(var_count: usize) -> Self {
    Self {
      reachable: false,
      ranges: vec![IntRange::Bottom; var_count],
    }
  }

  fn boundary(var_count: usize, entry_vars: &[u32]) -> Self {
    let mut state = Self {
      reachable: true,
      ranges: vec![IntRange::Bottom; var_count],
    };
    for &var in entry_vars {
      if let Some(slot) = state.ranges.get_mut(var as usize) {
        *slot = IntRange::Unknown;
      }
    }
    state
  }

  fn set_unreachable(&mut self) {
    self.reachable = false;
    for r in &mut self.ranges {
      *r = IntRange::Bottom;
    }
  }

  pub fn is_reachable(&self) -> bool {
    self.reachable
  }

  pub fn range_of_var(&self, var: u32) -> IntRange {
    if !self.reachable {
      return IntRange::Bottom;
    }
    self
      .ranges
      .get(var as usize)
      .copied()
      .unwrap_or(IntRange::Unknown)
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct RangeResult {
  result: ForwardEdgeDataFlowResult<State>,
}

impl RangeResult {
  /// State at basic block entry, after merging all incoming edges.
  pub fn entry(&self, label: u32) -> Option<&State> {
    self.result.block_entry.get(&label)
  }

  /// State at basic block exit, before successor-specific edge refinement.
  pub fn exit(&self, label: u32) -> Option<&State> {
    self.result.block_exit.get(&label)
  }

  pub fn state_at_block_entry(&self, label: u32) -> Option<&State> {
    self.entry(label)
  }

  pub fn state_at_block_exit(&self, label: u32) -> Option<&State> {
    self.exit(label)
  }

  /// State flowing into `edge.to` along the given edge.
  pub fn state_at_edge_entry(&self, edge: Edge) -> Option<&State> {
    self.result.edge_out.get(&(edge.from, edge.to))
  }

  pub fn edge_entry(&self, edge: Edge) -> Option<&State> {
    self.state_at_edge_entry(edge)
  }

  pub fn entry_state(&self, label: u32) -> &State {
    &self.result.block_entry[&label]
  }

  pub fn exit_state(&self, label: u32) -> &State {
    &self.result.block_exit[&label]
  }

  pub fn edge_state(&self, pred: u32, succ: u32) -> Option<&State> {
    self.result.edge_out.get(&(pred, succ))
  }

  pub fn edge_is_reachable(&self, pred: u32, succ: u32) -> bool {
    self.edge_state(pred, succ).is_some_and(|s| s.is_reachable())
  }

  pub fn range_of_var_at_entry(&self, label: u32, var: u32) -> IntRange {
    self.entry_state(label).range_of_var(var)
  }

  pub fn range_of_var_at_exit(&self, label: u32, var: u32) -> IntRange {
    self.exit_state(label).range_of_var(var)
  }

  pub fn range_of_var_on_edge(&self, pred: u32, succ: u32, var: u32) -> IntRange {
    self
      .edge_state(pred, succ)
      .map(|s| s.range_of_var(var))
      .unwrap_or(IntRange::Bottom)
  }

  pub fn var_at_entry(&self, label: u32, var: u32) -> Option<IntRange> {
    self.entry(label).map(|s| s.range_of_var(var))
  }

  pub fn var_at_exit(&self, label: u32, var: u32) -> Option<IntRange> {
    self.exit(label).map(|s| s.range_of_var(var))
  }

  pub fn var_at_edge(&self, edge: Edge, var: u32) -> Option<IntRange> {
    self.edge_entry(edge).map(|s| s.range_of_var(var))
  }

  /// Compute the analysis state immediately before `inst_idx` in `label`.
  ///
  /// This is computed by replaying the instruction transfer function inside the
  /// block starting from the stored block entry state.
  pub fn state_before_inst(&self, cfg: &Cfg, label: u32, inst_idx: usize) -> State {
    let entry = self
      .entry(label)
      .cloned()
      .unwrap_or_else(|| State::bottom(cfg_var_count(cfg)));
    let block = cfg.bblocks.get(label);
    assert!(
      inst_idx <= block.len(),
      "inst index {inst_idx} out of bounds for block {label} (len={})",
      block.len()
    );
    let mut analysis = RangeAnalysis::new_for_replay(entry.ranges.len());
    let mut state = entry.clone();
    for (idx, inst) in block.iter().enumerate().take(inst_idx) {
      analysis.apply_to_instruction_in_block(label, idx, inst, block, &mut state);
    }
    state
  }

  /// Compute the analysis state immediately after `inst_idx` in `label`.
  pub fn state_after_inst(&self, cfg: &Cfg, label: u32, inst_idx: usize) -> State {
    let block = cfg.bblocks.get(label);
    assert!(
      inst_idx < block.len(),
      "inst index {inst_idx} out of bounds for block {label} (len={})",
      block.len()
    );
    let mut state = self.state_before_inst(cfg, label, inst_idx);
    let mut analysis = RangeAnalysis::new_for_replay(state.ranges.len());
    analysis.apply_to_instruction_in_block(label, inst_idx, &block[inst_idx], block, &mut state);
    state
  }

  pub fn state_before_loc(&self, cfg: &Cfg, loc: InstLoc) -> State {
    self.state_before_inst(cfg, loc.block, loc.inst)
  }

  pub fn state_after_loc(&self, cfg: &Cfg, loc: InstLoc) -> State {
    self.state_after_inst(cfg, loc.block, loc.inst)
  }

  pub fn fact_for_arg(&self, state: &State, arg: &Arg) -> IntRange {
    let _ = self;
    RangeAnalysis::range_for_arg(state, arg)
  }

  pub fn result(&self) -> &ForwardEdgeDataFlowResult<State> {
    &self.result
  }

  /// Replay the analysis transfer function inside a single basic block, invoking
  /// `visit` after each instruction with the updated state.
  ///
  /// This is significantly more efficient than repeatedly calling
  /// [`RangeResult::state_after_inst`] for each instruction (which would replay
  /// from the block entry every time).
  pub fn visit_states_after_each_inst_in_block<F>(&self, cfg: &Cfg, label: u32, mut visit: F)
  where
    F: FnMut(usize, &Inst, &State),
  {
    let entry = self
      .entry(label)
      .cloned()
      .unwrap_or_else(|| State::bottom(cfg_var_count(cfg)));
    let mut analysis = RangeAnalysis::new_for_replay(entry.ranges.len());
    let mut state = entry;
    let block: &[Inst] = cfg
      .bblocks
      .maybe_get(label)
      .map(|bb| bb.as_slice())
      .unwrap_or(&[]);
    for (inst_idx, inst) in block.iter().enumerate() {
      analysis.apply_to_instruction(label, inst_idx, inst, &mut state);
      visit(inst_idx, inst, &state);
    }
  }
}

#[derive(Clone, Debug)]
struct PhiNode {
  tgt: u32,
  sources: Vec<(u32, Arg)>,
}

fn cfg_var_count(cfg: &Cfg) -> usize {
  let mut max: Option<u32> = None;
  for (_, block) in cfg.bblocks.all() {
    for inst in block.iter() {
      for &tgt in inst.tgts.iter() {
        max = Some(max.map_or(tgt, |m| m.max(tgt)));
      }
      for arg in inst.args.iter() {
        if let Arg::Var(v) = arg {
          max = Some(max.map_or(*v, |m| m.max(*v)));
        }
      }
    }
  }
  max.map(|m| m as usize + 1).unwrap_or(0)
}

fn cfg_entry_vars(cfg: &Cfg) -> Vec<u32> {
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
  entry_vars
}

fn cfg_phi_nodes(cfg: &Cfg) -> HashMap<u32, Vec<PhiNode>> {
  let mut out = HashMap::<u32, Vec<PhiNode>>::default();
  for (label, block) in cfg.bblocks.all() {
    let mut phis = Vec::new();
    for inst in block.iter() {
      if inst.t != InstTyp::Phi {
        break;
      }
      debug_assert_eq!(inst.labels.len(), inst.args.len());
      phis.push(PhiNode {
        tgt: inst.tgts[0],
        sources: inst
          .labels
          .iter()
          .copied()
          .zip(inst.args.iter().cloned())
          .collect(),
      });
    }
    if !phis.is_empty() {
      out.insert(label, phis);
    }
  }
  out
}

pub fn analyze_ranges(cfg: &Cfg) -> RangeResult {
  let var_count = cfg_var_count(cfg);
  let dom = Dom::calculate(cfg);
  let loops = LoopInfo::compute(cfg, &dom);
  let backedges: HashSet<(u32, u32)> = loops
    .loops
    .iter()
    .flat_map(|l| l.latches.iter().map(move |&lat| (lat, l.header)))
    .collect();

  let entry_vars = cfg_entry_vars(cfg);
  let phi_nodes = cfg_phi_nodes(cfg);
  let types = ValueTypeSummaries::new(cfg);

  let mut analysis = RangeAnalysis {
    var_count,
    entry_vars,
    backedges,
    phi_nodes,
    types,
  };
  let result = analysis.analyze(cfg, cfg.entry);
  RangeResult { result }
}

/// Annotate `cfg`'s instructions with best-effort integer range information.
///
/// This populates [`crate::il::meta::ValueFacts::int_range`] on each instruction
/// that defines a temp variable (`inst.tgts[0]`) with a non-`Unknown` range.
///
/// Consumers that need instruction-level range facts in tooling (e.g. debugger
/// UIs) should call this after running [`analyze_ranges`].
pub(crate) fn annotate_cfg_range_facts(cfg: &mut Cfg, result: &RangeResult) {
  let mut labels: Vec<u32> = cfg.bblocks.all().map(|(label, _)| label).collect();
  labels.sort_unstable();

  for label in labels {
    let Some(entry) = result.entry(label).cloned() else {
      continue;
    };
    let mut state = entry;
    let mut analysis = RangeAnalysis::new_for_replay(state.ranges.len());

    for (inst_idx, inst) in cfg.bblocks.get_mut(label).iter_mut().enumerate() {
      analysis.apply_to_instruction(label, inst_idx, &*inst, &mut state);
      let Some(tgt) = inst.tgts.get(0).copied() else {
        continue;
      };
      let IntRange::Interval { lo, hi } = state.range_of_var(tgt) else {
        continue;
      };
      let min = lo.as_i64();
      let max = hi.as_i64();
      if min.is_none() && max.is_none() {
        continue;
      }
      let facts = inst.meta.value.get_or_insert_with(Default::default);
      facts.int_range = Some(crate::il::meta::IntRange { min, max });
    }
  }
}

struct RangeAnalysis {
  var_count: usize,
  entry_vars: Vec<u32>,
  backedges: HashSet<(u32, u32)>,
  phi_nodes: HashMap<u32, Vec<PhiNode>>,
  types: ValueTypeSummaries,
}

impl RangeAnalysis {
  fn new_for_replay(var_count: usize) -> Self {
    Self {
      var_count,
      entry_vars: Vec::new(),
      backedges: HashSet::default(),
      phi_nodes: HashMap::default(),
      types: ValueTypeSummaries::default(),
    }
  }

  fn set_var(&self, state: &mut State, var: u32, range: IntRange) {
    if let Some(slot) = state.ranges.get_mut(var as usize) {
      *slot = range;
    }
  }

  fn maybe_i64_const(c: &Const) -> Option<i64> {
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
    // Validate that the cast is exact (f64 can't represent all i64 values).
    if as_i64 as f64 != *n {
      return None;
    }
    Some(as_i64)
  }

  fn const_range(c: &Const) -> IntRange {
    Self::maybe_i64_const(c).map(IntRange::const_i64).unwrap_or(IntRange::Unknown)
  }

  fn range_for_arg(state: &State, arg: &Arg) -> IntRange {
    match arg {
      Arg::Const(c) => Self::const_range(c),
      Arg::Var(v) => state.range_of_var(*v),
      Arg::Builtin(_) | Arg::Fn(_) => IntRange::Unknown,
    }
  }

  fn eval_arg(&self, state: &State, arg: &Arg) -> IntRange {
    Self::range_for_arg(state, arg)
  }

  fn find_cond_compare(block: &[Inst], cond_var: u32) -> Option<(u32, BinOp, i64)> {
    let def = block
      .iter()
      .rev()
      .find(|inst| inst.tgts.first() == Some(&cond_var))?;
    if def.t != InstTyp::Bin {
      return None;
    }
    let (_tgt, left, op, right) = def.as_bin();
    let x = left.maybe_var()?;
    let Arg::Const(c) = right else {
      return None;
    };
    let c = Self::maybe_i64_const(c)?;
    Some((x, op, c))
  }

  /// Attempt to chase through simple linear integer expressions so edge
  /// refinement can constrain the underlying source variable.
  ///
  /// We only chase `+/-` when the intermediate value is known to be a number in
  /// typed builds (via [`ValueTypeSummaries`]). In untyped builds `types` is
  /// empty so this is a no-op.
  fn chase_linear_int_expr(&self, block: &[Inst], mut var: u32) -> (u32, i64) {
    let mut offset: i64 = 0;
    for _ in 0..8 {
      let Some(def) = block
        .iter()
        .rev()
        .find(|inst| inst.tgts.first() == Some(&var))
      else {
        break;
      };

      match def.t {
        InstTyp::VarAssign => {
          let (_tgt, arg) = def.as_var_assign();
          let Some(inner) = arg.maybe_var() else {
            break;
          };
          var = inner;
        }
        InstTyp::Bin => {
          let (_tgt, left, op, right) = def.as_bin();
          // Only treat this as numeric arithmetic when the result is definitely
          // a number (typed builds).
          if !self
            .types
            .var(var)
            .is_some_and(|ty| ty.is_definitely_number())
          {
            break;
          }

          match op {
            BinOp::Add => match (left, right) {
              (Arg::Var(inner), Arg::Const(c)) | (Arg::Const(c), Arg::Var(inner)) => {
                let Some(k) = Self::maybe_i64_const(c) else {
                  break;
                };
                offset = offset.saturating_add(k);
                var = *inner;
              }
              _ => break,
            },
            BinOp::Sub => match (left, right) {
              (Arg::Var(inner), Arg::Const(c)) => {
                let Some(k) = Self::maybe_i64_const(c) else {
                  break;
                };
                offset = offset.saturating_sub(k);
                var = *inner;
              }
              _ => break,
            },
            _ => break,
          }
        }
        _ => break,
      }
    }
    (var, offset)
  }

  fn int32_range() -> IntRange {
    IntRange::interval(Bound::I64(i32::MIN as i64), Bound::I64(i32::MAX as i64))
  }

  fn uint32_range() -> IntRange {
    IntRange::interval(Bound::I64(0), Bound::I64(u32::MAX as i64))
  }

  fn add_range(a: IntRange, b: IntRange) -> IntRange {
    match (a, b) {
      (IntRange::Bottom, _) | (_, IntRange::Bottom) => IntRange::Bottom,
      (IntRange::Unknown, _) | (_, IntRange::Unknown) => IntRange::Unknown,
      (
        IntRange::Interval { lo: a_lo, hi: a_hi },
        IntRange::Interval { lo: b_lo, hi: b_hi },
      ) => {
        let lo = match (a_lo, b_lo) {
          (Bound::NegInf, _) | (_, Bound::NegInf) => Bound::NegInf,
          (Bound::PosInf, _) | (_, Bound::PosInf) => Bound::PosInf,
          (Bound::I64(x), Bound::I64(y)) => {
            let sum = x as i128 + y as i128;
            if sum < i64::MIN as i128 {
              Bound::NegInf
            } else if sum > i64::MAX as i128 {
              // Low bounds must stay <= the true lower bound; clamp instead of +inf.
              Bound::I64(i64::MAX)
            } else {
              Bound::I64(sum as i64)
            }
          }
        };
        let hi = match (a_hi, b_hi) {
          (Bound::NegInf, _) | (_, Bound::NegInf) => Bound::NegInf,
          (Bound::PosInf, _) | (_, Bound::PosInf) => Bound::PosInf,
          (Bound::I64(x), Bound::I64(y)) => {
            let sum = x as i128 + y as i128;
            if sum < i64::MIN as i128 {
              Bound::I64(i64::MIN)
            } else if sum > i64::MAX as i128 {
              Bound::PosInf
            } else {
              Bound::I64(sum as i64)
            }
          }
        };
        IntRange::interval(lo, hi)
      }
    }
  }

  fn sub_range(a: IntRange, b: IntRange) -> IntRange {
    match (a, b) {
      (IntRange::Bottom, _) | (_, IntRange::Bottom) => IntRange::Bottom,
      (IntRange::Unknown, _) | (_, IntRange::Unknown) => IntRange::Unknown,
      (
        IntRange::Interval { lo: a_lo, hi: a_hi },
        IntRange::Interval { lo: b_lo, hi: b_hi },
      ) => {
        let lo = match (a_lo, b_hi) {
          (Bound::NegInf, _) | (_, Bound::PosInf) => Bound::NegInf,
          (Bound::PosInf, Bound::NegInf) => Bound::PosInf,
          (Bound::PosInf, _) => Bound::PosInf,
          (_, Bound::NegInf) => Bound::PosInf,
          (Bound::I64(x), Bound::I64(y)) => {
            let diff = x as i128 - y as i128;
            if diff < i64::MIN as i128 {
              Bound::NegInf
            } else if diff > i64::MAX as i128 {
              Bound::I64(i64::MAX)
            } else {
              Bound::I64(diff as i64)
            }
          }
        };
        let hi = match (a_hi, b_lo) {
          (Bound::PosInf, _) | (_, Bound::NegInf) => Bound::PosInf,
          (Bound::NegInf, Bound::PosInf) => Bound::NegInf,
          (Bound::NegInf, _) => Bound::NegInf,
          (_, Bound::PosInf) => Bound::NegInf,
          (Bound::I64(x), Bound::I64(y)) => {
            let diff = x as i128 - y as i128;
            if diff < i64::MIN as i128 {
              Bound::I64(i64::MIN)
            } else if diff > i64::MAX as i128 {
              Bound::PosInf
            } else {
              Bound::I64(diff as i64)
            }
          }
        };
        IntRange::interval(lo, hi)
      }
    }
  }

  fn mul_range(a: IntRange, b: IntRange) -> IntRange {
    match (a, b) {
      (IntRange::Bottom, _) | (_, IntRange::Bottom) => IntRange::Bottom,
      (IntRange::Unknown, _) | (_, IntRange::Unknown) => IntRange::Unknown,
      (
        IntRange::Interval { lo: a_lo, hi: a_hi },
        IntRange::Interval { lo: b_lo, hi: b_hi },
      ) => {
        // Be conservative in the presence of infinities (multiplication sign matters).
        let (Some(a_lo), Some(a_hi), Some(b_lo), Some(b_hi)) =
          (a_lo.as_i64(), a_hi.as_i64(), b_lo.as_i64(), b_hi.as_i64())
        else {
          return IntRange::Unknown;
        };
        let candidates = [
          a_lo as i128 * b_lo as i128,
          a_lo as i128 * b_hi as i128,
          a_hi as i128 * b_lo as i128,
          a_hi as i128 * b_hi as i128,
        ];
        let lo_val = *candidates.iter().min().unwrap();
        let hi_val = *candidates.iter().max().unwrap();

        let lo = if lo_val < i64::MIN as i128 {
          Bound::NegInf
        } else if lo_val > i64::MAX as i128 {
          Bound::I64(i64::MAX)
        } else {
          Bound::I64(lo_val as i64)
        };
        let hi = if hi_val < i64::MIN as i128 {
          Bound::I64(i64::MIN)
        } else if hi_val > i64::MAX as i128 {
          Bound::PosInf
        } else {
          Bound::I64(hi_val as i64)
        };
        IntRange::interval(lo, hi)
      }
    }
  }

  fn bin_range(op: BinOp, left: IntRange, right: IntRange) -> IntRange {
    use BinOp::*;
    match op {
      Add => Self::add_range(left, right),
      Sub => Self::sub_range(left, right),
      Mul => Self::mul_range(left, right),
      BitAnd | BitOr | BitXor | Shl | Shr | UShr => {
        let left_c = left.singleton_i64();
        let right_c = right.singleton_i64();
        if let (Some(a), Some(b)) = (left_c, right_c) {
          let shift = (b as u32) & 0x1f;
          let res = match op {
            BitAnd => (a as i32) & (b as i32),
            BitOr => (a as i32) | (b as i32),
            BitXor => (a as i32) ^ (b as i32),
            Shl => (a as i32).wrapping_shl(shift),
            Shr => (a as i32).wrapping_shr(shift),
            UShr => {
              let u = (a as u32).wrapping_shr(shift);
              return IntRange::const_i64(u as i64);
            }
            _ => unreachable!(),
          };
          return IntRange::const_i64(res as i64);
        }
        match op {
          UShr => Self::uint32_range(),
          _ => Self::int32_range(),
        }
      }
      // Comparisons and other operations yield non-integer values.
      _ => IntRange::Unknown,
    }
  }

  fn un_range(op: UnOp, arg: IntRange) -> IntRange {
    use UnOp::*;
    match op {
      Neg => {
        let Some((lo, hi)) = arg.bounds() else {
          return IntRange::Bottom;
        };
        match (hi.checked_neg(), lo.checked_neg()) {
          (Some(lo), Some(hi)) => IntRange::interval(lo, hi),
          _ => IntRange::Unknown,
        }
      }
      BitNot => {
        if let Some(v) = arg.singleton_i64() {
          return IntRange::const_i64((!(v as i32)) as i64);
        }
        Self::int32_range()
      }
      Plus => IntRange::Unknown,
      Void | Typeof | Not | _Dummy => IntRange::Unknown,
    }
  }

  fn apply_to_instruction_in_block(
    &mut self,
    label: u32,
    inst_idx: usize,
    inst: &Inst,
    block: &[Inst],
    state: &mut State,
  ) {
    if inst.t == InstTyp::Assume {
      self.apply_assume(&block[..inst_idx], inst.as_assume(), state);
    } else {
      self.apply_to_instruction(label, inst_idx, inst, state);
    }
  }

  fn apply_assume(&self, prior_insts: &[Inst], cond: &Arg, state: &mut State) {
    if !state.reachable {
      return;
    }

    match cond {
      Arg::Const(Const::Bool(true)) => {}
      Arg::Const(Const::Bool(false)) => state.set_unreachable(),
      Arg::Var(cond_var) => self.refine_for_condition(prior_insts, *cond_var, true, state),
      _ => {}
    }
  }

  fn refine_for_condition(&self, block: &[Inst], cond_var: u32, mut is_true: bool, state: &mut State) {
    // Chase boolean negation and var assignments to find a comparison we can use
    // to refine integer ranges.
    let mut probe_var = cond_var;
    let mut negate = false;
    let mut resolved: Option<(u32, BinOp, i64)> = None;
    for _ in 0..8 {
      if let Some((x, op, c)) = Self::find_cond_compare(block, probe_var) {
        resolved = Some((x, op, c));
        break;
      }

      let Some(def) = block
        .iter()
        .rev()
        .find(|inst| inst.tgts.first() == Some(&probe_var))
      else {
        break;
      };

      match def.t {
        InstTyp::Un => {
          let (_tgt, op, arg) = def.as_un();
          if op != UnOp::Not {
            break;
          }
          let Some(inner) = arg.maybe_var() else {
            break;
          };
          probe_var = inner;
          negate = !negate;
        }
        InstTyp::VarAssign => {
          let (_tgt, arg) = def.as_var_assign();
          let Some(inner) = arg.maybe_var() else {
            break;
          };
          probe_var = inner;
        }
        _ => break,
      }
    }

    let Some((x, op, c)) = resolved else {
      return;
    };
    let (x, c) = {
      let (base, offset) = self.chase_linear_int_expr(block, x);
      (base, c.saturating_sub(offset))
    };

    if negate {
      is_true = !is_true;
    }

    let c_minus_1 = c.saturating_sub(1);
    let c_plus_1 = c.saturating_add(1);

    let current = state.range_of_var(x);
    let new_range = match op {
      BinOp::Lt => {
        let constraint = if is_true {
          IntRange::interval(Bound::NegInf, Bound::I64(c_minus_1))
        } else {
          IntRange::interval(Bound::I64(c), Bound::PosInf)
        };
        current.intersect(constraint)
      }
      BinOp::Leq => {
        let constraint = if is_true {
          IntRange::interval(Bound::NegInf, Bound::I64(c))
        } else {
          IntRange::interval(Bound::I64(c_plus_1), Bound::PosInf)
        };
        current.intersect(constraint)
      }
      BinOp::Gt => {
        let constraint = if is_true {
          IntRange::interval(Bound::I64(c_plus_1), Bound::PosInf)
        } else {
          IntRange::interval(Bound::NegInf, Bound::I64(c))
        };
        current.intersect(constraint)
      }
      BinOp::Geq => {
        let constraint = if is_true {
          IntRange::interval(Bound::I64(c), Bound::PosInf)
        } else {
          IntRange::interval(Bound::NegInf, Bound::I64(c_minus_1))
        };
        current.intersect(constraint)
      }
      BinOp::StrictEq => {
        if is_true {
          current.intersect(IntRange::const_i64(c))
        } else {
          // Excluding a single value is optional; skip.
          return;
        }
      }
      BinOp::NotStrictEq => {
        if !is_true {
          // `!(x !== c)` implies `x === c`.
          current.intersect(IntRange::const_i64(c))
        } else {
          // Best-effort exclusion of a single value. We can represent this when
          // the current interval lies entirely on one side of `c` or `c` is a
          // boundary.
          match current {
            IntRange::Bottom => IntRange::Bottom,
            IntRange::Unknown => IntRange::Unknown,
            IntRange::Interval { lo, hi } => {
              let lo_i = lo.as_i64();
              let hi_i = hi.as_i64();
              // If the current interval is a singleton equal to `c`, this path
              // is unreachable.
              if lo_i == Some(c) && hi_i == Some(c) {
                IntRange::Bottom
              } else if lo_i == Some(c) {
                // [c, hi] -> [c+1, hi]
                IntRange::interval(Bound::I64(c_plus_1), hi).intersect(current)
              } else if hi_i == Some(c) {
                // [lo, c] -> [lo, c-1]
                IntRange::interval(lo, Bound::I64(c_minus_1)).intersect(current)
              } else {
                current
              }
            }
          }
        }
      }
      _ => return,
    };

    if let Some(slot) = state.ranges.get_mut(x as usize) {
      *slot = new_range;
      if matches!(*slot, IntRange::Bottom) {
        state.set_unreachable();
      }
    }
  }
}

impl ForwardEdgeDataFlowAnalysis for RangeAnalysis {
  type State = State;

  fn bottom(&self, _cfg: &Cfg) -> Self::State {
    State::bottom(self.var_count)
  }

  fn boundary_state(&self, _entry: u32, _cfg: &Cfg) -> Self::State {
    State::boundary(self.var_count, &self.entry_vars)
  }

  fn meet(&mut self, incoming: &[(u32, &Self::State)]) -> Self::State {
    let mut out = State {
      reachable: false,
      ranges: vec![IntRange::Bottom; self.var_count],
    };
    for (_, s) in incoming {
      if !s.reachable {
        continue;
      }
      if !out.reachable {
        out.reachable = true;
      }
      for (dst, src) in out.ranges.iter_mut().zip(s.ranges.iter()) {
        *dst = (*dst).join(*src);
      }
    }
    out
  }

  fn meet_block(
    &mut self,
    label: u32,
    prev_entry: &Self::State,
    incoming: &[(u32, &Self::State)],
  ) -> Self::State {
    let _ = prev_entry;
    let mut merged = self.meet(incoming);
    if !merged.reachable {
      return merged;
    }

    let Some(phis) = self.phi_nodes.get(&label) else {
      return merged;
    };

    for phi in phis {
      let mut acc = IntRange::Bottom;
      for (pred, arg) in &phi.sources {
        let pred_state = incoming
          .iter()
          .find_map(|(p, s)| if p == pred { Some(*s) } else { None });
        let Some(pred_state) = pred_state else {
          continue;
        };
        acc = acc.join(self.eval_arg(pred_state, arg));
      }
      self.set_var(&mut merged, phi.tgt, acc);
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
    if !state.reachable {
      return;
    }

    match inst.t {
      InstTyp::VarAssign => {
        let (tgt, arg) = inst.as_var_assign();
        let r = self.eval_arg(state, arg);
        self.set_var(state, tgt, r);
      }
      InstTyp::NullCheck => {
        let (tgt, value) = inst.as_null_check();
        if let Some(tgt) = tgt {
          let r = self.eval_arg(state, value);
          self.set_var(state, tgt, r);
        }
      }
      InstTyp::Bin => {
        let (tgt, left, op, right) = inst.as_bin();
        let left = self.eval_arg(state, left);
        let right = self.eval_arg(state, right);
        let out = Self::bin_range(op, left, right);
        self.set_var(state, tgt, out);
      }
      InstTyp::Un => {
        let (tgt, op, arg) = inst.as_un();
        let arg = self.eval_arg(state, arg);
        let out = Self::un_range(op, arg);
        self.set_var(state, tgt, out);
      }
      InstTyp::Call => {
        let (tgt, _callee, _this, _args, _spreads) = inst.as_call();
        if let Some(tgt) = tgt {
          self.set_var(state, tgt, IntRange::Unknown);
        }
      }
      InstTyp::Invoke => {
        let (tgt, _callee, _this, _args, _spreads, _normal, _exception) = inst.as_invoke();
        if let Some(tgt) = tgt {
          self.set_var(state, tgt, IntRange::Unknown);
        }
      }
      #[cfg(feature = "semantic-ops")]
      InstTyp::KnownApiCall { .. } => {
        let (tgt, _api, _args) = inst.as_known_api_call();
        if let Some(tgt) = tgt {
          self.set_var(state, tgt, IntRange::Unknown);
        }
      }
      InstTyp::StringConcat => {
        let tgt = inst.tgts[0];
        self.set_var(state, tgt, IntRange::Unknown);
      }
      #[cfg(feature = "native-async-ops")]
      InstTyp::Await | InstTyp::PromiseAll | InstTyp::PromiseRace => {
        if let Some(&tgt) = inst.tgts.get(0) {
          self.set_var(state, tgt, IntRange::Unknown);
        }
      }
      #[cfg(any(feature = "native-fusion", feature = "native-array-ops"))]
      InstTyp::ArrayChain => {
        let (tgt, _base, _ops) = inst.as_array_chain();
        if let Some(tgt) = tgt {
          self.set_var(state, tgt, IntRange::Unknown);
        }
      }
      InstTyp::Catch => {
        let tgt = inst.as_catch();
        self.set_var(state, tgt, IntRange::Unknown);
      }
      InstTyp::ForeignLoad => {
        let (tgt, _foreign) = inst.as_foreign_load();
        self.set_var(state, tgt, IntRange::Unknown);
      }
      InstTyp::UnknownLoad => {
        let (tgt, _unknown) = inst.as_unknown_load();
        self.set_var(state, tgt, IntRange::Unknown);
      }
      InstTyp::FieldLoad => {
        let (tgt, _obj, _field) = inst.as_field_load();
        self.set_var(state, tgt, IntRange::Unknown);
      }
      // Phi nodes are handled in `meet_block` using predecessor-specific values.
      InstTyp::Phi => {}
      // Non-defining instructions.
      InstTyp::PropAssign
      | InstTyp::FieldStore
      | InstTyp::Assume
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

  fn apply_to_block(&mut self, label: u32, block: &[Inst], state: &Self::State) -> Self::State {
    let mut next = state.clone();
    for (idx, inst) in block.iter().enumerate() {
      self.apply_to_instruction_in_block(label, idx, inst, block, &mut next);
    }
    next
  }

  fn apply_edge(
    &mut self,
    _pred: u32,
    succ: u32,
    pred_block: &[Inst],
    state_at_end_of_pred: &Self::State,
    _cfg: &Cfg,
  ) -> Self::State {
    let mut next = state_at_end_of_pred.clone();
    if !next.reachable {
      return next;
    }

    let Some(term) = pred_block.last() else {
      return next;
    };
    if term.t != InstTyp::CondGoto {
      return next;
    }

    let (cond, then_label, else_label) = term.as_cond_goto();
    let Some(cond_var) = cond.maybe_var() else {
      return next;
    };

    let is_true_edge = if succ == then_label {
      true
    } else if succ == else_label {
      false
    } else {
      return next;
    };
    self.refine_for_condition(pred_block, cond_var, is_true_edge, &mut next);
    next
  }

  fn widen_edge(
    &mut self,
    from: u32,
    to: u32,
    old: &Self::State,
    new: &Self::State,
  ) -> Self::State {
    if !self.backedges.contains(&(from, to)) {
      return new.clone();
    }

    if !old.reachable {
      return new.clone();
    }
    if !new.reachable {
      return old.clone();
    }

    let ranges = old
      .ranges
      .iter()
      .zip(new.ranges.iter())
      .map(|(o, n)| (*o).widen_backedge(*n))
      .collect();
    State {
      reachable: true,
      ranges,
    }
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
    // Ensure labels exist in the graph even if disconnected.
    for &label in &labels {
      if !graph.contains(label) {
        graph.ensure_label(label);
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
      result.range_of_var_at_entry(1, 0),
      IntRange::interval(Bound::NegInf, Bound::I64(9))
    );
    assert_eq!(
      result.range_of_var_at_entry(2, 0),
      IntRange::interval(Bound::I64(10), Bound::PosInf)
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
            Inst::un(2, UnOp::Not, Arg::Var(1)),
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
      result.range_of_var_at_entry(1, 0),
      IntRange::interval(Bound::I64(10), Bound::PosInf)
    );
    assert_eq!(
      result.range_of_var_at_entry(2, 0),
      IntRange::interval(Bound::NegInf, Bound::I64(9))
    );
  }

  #[test]
  fn narrows_on_lt_branch_edges_through_var_assign() {
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
            Inst::var_assign(2, Arg::Var(1)),
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
      result.range_of_var_at_entry(1, 0),
      IntRange::interval(Bound::NegInf, Bound::I64(9))
    );
    assert_eq!(
      result.range_of_var_at_entry(2, 0),
      IntRange::interval(Bound::I64(10), Bound::PosInf)
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
      result.range_of_var_at_entry(1, 2),
      IntRange::interval(Bound::I64(1), Bound::PosInf)
    );
  }

  #[test]
  fn deterministic_across_edge_ordering() {
    let blocks = &[
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
    ];

    let cfg1 = cfg_with_blocks(blocks, &[(0, 1), (0, 2)]);
    let cfg2 = cfg_with_blocks(blocks, &[(0, 2), (0, 1)]);

    let r1 = analyze_ranges(&cfg1);
    let r2 = analyze_ranges(&cfg2);
    assert_eq!(r1, r2);
  }
}
