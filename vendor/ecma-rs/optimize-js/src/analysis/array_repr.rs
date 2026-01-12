use crate::analysis::alias::{AbstractLoc, AliasResult};
use crate::analysis::escape;
use crate::analysis::loop_info::LoopInfo;
use crate::analysis::value_types::ValueTypeSummaries;
use crate::cfg::cfg::Cfg;
use crate::dom::Dom;
use crate::il::inst::{
  Arg, ArrayElemRepr, BinOp, Const, Inst, InstTyp, Purity, ValueTypeSummary, VectorizeHint,
  VectorizeNoReason,
};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct ArrayReprResult {
  /// Per-variable inferred element representation for values that are definitely
  /// arrays. `None` means "not definitely an array".
  pub vars: Vec<Option<ArrayElemRepr>>,
}

impl ArrayReprResult {
  pub fn elem_repr_of_var(&self, var: u32) -> Option<ArrayElemRepr> {
    self.vars.get(var as usize).copied().flatten()
  }
}

fn cfg_var_count(cfg: &Cfg) -> usize {
  let mut max: Option<u32> = None;
  for (_, block) in cfg.bblocks.all() {
    for inst in block.iter() {
      for &tgt in &inst.tgts {
        max = Some(max.map_or(tgt, |m| m.max(tgt)));
      }
      for arg in &inst.args {
        if let Arg::Var(v) = arg {
          max = Some(max.map_or(*v, |m| m.max(*v)));
        }
      }
    }
  }
  max.map(|m| m as usize + 1).unwrap_or(0)
}

fn cfg_block_labels_sorted(cfg: &Cfg) -> Vec<u32> {
  let mut labels: Vec<u32> = cfg.bblocks.all().map(|(label, _)| label).collect();
  labels.sort_unstable();
  labels
}

fn is_array_alloc_builder(callee: &Arg) -> bool {
  matches!(callee, Arg::Builtin(name) if name == "__optimize_js_array")
}

fn is_array_hole(arg: &Arg) -> bool {
  matches!(arg, Arg::Builtin(name) if name == "__optimize_js_array_hole")
}

fn is_length_prop(arg: &Arg) -> bool {
  matches!(arg, Arg::Const(Const::Str(s)) if s == "length")
}

fn is_index_prop(types: &ValueTypeSummaries, arg: &Arg) -> bool {
  match arg {
    Arg::Const(Const::Num(_)) => true,
    Arg::Var(v) => types.var(*v).is_some_and(|ty| ty.is_definitely_number()),
    _ => false,
  }
}

fn elem_repr_from_summary(summary: ValueTypeSummary, unknown_is_conflict: bool) -> Option<ArrayElemRepr> {
  if summary.is_unknown() {
    return unknown_is_conflict.then_some(ArrayElemRepr::Unknown);
  }
  if summary.is_definitely_number() {
    return Some(ArrayElemRepr::F64);
  }
  if summary.is_definitely_bigint() {
    return Some(ArrayElemRepr::I64);
  }
  if summary.is_definitely_string()
    || summary == ValueTypeSummary::OBJECT
    || summary == ValueTypeSummary::FUNCTION
    || summary == ValueTypeSummary::SYMBOL
  {
    return Some(ArrayElemRepr::Ptr);
  }
  if summary == ValueTypeSummary::BOOLEAN {
    return Some(ArrayElemRepr::I32);
  }
  // Union / nullish / other primitives: cannot represent as a single packed array.
  Some(ArrayElemRepr::Unknown)
}

fn elem_repr_from_value(types: &ValueTypeSummaries, arg: &Arg, unknown_is_conflict: bool) -> Option<ArrayElemRepr> {
  if is_array_hole(arg) {
    return None;
  }
  let summary = types.arg(arg).unwrap_or(ValueTypeSummary::UNKNOWN);
  elem_repr_from_summary(summary, unknown_is_conflict)
}

fn join_elem_repr(a: ArrayElemRepr, b: ArrayElemRepr) -> ArrayElemRepr {
  if a == ArrayElemRepr::Unknown || b == ArrayElemRepr::Unknown {
    return ArrayElemRepr::Unknown;
  }
  if a == b { a } else { ArrayElemRepr::Unknown }
}

