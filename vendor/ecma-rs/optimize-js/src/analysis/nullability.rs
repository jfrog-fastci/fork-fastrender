use crate::analysis::dataflow_edge::{ForwardEdgeDataFlowAnalysis, ForwardEdgeDataFlowResult};
use crate::cfg::cfg::Cfg;
use crate::il::inst::{Arg, BinOp, Const, Inst, InstTyp, UnOp};
use std::fmt;
use std::fmt::Formatter;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
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

#[derive(Clone, Debug)]
pub struct NullabilityResult {
  result: ForwardEdgeDataFlowResult<State>,
}

impl NullabilityResult {
  pub fn entry_state(&self, label: u32) -> &State {
    &self.result.block_entry[&label]
  }

  pub fn mask_of_var_at_entry(&self, label: u32, var: u32) -> NullabilityMask {
    self.entry_state(label).mask_of_var(var)
  }
}

pub fn calculate_nullability(cfg: &Cfg) -> NullabilityResult {
  let var_count = cfg_var_count(cfg);
  let mut analysis = NullabilityAnalysis { var_count };
  let result = analysis.analyze(cfg, cfg.entry);
  NullabilityResult { result }
}

struct NullabilityAnalysis {
  var_count: usize,
}

impl NullabilityAnalysis {
  fn arg_mask(&self, arg: &Arg, state: &State) -> NullabilityMask {
    match arg {
      Arg::Var(v) => state.mask_of_var(*v),
      Arg::Const(c) => const_mask(c),
      Arg::Builtin(_) | Arg::Fn(_) => NullabilityMask::OTHER,
    }
  }

  fn set_var(&self, state: &mut State, var: u32, mask: NullabilityMask) {
    if let Some(slot) = state.masks.get_mut(var as usize) {
      *slot = mask;
    }
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
        self.set_var(state, tgt, mask);
      }
      InstTyp::Bin => {
        let (tgt, _left, op, _right) = inst.as_bin();
        let mask = match op {
          BinOp::GetProp => NullabilityMask::TOP,
          BinOp::_Dummy => NullabilityMask::TOP,
          _ => NullabilityMask::OTHER,
        };
        self.set_var(state, tgt, mask);
      }
      InstTyp::Un => {
        let (tgt, op, _arg) = inst.as_un();
        let mask = match op {
          UnOp::Void => NullabilityMask::UNDEF,
          UnOp::_Dummy => NullabilityMask::TOP,
          _ => NullabilityMask::OTHER,
        };
        self.set_var(state, tgt, mask);
      }
      InstTyp::Call => {
        let (tgt, callee, _this, _args, _spreads) = inst.as_call();
        if let Some(tgt) = tgt {
          let mask = self.call_result_mask(callee);
          self.set_var(state, tgt, mask);
        }
      }
      InstTyp::ForeignLoad => {
        let (tgt, _foreign) = inst.as_foreign_load();
        self.set_var(state, tgt, NullabilityMask::TOP);
      }
      InstTyp::UnknownLoad => {
        let (tgt, _unknown) = inst.as_unknown_load();
        self.set_var(state, tgt, NullabilityMask::TOP);
      }
      InstTyp::Phi => {
        let Some(&tgt) = inst.tgts.get(0) else {
          return;
        };
        if inst.args.is_empty() {
          self.set_var(state, tgt, NullabilityMask::TOP);
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
        self.set_var(state, tgt, mask);
      }
      _ => {}
    }
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

    // Find the defining instruction for the boolean condition value.
    let mut cmp: Option<(&Arg, BinOp, &Arg)> = None;
    for inst in pred_block.iter().rev() {
      if inst.t == InstTyp::CondGoto {
        continue;
      }
      if inst.t != InstTyp::Bin {
        continue;
      }
      if inst.tgts.get(0).copied() != Some(*cond_var) {
        continue;
      }
      let (_tgt, left, op, right) = inst.as_bin();
      cmp = Some((left, op, right));
      break;
    }

    let Some((left, op, right)) = cmp else {
      return next;
    };

    let (var, is_null) = match (left, right) {
      (Arg::Var(v), Arg::Const(Const::Null)) | (Arg::Const(Const::Null), Arg::Var(v)) => (*v, true),
      (Arg::Var(v), Arg::Const(Const::Undefined))
      | (Arg::Const(Const::Undefined), Arg::Var(v)) => (*v, false),
      _ => return next,
    };

    let nullish = NullabilityMask::NULL | NullabilityMask::UNDEF;
    let non_nullish = NullabilityMask::OTHER;

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
