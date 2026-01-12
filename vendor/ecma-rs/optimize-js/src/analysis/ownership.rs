use crate::analysis::call_summary::{FnSummary, ReturnKind};
use crate::analysis::escape::{analyze_cfg_escapes, EscapeResult, EscapeState};
use crate::analysis::liveness;
use crate::cfg::cfg::Cfg;
use crate::il::inst::{Arg, ArgUseMode, BinOp, Inst, InstTyp, OwnershipState};
use ahash::{HashMap, HashSet};
use std::collections::{BTreeMap, BTreeSet};
 
/// Ownership classification used by the main optimizer pipeline.
pub type OwnershipResult = BTreeMap<u32, OwnershipState>;
 
/// Ownership classification for a value.
///
/// Note: this is currently an alias for [`OwnershipState`], preserved to keep
/// integration tests stable.
pub type ValueOwnership = OwnershipState;
 
/// Whether an instruction argument should be treated as borrowed or consumed.
///
/// This is a per-argument analogue of [`OwnershipState`] and is used by
/// integration tests. The optimizer metadata uses the same underlying enum.
pub type UseMode = ArgUseMode;
 
/// Combined ownership + argument consumption inference results.
#[derive(Debug, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct OwnershipResults {
  pub var_ownership: HashMap<u32, ValueOwnership>,
  pub arg_use: HashMap<(u32, usize), Vec<UseMode>>,
}
 
#[derive(Clone, Copy, Debug)]
struct AliasFact {
  label: u32,
  inst_idx: usize,
  tgt: u32,
  src: u32,
  src_live_out: bool,
  tgt_live_out: bool,
}
 
fn cfg_labels_sorted(cfg: &Cfg) -> Vec<u32> {
  let mut labels = cfg.graph.labels_sorted();
  labels.extend(cfg.bblocks.all().map(|(label, _)| label));
  labels.sort_unstable();
  labels.dedup();
  labels
}
 
fn inst_defines_value(inst: &Inst) -> Option<u32> {
  inst.tgts.get(0).copied()
}
 
fn is_allocation_inst(inst: &Inst) -> bool {
  match inst.t {
    InstTyp::Call | InstTyp::Invoke => {
      if inst.tgts.is_empty() {
        return false;
      }
      matches!(
        inst.args.get(0),
        Some(Arg::Builtin(name))
          if matches!(
            name.as_str(),
            "__optimize_js_array"
              | "__optimize_js_object"
              | "__optimize_js_regex"
              | "__optimize_js_template"
          )
      )
    }
    // String concatenation produces a fresh string value.
    InstTyp::StringConcat => !inst.tgts.is_empty(),
    _ => false,
  }
}
 