fn join_elem_evidence(slot: &mut Option<ArrayElemRepr>, new: ArrayElemRepr) {
  match slot {
    None => *slot = Some(new),
    Some(existing) => *slot = Some(join_elem_repr(*existing, new)),
  }
}

#[derive(Clone, Debug)]
struct ArrayAllocInfo {
  alloc_var: u32,
  elem: Option<ArrayElemRepr>,
}

fn array_allocs_for_definitely_array_var(
  alias: &AliasResult,
  var: u32,
  arrays: &BTreeMap<AbstractLoc, ArrayAllocInfo>,
) -> Option<Vec<AbstractLoc>> {
  let pts = alias.points_to.get(&var)?;
  if pts.is_top() || pts.is_empty() {
    return None;
  }
  // Only treat this as "definitely an array" when every possible location is an
  // array allocation we recognize.
  if pts.iter().any(|loc| !arrays.contains_key(loc)) {
    return None;
  }
  Some(pts.iter().cloned().collect())
}

fn array_allocs_in_points_to(
  alias: &AliasResult,
  var: u32,
  arrays: &BTreeMap<AbstractLoc, ArrayAllocInfo>,
) -> Vec<AbstractLoc> {
  let Some(pts) = alias.points_to.get(&var) else {
    return Vec::new();
  };
  if pts.is_top() || pts.is_empty() {
    return Vec::new();
  }
  pts
    .iter()
    .filter(|loc| arrays.contains_key(*loc))
    .cloned()
    .collect()
}

