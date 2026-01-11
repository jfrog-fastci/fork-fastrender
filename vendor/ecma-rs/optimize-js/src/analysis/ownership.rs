use crate::analysis::escape::{analyze_cfg_escapes, EscapeResult, EscapeState};
use crate::cfg::cfg::Cfg;
use crate::il::inst::{Arg, BinOp, Inst, InstTyp, OwnershipState};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

pub type OwnershipResult = BTreeMap<u32, OwnershipState>;

fn inst_defines_value(inst: &Inst) -> Option<u32> {
  inst.tgts.get(0).copied()
}

fn is_allocation_inst(inst: &Inst) -> bool {
  if inst.t != InstTyp::Call {
    return false;
  }
  if inst.tgts.is_empty() {
    return false;
  }
  matches!(
    inst.args.get(0),
    Some(Arg::Builtin(name)) if name.starts_with("__optimize_js_")
  )
}

fn cfg_labels_sorted(cfg: &Cfg) -> Vec<u32> {
  let mut labels = cfg.graph.labels_sorted();
  labels.extend(cfg.bblocks.all().map(|(label, _)| label));
  labels.sort_unstable();
  labels.dedup();
  labels
}

fn collect_defined_vars(cfg: &Cfg) -> BTreeSet<u32> {
  let mut defs = BTreeSet::new();
  for label in cfg_labels_sorted(cfg) {
    let Some(block) = cfg.bblocks.maybe_get(label) else {
      continue;
    };
    for inst in block {
      defs.extend(inst.tgts.iter().copied());
    }
  }
  defs
}

fn collect_used_vars(cfg: &Cfg) -> BTreeSet<u32> {
  let mut uses = BTreeSet::new();
  for label in cfg_labels_sorted(cfg) {
    let Some(block) = cfg.bblocks.maybe_get(label) else {
      continue;
    };
    for inst in block {
      for arg in inst.args.iter() {
        if let Arg::Var(v) = arg {
          uses.insert(*v);
        }
      }
    }
  }
  uses
}

fn collect_input_vars(
  cfg: &Cfg,
  defs: &BTreeSet<u32>,
  uses: &BTreeSet<u32>,
  params: &[u32],
) -> BTreeSet<u32> {
  let mut inputs = BTreeSet::new();

  inputs.extend(params.iter().copied());

  // Temps that are used but never defined in this function (typical for parameters).
  for &v in uses.iter() {
    if !defs.contains(&v) {
      inputs.insert(v);
    }
  }

  // Foreign/unknown loads are treated as coming from outside the function.
  for label in cfg_labels_sorted(cfg) {
    let Some(block) = cfg.bblocks.maybe_get(label) else {
      continue;
    };
    for inst in block {
      match inst.t {
        InstTyp::ForeignLoad | InstTyp::UnknownLoad => {
          if let Some(tgt) = inst_defines_value(inst) {
            inputs.insert(tgt);
          }
        }
        _ => {}
      }
    }
  }

  inputs
}

fn alloc_escape_to_ownership(esc: EscapeState) -> OwnershipState {
  match esc {
    EscapeState::NoEscape | EscapeState::ReturnEscape => OwnershipState::Owned,
    EscapeState::ArgEscape(_) | EscapeState::GlobalEscape | EscapeState::Unknown => OwnershipState::Shared,
  }
}

