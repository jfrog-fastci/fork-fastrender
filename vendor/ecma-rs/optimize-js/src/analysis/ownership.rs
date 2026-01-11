use crate::analysis::escape::{analyze_cfg_escapes, EscapeResult, EscapeState};
use crate::cfg::cfg::Cfg;
use crate::il::inst::Arg;
use crate::il::inst::Inst;
use crate::il::inst::InstTyp;
use crate::il::inst::OwnershipState;
use ahash::HashMap;
use ahash::HashMapExt;
use ahash::HashSet;
use ahash::HashSetExt;
use std::collections::VecDeque;

pub type OwnershipResult = HashMap<u32, OwnershipState>;

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

fn collect_use_counts(cfg: &Cfg) -> HashMap<u32, usize> {
  let mut counts: HashMap<u32, usize> = HashMap::new();
  for (_label, insts) in cfg.bblocks.all() {
    for inst in insts {
      for arg in &inst.args {
        if let Arg::Var(v) = arg {
          *counts.entry(*v).or_insert(0) += 1;
        }
      }
    }
  }
  counts
}

fn collect_defined_vars(cfg: &Cfg) -> HashSet<u32> {
  let mut defs = HashSet::new();
  for (_label, insts) in cfg.bblocks.all() {
    for inst in insts {
      for &tgt in &inst.tgts {
        defs.insert(tgt);
      }
    }
  }
  defs
}

fn collect_input_vars(cfg: &Cfg, defs: &HashSet<u32>, uses: &HashMap<u32, usize>) -> HashSet<u32> {
  let mut inputs = HashSet::new();

  // Temps that are used but never defined in this function (typical for parameters).
  for &v in uses.keys() {
    if !defs.contains(&v) {
      inputs.insert(v);
    }
  }

  // Foreign/unknown loads are treated as coming from outside the function.
  for (_label, insts) in cfg.bblocks.all() {
    for inst in insts {
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

fn infer_ownership_with_escapes(cfg: &Cfg, escapes: &EscapeResult) -> OwnershipResult {
  let use_counts = collect_use_counts(cfg);
  let defs = collect_defined_vars(cfg);
  let inputs = collect_input_vars(cfg, &defs, &use_counts);

  let mut allocations = HashSet::new();
  let mut flow_edges: HashMap<u32, Vec<u32>> = HashMap::new();

  for (_label, insts) in cfg.bblocks.all() {
    for inst in insts {
      if is_allocation_inst(inst) {
        if let Some(tgt) = inst_defines_value(inst) {
          allocations.insert(tgt);
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

  let mut all_vars = defs;
  all_vars.extend(use_counts.keys().copied());

  let mut out: OwnershipResult = HashMap::new();
  for v in all_vars.iter().copied() {
    let uses = use_counts.get(&v).copied().unwrap_or(0);
    let esc = escapes.get(&v).copied().unwrap_or(EscapeState::NoEscape);

    let state = if inputs.contains(&v) {
      OwnershipState::Borrowed
    } else if esc.escapes() {
      OwnershipState::Shared
    } else if uses > 1 {
      OwnershipState::Shared
    } else if allocations.contains(&v) && uses <= 1 {
      // Special case: allocations with no escape and <= 1 observed uses can be treated as owned.
      OwnershipState::Owned
    } else if uses == 1 {
      OwnershipState::Owned
    } else {
      OwnershipState::Unknown
    };

    out.insert(v, state);
  }

  // Propagate through VarAssign/Phi. This can only make the result more conservative.
  let mut queue: VecDeque<u32> = out.keys().copied().collect();
  while let Some(src) = queue.pop_front() {
    let src_state = out.get(&src).copied().unwrap_or(OwnershipState::Unknown);
    let Some(targets) = flow_edges.get(&src) else {
      continue;
    };
    for &tgt in targets {
      let entry = out.entry(tgt).or_insert(OwnershipState::Unknown);
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
  infer_ownership_with_escapes(cfg, escapes)
}

pub fn annotate_cfg_ownership(cfg: &mut Cfg, ownership: &OwnershipResult) {
  for (_label, insts) in cfg.bblocks.all_mut() {
    for inst in insts {
      let Some(tgt) = inst_defines_value(inst) else {
        continue;
      };
      inst.meta.ownership = ownership
        .get(&tgt)
        .copied()
        .unwrap_or(OwnershipState::Unknown);
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
  fn multi_use_is_shared() {
    let cfg = cfg_with_block0(vec![
      Inst::var_assign(1, Arg::Const(Const::Bool(true))),
      Inst::un(2, UnOp::Not, Arg::Var(1)),
      Inst::un(3, UnOp::Not, Arg::Var(1)),
    ]);
    let ownership = analyze_cfg_ownership(&cfg);
    assert_eq!(ownership.get(&1), Some(&OwnershipState::Shared));
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