pub fn analyze_array_repr(
  cfg: &Cfg,
  alias: &AliasResult,
  escapes: Option<&escape::EscapeResult>,
) -> ArrayReprResult {
  let var_count = cfg_var_count(cfg);
  let types = ValueTypeSummaries::new(cfg);

  // 1) Collect all local array allocation sites and initial element evidence.
  //
  // We key allocations using the same stable `AbstractLoc::Alloc { block, inst_idx }`
  // representation as `analysis::alias`.
  let mut arrays: BTreeMap<AbstractLoc, ArrayAllocInfo> = BTreeMap::new();
  for label in cfg_block_labels_sorted(cfg) {
    let Some(block) = cfg.bblocks.maybe_get(label) else {
      continue;
    };
    for (inst_idx, inst) in block.iter().enumerate() {
      if inst.t != InstTyp::Call {
        continue;
      }
      let (tgt, callee, _this, args, spreads) = inst.as_call();
      let Some(tgt) = tgt else {
        continue;
      };
      if !is_array_alloc_builder(callee) {
        continue;
      }

      let mut elem: Option<ArrayElemRepr> = None;
      // Any spread into the array literal may introduce unknown elements.
      if !spreads.is_empty() {
        elem = Some(ArrayElemRepr::Unknown);
      }
      for (idx, arg) in args.iter().enumerate() {
        if is_array_hole(arg) {
          continue;
        }
        // Spread arguments are indexed into the call args including `callee` and `this`.
        if spreads.contains(&(idx + 2)) {
          join_elem_evidence(&mut elem, ArrayElemRepr::Unknown);
          continue;
        }
        if let Some(r) = elem_repr_from_value(&types, arg, true) {
          join_elem_evidence(&mut elem, r);
        }
      }

      arrays.insert(
        AbstractLoc::Alloc {
          block: label,
          inst_idx: inst_idx as u32,
        },
        ArrayAllocInfo { alloc_var: tgt, elem },
      );
    }
  }

  // 2) Collect evidence from array element loads/stores.
  //
  // This allows us to infer element representations even when the array literal
  // is empty, as long as downstream code performs typed element accesses.
  for label in cfg_block_labels_sorted(cfg) {
    let Some(block) = cfg.bblocks.maybe_get(label) else {
      continue;
    };
    for inst in block.iter() {
      match inst.t {
        InstTyp::Bin if inst.bin_op == BinOp::GetProp => {
          let (tgt, obj, _op, prop) = inst.as_bin();
          let Some(obj_var) = obj.maybe_var() else {
            continue;
          };
          if is_length_prop(prop) {
            continue;
          }
          if !is_index_prop(&types, prop) {
            continue;
          }

          let Some(locs) = array_allocs_for_definitely_array_var(alias, obj_var, &arrays) else {
            continue;
          };
          let summary = types.var(tgt).unwrap_or(ValueTypeSummary::UNKNOWN);
          let Some(evidence) = elem_repr_from_summary(summary, false) else {
            continue;
          };
          for loc in locs {
            if let Some(info) = arrays.get_mut(&loc) {
              join_elem_evidence(&mut info.elem, evidence);
            }
          }
        }
        InstTyp::PropAssign => {
          let (obj, prop, val) = inst.as_prop_assign();
          let Some(obj_var) = obj.maybe_var() else {
            continue;
          };
          if !is_index_prop(&types, prop) {
            continue;
          }
          let Some(locs) = array_allocs_for_definitely_array_var(alias, obj_var, &arrays) else {
            continue;
          };
          let Some(evidence) = elem_repr_from_value(&types, val, true) else {
            continue;
          };
          for loc in locs {
            if let Some(info) = arrays.get_mut(&loc) {
              join_elem_evidence(&mut info.elem, evidence);
            }
          }
        }
        _ => {}
      }
    }
  }

  // 3) Downgrade escaped arrays (their contents can be observed/mutated outside
  // the current analysis scope).
  if let Some(escapes) = escapes {
    for info in arrays.values_mut() {
      if escapes
        .get(&info.alloc_var)
        .is_some_and(|state| state.escapes())
      {
        info.elem = Some(ArrayElemRepr::Unknown);
      }
    }
  }

  // 4) Downgrade arrays passed to impure calls (may mutate contents).
  for label in cfg_block_labels_sorted(cfg) {
    let Some(block) = cfg.bblocks.maybe_get(label) else {
      continue;
    };
    for inst in block.iter() {
      if inst.t != InstTyp::Call {
        continue;
      }
      let (_tgt, callee, _this, _args, _spreads) = inst.as_call();
      // Marker builders are treated as safe.
      if is_array_alloc_builder(callee) {
        continue;
      }
      if inst.meta.callee_purity != Purity::Impure {
        continue;
      }

      // NOTE: we conservatively treat `this` + all arguments as potentially
      // mutating aliases.
      for arg in inst.args.iter().skip(1) {
        let Some(var) = arg.maybe_var() else {
          continue;
        };
        let locs = array_allocs_in_points_to(alias, var, &arrays);
        for loc in locs {
          if let Some(info) = arrays.get_mut(&loc) {
            info.elem = Some(ArrayElemRepr::Unknown);
          }
        }
      }
    }
  }

  // 5) Map per-allocation results back onto SSA vars.
  let mut vars = vec![None; var_count];
  for var in 0..(var_count as u32) {
    let Some(pts) = alias.points_to.get(&var) else {
      continue;
    };
    if pts.is_top() || pts.is_empty() {
      continue;
    }
    if pts.iter().any(|loc| !arrays.contains_key(loc)) {
      continue;
    }

    let mut merged: Option<ArrayElemRepr> = None;
    for loc in pts.iter() {
      let info = arrays.get(loc).expect("checked contains_key");
      let repr = info.elem.unwrap_or(ArrayElemRepr::Unknown);
      merged = Some(match merged {
        None => repr,
        Some(existing) => join_elem_repr(existing, repr),
      });
    }

    vars[var as usize] = Some(merged.unwrap_or(ArrayElemRepr::Unknown));
  }

  ArrayReprResult { vars }
}

pub fn annotate_cfg_array_elem_repr(cfg: &mut Cfg, arrays: &ArrayReprResult) {
  let types = ValueTypeSummaries::new(cfg);
  for label in cfg_block_labels_sorted(cfg) {
    let block = cfg.bblocks.get_mut(label);
    for inst in block.iter_mut() {
      match inst.t {
        InstTyp::Bin if inst.bin_op == BinOp::GetProp => {
          let (_tgt, obj, _op, prop) = inst.as_bin();
          let Some(obj_var) = obj.maybe_var() else {
            continue;
          };
          let Some(elem_repr) = arrays.elem_repr_of_var(obj_var) else {
            continue;
          };
          if is_length_prop(prop) || is_index_prop(&types, prop) {
            inst.meta.array_elem_repr = Some(elem_repr);
          }
        }
        InstTyp::PropAssign => {
          let (obj, prop, _val) = inst.as_prop_assign();
          let Some(obj_var) = obj.maybe_var() else {
            continue;
          };
          let Some(elem_repr) = arrays.elem_repr_of_var(obj_var) else {
            continue;
          };
          if is_index_prop(&types, prop) {
            inst.meta.array_elem_repr = Some(elem_repr);
          }
        }
        _ => {}
      }
    }
  }
}

