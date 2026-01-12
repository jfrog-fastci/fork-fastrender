use crate::analysis::escape::EscapeState;
use crate::cfg::cfg::Cfg;
use crate::dom::Dom;
use crate::il::inst::{Arg, BinOp, Const, Inst, InstTyp};
use crate::opt::PassResult;
use ahash::HashSet;
use std::collections::{BTreeMap, BTreeSet};
use std::collections::VecDeque;
use std::sync::LazyLock;
 
static ALLOC_MARKER_BUILTINS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
  HashSet::from_iter([
    "__optimize_js_object",
    "__optimize_js_array",
    "__optimize_js_regex",
    "__optimize_js_template",
    // NOTE: We intentionally exclude helpers like `__optimize_js_new` /
    // `__optimize_js_tagged_template` here because they can invoke user code and/or
    // have complex semantics beyond "allocate a fresh container". Even if escape
    // analysis reports `NoEscape`, stack-allocating such values can be unsound if
    // the callee leaks the allocation via hidden paths.
  ])
});
 
fn next_temp_id(cfg: &Cfg) -> u32 {
  let mut max: Option<u32> = None;
  for (_, block) in cfg.bblocks.all() {
    for inst in block.iter() {
      for &tgt in inst.tgts.iter() {
        max = Some(max.map_or(tgt, |cur| cur.max(tgt)));
      }
      for arg in inst.args.iter() {
        if let Arg::Var(v) = arg {
          max = Some(max.map_or(*v, |cur| cur.max(*v)));
        }
      }
    }
  }
  max.and_then(|v| v.checked_add(1)).unwrap_or(0)
}
 
fn is_alloc_marker_call(inst: &Inst) -> bool {
  if inst.t != InstTyp::Call {
    return false;
  }
  let (tgt, callee, _this, _args, _spreads) = inst.as_call();
  if tgt.is_none() {
    return false;
  }
  matches!(callee, Arg::Builtin(name) if ALLOC_MARKER_BUILTINS.contains(name.as_str()))
}
 
fn is_object_literal_alloc(inst: &Inst) -> bool {
  if inst.t != InstTyp::Call {
    return false;
  }
  let (tgt, callee, _this, _args, spreads) = inst.as_call();
  if tgt.is_none() || !spreads.is_empty() {
    return false;
  }
  matches!(callee, Arg::Builtin(name) if name == "__optimize_js_object")
}
 
fn collect_object_allocs(cfg: &Cfg) -> Vec<(u32, usize, u32)> {
  let mut allocs = Vec::new();
  for label in cfg.graph.labels_sorted() {
    let block = cfg.bblocks.get(label);
    for (idx, inst) in block.iter().enumerate() {
      if !is_object_literal_alloc(inst) {
        continue;
      }
      let Some(&tgt) = inst.tgts.get(0) else {
        continue;
      };
      allocs.push((label, idx, tgt));
    }
  }
  allocs.sort_by_key(|(label, idx, _)| (*label, *idx));
  allocs
}
 
fn build_alias_set(cfg: &Cfg, alloc_var: u32) -> HashSet<u32> {
  let mut aliases = HashSet::default();
  aliases.insert(alloc_var);
 
  // Deterministic fixpoint by repeatedly scanning blocks in sorted order.
  let mut changed = true;
  while changed {
    changed = false;
    for label in cfg.graph.labels_sorted() {
      let block = cfg.bblocks.get(label);
      for inst in block.iter() {
        if inst.t != InstTyp::VarAssign {
          continue;
        }
        let (tgt, arg) = inst.as_var_assign();
        let Arg::Var(src) = arg else {
          continue;
        };
        if aliases.contains(src) && aliases.insert(tgt) {
          changed = true;
        }
      }
    }
  }
  aliases
}
 
