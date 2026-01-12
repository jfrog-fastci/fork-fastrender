use crate::analysis::dataflow_edge::{ForwardEdgeDataFlowAnalysis, ForwardEdgeDataFlowResult};
use crate::analysis::facts::{Edge, InstLoc};
use crate::analysis::value_types::ValueTypeSummaries;
use crate::cfg::cfg::Cfg;
use crate::il::inst::{Arg, BinOp, Const, Inst, InstTyp, UnOp};
use std::fmt;
use std::fmt::Formatter;

/// Two-bit nullishness summary (`null` / `undefined`) for consumers that do not
/// care about non-nullish value tracking.
///
/// This is derived from [`NullabilityMask`], which additionally tracks whether a
/// value may be *some other* non-nullish value (needed to prove branch
/// unreachability for strict comparisons).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct NullishFlags(u8);

impl NullishFlags {
  pub const NON_NULLISH: Self = Self(0);
  pub const MAYBE_NULL: Self = Self(1 << 0);
  pub const MAYBE_UNDEF: Self = Self(1 << 1);
  pub const UNKNOWN: Self = Self(Self::MAYBE_NULL.0 | Self::MAYBE_UNDEF.0);

  pub fn may_be_null(self) -> bool {
    (self.0 & Self::MAYBE_NULL.0) != 0
  }

  pub fn may_be_undefined(self) -> bool {
    (self.0 & Self::MAYBE_UNDEF.0) != 0
  }
}

impl fmt::Debug for NullishFlags {
  fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
    match *self {
      Self::NON_NULLISH => write!(f, "NonNullish"),
      Self::MAYBE_NULL => write!(f, "MaybeNull"),
      Self::MAYBE_UNDEF => write!(f, "MaybeUndef"),
      Self::UNKNOWN => write!(f, "MaybeNull|MaybeUndef"),
      Self(bits) => write!(f, "NullishFlags({bits:#b})"),
    }
  }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct NullabilityMask(u8);

impl NullabilityMask {
  pub const BOTTOM: Self = Self(0);
  pub const NULL: Self = Self(1 << 0);
  pub const UNDEF: Self = Self(1 << 1);
  pub const OTHER: Self = Self(1 << 2);

  pub const TOP: Self = Self(Self::NULL.0 | Self::UNDEF.0 | Self::OTHER.0);

  pub fn is_bottom(self) -> bool {
    self == Self::BOTTOM
  }

  pub fn contains(self, other: Self) -> bool {
    (self.0 & other.0) == other.0
  }

  pub fn is_non_nullish(self) -> bool {
    self == Self::OTHER
  }

  pub fn may_be_null(self) -> bool {
    self.contains(Self::NULL)
  }

  pub fn may_be_undefined(self) -> bool {
    self.contains(Self::UNDEF)
  }
}

impl From<NullabilityMask> for NullishFlags {
  fn from(mask: NullabilityMask) -> Self {
    let mut bits = 0u8;
    if mask.contains(NullabilityMask::NULL) {
      bits |= NullishFlags::MAYBE_NULL.0;
    }
    if mask.contains(NullabilityMask::UNDEF) {
      bits |= NullishFlags::MAYBE_UNDEF.0;
    }
    NullishFlags(bits)
  }
}

impl fmt::Debug for NullabilityMask {
  fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
    if self.is_bottom() {
      return write!(f, "⊥");
    }
    if *self == Self::TOP {
      return write!(f, "⊤");
    }
    let mut first = true;
    let mut write_flag = |name: &str| {
      if !first {
        let _ = write!(f, "|");
      }
      first = false;
      write!(f, "{name}")
    };
    if self.contains(Self::NULL) {
      write_flag("NULL")?;
    }
    if self.contains(Self::UNDEF) {
      write_flag("UNDEF")?;
    }
    if self.contains(Self::OTHER) {
      write_flag("OTHER")?;
    }
    Ok(())
  }
}

impl std::ops::BitOr for NullabilityMask {
  type Output = Self;

  fn bitor(self, rhs: Self) -> Self::Output {
    Self(self.0 | rhs.0)
  }
}