fn find_last_cond_goto_idx(block: &[Inst]) -> Option<usize> {
  block.iter().enumerate().rev().find_map(|(idx, inst)| {
    (inst.t == InstTyp::CondGoto).then_some(idx)
  })
}

fn find_def_in_block<'a>(block: &'a [Inst], var: u32, before_idx: usize) -> Option<&'a Inst> {
  block
    .iter()
    .take(before_idx)
    .rev()
    .find(|inst| inst.tgts.first() == Some(&var))
}

fn resolve_var_through_var_assigns(block: &[Inst], mut var: u32) -> u32 {
  for _ in 0..8 {
    let Some(def) = find_def_in_block(block, var, block.len()) else {
      break;
    };
    if def.t != InstTyp::VarAssign {
      break;
    }
    let (_tgt, arg) = def.as_var_assign();
    let Some(src) = arg.maybe_var() else {
      break;
    };
    var = src;
  }
  var
}

fn is_const_one(arg: &Arg) -> bool {
  matches!(arg, Arg::Const(Const::Num(n)) if n.0 == 1.0)
}

fn loop_vectorize_hint(
  cfg: &Cfg,
  loop_header: u32,
  loop_latches: &[u32],
  loop_nodes: &[u32],
  arrays: &ArrayReprResult,
  alias: &AliasResult,
  types: &ValueTypeSummaries,
) -> VectorizeHint {
  let Some(header_block) = cfg.bblocks.maybe_get(loop_header) else {
    return VectorizeHint::No {
      reason: VectorizeNoReason::Unknown,
    };
  };
  let Some(cond_idx) = find_last_cond_goto_idx(header_block) else {
    return VectorizeHint::No {
      reason: VectorizeNoReason::Unknown,
    };
  };
  let Some(mut cond_var) = header_block[cond_idx].args[0].maybe_var() else {
    return VectorizeHint::No {
      reason: VectorizeNoReason::Unknown,
    };
  };

  // Chase through simple var assignments (SSA cleanup sometimes inserts copies).
  cond_var = resolve_var_through_var_assigns(header_block, cond_var);
  let Some(def) = find_def_in_block(header_block, cond_var, cond_idx) else {
    return VectorizeHint::No {
      reason: VectorizeNoReason::Unknown,
    };
  };
  if def.t != InstTyp::Bin {
    return VectorizeHint::No {
      reason: VectorizeNoReason::Unknown,
    };
  }
  let (_tgt, left, op, right) = def.as_bin();
  if op != BinOp::Lt {
    return VectorizeHint::No {
      reason: VectorizeNoReason::Unknown,
    };
  }
  let Some(index_var) = left.maybe_var() else {
    return VectorizeHint::No {
      reason: VectorizeNoReason::Unknown,
    };
  };
  let index_var = resolve_var_through_var_assigns(header_block, index_var);
  let _ = right;

  // Require a single latch so we can prove contiguity.
  let [latch] = loop_latches else {
    return VectorizeHint::No {
      reason: VectorizeNoReason::NonContiguousIndex,
    };
  };

  // Find the induction phi node for `index_var` in the header.
  let phi = header_block
    .iter()
    .take_while(|inst| inst.t == InstTyp::Phi)
    .find(|inst| inst.tgts.get(0) == Some(&index_var));
  let Some(phi) = phi else {
    return VectorizeHint::No {
      reason: VectorizeNoReason::NonContiguousIndex,
    };
  };
  let latch_pos = phi.labels.iter().position(|l| l == latch);
  let Some(latch_pos) = latch_pos else {
    return VectorizeHint::No {
      reason: VectorizeNoReason::NonContiguousIndex,
    };
  };
  let Some(mut index_next) = phi.args.get(latch_pos).and_then(|a| a.maybe_var()) else {
    return VectorizeHint::No {
      reason: VectorizeNoReason::NonContiguousIndex,
    };
  };

  let Some(latch_block) = cfg.bblocks.maybe_get(*latch) else {
    return VectorizeHint::No {
      reason: VectorizeNoReason::NonContiguousIndex,
    };
  };
  index_next = resolve_var_through_var_assigns(latch_block, index_next);
  let Some(next_def) = find_def_in_block(latch_block, index_next, latch_block.len()) else {
    return VectorizeHint::No {
      reason: VectorizeNoReason::NonContiguousIndex,
    };
  };
  if next_def.t != InstTyp::Bin {
    return VectorizeHint::No {
      reason: VectorizeNoReason::NonContiguousIndex,
    };
  }
  let (_tgt, step_left, step_op, step_right) = next_def.as_bin();
  let is_plus_one = step_op == BinOp::Add
    && ((step_left.maybe_var() == Some(index_var) && is_const_one(step_right))
      || (step_right.maybe_var() == Some(index_var) && is_const_one(step_left)));
  if !is_plus_one {
    return VectorizeHint::No {
      reason: VectorizeNoReason::NonContiguousIndex,
    };
  }

  // Collect array element loads indexed by the induction variable.
  let mut indexed_arrays: Vec<u32> = Vec::new();
  for &node in loop_nodes {
    let Some(block) = cfg.bblocks.maybe_get(node) else {
      continue;
    };
    for inst in block.iter() {
      match inst.t {
        InstTyp::Call if inst.meta.callee_purity == Purity::Impure => {
          return VectorizeHint::No {
            reason: VectorizeNoReason::HasSideEffects,
          };
        }
        InstTyp::PropAssign => {
          return VectorizeHint::No {
            reason: VectorizeNoReason::HasSideEffects,
          };
        }
        InstTyp::Bin if inst.bin_op == BinOp::GetProp => {
          let (_tgt, obj, _op, prop) = inst.as_bin();
          let Some(obj_var) = obj.maybe_var() else {
            continue;
          };
          let Some(prop_var) = prop.maybe_var() else {
            continue;
          };
          if prop_var != index_var {
            continue;
          }
          // Only consider element loads.
          if !is_index_prop(types, prop) {
            continue;
          }
          let Some(elem_repr) = arrays.elem_repr_of_var(obj_var) else {
            continue;
          };
          if !elem_repr.is_numeric() {
            return VectorizeHint::No {
              reason: VectorizeNoReason::NonNumericElems,
            };
          }
          indexed_arrays.push(obj_var);
        }
        _ => {}
      }
    }
  }

  indexed_arrays.sort_unstable();
  indexed_arrays.dedup();

  if indexed_arrays.is_empty() {
    return VectorizeHint::No {
      reason: VectorizeNoReason::Unknown,
    };
  }

  // Prove no aliasing between arrays accessed in the vectorizable loop.
  for i in 0..indexed_arrays.len() {
    for j in (i + 1)..indexed_arrays.len() {
      if alias.may_alias(indexed_arrays[i], indexed_arrays[j]) {
        return VectorizeHint::No {
          reason: VectorizeNoReason::MayAlias,
        };
      }
    }
  }

  VectorizeHint::Yes
}

pub fn annotate_cfg_vectorize_hints(cfg: &mut Cfg, arrays: &ArrayReprResult, alias: &AliasResult) {
  let types = ValueTypeSummaries::new(cfg);
  let dom = Dom::calculate(cfg);
  let loops = LoopInfo::compute(cfg, &dom);

  // Loop nodes are sets; sort to keep the analysis deterministic.
  for l in &loops.loops {
    let mut nodes: Vec<u32> = l.nodes.iter().copied().collect();
    nodes.sort_unstable();
    let hint = loop_vectorize_hint(
      cfg,
      l.header,
      &l.latches,
      &nodes,
      arrays,
      alias,
      &types,
    );

    let block = cfg.bblocks.get_mut(l.header);
    let Some(cond_idx) = find_last_cond_goto_idx(block) else {
      continue;
    };
    block[cond_idx].meta.vectorize_hint = Some(hint);
  }
}