fn parse_object_literal_initializers(inst: &Inst) -> Option<Vec<(String, Arg)>> {
  if !is_object_literal_alloc(inst) {
    return None;
  }

  let (_tgt, _callee, _this, args, _spreads) = inst.as_call();
  // `__optimize_js_object` encodes each property as a triple:
  //   marker, key, value
  // where marker is one of:
  //   - __optimize_js_object_prop
  //   - __optimize_js_object_prop_computed
  //   - __optimize_js_object_spread
  // Preserve property order from the literal. This isn't currently observable in our IR encoding
  // because initializer expressions are evaluated before the allocation call is emitted, but
  // keeping the literal order makes the scalar replacement semantics easier to reason about and
  // matches JS source semantics.
  let mut out: Vec<(String, Arg)> = Vec::new();
  let mut seen: BTreeSet<String> = BTreeSet::new();
  for chunk in args.chunks(3) {
    if chunk.len() != 3 {
      return None;
    }
    let (marker, key, value) = (&chunk[0], &chunk[1], &chunk[2]);
    let Arg::Builtin(marker) = marker else {
      return None;
    };
    if marker != "__optimize_js_object_prop" {
      return None;
    }
    let Arg::Const(Const::Str(key)) = key else {
      return None;
    };
    // `__proto__` has special semantics in JS object literals; stay conservative.
    if key == "__proto__" {
      return None;
    }
    if !seen.insert(key.clone()) {
      // Duplicate keys are legal in JS but introduce subtle ordering/overwrite
      // semantics; keep the initial scalar replacement conservative.
      return None;
    }
    out.push((key.clone(), value.clone()));
  }
  Some(out)
}
 
#[derive(Clone, Copy, Debug)]
struct PhiInfo {
  tgt: u32,
  idx: usize,
}
 
#[derive(Debug)]
struct ScalarReplacePlan {
  alloc_block: u32,
  alloc_var: u32,
  aliases: HashSet<u32>,
  // Field keys in deterministic order.
  fields: Vec<String>,
  // Field initializers from the literal allocation site.
  init: Vec<(String, Arg)>,
  // Def blocks per field (for phi insertion), including the allocation block.
  def_blocks: BTreeMap<String, BTreeSet<u32>>,
}

fn build_plan(cfg: &Cfg, alloc_block: u32, alloc_inst: &Inst, alloc_var: u32) -> Option<ScalarReplacePlan> {
  if alloc_inst.meta.result_escape != Some(EscapeState::NoEscape) {
    return None;
  }
 
  let init = parse_object_literal_initializers(alloc_inst)?;
  let aliases = build_alias_set(cfg, alloc_var);

  // Collect all accessed field names + definition blocks for stores.
  let mut fields: BTreeSet<String> = init.iter().map(|(k, _)| k.clone()).collect();
  let mut def_blocks: BTreeMap<String, BTreeSet<u32>> = BTreeMap::new();
 
  // The allocation block is the implicit "initial definition point" for all
  // scalar-replaced fields (defaulting to `undefined` when not initialized).
  for key in fields.iter() {
    def_blocks.entry(key.clone()).or_default().insert(alloc_block);
  }
 
  for label in cfg.graph.labels_sorted() {
    let block = cfg.bblocks.get(label);
    for inst in block.iter() {
      // Reject any instruction that defines an alias var via something other than
      // a simple `tgt = src` copy.
      for &tgt in inst.tgts.iter() {
        if !aliases.contains(&tgt) {
          continue;
        }
        if tgt == alloc_var {
          // Allocation definition site.
          if !is_object_literal_alloc(inst) {
            return None;
          }
          continue;
        }
        if inst.t != InstTyp::VarAssign {
          return None;
        }
        let (_tgt, arg) = inst.as_var_assign();
        if !matches!(arg, Arg::Var(src) if aliases.contains(src)) {
          return None;
        }
      }
 
      match inst.t {
        InstTyp::VarAssign => {
          let (tgt, arg) = inst.as_var_assign();
          if matches!(arg, Arg::Var(src) if aliases.contains(src)) {
            // SSA alias copy; ok.
            let _ = tgt;
            continue;
          }
          // If this VarAssign uses an alias var in some other way, reject.
          if matches!(arg, Arg::Var(v) if aliases.contains(v)) {
            return None;
          }
        }
        InstTyp::Bin if inst.bin_op == BinOp::GetProp => {
          let (_tgt, obj, _op, prop) = inst.as_bin();
          // FieldLoad(obj, field)
          if matches!(obj, Arg::Var(v) if aliases.contains(v)) {
            let Arg::Const(Const::Str(field)) = prop else {
              return None;
            };
            if field == "__proto__" {
              return None;
            }
            fields.insert(field.clone());
            def_blocks
              .entry(field.clone())
              .or_default()
              .insert(alloc_block);
          } else {
            // If this instruction uses any alias vars anywhere, reject.
            if inst
              .args
              .iter()
              .any(|arg| matches!(arg, Arg::Var(v) if aliases.contains(v)))
            {
              return None;
            }
          }
        }
        InstTyp::PropAssign => {
          let (obj, prop, val) = inst.as_prop_assign();
          // FieldStore(obj, field, value)
          if matches!(obj, Arg::Var(v) if aliases.contains(v)) {
            // Reject self-referential stores (`o.x = o`).
            if matches!(val, Arg::Var(v) if aliases.contains(v)) {
              return None;
            }
            let Arg::Const(Const::Str(field)) = prop else {
              return None;
            };
            if field == "__proto__" {
              return None;
            }
            fields.insert(field.clone());
            def_blocks
              .entry(field.clone())
              .or_default()
              .insert(alloc_block);
            def_blocks.entry(field.clone()).or_default().insert(label);
          } else {
            // If this instruction uses any alias vars anywhere, reject.
            if inst
              .args
              .iter()
              .any(|arg| matches!(arg, Arg::Var(v) if aliases.contains(v)))
            {
              return None;
            }
          }
        }
        _ => {
          if inst
            .args
            .iter()
            .any(|arg| matches!(arg, Arg::Var(v) if aliases.contains(v)))
          {
            return None;
          }
        }
      }
    }
  }
 
  // Ensure every discovered field has a definition set seeded with the allocation block.
  for key in fields.iter() {
    def_blocks.entry(key.clone()).or_default().insert(alloc_block);
  }
 
  Some(ScalarReplacePlan {
    alloc_block,
    alloc_var,
    aliases,
    fields: fields.into_iter().collect(),
    init,
    def_blocks,
  })
}
 