impl std::ops::BitOrAssign for NullabilityMask {
  fn bitor_assign(&mut self, rhs: Self) {
    self.0 |= rhs.0;
  }
}

impl std::ops::BitAnd for NullabilityMask {
  type Output = Self;

  fn bitand(self, rhs: Self) -> Self::Output {
    Self(self.0 & rhs.0)
  }
}

impl std::ops::BitAndAssign for NullabilityMask {
  fn bitand_assign(&mut self, rhs: Self) {
    self.0 &= rhs.0;
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct State {
  reachable: bool,
  masks: Vec<NullabilityMask>,
}

impl State {
  fn bottom(var_count: usize) -> Self {
    Self {
      reachable: false,
      masks: vec![NullabilityMask::BOTTOM; var_count],
    }
  }

  fn top(var_count: usize) -> Self {
    Self {
      reachable: true,
      masks: vec![NullabilityMask::TOP; var_count],
    }
  }

  fn set_unreachable(&mut self) {
    self.reachable = false;
    for mask in &mut self.masks {
      *mask = NullabilityMask::BOTTOM;
    }
  }

  pub fn is_reachable(&self) -> bool {
    self.reachable
  }

  pub fn mask_of_var(&self, var: u32) -> NullabilityMask {
    if !self.reachable {
      return NullabilityMask::BOTTOM;
    }
    self
      .masks
      .get(var as usize)
      .copied()
      .unwrap_or(NullabilityMask::TOP)
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct NullabilityResult {
  result: ForwardEdgeDataFlowResult<State>,
}

impl NullabilityResult {
  /// State at basic block entry, after merging all incoming edges.
  pub fn state_at_block_entry(&self, label: u32) -> Option<&State> {
    self.result.block_entry.get(&label)
  }

  /// State at basic block exit, before successor-specific edge refinement.
  pub fn state_at_block_exit(&self, label: u32) -> Option<&State> {
    self.result.block_exit.get(&label)
  }

  /// State flowing into `edge.to` along the given edge.
  pub fn state_at_edge_entry(&self, edge: Edge) -> Option<&State> {
    self.result.edge_out.get(&(edge.from, edge.to))
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

  pub fn mask_of_var_at_entry(&self, label: u32, var: u32) -> NullabilityMask {
    self.entry_state(label).mask_of_var(var)
  }

  /// Compute the analysis state immediately before `inst_idx` in `label`.
  ///
  /// This is computed by replaying the instruction transfer function inside the
  /// block starting from the stored block entry state.
  pub fn state_before_inst(&self, cfg: &Cfg, label: u32, inst_idx: usize) -> State {
    let entry = self.entry_state(label).clone();
    let mut analysis = NullabilityAnalysis {
      var_count: entry.masks.len(),
      types: ValueTypeSummaries::new(cfg),
    };
    let block = cfg.bblocks.get(label);
    assert!(
      inst_idx <= block.len(),
      "inst index {inst_idx} out of bounds for block {label} (len={})",
      block.len()
    );
    let mut state = entry;
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
    let mut analysis = NullabilityAnalysis {
      var_count: state.masks.len(),
      types: ValueTypeSummaries::new(cfg),
    };
    analysis.apply_to_instruction_in_block(label, inst_idx, &block[inst_idx], block, &mut state);
    state
  }

  pub fn state_before_loc(&self, cfg: &Cfg, loc: InstLoc) -> State {
    self.state_before_inst(cfg, loc.block, loc.inst)
  }

  pub fn state_after_loc(&self, cfg: &Cfg, loc: InstLoc) -> State {
    self.state_after_inst(cfg, loc.block, loc.inst)
  }

  pub fn fact_for_arg(&self, state: &State, arg: &Arg) -> NullabilityMask {
    let _ = self;
    match arg {
      Arg::Var(v) => state.mask_of_var(*v),
      Arg::Const(c) => const_mask(c),
      Arg::Builtin(name) if name == "undefined" => NullabilityMask::UNDEF,
      Arg::Builtin(_) | Arg::Fn(_) => NullabilityMask::OTHER,
    }
  }

  /// Convenience helper for tests/clients that need instruction-level precision.
  ///
  /// Returns the nullability mask for `var` immediately *before* `inst_idx` in `label`.
  pub fn mask_of_var_before_inst(
    &self,
    cfg: &Cfg,
    label: u32,
    inst_idx: usize,
    var: u32,
  ) -> NullabilityMask {
    self.state_before_inst(cfg, label, inst_idx).mask_of_var(var)
  }

  pub fn nullability_of_arg(&self, state: &State, arg: &Arg) -> NullishFlags {
    let _ = self;
    if !state.reachable {
      return NullishFlags::UNKNOWN;
    }
    match arg {
      Arg::Var(v) => state.mask_of_var(*v).into(),
      Arg::Const(Const::Null) => NullishFlags::MAYBE_NULL,
      Arg::Const(Const::Undefined) => NullishFlags::MAYBE_UNDEF,
      Arg::Const(_) => NullishFlags::NON_NULLISH,
      Arg::Fn(_) => NullishFlags::NON_NULLISH,
      Arg::Builtin(name) if name == "undefined" => NullishFlags::MAYBE_UNDEF,
      Arg::Builtin(_) => NullishFlags::UNKNOWN,
    }
  }

  /// Replay the analysis transfer function inside a single basic block, invoking
  /// `visit` after each instruction with the updated state.
  ///
  /// This is significantly more efficient than repeatedly calling
  /// [`NullabilityResult::state_after_inst`] for each instruction.
  pub fn visit_states_after_each_inst_in_block<F>(&self, cfg: &Cfg, label: u32, mut visit: F)
  where
    F: FnMut(usize, &Inst, &State),
  {
    let entry = self
      .state_at_block_entry(label)
      .cloned()
      .unwrap_or_else(|| State::bottom(cfg_var_count(cfg)));
    let mut analysis = NullabilityAnalysis {
      var_count: entry.masks.len(),
      types: ValueTypeSummaries::new(cfg),
    };
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

pub fn calculate_nullability(cfg: &Cfg) -> NullabilityResult {
  let var_count = cfg_var_count(cfg);
  let types = ValueTypeSummaries::new(cfg);
  let mut analysis = NullabilityAnalysis { var_count, types };
  let result = analysis.analyze(cfg, cfg.entry);
  NullabilityResult { result }
}

/// Annotate `cfg`'s instructions with coarse nullability information.
///
/// This populates [`crate::il::meta::ValueFacts::nullability`] for each
/// instruction that defines a temp variable (`inst.tgts[0]`) when the analysis
/// can prove the value is definitely nullish or definitely non-nullish.
pub(crate) fn annotate_cfg_nullability_facts(cfg: &mut Cfg, result: &NullabilityResult) {
  let mut labels: Vec<u32> = cfg.bblocks.all().map(|(label, _)| label).collect();
  labels.sort_unstable();
  let types = ValueTypeSummaries::new(cfg);

  for label in labels {
    let Some(entry) = result.state_at_block_entry(label).cloned() else {
      continue;
    };
    let mut state = entry;
    let mut analysis = NullabilityAnalysis {
      var_count: state.masks.len(),
      types: types.clone(),
    };

    for (inst_idx, inst) in cfg.bblocks.get_mut(label).iter_mut().enumerate() {
      analysis.apply_to_instruction(label, inst_idx, &*inst, &mut state);
      let Some(tgt) = inst.tgts.get(0).copied() else {
        continue;
      };
      if !state.is_reachable() {
        continue;
      }
      let mask = state.mask_of_var(tgt);
      let nullability = if mask.is_non_nullish() {
        Some(crate::il::meta::Nullability::NonNullish)
      } else if !mask.contains(NullabilityMask::OTHER) && !mask.is_bottom() {
        Some(crate::il::meta::Nullability::Nullish)
      } else {
        None
      };
      let Some(nullability) = nullability else {
        continue;
      };
      let facts = inst.meta.value.get_or_insert_with(Default::default);
      facts.nullability = Some(nullability);
    }
  }
}

struct NullabilityAnalysis {
  var_count: usize,
  types: ValueTypeSummaries,
}

impl NullabilityAnalysis {
  fn arg_mask(&self, arg: &Arg, state: &State) -> NullabilityMask {
    match arg {
      Arg::Var(v) => state.mask_of_var(*v),
      Arg::Const(c) => const_mask(c),
      Arg::Builtin(name) if name == "undefined" => NullabilityMask::UNDEF,
      Arg::Builtin(_) | Arg::Fn(_) => NullabilityMask::OTHER,
    }
  }

  fn set_var(&self, state: &mut State, var: u32, mask: NullabilityMask) {
    if let Some(slot) = state.masks.get_mut(var as usize) {
      *slot = mask;
    }
  }

  fn set_value_result(&self, inst: &Inst, state: &mut State, tgt: u32, mut mask: NullabilityMask) {
    // In typed builds, lowering annotates some value-producing instructions with
    // `excludes_nullish` even when the coarse `ValueTypeSummary` is `Unknown`
    // (e.g. `NonNullable<T>`). Prefer the per-instruction flag when present,
    // and fall back to the global per-variable type summaries when they prove
    // the target never includes `null`/`undefined`.
    if inst.meta.excludes_nullish || self.types.var(tgt).is_some_and(|ty| ty.excludes_nullish()) {
      mask &= NullabilityMask::OTHER;
      if mask.is_bottom() {
        state.set_unreachable();
        return;
      }
    }
    self.set_var(state, tgt, mask);
  }

  fn call_result_mask(&self, callee: &Arg) -> NullabilityMask {
    match callee {
      Arg::Builtin(path) => {
        if matches!(
          path.as_str(),
          "__optimize_js_array" | "__optimize_js_object" | "__optimize_js_regex"
        ) {
          return NullabilityMask::OTHER;
        }
        if path.starts_with("Math.") {
          return NullabilityMask::OTHER;
        }
        NullabilityMask::TOP
      }
      _ => NullabilityMask::TOP,
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
      Arg::Var(cond_var) => {
        // Treat `assume(%x)` as "x is truthy", which implies it is not nullish.
        if let Some(slot) = state.masks.get_mut(*cond_var as usize) {
          *slot &= NullabilityMask::OTHER;
          if slot.is_bottom() {
            state.set_unreachable();
            return;
          }
        }

        // Existing narrowing based on null/undefined comparisons.
        let mut negate = false;
        let mut probe_var = *cond_var;
        let mut cmp: Option<(&Arg, BinOp, &Arg)> = None;
        for _ in 0..8 {
          let mut found_def = false;
          for inst in prior_insts.iter().rev() {
            if inst.t == InstTyp::CondGoto {
              continue;
            }
            if inst.tgts.get(0).copied() != Some(probe_var) {
              continue;
            }
            found_def = true;
            match inst.t {
              InstTyp::Bin => {
                let (_tgt, left, op, right) = inst.as_bin();
                cmp = Some((left, op, right));
              }
              InstTyp::Un => {
                let (_tgt, un_op, arg) = inst.as_un();
                if un_op == UnOp::Not {
                  if let Arg::Var(v) = arg {
                    probe_var = *v;
                    negate = !negate;
                    continue;
                  }
                }
              }
              InstTyp::VarAssign => {
                let (_tgt, arg) = inst.as_var_assign();
                if let Arg::Var(v) = arg {
                  probe_var = *v;
                  continue;
                }
              }
              _ => {}
            }
            break;
          }
          if cmp.is_some() || !found_def {
            break;
          }
        }

        let Some((left, op, right)) = cmp else {
          return;
        };

        let (var, is_null) = match (left, right) {
          (Arg::Var(v), other) | (other, Arg::Var(v)) => {
            if matches!(other, Arg::Const(Const::Null)) {
              (*v, true)
            } else if matches!(other, Arg::Const(Const::Undefined))
              || matches!(other, Arg::Builtin(name) if name == "undefined")
            {
              (*v, false)
            } else {
              return;
            }
          }
          _ => return,
        };

        let nullish = NullabilityMask::NULL | NullabilityMask::UNDEF;
        let non_nullish = NullabilityMask::OTHER;

        let (mut when_true, mut when_false) = match op {
          BinOp::LooseEq => (nullish, non_nullish),
          BinOp::NotLooseEq => (non_nullish, nullish),
          BinOp::StrictEq => {
            if is_null {
              (NullabilityMask::NULL, NullabilityMask::UNDEF | NullabilityMask::OTHER)
            } else {
              (NullabilityMask::UNDEF, NullabilityMask::NULL | NullabilityMask::OTHER)
            }
          }
          BinOp::NotStrictEq => {
            if is_null {
              (NullabilityMask::UNDEF | NullabilityMask::OTHER, NullabilityMask::NULL)
            } else {
              (NullabilityMask::NULL | NullabilityMask::OTHER, NullabilityMask::UNDEF)
            }
          }
          _ => return,
        };

        if negate {
          std::mem::swap(&mut when_true, &mut when_false);
        }
        let refine = when_true; // assume condition is true
        if let Some(slot) = state.masks.get_mut(var as usize) {
          *slot &= refine;
          if slot.is_bottom() {
            state.set_unreachable();
          }
        }
      }
      _ => {}
    }
  }
}

impl ForwardEdgeDataFlowAnalysis for NullabilityAnalysis {
  type State = State;

  fn bottom(&self, _cfg: &Cfg) -> Self::State {
    State::bottom(self.var_count)
  }

  fn boundary_state(&self, _entry: u32, _cfg: &Cfg) -> Self::State {
    State::top(self.var_count)
  }

  fn meet(&mut self, states: &[(u32, &Self::State)]) -> Self::State {
    let mut out = State::bottom(self.var_count);
    for (_, s) in states {
      if !s.reachable {
        continue;
      }
      if !out.reachable {
        out.reachable = true;
      }
      for (dst, src) in out.masks.iter_mut().zip(s.masks.iter()) {
        *dst |= *src;
      }
    }
    out
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
        let mask = self.arg_mask(arg, state);
        self.set_value_result(inst, state, tgt, mask);
      }
      InstTyp::Bin => {
        let (tgt, _left, op, _right) = inst.as_bin();
        let mask = match op {
          BinOp::GetProp => NullabilityMask::TOP,
          BinOp::_Dummy => NullabilityMask::TOP,
          _ => NullabilityMask::OTHER,
        };
        self.set_value_result(inst, state, tgt, mask);
      }
      InstTyp::Un => {
        let (tgt, op, _arg) = inst.as_un();
        let mask = match op {
          UnOp::Void => NullabilityMask::UNDEF,
          UnOp::_Dummy => NullabilityMask::TOP,
          _ => NullabilityMask::OTHER,
        };
        self.set_value_result(inst, state, tgt, mask);
      }
      InstTyp::Call => {
        let (tgt, callee, _this, _args, _spreads) = inst.as_call();
        if let Some(tgt) = tgt {
          let mask = self.call_result_mask(callee);
          self.set_value_result(inst, state, tgt, mask);
        }
      }
      #[cfg(feature = "semantic-ops")]
      InstTyp::KnownApiCall { .. } => {
        let (tgt, _api, _args) = inst.as_known_api_call();
        if let Some(tgt) = tgt {
          // Without an API database, treat known API calls as producing an unknown value.
          self.set_value_result(inst, state, tgt, NullabilityMask::TOP);
        }
      }
      InstTyp::ForeignLoad => {
        let (tgt, _foreign) = inst.as_foreign_load();
        self.set_value_result(inst, state, tgt, NullabilityMask::TOP);
      }
      InstTyp::UnknownLoad => {
        let (tgt, _unknown) = inst.as_unknown_load();
        self.set_value_result(inst, state, tgt, NullabilityMask::TOP);
      }
      InstTyp::Phi => {
        let Some(&tgt) = inst.tgts.get(0) else {
          return;
        };
        if inst.args.is_empty() {
          self.set_value_result(inst, state, tgt, NullabilityMask::TOP);
          return;
        }
        let mut mask = NullabilityMask::BOTTOM;
        for arg in inst.args.iter() {
          mask |= self.arg_mask(arg, state);
        }
        if mask.is_bottom() {
          // This should be unreachable in well-formed SSA; stay conservative.
          mask = NullabilityMask::TOP;
        }
        self.set_value_result(inst, state, tgt, mask);
      }
      _ => {}
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
    let is_true_edge = if succ == then_label {
      true
    } else if succ == else_label {
      false
    } else {
      return next;
    };

    let Arg::Var(cond_var) = cond else {
      return next;
    };

    // Truthiness-based nullability narrowing:
    //
    // - `if (x)` taking the true edge means `x` is truthy. `null` and `undefined`
    //   are always falsy, so `x` cannot be nullish on that edge.
    // - `if (!x)` lowers to `tmp = !x; if (tmp) ...`. Taking the *false* edge
    //   means `!x` is falsy, so `x` is truthy and therefore non-nullish.
    let non_nullish = NullabilityMask::OTHER;
    if is_true_edge {
      if let Some(slot) = next.masks.get_mut(*cond_var as usize) {
        *slot &= non_nullish;
        if slot.is_bottom() {
          next.set_unreachable();
          return next;
        }
      }
    } else if let Some(def) = pred_block.iter().rev().nth(1) {
      if def.t == InstTyp::Un && def.tgts.get(0).copied() == Some(*cond_var) {
        let (_tgt, op, arg) = def.as_un();
        if op == UnOp::Not {
          if let Arg::Var(v) = arg {
            if let Some(slot) = next.masks.get_mut(*v as usize) {
              *slot &= non_nullish;
              if slot.is_bottom() {
                next.set_unreachable();
                return next;
              }
            }
          }
        }
      }
    }

    // Existing narrowing based on null/undefined comparisons.
    //
    // Find the defining instruction for the boolean condition value. We allow a
    // small amount of indirection through `!` and `VarAssign` so patterns like
    // `if (!(x == null))` and `if ((x == null) as any)` (lowered as var assigns)
    // still narrow.
    let mut negate = false;
    let mut probe_var = *cond_var;
    let mut cmp: Option<(&Arg, BinOp, &Arg)> = None;
    for _ in 0..8 {
      let mut found_def = false;
      for inst in pred_block.iter().rev() {
        if inst.t == InstTyp::CondGoto {
          continue;
        }
        if inst.tgts.get(0).copied() != Some(probe_var) {
          continue;
        }
        found_def = true;
        match inst.t {
          InstTyp::Bin => {
            let (_tgt, left, op, right) = inst.as_bin();
            cmp = Some((left, op, right));
          }
          InstTyp::Un => {
            let (_tgt, un_op, arg) = inst.as_un();
            if un_op == UnOp::Not {
              if let Arg::Var(v) = arg {
                probe_var = *v;
                negate = !negate;
                continue;
              }
            }
          }
          InstTyp::VarAssign => {
            let (_tgt, arg) = inst.as_var_assign();
            if let Arg::Var(v) = arg {
              probe_var = *v;
              continue;
            }
          }
          _ => {}
        }
        break;
      }
      if cmp.is_some() || !found_def {
        break;
      }
    }

    let Some((left, op, right)) = cmp else {
      return next;
    };

    let (var, is_null) = match (left, right) {
      (Arg::Var(v), other) | (other, Arg::Var(v)) => {
        if matches!(other, Arg::Const(Const::Null)) {
          (*v, true)
        } else if matches!(other, Arg::Const(Const::Undefined))
          || matches!(other, Arg::Builtin(name) if name == "undefined")
        {
          (*v, false)
        } else {
          return next;
        }
      }
      _ => return next,
    };

    let nullish = NullabilityMask::NULL | NullabilityMask::UNDEF;

    let (true_refine, false_refine) = match op {
      BinOp::LooseEq => (nullish, non_nullish),
      BinOp::NotLooseEq => (non_nullish, nullish),
      BinOp::StrictEq => {
        if is_null {
          (NullabilityMask::NULL, NullabilityMask::UNDEF | NullabilityMask::OTHER)
        } else {
          (NullabilityMask::UNDEF, NullabilityMask::NULL | NullabilityMask::OTHER)
        }
      }
      BinOp::NotStrictEq => {
        if is_null {
          (NullabilityMask::UNDEF | NullabilityMask::OTHER, NullabilityMask::NULL)
        } else {
          (NullabilityMask::NULL | NullabilityMask::OTHER, NullabilityMask::UNDEF)
        }
      }
      _ => return next,
    };

    let is_true_edge = if negate { !is_true_edge } else { is_true_edge };
    let refine = if is_true_edge { true_refine } else { false_refine };
    let idx = var as usize;
    if let Some(slot) = next.masks.get_mut(idx) {
      *slot &= refine;
      if slot.is_bottom() {
        next.set_unreachable();
      }
    }

    next
  }
}

fn const_mask(c: &Const) -> NullabilityMask {
  match c {
    Const::Null => NullabilityMask::NULL,
    Const::Undefined => NullabilityMask::UNDEF,
    Const::Bool(_) | Const::Num(_) | Const::BigInt(_) | Const::Str(_) => NullabilityMask::OTHER,
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
        if let Arg::Var(v) = arg {
          max = Some(max.map_or(*v, |m| m.max(*v)));
        }
      }
    }
  }
  max.map(|m| m as usize + 1).unwrap_or(0)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::cfg::cfg::{Cfg, CfgBBlocks, CfgGraph};

  fn build_cfg(edges: &[(u32, u32)], blocks: &[(u32, Vec<Inst>)]) -> Cfg {
    let mut graph = CfgGraph::default();
    for &(p, c) in edges {
      graph.connect(p, c);
    }
    let mut bblocks = CfgBBlocks::default();
    for (label, insts) in blocks {
      bblocks.add(*label, insts.clone());
    }
    Cfg {
      graph,
      bblocks,
      entry: 0,
    }
  }

  #[test]
  fn narrows_loose_eq_null_on_branches() {
    // %0 = unknown; %1 = (%0 == null); if %1 goto 1 else 2
    let cfg = build_cfg(
      &[(0, 1), (0, 2)],
      &[
        (
          0,
          vec![
            Inst::unknown_load(0, "x".to_string()),
            Inst::bin(1, Arg::Var(0), BinOp::LooseEq, Arg::Const(Const::Null)),
            Inst::cond_goto(Arg::Var(1), 1, 2),
          ],
        ),
        (1, vec![]),
        (2, vec![]),
      ],
    );

    let result = calculate_nullability(&cfg);
    assert_eq!(
      result.mask_of_var_at_entry(1, 0),
      NullabilityMask::NULL | NullabilityMask::UNDEF
    );
    assert_eq!(result.mask_of_var_at_entry(2, 0), NullabilityMask::OTHER);
  }

  #[test]
  fn narrows_strict_eq_undefined_on_branches() {
    // %0 = unknown; %1 = (%0 === undefined); if %1 goto 1 else 2
    let cfg = build_cfg(
      &[(0, 1), (0, 2)],
      &[
        (
          0,
          vec![
            Inst::unknown_load(0, "x".to_string()),
            Inst::bin(1, Arg::Var(0), BinOp::StrictEq, Arg::Const(Const::Undefined)),
            Inst::cond_goto(Arg::Var(1), 1, 2),
          ],
        ),
        (1, vec![]),
        (2, vec![]),
      ],
    );

    let result = calculate_nullability(&cfg);
    assert_eq!(result.mask_of_var_at_entry(1, 0), NullabilityMask::UNDEF);
    assert_eq!(
      result.mask_of_var_at_entry(2, 0),
      NullabilityMask::NULL | NullabilityMask::OTHER
    );
  }

  #[test]
  fn builtin_undefined_value_is_tracked() {
    // %0 = undefined; %1 = (%0 === undefined); if %1 goto 1 else 2
    let cfg = build_cfg(
      &[(0, 1), (0, 2)],
      &[
        (
          0,
          vec![
            Inst::var_assign(0, Arg::Builtin("undefined".to_string())),
            Inst::bin(1, Arg::Var(0), BinOp::StrictEq, Arg::Const(Const::Undefined)),
            Inst::cond_goto(Arg::Var(1), 1, 2),
          ],
        ),
        (1, vec![]),
        (2, vec![]),
      ],
    );

    let result = calculate_nullability(&cfg);
    assert!(result.entry_state(1).is_reachable());
    assert_eq!(result.mask_of_var_at_entry(1, 0), NullabilityMask::UNDEF);
    assert!(!result.entry_state(2).is_reachable());
  }

  #[test]
  fn unreachable_edge_when_refinement_contradicts_known_value() {
    // %0 = null; %1 = (%0 !== null); if %1 goto 1 else 2
    let cfg = build_cfg(
      &[(0, 1), (0, 2)],
      &[
        (
          0,
          vec![
            Inst::var_assign(0, Arg::Const(Const::Null)),
            Inst::bin(1, Arg::Var(0), BinOp::NotStrictEq, Arg::Const(Const::Null)),
            Inst::cond_goto(Arg::Var(1), 1, 2),
          ],
        ),
        (1, vec![]),
        (2, vec![]),
      ],
    );

    let result = calculate_nullability(&cfg);
    assert_eq!(result.entry_state(1).is_reachable(), false);
    assert_eq!(result.mask_of_var_at_entry(1, 0), NullabilityMask::BOTTOM);
    assert_eq!(result.mask_of_var_at_entry(2, 0), NullabilityMask::NULL);
  }

  #[test]
  fn narrows_through_not_of_null_test() {
    // %0 = unknown; %1 = (%0 == null); %2 = !%1; if %2 goto 1 else 2
    let cfg = build_cfg(
      &[(0, 1), (0, 2)],
      &[
        (
          0,
          vec![
            Inst::unknown_load(0, "x".to_string()),
            Inst::bin(1, Arg::Var(0), BinOp::LooseEq, Arg::Const(Const::Null)),
            Inst::un(2, UnOp::Not, Arg::Var(1)),
            Inst::cond_goto(Arg::Var(2), 1, 2),
          ],
        ),
        (1, vec![]),
        (2, vec![]),
      ],
    );

    let result = calculate_nullability(&cfg);
    assert_eq!(result.mask_of_var_at_entry(1, 0), NullabilityMask::OTHER);
    assert_eq!(
      result.mask_of_var_at_entry(2, 0),
      NullabilityMask::NULL | NullabilityMask::UNDEF
    );
  }

  #[test]
  fn narrows_through_var_assign_of_null_test() {
    // %0 = unknown; %1 = (%0 == null); %2 = %1; if %2 goto 1 else 2
    let cfg = build_cfg(
      &[(0, 1), (0, 2)],
      &[
        (
          0,
          vec![
            Inst::unknown_load(0, "x".to_string()),
            Inst::bin(1, Arg::Var(0), BinOp::LooseEq, Arg::Const(Const::Null)),
            Inst::var_assign(2, Arg::Var(1)),
            Inst::cond_goto(Arg::Var(2), 1, 2),
          ],
        ),
        (1, vec![]),
        (2, vec![]),
      ],
    );

    let result = calculate_nullability(&cfg);
    assert_eq!(
      result.mask_of_var_at_entry(1, 0),
      NullabilityMask::NULL | NullabilityMask::UNDEF
    );
    assert_eq!(result.mask_of_var_at_entry(2, 0), NullabilityMask::OTHER);
  }

  #[test]
  fn deterministic_across_edge_insertion_order() {
    let blocks = &[
      (
        0,
        vec![
          Inst::unknown_load(0, "x".to_string()),
          Inst::bin(1, Arg::Var(0), BinOp::LooseEq, Arg::Const(Const::Null)),
          Inst::cond_goto(Arg::Var(1), 1, 2),
        ],
      ),
      (1, vec![]),
      (2, vec![]),
    ];
    let cfg1 = build_cfg(&[(0, 1), (0, 2)], blocks);
    let cfg2 = build_cfg(&[(0, 2), (0, 1)], blocks);

    let r1 = calculate_nullability(&cfg1);
    let r2 = calculate_nullability(&cfg2);
    assert_eq!(r1.result, r2.result);
  }
}