fn call_return_kind(inst: &Inst, call_summaries: Option<&[FnSummary]>) -> ReturnKind {
  if !matches!(inst.t, InstTyp::Call | InstTyp::Invoke) {
    return ReturnKind::Unknown;
  }
 
  let (_tgt, callee, _this, _args, spreads) = match inst.t {
    InstTyp::Call => inst.as_call(),
    InstTyp::Invoke => {
      let (tgt, callee, this, args, spreads, _normal, _exception) = inst.as_invoke();
      (tgt, callee, this, args, spreads)
    }
    _ => unreachable!(),
  };

  let kind = match (call_summaries, callee) {
    (Some(summaries), Arg::Fn(id)) => summaries
      .get(*id)
      .map(|s| s.return_kind)
      .unwrap_or(ReturnKind::Unknown),
    _ => ReturnKind::Unknown,
  };

  // When a call site contains spreads, argument indexing is ambiguous after the
  // first spread. Only preserve `AliasParam(i)` when we can prove `i` is before
  // any spread argument; otherwise stay conservative.
  if let ReturnKind::AliasParam(i) = kind {
    let first_spread_arg = spreads.iter().copied().min().map(|idx| idx.saturating_sub(2));
    if first_spread_arg.is_some_and(|first| i >= first) {
      ReturnKind::Unknown
    } else {
      kind
    }
  } else {
    kind
  }
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
    for inst in block.iter() {
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
 
fn collect_alloc_vars(cfg: &Cfg, call_summaries: Option<&[FnSummary]>) -> BTreeSet<u32> {
  let mut allocs = BTreeSet::<u32>::new();
  for label in cfg_labels_sorted(cfg) {
    let Some(block) = cfg.bblocks.maybe_get(label) else {
      continue;
    };
    for inst in block.iter() {
      if let Some(tgt) = inst_defines_value(inst) {
        if is_allocation_inst(inst)
          || matches!(
            call_return_kind(inst, call_summaries),
            ReturnKind::FreshAlloc
          )
        {
          allocs.insert(tgt);
        }
      }
    }
  }
  allocs
}
 
fn collect_borrowed_defs(cfg: &Cfg, call_summaries: Option<&[FnSummary]>) -> BTreeSet<u32> {
  let mut borrowed = BTreeSet::<u32>::new();
  for label in cfg_labels_sorted(cfg) {
    let Some(block) = cfg.bblocks.maybe_get(label) else {
      continue;
    };
    for inst in block.iter() {
      let Some(tgt) = inst_defines_value(inst) else {
        continue;
      };
      match inst.t {
        InstTyp::ForeignLoad | InstTyp::UnknownLoad => {
          borrowed.insert(tgt);
        }
        InstTyp::Bin if inst.bin_op == BinOp::GetProp => {
          borrowed.insert(tgt);
        }
        #[cfg(feature = "semantic-ops")]
        InstTyp::KnownApiCall { .. } => {
          // KnownApiCall has no callee expression in IL and is conservatively treated as an
          // unknown call for ownership purposes.
          borrowed.insert(tgt);
        }
        #[cfg(any(feature = "native-fusion", feature = "native-array-ops"))]
        InstTyp::ArrayChain => {
          // Array semantic ops may allocate or return aliases into existing arrays. Conservatively
          // treat their results as borrowed (non-owned) values.
          borrowed.insert(tgt);
        }
        InstTyp::Call | InstTyp::Invoke => {
          if is_allocation_inst(inst) {
            continue;
          }
          match call_return_kind(inst, call_summaries) {
            ReturnKind::FreshAlloc | ReturnKind::Const => {}
            ReturnKind::AliasParam(i) => {
              let (_tgt, _callee, _this, args, _spreads) = match inst.t {
                InstTyp::Call => inst.as_call(),
                InstTyp::Invoke => {
                  let (tgt, callee, this, args, spreads, _normal, _exception) = inst.as_invoke();
                  (tgt, callee, this, args, spreads)
                }
                _ => unreachable!(),
              };
              if !matches!(args.get(i), Some(Arg::Var(_))) {
                borrowed.insert(tgt);
              }
            }
            ReturnKind::Unknown => {
              borrowed.insert(tgt);
            }
          }
        }
        _ => {}
      }
    }
  }
  borrowed
}
 
fn collect_phi_edges(cfg: &Cfg) -> Vec<(u32, u32)> {
  let mut edges = Vec::new();
  for label in cfg_labels_sorted(cfg) {
    let Some(block) = cfg.bblocks.maybe_get(label) else {
      continue;
    };
    for inst in block.iter() {
      if inst.t != InstTyp::Phi {
        continue;
      }
      let Some(tgt) = inst_defines_value(inst) else {
        continue;
      };
      for arg in inst.args.iter() {
        if let Arg::Var(src) = arg {
          edges.push((tgt, *src));
        }
      }
    }
  }
  edges.sort_unstable();
  edges.dedup();
  edges
}
 
fn apply_escape_override(var: u32, state: OwnershipState, escapes: &EscapeResult) -> OwnershipState {
  if state != OwnershipState::Owned {
    return state;
  }
  let esc = escapes.get(&var).copied().unwrap_or(EscapeState::NoEscape);
  match esc {
    EscapeState::NoEscape | EscapeState::ReturnEscape => OwnershipState::Owned,
    EscapeState::ArgEscape(_) | EscapeState::GlobalEscape | EscapeState::Unknown => OwnershipState::Shared,
  }
}
 
fn join_var_state(
  states: &mut HashMap<u32, OwnershipState>,
  var: u32,
  add: OwnershipState,
  escapes: &EscapeResult,
) -> bool {
  let current = states.get(&var).copied().unwrap_or(OwnershipState::Unknown);
  let mut next = current.join(add);
  next = apply_escape_override(var, next, escapes);
  if next != current {
    states.insert(var, next);
    true
  } else {
    false
  }
}
 
fn collect_alias_facts(
  cfg: &Cfg,
  live_outs: &liveness::LiveOutBits,
  call_summaries: Option<&[FnSummary]>,
) -> Vec<AliasFact> {
  let mut facts = Vec::new();
  for label in cfg_labels_sorted(cfg) {
    let Some(block) = cfg.bblocks.maybe_get(label) else {
      continue;
    };
    for (inst_idx, inst) in block.iter().enumerate() {
      match inst.t {
        InstTyp::VarAssign => {
          let Some(tgt) = inst_defines_value(inst) else {
            continue;
          };
          let Some(Arg::Var(src)) = inst.args.get(0) else {
            continue;
          };
          let src_live_out = live_outs.contains(label, inst_idx, *src);
          let tgt_live_out = live_outs.contains(label, inst_idx, tgt);
          facts.push(AliasFact {
            label,
            inst_idx,
            tgt,
            src: *src,
            src_live_out,
            tgt_live_out,
          });
        }
        InstTyp::Call => {
          let Some(tgt) = inst_defines_value(inst) else {
            continue;
          };
          let ReturnKind::AliasParam(i) = call_return_kind(inst, call_summaries) else {
            continue;
          };
          let (_tgt, _callee, _this, args, _spreads) = inst.as_call();
          let Some(Arg::Var(src)) = args.get(i) else {
            continue;
          };
          let src_live_out = live_outs.contains(label, inst_idx, *src);
          let tgt_live_out = live_outs.contains(label, inst_idx, tgt);
          facts.push(AliasFact {
            label,
            inst_idx,
            tgt,
            src: *src,
            src_live_out,
            tgt_live_out,
          });
        }
        _ => {}
      }
    }
  }
  facts.sort_by_key(|f| (f.label, f.inst_idx, f.tgt, f.src));
  facts
}
 
fn is_consume_site(inst: &Inst, arg_idx: usize) -> bool {
  match inst.t {
    InstTyp::VarAssign => arg_idx == 0,
    InstTyp::PropAssign => arg_idx == 2,
    InstTyp::Call | InstTyp::Invoke => arg_idx >= 1, // this + call args; callee is always borrowed
    #[cfg(feature = "semantic-ops")]
    InstTyp::KnownApiCall { .. } => true,
    #[cfg(any(feature = "native-fusion", feature = "native-array-ops"))]
    InstTyp::ArrayChain => {
      let _ = arg_idx;
      true // may invoke callbacks / retain references to operands
    }
    InstTyp::Return | InstTyp::Throw => arg_idx == 0,
    InstTyp::ForeignStore | InstTyp::UnknownStore => arg_idx == 0,
    _ => false,
  }
}
 
fn infer_ownership_with_params_and_summaries(
  cfg: &Cfg,
  params: &[u32],
  escapes: &EscapeResult,
  call_summaries: Option<&[FnSummary]>,
) -> OwnershipResults {
  let live_outs = liveness::calculate_live_outs_bits(cfg, &HashMap::default(), &HashSet::default());
  let (all_vars, defs, uses) = collect_vars(cfg);
  let inputs = collect_input_vars(cfg, &defs, &uses, params);
  let alloc_vars = collect_alloc_vars(cfg, call_summaries);
  let borrowed_defs = collect_borrowed_defs(cfg, call_summaries);
  let alias_facts = collect_alias_facts(cfg, &live_outs, call_summaries);
  let phi_edges = collect_phi_edges(cfg);

  // 1) Compute per-variable ownership using a monotone fixpoint.
  let mut states: HashMap<u32, OwnershipState> = HashMap::default();
  for v in all_vars.iter().copied() {
    let state = if inputs.contains(&v) {
      OwnershipState::Borrowed
    } else if alloc_vars.contains(&v) {
      // Allocation ownership may be degraded by escape info below.
      OwnershipState::Owned
    } else if borrowed_defs.contains(&v) {
      OwnershipState::Borrowed
    } else {
      // Local SSA values default to owned; this can be degraded by propagation/aliasing.
      OwnershipState::Owned
    };
    states.insert(v, apply_escape_override(v, state, escapes));
  }
 
  let mut changed = true;
  while changed {
    changed = false;
 
    // Alias propagation: treat as a move when `src` is dead after the instruction.
    for fact in alias_facts.iter() {
      let src_state = states
        .get(&fact.src)
        .copied()
        .unwrap_or(OwnershipState::Unknown);
 
      if !fact.src_live_out {
        // Move-capable: ownership transfers.
        changed |= join_var_state(&mut states, fact.tgt, src_state, escapes);
        continue;
      }
 
      // Copy/alias: if both names remain live after the instruction, the value becomes shared.
      if src_state == OwnershipState::Owned && fact.tgt_live_out {
        changed |= join_var_state(&mut states, fact.src, OwnershipState::Shared, escapes);
        changed |= join_var_state(&mut states, fact.tgt, OwnershipState::Shared, escapes);
        continue;
      }
 
      // Propagate borrowed/shared classification through obvious copies.
      if src_state != OwnershipState::Owned {
        changed |= join_var_state(&mut states, fact.tgt, src_state, escapes);
      }
    }
 
    // Phi propagation: join ownership into the phi output.
    for (tgt, src) in phi_edges.iter() {
      let src_state = states.get(src).copied().unwrap_or(OwnershipState::Unknown);
      changed |= join_var_state(&mut states, *tgt, src_state, escapes);
    }
  }
 
  let mut var_ownership = HashMap::default();
  for v in all_vars.iter().copied() {
    // Avoid exposing `Unknown` outside the fixpoint unless we truly failed to classify a var.
    let state = states.get(&v).copied().unwrap_or(OwnershipState::Unknown);
    let public = match state {
      OwnershipState::Unknown => OwnershipState::Shared,
      other => other,
    };
    var_ownership.insert(v, public);
  }
 
  // 2) Per-instruction argument use modes.
  let mut arg_use: HashMap<(u32, usize), Vec<UseMode>> = HashMap::default();

  for label in cfg_labels_sorted(cfg) {
    let Some(block) = cfg.bblocks.maybe_get(label) else {
      continue;
    };
    for (inst_idx, inst) in block.iter().enumerate() {
      let mut modes = vec![UseMode::Borrow; inst.args.len()];
      for (arg_idx, arg) in inst.args.iter().enumerate() {
        let Arg::Var(v) = arg else {
          continue;
        };
        if !is_consume_site(inst, arg_idx) {
          continue;
        }
        if var_ownership.get(v) != Some(&OwnershipState::Owned) {
          continue;
        }
        if live_outs.contains(label, inst_idx, *v) {
          continue;
        }
        modes[arg_idx] = UseMode::Consume;
      }
 
      arg_use.insert((label, inst_idx), modes);
    }
  }
 
  OwnershipResults {
    var_ownership,
    arg_use,
  }
}
 
fn infer_ownership_with_params(cfg: &Cfg, params: &[u32], escapes: &EscapeResult) -> OwnershipResults {
  infer_ownership_with_params_and_summaries(cfg, params, escapes, None)
}
 
pub fn infer_ownership(cfg: &Cfg, escapes: &EscapeResult) -> OwnershipResults {
  infer_ownership_with_params(cfg, &[], escapes)
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
  analyze_cfg_ownership_with_escapes_and_params(cfg, &[], escapes)
}
 
pub fn analyze_cfg_ownership_with_escapes_and_params(
  cfg: &Cfg,
  params: &[u32],
  escapes: &EscapeResult,
) -> OwnershipResult {
  analyze_cfg_ownership_with_escapes_and_params_and_summaries(cfg, params, escapes, None)
}
 
pub fn analyze_cfg_ownership_with_escapes_and_params_and_summaries(
  cfg: &Cfg,
  params: &[u32],
  escapes: &EscapeResult,
  call_summaries: Option<&[FnSummary]>,
) -> OwnershipResult {
  let inferred = infer_ownership_with_params_and_summaries(cfg, params, escapes, call_summaries);
  let mut out = OwnershipResult::new();
  for (var, own) in inferred.var_ownership {
    out.insert(var, own);
  }
  out
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
    let mut graph = CfgGraph::default();
    graph.ensure_label(0);
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
      Inst::ret(Some(Arg::Var(0))),
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
