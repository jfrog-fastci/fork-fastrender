use crate::analysis::escape::{EscapeResult, EscapeState};
use crate::analysis::liveness;
use crate::cfg::cfg::Cfg;
use crate::il::inst::{Arg, Inst, InstTyp, OwnershipState};
use ahash::{HashMap, HashSet};
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum ValueOwnership {
  Owned,
  Borrowed,
  Shared,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum UseMode {
  Borrow,
  Consume,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum InPlaceHint {
  /// This `VarAssign` is a move of an owned value and can be implemented as a transfer/no-clone in
  /// downstream lowering.
  MoveNoClone { src: u32, tgt: u32 },
}

#[derive(Debug, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct OwnershipResults {
  pub var_ownership: HashMap<u32, ValueOwnership>,
  pub arg_use: HashMap<(u32, usize), Vec<UseMode>>,
  pub in_place: HashMap<(u32, usize), InPlaceHint>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OwnershipLattice {
  Unknown,
  Owned,
  Borrowed,
  Shared,
}

impl OwnershipLattice {
  fn join(self, other: Self) -> Self {
    use OwnershipLattice::*;
    match (self, other) {
      (Shared, _) | (_, Shared) => Shared,
      (Unknown, x) | (x, Unknown) => x,
      (Owned, Owned) => Owned,
      (Borrowed, Borrowed) => Borrowed,
      (Owned, Borrowed) | (Borrowed, Owned) => Shared,
    }
  }
}

fn cfg_labels_sorted(cfg: &Cfg) -> Vec<u32> {
  let mut labels = cfg.graph.labels_sorted();
  labels.extend(cfg.bblocks.all().map(|(label, _)| label));
  labels.sort_unstable();
  labels.dedup();
  labels
}

fn is_internal_alloc_builder(callee: &Arg) -> bool {
  let Arg::Builtin(name) = callee else {
    return false;
  };
  matches!(
    name.as_str(),
    "__optimize_js_array"
      | "__optimize_js_object"
      | "__optimize_js_regex"
      | "__optimize_js_template"
  )
}

fn collect_vars(cfg: &Cfg) -> (BTreeSet<u32>, BTreeSet<u32>, BTreeSet<u32>) {
  let mut all = BTreeSet::<u32>::new();
  let mut defs = BTreeSet::<u32>::new();
  let mut uses = BTreeSet::<u32>::new();

  for label in cfg_labels_sorted(cfg) {
    let Some(block) = cfg.bblocks.maybe_get(label) else {
      continue;
    };
    for inst in block.iter() {
      for &tgt in inst.tgts.iter() {
        all.insert(tgt);
        defs.insert(tgt);
      }
      for arg in inst.args.iter() {
        if let Arg::Var(v) = arg {
          all.insert(*v);
          uses.insert(*v);
        }
      }
    }
  }

  (all, defs, uses)
}

fn collect_borrowed_vars(
  cfg: &Cfg,
  defs: &BTreeSet<u32>,
  uses: &BTreeSet<u32>,
  params: &[u32],
) -> BTreeSet<u32> {
  let mut borrowed: BTreeSet<u32> = uses.iter().copied().filter(|v| !defs.contains(v)).collect();

  // Treat known parameters as borrowed inputs even if they are otherwise considered "defined" in
  // the CFG representation.
  borrowed.extend(params.iter().copied());

  for label in cfg_labels_sorted(cfg) {
    let Some(block) = cfg.bblocks.maybe_get(label) else {
      continue;
    };
    for inst in block.iter() {
      match inst.t {
        InstTyp::ForeignLoad | InstTyp::UnknownLoad => {
          if let Some(&tgt) = inst.tgts.get(0) {
            borrowed.insert(tgt);
          }
        }
        _ => {}
      }
    }
  }

  borrowed
}

fn collect_alloc_vars(cfg: &Cfg) -> BTreeSet<u32> {
  let mut allocs = BTreeSet::<u32>::new();
  for label in cfg_labels_sorted(cfg) {
    let Some(block) = cfg.bblocks.maybe_get(label) else {
      continue;
    };
    for inst in block.iter() {
      if inst.t != InstTyp::Call {
        continue;
      }
      let Some(&tgt) = inst.tgts.get(0) else {
        continue;
      };
      let callee = inst.args.get(0).expect("call must have callee arg");
      if is_internal_alloc_builder(callee) {
        allocs.insert(tgt);
      }
    }
  }
  allocs
}

fn apply_escape_override(var: u32, state: OwnershipLattice, escapes: &EscapeResult) -> OwnershipLattice {
  if state != OwnershipLattice::Owned {
    return state;
  }
  let esc = escapes.get(&var).copied().unwrap_or(EscapeState::NoEscape);
  match esc {
    EscapeState::NoEscape | EscapeState::ReturnEscape => OwnershipLattice::Owned,
    EscapeState::GlobalEscape | EscapeState::ArgEscape(_) | EscapeState::Unknown => OwnershipLattice::Shared,
  }
}

fn join_var_state(
  states: &mut HashMap<u32, OwnershipLattice>,
  var: u32,
  add: OwnershipLattice,
  escapes: &EscapeResult,
) -> bool {
  let current = states.get(&var).copied().unwrap_or(OwnershipLattice::Unknown);
  let mut next = current.join(add);
  next = apply_escape_override(var, next, escapes);
  if next != current {
    states.insert(var, next);
    true
  } else {
    false
  }
}

fn lattice_to_public(state: OwnershipLattice) -> ValueOwnership {
  match state {
    OwnershipLattice::Owned => ValueOwnership::Owned,
    OwnershipLattice::Borrowed => ValueOwnership::Borrowed,
    OwnershipLattice::Shared | OwnershipLattice::Unknown => ValueOwnership::Shared,
  }
}

fn is_consume_site(inst: &Inst, arg_idx: usize) -> bool {
  match inst.t {
    InstTyp::VarAssign => arg_idx == 0,
    InstTyp::PropAssign => arg_idx == 2,
    InstTyp::Call => arg_idx >= 1, // this + call args; callee is always borrowed
    InstTyp::Return | InstTyp::Throw => arg_idx == 0,
    _ => false,
  }
}

fn to_inst_ownership(own: ValueOwnership) -> OwnershipState {
  match own {
    ValueOwnership::Owned => OwnershipState::Owned,
    ValueOwnership::Borrowed => OwnershipState::Borrowed,
    ValueOwnership::Shared => OwnershipState::Shared,
  }
}

#[derive(Clone, Copy, Debug)]
struct VarAssignFact {
  label: u32,
  inst_idx: usize,
  tgt: u32,
  src: u32,
  src_live_out: bool,
  tgt_live_out: bool,
}

fn collect_var_assign_facts(
  cfg: &Cfg,
  live_outs: &HashMap<(u32, usize), HashSet<u32>>,
) -> Vec<VarAssignFact> {
  let mut facts = Vec::new();
  for label in cfg_labels_sorted(cfg) {
    let Some(block) = cfg.bblocks.maybe_get(label) else {
      continue;
    };
    for (inst_idx, inst) in block.iter().enumerate() {
      if inst.t != InstTyp::VarAssign {
        continue;
      }
      let Some(&tgt) = inst.tgts.get(0) else {
        continue;
      };
      let Some(Arg::Var(src)) = inst.args.get(0) else {
        continue;
      };
      let live_out = live_outs.get(&(label, inst_idx));
      let src_live_out = live_out.is_some_and(|s| s.contains(src));
      let tgt_live_out = live_out.is_some_and(|s| s.contains(&tgt));
      facts.push(VarAssignFact {
        label,
        inst_idx,
        tgt,
        src: *src,
        src_live_out,
        tgt_live_out,
      });
    }
  }
  facts.sort_by_key(|f| (f.label, f.inst_idx, f.tgt, f.src));
  facts
}

fn infer_ownership_with_params(cfg: &Cfg, params: &[u32], escapes: &EscapeResult) -> OwnershipResults {
  let live = liveness::calculate_live_in_outs(cfg, &HashMap::default(), &HashSet::default());
  let (all_vars, defs, uses) = collect_vars(cfg);
  let borrowed_vars = collect_borrowed_vars(cfg, &defs, &uses, params);
  let alloc_vars = collect_alloc_vars(cfg);

  // 1) Compute per-variable ownership using a simple monotone fixpoint.
  let mut states: HashMap<u32, OwnershipLattice> = HashMap::default();
  for v in all_vars.iter().copied() {
    states.insert(v, OwnershipLattice::Unknown);
  }

  for v in borrowed_vars.iter().copied() {
    join_var_state(&mut states, v, OwnershipLattice::Borrowed, escapes);
  }
  for v in alloc_vars.iter().copied() {
    join_var_state(&mut states, v, OwnershipLattice::Owned, escapes);
  }

  let var_assigns = collect_var_assign_facts(cfg, &live.live_outs);
  let mut changed = true;
  while changed {
    changed = false;
    for fact in var_assigns.iter() {
      let src_state = states.get(&fact.src).copied().unwrap_or(OwnershipLattice::Unknown);
      if !fact.src_live_out {
        // Move-capable (`src` is dead after this instruction): ownership transfers.
        changed |= join_var_state(&mut states, fact.tgt, src_state, escapes);
        continue;
      }

      // Copy/alias.
      if src_state == OwnershipLattice::Owned && fact.tgt_live_out {
        // The owned value now has multiple live aliases.
        changed |= join_var_state(&mut states, fact.src, OwnershipLattice::Shared, escapes);
        changed |= join_var_state(&mut states, fact.tgt, OwnershipLattice::Shared, escapes);
        continue;
      }

      // Propagate borrowed/shared classification through simple copies.
      if src_state != OwnershipLattice::Owned {
        changed |= join_var_state(&mut states, fact.tgt, src_state, escapes);
      }
    }
  }

  let mut var_ownership = HashMap::default();
  for v in all_vars.iter().copied() {
    let state = states.get(&v).copied().unwrap_or(OwnershipLattice::Unknown);
    var_ownership.insert(v, lattice_to_public(state));
  }

  // 2) Per-instruction argument use modes + in-place hints.
  let mut arg_use: HashMap<(u32, usize), Vec<UseMode>> = HashMap::default();
  let mut in_place: HashMap<(u32, usize), InPlaceHint> = HashMap::default();
  let empty_live_out = HashSet::<u32>::default();

  for label in cfg_labels_sorted(cfg) {
    let Some(block) = cfg.bblocks.maybe_get(label) else {
      continue;
    };
    for (inst_idx, inst) in block.iter().enumerate() {
      let live_out = live
        .live_outs
        .get(&(label, inst_idx))
        .unwrap_or(&empty_live_out);

      let mut modes = vec![UseMode::Borrow; inst.args.len()];
      for (arg_idx, arg) in inst.args.iter().enumerate() {
        let Arg::Var(v) = arg else {
          continue;
        };
        if !is_consume_site(inst, arg_idx) {
          continue;
        }
        if var_ownership.get(v) != Some(&ValueOwnership::Owned) {
          continue;
        }
        if live_out.contains(v) {
          continue;
        }
        modes[arg_idx] = UseMode::Consume;
      }

      arg_use.insert((label, inst_idx), modes.clone());

      if inst.t == InstTyp::VarAssign && modes.get(0) == Some(&UseMode::Consume) {
        let Some(&tgt) = inst.tgts.get(0) else {
          continue;
        };
        let src = match &inst.args[0] {
          Arg::Var(src) => *src,
          _ => continue,
        };
        if var_ownership.get(&src) == Some(&ValueOwnership::Owned) {
          in_place.insert((label, inst_idx), InPlaceHint::MoveNoClone { src, tgt });
        }
      }
    }
  }

  OwnershipResults {
    var_ownership,
    arg_use,
    in_place,
  }
}

pub fn infer_ownership(cfg: &Cfg, escapes: &EscapeResult) -> OwnershipResults {
  infer_ownership_with_params(cfg, &[], escapes)
}

/// Compute ownership using precomputed escape results (and optional parameter list).
///
/// This is used by the program-wide analysis driver to avoid recomputing escape analysis.
pub fn analyze_cfg_ownership_with_escapes_and_params(
  cfg: &Cfg,
  params: &[u32],
  escapes: &EscapeResult,
) -> OwnershipResults {
  infer_ownership_with_params(cfg, params, escapes)
}

/// Convenience wrapper that computes escape analysis internally.
pub fn analyze_cfg_ownership(cfg: &Cfg) -> OwnershipResults {
  let escapes = crate::analysis::escape::analyze_cfg_escapes(cfg);
  infer_ownership(cfg, &escapes)
}

/// Convenience wrapper that uses precomputed escape results.
pub fn analyze_cfg_ownership_with_escapes(cfg: &Cfg, escapes: &EscapeResult) -> OwnershipResults {
  infer_ownership(cfg, escapes)
}

pub fn annotate_cfg_ownership(cfg: &mut Cfg, ownership: &OwnershipResults) {
  for label in cfg_labels_sorted(cfg) {
    let block = cfg.bblocks.get_mut(label);
    for inst in block.iter_mut() {
      let Some(&tgt) = inst.tgts.get(0) else {
        continue;
      };
      let own = ownership
        .var_ownership
        .get(&tgt)
        .copied()
        .unwrap_or(ValueOwnership::Shared);
      inst.meta.ownership = to_inst_ownership(own);
    }
  }
}