fn insert_phi_nodes(
  cfg: &mut Cfg,
  dom: &Dom,
  domfront: &BTreeMap<u32, BTreeSet<u32>>,
  plan: &ScalarReplacePlan,
  next_temp: &mut u32,
) -> BTreeMap<u32, BTreeMap<String, PhiInfo>> {
  let dominates = dom.dominates_graph();
  let mut phi_by_block: BTreeMap<u32, BTreeMap<String, PhiInfo>> = BTreeMap::new();
 
  // Collect all required phi nodes per block, per field.
  let mut required: BTreeMap<u32, BTreeSet<String>> = BTreeMap::new();
 
  for field in plan.fields.iter() {
    let mut inserted: BTreeSet<u32> = BTreeSet::new();
    let mut work: VecDeque<u32> = {
      let mut items: Vec<u32> = plan
        .def_blocks
        .get(field)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .collect();
      items.sort_unstable();
      VecDeque::from(items)
    };
    let mut seen: HashSet<u32> = HashSet::default();
    for &d in work.iter() {
      seen.insert(d);
    }
 
    while let Some(d) = work.pop_front() {
      let Some(frontier) = domfront.get(&d) else {
        continue;
      };
      for &y in frontier.iter() {
        if !dominates.dominates(plan.alloc_block, y) {
          continue;
        }
        // Avoid inserting Phi nodes into the allocation block unless all of its
        // predecessors are also dominated by the allocation block (otherwise we
        // would not visit those predecessors during the rename traversal, and
        // the Phi would remain incomplete).
        if y == plan.alloc_block {
          let parents = cfg.graph.parents_sorted(y);
          if parents
            .iter()
            .any(|&p| !dominates.dominates(plan.alloc_block, p))
          {
            continue;
          }
        }
        if !inserted.insert(y) {
          continue;
        }
        required.entry(y).or_default().insert(field.clone());
        if seen.insert(y) {
          work.push_back(y);
        }
      }
    }
  }
 
  // Insert phi nodes deterministically by (block label, field key).
  for (label, fields) in required.iter() {
    let bblock = cfg.bblocks.get_mut(*label);
    // Insert after any existing phi nodes.
    let mut insert_pos = 0usize;
    while insert_pos < bblock.len() && bblock[insert_pos].t == InstTyp::Phi {
      insert_pos += 1;
    }
 
    for field in fields.iter() {
      let tgt = *next_temp;
      *next_temp = next_temp
        .checked_add(1)
        .expect("temp id overflow while inserting scalar replacement phis");
 
      let phi = Inst::phi_empty(tgt);
      bblock.insert(insert_pos, phi);
      phi_by_block
        .entry(*label)
        .or_default()
        .insert(field.clone(), PhiInfo { tgt, idx: insert_pos });
      insert_pos += 1;
    }
  }
 
  phi_by_block
}
 