fn infer_ownership_with_escapes(cfg: &Cfg, params: &[u32], escapes: &EscapeResult) -> OwnershipResult {
  let defs = collect_defined_vars(cfg);
  let uses = collect_used_vars(cfg);
  let inputs = collect_input_vars(cfg, &defs, &uses, params);

  let mut allocations = BTreeSet::new();
  let mut borrowed_defs = BTreeSet::new();
  let mut flow_edges: BTreeMap<u32, Vec<u32>> = BTreeMap::new();

  for label in cfg_labels_sorted(cfg) {
    let Some(block) = cfg.bblocks.maybe_get(label) else {
      continue;
    };
    for inst in block {
      if is_allocation_inst(inst) {
        if let Some(tgt) = inst_defines_value(inst) {
          allocations.insert(tgt);
        }
      }

      if let Some(tgt) = inst_defines_value(inst) {
        match inst.t {
          InstTyp::ForeignLoad | InstTyp::UnknownLoad => {
            borrowed_defs.insert(tgt);
          }
          InstTyp::Bin if inst.bin_op == BinOp::GetProp => {
            borrowed_defs.insert(tgt);
          }
          InstTyp::Call => {
            if !is_allocation_inst(inst) {
              // Unknown call results are treated as coming from outside the function.
              borrowed_defs.insert(tgt);
            }
          }
          _ => {}
        }
      }

      // Propagate ownership through obvious aliasing operations.
      match inst.t {
        InstTyp::VarAssign => {
          if let (Some(tgt), Some(&Arg::Var(src))) = (inst_defines_value(inst), inst.args.get(0)) {
            flow_edges.entry(src).or_default().push(tgt);
          }
        }
        InstTyp::Phi => {
          if let Some(tgt) = inst_defines_value(inst) {
            for arg in &inst.args {
              if let &Arg::Var(src) = arg {
                flow_edges.entry(src).or_default().push(tgt);
              }
            }
          }
        }
        _ => {}
      }
    }
  }

  let mut all_vars = BTreeSet::new();
  all_vars.extend(defs.iter().copied());
  all_vars.extend(uses.iter().copied());

  let mut out: OwnershipResult = OwnershipResult::new();
  for v in all_vars.iter().copied() {
    let state = if inputs.contains(&v) {
      OwnershipState::Borrowed
    } else if allocations.contains(&v) {
      alloc_escape_to_ownership(escapes.get(&v).copied().unwrap_or(EscapeState::NoEscape))
    } else if borrowed_defs.contains(&v) {
      OwnershipState::Borrowed
    } else {
      // Local SSA values default to Owned; this can be degraded by propagation from aliased sources.
      OwnershipState::Owned
    };
    out.insert(v, state);
  }

  // Normalize edge ordering for deterministic fixed-point iteration.
  for targets in flow_edges.values_mut() {
    targets.sort_unstable();
    targets.dedup();
  }

  // Propagate through VarAssign/Phi. This can only make the result more conservative.
  let mut queue: VecDeque<u32> = all_vars.iter().copied().collect();
  while let Some(src) = queue.pop_front() {
    let src_state = out.get(&src).copied().unwrap_or(OwnershipState::Unknown);
    let Some(targets) = flow_edges.get(&src) else {
      continue;
    };
    for &tgt in targets {
      let entry = out.entry(tgt).or_insert(OwnershipState::Owned);
      let next = entry.join(src_state);
      if next != *entry {
        *entry = next;
        queue.push_back(tgt);
      }
    }
  }

  out
}

pub fn analyze_cfg_ownership(cfg: &Cfg) -> OwnershipResult {
  let escapes = analyze_cfg_escapes(cfg);
  analyze_cfg_ownership_with_escapes(cfg, &escapes)
}

/// Ownership analysis with precomputed escape information.
///
/// This is useful for program-wide drivers that already computed escapes (e.g.
/// to expose them via a side table) and want to avoid recomputing them.
pub fn analyze_cfg_ownership_with_escapes(cfg: &Cfg, escapes: &EscapeResult) -> OwnershipResult {
  infer_ownership_with_escapes(cfg, &[], escapes)
}

pub fn analyze_cfg_ownership_with_escapes_and_params(
  cfg: &Cfg,
  params: &[u32],
  escapes: &EscapeResult,
) -> OwnershipResult {
  infer_ownership_with_escapes(cfg, params, escapes)
}