fn scalar_replace_object(
  cfg: &mut Cfg,
  dom: &Dom,
  domfront: &BTreeMap<u32, BTreeSet<u32>>,
  alloc_var: u32,
  next_temp: &mut u32,
) -> bool {
  // Find the allocation site.
  let mut alloc_site: Option<(u32, usize, Inst)> = None;
  for label in cfg.graph.labels_sorted() {
    let block = cfg.bblocks.get(label);
    for (idx, inst) in block.iter().enumerate() {
      if !is_object_literal_alloc(inst) {
        continue;
      }
      if inst.tgts.get(0).copied() != Some(alloc_var) {
        continue;
      }
      alloc_site = Some((label, idx, inst.clone()));
      break;
    }
    if alloc_site.is_some() {
      break;
    }
  }
  let Some((alloc_block, _alloc_inst_idx, alloc_inst)) = alloc_site else {
    return false;
  };
 
  let Some(plan) = build_plan(cfg, alloc_block, &alloc_inst, alloc_var) else {
    return false;
  };
 
  // Precompute dominance frontiers in deterministic BTreeMap form (so our phi insertion doesn't
  // depend on hash iteration order).
  let phi_by_block = insert_phi_nodes(cfg, dom, domfront, &plan, next_temp);
 
  // Field value stacks during the dominator-tree rename traversal.
  let mut stacks: BTreeMap<String, Vec<u32>> =
    plan.fields.iter().map(|f| (f.clone(), Vec::new())).collect();
 
  fn current_or_undef(stacks: &BTreeMap<String, Vec<u32>>, field: &str) -> Arg {
    stacks
      .get(field)
      .and_then(|s| s.last().copied())
      .map(Arg::Var)
      .unwrap_or_else(|| Arg::Const(Const::Undefined))
  }
 
  fn visit(
    cfg: &mut Cfg,
    dom: &Dom,
    label: u32,
    plan: &ScalarReplacePlan,
    phi_by_block: &BTreeMap<u32, BTreeMap<String, PhiInfo>>,
    stacks: &mut BTreeMap<String, Vec<u32>>,
    next_temp: &mut u32,
  ) {
    // Track how many values we pushed for each field in this block so we can pop on exit.
    let mut to_pop: BTreeMap<String, usize> = BTreeMap::new();
 
    // Phi nodes at block entry define the starting field values for this block.
    if let Some(phi_fields) = phi_by_block.get(&label) {
      for (field, info) in phi_fields.iter() {
        stacks
          .get_mut(field)
          .expect("field stack should exist")
          .push(info.tgt);
        *to_pop.entry(field.clone()).or_default() += 1;
      }
    }
 
    // Rewrite instructions in-place. We only mutate within blocks dominated by the allocation,
    // which is exactly the dominator-tree subtree rooted at `alloc_block`.
    let bblock = cfg.bblocks.get_mut(label);
    let mut i = 0usize;
    while i < bblock.len() {
      if bblock[i].t == InstTyp::Phi {
        i += 1;
        continue;
      }
 
      // Remove SSA alias copies of the object reference (`tgt = %alloc`).
      if bblock[i].t == InstTyp::VarAssign {
        let is_alias_copy = {
          let inst = &bblock[i];
          matches!(inst.args.get(0), Some(Arg::Var(src)) if plan.aliases.contains(src))
        };
        if is_alias_copy {
          bblock.remove(i);
          continue;
        }
      }
 
      // Replace the allocation instruction with field initializers.
      if label == plan.alloc_block {
        let is_alloc = {
          let inst = &bblock[i];
          is_object_literal_alloc(inst) && inst.tgts.get(0).copied() == Some(plan.alloc_var)
        };
        if is_alloc {
          // Remove the allocation itself.
          bblock.remove(i);
 
          // Insert per-field initializations.
          //
          // Keep the initialized properties in source order, then initialize any remaining
          // scalar-replaced fields to `undefined` in a deterministic order.
          let mut init_insts = Vec::new();
          let mut initialized: BTreeSet<String> = BTreeSet::new();
          for (field, value) in plan.init.iter() {
            let tgt = *next_temp;
            *next_temp = next_temp
              .checked_add(1)
              .expect("temp id overflow while creating scalar replacement init temps");

            init_insts.push(Inst::var_assign(tgt, value.clone()));
            stacks
              .get_mut(field)
              .expect("field stack should exist")
              .push(tgt);
            *to_pop.entry(field.clone()).or_default() += 1;
            initialized.insert(field.clone());
          }

          for field in plan.fields.iter() {
            if initialized.contains(field) {
              continue;
            }
            let tgt = *next_temp;
            *next_temp = next_temp
              .checked_add(1)
              .expect("temp id overflow while creating scalar replacement init temps");

            init_insts.push(Inst::var_assign(tgt, Arg::Const(Const::Undefined)));
            stacks
              .get_mut(field)
              .expect("field stack should exist")
              .push(tgt);
            *to_pop.entry(field.clone()).or_default() += 1;
          }

          if !init_insts.is_empty() {
            bblock.splice(i..i, init_insts);
            i += plan.fields.len();
          }
          continue;
        }
      }
 
      // Rewrite field stores and loads.
      match bblock[i].t {
        InstTyp::PropAssign => {
          let (obj, prop, val) = {
            let inst = &bblock[i];
            (inst.args[0].clone(), inst.args[1].clone(), inst.args[2].clone())
          };
          if matches!(obj, Arg::Var(v) if plan.aliases.contains(&v)) {
            let Arg::Const(Const::Str(field)) = prop else {
              unreachable!("scalar replacement plan should have rejected non-const PropAssign keys");
            };
            let tgt = *next_temp;
            *next_temp = next_temp
              .checked_add(1)
              .expect("temp id overflow while creating scalar replacement store temps");
 
            bblock[i] = Inst::var_assign(tgt, val);
            stacks
              .get_mut(&field)
              .expect("field stack should exist")
              .push(tgt);
            *to_pop.entry(field).or_default() += 1;
          }
        }
        InstTyp::Bin if bblock[i].bin_op == BinOp::GetProp => {
          let (tgt, obj, _op, prop) = {
            let inst = &bblock[i];
            (inst.tgts[0], inst.args[0].clone(), inst.bin_op, inst.args[1].clone())
          };
          if matches!(obj, Arg::Var(v) if plan.aliases.contains(&v)) {
            let Arg::Const(Const::Str(field)) = prop else {
              unreachable!("scalar replacement plan should have rejected non-const GetProp keys");
            };
            let old_meta = bblock[i].meta.clone();
            let old_value_type = bblock[i].value_type;
 
            let mut new_inst = Inst::var_assign(tgt, current_or_undef(stacks, &field));
            new_inst.value_type = old_value_type;
            new_inst.meta.copy_result_var_metadata_from(&old_meta);
            // Clear stale per-argument consumption metadata (it no longer matches
            // `Inst::args` after the rewrite).
            new_inst.meta.arg_use_modes.clear();
            new_inst.meta.in_place_hint = None;
            bblock[i] = new_inst;
          }
        }
        _ => {}
      }
 
      i += 1;
    }
 
    // Populate phi arguments in successors.
    let succs = cfg.graph.children_sorted(label);
    for succ in succs {
      let Some(phi_fields) = phi_by_block.get(&succ) else {
        continue;
      };
      // Clone in deterministic order so we can mutate the successor block without borrowing issues.
      let phi_fields: Vec<(String, PhiInfo)> = phi_fields
        .iter()
        .map(|(k, v)| (k.clone(), *v))
        .collect();
 
      for (field, info) in phi_fields {
        let arg = current_or_undef(stacks, &field);
        let phi_inst = &mut cfg.bblocks.get_mut(succ)[info.idx];
        phi_inst.insert_phi(label, arg);
      }
    }
 
    // Recurse into dominated children.
    for child in dom.immediately_dominated_by(label) {
      visit(cfg, dom, child, plan, phi_by_block, stacks, next_temp);
    }
 
    // Pop any definitions introduced in this block.
    for (field, cnt) in to_pop {
      let stack = stacks.get_mut(&field).expect("field stack should exist");
      for _ in 0..cnt {
        stack.pop().expect("field stack underflow during scalar replacement");
      }
    }
  }
 
  visit(
    cfg,
    dom,
    plan.alloc_block,
    &plan,
    &phi_by_block,
    &mut stacks,
    next_temp,
  );
 
  #[cfg(debug_assertions)]
  {
    // Ensure no references to the eliminated object (or its SSA aliases) remain.
    for label in cfg.graph.labels_sorted() {
      for inst in cfg.bblocks.get(label) {
        for arg in inst.args.iter() {
          if let Arg::Var(v) = arg {
            debug_assert!(
              !plan.aliases.contains(v),
              "scalar replacement left a use of eliminated object var %{v} in {inst:?}"
            );
          }
        }
      }
    }
  }
 
  true
}
 
fn compute_domfront_btree(dom: &Dom, cfg: &Cfg) -> BTreeMap<u32, BTreeSet<u32>> {
  let domfront = dom.dominance_frontiers(cfg);
  let mut out: BTreeMap<u32, BTreeSet<u32>> = BTreeMap::new();
  for (k, v) in domfront.into_iter() {
    let mut labels: Vec<u32> = v.into_iter().collect();
    labels.sort_unstable();
    out.insert(k, labels.into_iter().collect());
  }
  out
}
 
fn mark_stack_alloc_candidates(cfg: &mut Cfg) -> bool {
  let mut changed = false;
  for label in cfg.graph.labels_sorted() {
    for inst in cfg.bblocks.get_mut(label).iter_mut() {
      if !is_alloc_marker_call(inst) {
        continue;
      }
      if inst.meta.result_escape != Some(EscapeState::NoEscape) {
        continue;
      }
      if inst.meta.stack_alloc_candidate {
        continue;
      }
      inst.meta.stack_alloc_candidate = true;
      changed = true;
    }
  }
  changed
}
 
/// Scalar replacement of non-escaping object literals and stack-allocation hints.
///
/// This pass is intentionally conservative:
/// - it only scalar-replaces `__optimize_js_object` allocations with constant keys
/// - the allocation must be marked `NoEscape` by escape analysis (`InstMeta.result_escape`)
/// - the object reference must only be used via `GetProp`, `PropAssign`, or SSA alias copies
///
/// For any non-escaping internal literal allocation call (e.g. `__optimize_js_object`,
/// `__optimize_js_array`) that is not scalar-replaced, the pass sets
/// `InstMeta.stack_alloc_candidate=true` as a backend hint.
pub fn optpass_scalar_replace(cfg: &mut Cfg) -> PassResult {
  let dom = Dom::calculate(cfg);
  let domfront = compute_domfront_btree(&dom, cfg);
  let mut next_temp = next_temp_id(cfg);
 
  let mut result = PassResult::default();
 
  for (_label, _idx, alloc_var) in collect_object_allocs(cfg) {
    if scalar_replace_object(cfg, &dom, &domfront, alloc_var, &mut next_temp) {
      result.mark_changed();
    }
  }
 
  if mark_stack_alloc_candidates(cfg) {
    result.mark_changed();
  }
 
  result
}
 
#[cfg(test)]
mod tests {
  use super::*;
  use crate::cfg::cfg::{CfgBBlocks, CfgGraph};
 
  fn cfg_single_block(insts: Vec<Inst>) -> Cfg {
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
  fn marks_stack_alloc_candidate_for_noescape_allocations() {
    let mut inst = Inst::call(
      0,
      // Use an allocation marker builtin that is *not* eligible for scalar replacement.
      Arg::Builtin("__optimize_js_array".to_string()),
      Arg::Const(Const::Undefined),
      Vec::new(),
      Vec::new(),
    );
    inst.meta.result_escape = Some(EscapeState::NoEscape);
    let mut cfg = cfg_single_block(vec![inst]);
    let result = optpass_scalar_replace(&mut cfg);
    assert!(result.changed);
    let insts = cfg.bblocks.get(0);
    assert!(insts[0].meta.stack_alloc_candidate);
  }
}