pub fn annotate_cfg_ownership(cfg: &mut Cfg, ownership: &OwnershipResult) {
  let mut labels: Vec<u32> = cfg.bblocks.all().map(|(label, _)| label).collect();
  labels.sort_unstable();
  for label in labels {
    for inst in cfg.bblocks.get_mut(label).iter_mut() {
      let Some(tgt) = inst_defines_value(inst) else {
        continue;
      };
      inst.meta.ownership = ownership.get(&tgt).copied().unwrap_or(OwnershipState::Unknown);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::cfg::cfg::Cfg;
  use crate::cfg::cfg::CfgBBlocks;
  use crate::cfg::cfg::CfgGraph;
  use crate::il::inst::Const;
  use crate::il::inst::UnOp;
  use crate::symbol::semantics::SymbolId;

  fn cfg_with_block0(insts: Vec<Inst>) -> Cfg {
    let graph = CfgGraph::default();
    let mut bblocks = CfgBBlocks::default();
    bblocks.add(0, insts);
    Cfg {
      graph,
      bblocks,
      entry: 0,
    }
  }

  #[test]
  fn single_use_local_is_owned() {
    let cfg = cfg_with_block0(vec![
      Inst::var_assign(1, Arg::Const(Const::Bool(true))),
      Inst::un(2, UnOp::Not, Arg::Var(1)),
    ]);
    let ownership = analyze_cfg_ownership(&cfg);
    assert_eq!(ownership.get(&1), Some(&OwnershipState::Owned));
  }

  #[test]
  fn multi_use_non_escaping_allocation_is_owned() {
    let cfg = cfg_with_block0(vec![
      Inst::call(
        0,
        Arg::Builtin("__optimize_js_object".to_string()),
        Arg::Const(Const::Undefined),
        vec![],
        vec![],
      ),
      Inst::prop_assign(
        Arg::Var(0),
        Arg::Const(Const::Str("x".to_string())),
        Arg::Const(Const::Bool(true)),
      ),
      Inst::prop_assign(
        Arg::Var(0),
        Arg::Const(Const::Str("y".to_string())),
        Arg::Const(Const::Bool(false)),
      ),
    ]);
    let ownership = analyze_cfg_ownership(&cfg);
    assert_eq!(ownership.get(&0), Some(&OwnershipState::Owned));
  }

  #[test]
  fn foreign_load_is_borrowed() {
    let cfg = cfg_with_block0(vec![
      Inst::foreign_load(1, SymbolId(1)),
      Inst::un(2, UnOp::Not, Arg::Var(1)),
    ]);
    let ownership = analyze_cfg_ownership(&cfg);
    assert_eq!(ownership.get(&1), Some(&OwnershipState::Borrowed));
  }

  #[test]
  fn returned_allocation_is_owned() {
    let cfg = cfg_with_block0(vec![
      Inst::call(
        0,
        Arg::Builtin("__optimize_js_object".to_string()),
        Arg::Const(Const::Undefined),
        vec![],
        vec![],
      ),
      Inst::ret(Arg::Var(0)),
    ]);
    let ownership = analyze_cfg_ownership(&cfg);
    assert_eq!(ownership.get(&0), Some(&OwnershipState::Owned));
  }

  #[test]
  fn foreign_store_causes_escape_shared() {
    let cfg = cfg_with_block0(vec![
      Inst::call(
        1,
        Arg::Builtin("__optimize_js_array".to_string()),
        Arg::Const(Const::Undefined),
        vec![],
        vec![],
      ),
      Inst::foreign_store(SymbolId(2), Arg::Var(1)),
    ]);
    let ownership = analyze_cfg_ownership(&cfg);
    assert_eq!(ownership.get(&1), Some(&OwnershipState::Shared));
  }

  #[test]
  fn annotate_writes_inst_meta() {
    let mut cfg = cfg_with_block0(vec![
      Inst::var_assign(1, Arg::Const(Const::Bool(true))),
      Inst::un(2, UnOp::Not, Arg::Var(1)),
      Inst::cond_goto(Arg::Var(2), 0, 0),
    ]);
    let ownership = analyze_cfg_ownership(&cfg);
    annotate_cfg_ownership(&mut cfg, &ownership);
    let insts = cfg.bblocks.get(0);
    assert_eq!(insts[0].meta.ownership, OwnershipState::Owned);
    assert_eq!(insts[1].meta.ownership, OwnershipState::Owned);
  }
}
