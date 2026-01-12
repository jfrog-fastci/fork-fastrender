use crate::cfg::cfg::Cfg;
use crate::il::inst::{Arg, Const, InPlaceHint, Inst, InstTyp};
use crate::opt::PassResult;
use crate::ssa::phi_simplify::simplify_phis;
use crate::symbol::semantics::SymbolId;
use crate::util::counter::Counter;
use crate::{FnId, InlineOptions, Program, ProgramFunction};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug)]
struct FnSummary {
  inst_count: usize,
}

#[derive(Clone, Debug)]
struct CalleeSnapshot {
  cfg: Cfg,
  params: Vec<u32>,
}

#[derive(Clone, Debug)]
enum CalleeVarDef {
  Alias(u32),
  Fn(FnId),
  Phi(Vec<Arg>),
  Unknown,
}

fn cfg_for_inlining(func: &ProgramFunction) -> &Cfg {
  func.ssa_body.as_ref().unwrap_or(&func.body)
}

fn cfg_for_inlining_mut(func: &mut ProgramFunction) -> &mut Cfg {
  func.ssa_body.as_mut().unwrap_or(&mut func.body)
}

fn collect_reachable_insts(cfg: &Cfg) -> impl Iterator<Item = &Inst> + '_ {
  cfg
    .reverse_postorder()
    .into_iter()
    .flat_map(|label| cfg.bblocks.maybe_get(label).into_iter().flat_map(|bb| bb.iter()))
}

fn collect_constant_foreign_fns(program: &Program) -> BTreeMap<SymbolId, FnId> {
  // Recover direct calls through captured constant function bindings.
  //
  // Inside a nested function, references to captured variables lower to:
  //   %tmp = ForeignLoad(sym)
  // and calls go through the loaded temp. If the captured symbol is only ever assigned a single
  // `Arg::Fn(id)`, treat loads from that symbol as that function ID for inlining purposes.
  let mut candidates = BTreeMap::<SymbolId, FnId>::new();
  let mut invalid = BTreeSet::<SymbolId>::new();

  let mut scan_cfg = |cfg: &Cfg| {
    for inst in collect_reachable_insts(cfg) {
      if inst.t != InstTyp::ForeignStore {
        continue;
      }
      let sym = inst.foreign;
      if invalid.contains(&sym) {
        continue;
      }
      match inst.args.get(0) {
        Some(Arg::Fn(id)) => match candidates.get(&sym).copied() {
          None => {
            candidates.insert(sym, *id);
          }
          Some(existing) if existing == *id => {}
          Some(_) => {
            candidates.remove(&sym);
            invalid.insert(sym);
          }
        },
        _ => {
          candidates.remove(&sym);
          invalid.insert(sym);
        }
      }
    }
  };

  scan_cfg(cfg_for_inlining(&program.top_level));
  for func in &program.functions {
    scan_cfg(cfg_for_inlining(func));
  }

  candidates
}

fn build_callee_var_defs(
  cfg: &Cfg,
  foreign_fns: &BTreeMap<SymbolId, FnId>,
) -> BTreeMap<u32, CalleeVarDef> {
  let mut defs = BTreeMap::<u32, CalleeVarDef>::new();
  for inst in collect_reachable_insts(cfg) {
    let Some(&tgt) = inst.tgts.get(0) else {
      continue;
    };

    let def = match inst.t {
      InstTyp::VarAssign => match &inst.args[0] {
        Arg::Var(src) => CalleeVarDef::Alias(*src),
        Arg::Fn(id) => CalleeVarDef::Fn(*id),
        _ => CalleeVarDef::Unknown,
      },
      InstTyp::Phi => CalleeVarDef::Phi(inst.args.clone()),
      InstTyp::ForeignLoad => foreign_fns
        .get(&inst.foreign)
        .copied()
        .map(CalleeVarDef::Fn)
        .unwrap_or(CalleeVarDef::Unknown),
      _ => CalleeVarDef::Unknown,
    };

    defs
      .entry(tgt)
      .and_modify(|existing| {
        // Non-SSA CFGs may assign the same temp multiple times. Only keep definitions when we can
        // prove the value is constant.
        if !matches!(existing, CalleeVarDef::Unknown) {
          *existing = CalleeVarDef::Unknown;
        }
      })
      .or_insert(def);
  }
  defs
}

fn resolve_fn_id(arg: &Arg, defs: &BTreeMap<u32, CalleeVarDef>, visiting: &mut Vec<u32>) -> Option<FnId> {
  match arg {
    Arg::Fn(id) => Some(*id),
    Arg::Var(v) => resolve_var_fn_id(*v, defs, visiting),
    _ => None,
  }
}

fn resolve_var_fn_id(var: u32, defs: &BTreeMap<u32, CalleeVarDef>, visiting: &mut Vec<u32>) -> Option<FnId> {
  if visiting.contains(&var) {
    return None;
  }
  visiting.push(var);

  let out = match defs.get(&var) {
    Some(CalleeVarDef::Fn(id)) => Some(*id),
    Some(CalleeVarDef::Alias(src)) => resolve_var_fn_id(*src, defs, visiting),
    Some(CalleeVarDef::Phi(args)) => {
      let mut merged: Option<FnId> = None;
      for arg in args {
        let Some(id) = resolve_fn_id(arg, defs, visiting) else {
          visiting.pop();
          return None;
        };
        merged = match merged {
          None => Some(id),
          Some(prev) if prev == id => Some(prev),
          _ => {
            visiting.pop();
            return None;
          }
        };
      }
      merged
    }
    _ => None,
  };

  visiting.pop();
  out
}

fn compute_callsite_counts(program: &Program, foreign_fns: &BTreeMap<SymbolId, FnId>) -> Vec<usize> {
  let mut counts = vec![0usize; program.functions.len()];
  let mut scan_cfg = |cfg: &Cfg| {
    let defs = build_callee_var_defs(cfg, foreign_fns);
    for inst in collect_reachable_insts(cfg) {
      if inst.t != InstTyp::Call {
        continue;
      }
      let mut visiting = Vec::new();
      let Some(id) = resolve_fn_id(&inst.args[0], &defs, &mut visiting) else {
        continue;
      };
      if let Some(slot) = counts.get_mut(id) {
        *slot += 1;
      }
    }
  };

  scan_cfg(cfg_for_inlining(&program.top_level));
  for func in &program.functions {
    scan_cfg(cfg_for_inlining(func));
  }

  counts
}

fn compute_fn_summaries(program: &Program) -> Vec<FnSummary> {
  program
    .functions
    .iter()
    .map(|func| {
      let cfg = cfg_for_inlining(func);
      let mut inst_count = 0usize;
      for inst in collect_reachable_insts(cfg) {
        if inst.t == InstTyp::Phi {
          continue;
        }
        inst_count += 1;
      }
      FnSummary { inst_count }
    })
    .collect()
}

fn cfg_next_label(cfg: &Cfg) -> u32 {
  cfg.graph.labels_sorted().into_iter().max().unwrap_or(0).saturating_add(1)
}

fn cfg_next_temp(cfg: &Cfg) -> u32 {
  let mut max_temp: Option<u32> = None;
  let mut labels: Vec<u32> = cfg.bblocks.all().map(|(label, _)| label).collect();
  labels.sort_unstable();
  for label in labels {
    for inst in cfg.bblocks.get(label) {
      for &tgt in &inst.tgts {
        max_temp = Some(max_temp.map_or(tgt, |cur| cur.max(tgt)));
      }
      for arg in &inst.args {
        if let Arg::Var(v) = arg {
          max_temp = Some(max_temp.map_or(*v, |cur| cur.max(*v)));
        }
      }
      if let Some(narrowing) = inst.meta.nullability_narrowing {
        max_temp = Some(max_temp.map_or(narrowing.var, |cur| cur.max(narrowing.var)));
      }
      if let Some(hint) = inst.meta.in_place_hint {
        match hint {
          InPlaceHint::MoveNoClone { src, tgt } => {
            max_temp = Some(max_temp.map_or(src, |cur| cur.max(src)));
            max_temp = Some(max_temp.map_or(tgt, |cur| cur.max(tgt)));
          }
        }
      }
    }
  }
  max_temp.map(|v| v.saturating_add(1)).unwrap_or(0)
}

struct InlineCfgCtx {
  c_label: Counter,
  c_temp: Counter,
  /// Per-block inlining stack (callee chain).
  ///
  /// The stack is empty for the original body. When a call to `FnId` is inlined, all blocks cloned
  /// from the callee are tagged with `parent_stack + [FnId]`. This drives:
  /// - cycle detection (`FnId` already in stack)
  /// - max inlining depth (`stack.len()`).
  block_stack: BTreeMap<u32, Vec<FnId>>,
}

impl InlineCfgCtx {
  fn new(cfg: &Cfg) -> Self {
    let mut labels: Vec<u32> = cfg.bblocks.all().map(|(label, _)| label).collect();
    labels.sort_unstable();
    let block_stack = labels.into_iter().map(|label| (label, Vec::new())).collect();
    Self {
      c_label: Counter::new(cfg_next_label(cfg)),
      c_temp: Counter::new(cfg_next_temp(cfg)),
      block_stack,
    }
  }

  fn stack_for(&self, label: u32) -> &[FnId] {
    self
      .block_stack
      .get(&label)
      .map(|s| s.as_slice())
      .unwrap_or(&[])
  }

  fn insert_block(&mut self, label: u32, stack: Vec<FnId>) {
    self.block_stack.insert(label, stack);
  }

  fn remove_blocks(&mut self, labels: impl IntoIterator<Item = u32>) {
    for label in labels {
      self.block_stack.remove(&label);
    }
  }
}

fn prune_unreachable(cfg: &mut Cfg, ctx: &mut InlineCfgCtx) -> bool {
  let mut to_delete: Vec<u32> = cfg.graph.find_unreachable(cfg.entry).collect();
  if to_delete.is_empty() {
    return false;
  }
  to_delete.sort_unstable();
  cfg.graph.delete_many(to_delete.iter().copied());
  cfg.bblocks.remove_many(to_delete.iter().copied());
  ctx.remove_blocks(to_delete);
  true
}

fn update_phi_pred(cfg: &mut Cfg, block: u32, old_pred: u32, new_pred: u32) {
  for inst in cfg.bblocks.get_mut(block).iter_mut() {
    if inst.t != InstTyp::Phi {
      break;
    }
    for label in inst.labels.iter_mut() {
      if *label == old_pred {
        *label = new_pred;
      }
    }
  }
}

fn build_temp_map(
  cfg: &Cfg,
  ctx: &mut InlineCfgCtx,
  params: &[u32],
) -> BTreeMap<u32, u32> {
  let param_set: BTreeSet<u32> = params.iter().copied().collect();
  let mut temps = BTreeSet::<u32>::new();
  for inst in collect_reachable_insts(cfg) {
    for &tgt in &inst.tgts {
      if !param_set.contains(&tgt) {
        temps.insert(tgt);
      }
    }
    for arg in &inst.args {
      if let Arg::Var(v) = arg {
        if !param_set.contains(v) {
          temps.insert(*v);
        }
      }
    }
    if let Some(narrowing) = inst.meta.nullability_narrowing {
      if !param_set.contains(&narrowing.var) {
        temps.insert(narrowing.var);
      }
    }
    if let Some(hint) = inst.meta.in_place_hint {
      match hint {
        InPlaceHint::MoveNoClone { src, tgt } => {
          if !param_set.contains(&src) {
            temps.insert(src);
          }
          if !param_set.contains(&tgt) {
            temps.insert(tgt);
          }
        }
      }
    }
  }

  let mut map = BTreeMap::<u32, u32>::new();
  for old in temps {
    map.insert(old, ctx.c_temp.bump());
  }
  map
}

fn remap_var_to_u32(
  var: u32,
  temp_map: &BTreeMap<u32, u32>,
  param_map: &BTreeMap<u32, Arg>,
) -> Option<u32> {
  if let Some(arg) = param_map.get(&var) {
    match arg {
      Arg::Var(v) => return Some(*v),
      _ => return None,
    }
  }
  temp_map.get(&var).copied()
}

fn remap_arg(
  arg: &Arg,
  temp_map: &BTreeMap<u32, u32>,
  param_map: &BTreeMap<u32, Arg>,
  this_arg: &Arg,
) -> Arg {
  match arg {
    Arg::Var(v) => param_map
      .get(v)
      .cloned()
      .unwrap_or_else(|| Arg::Var(temp_map[v])),
    Arg::Builtin(path) if path == "this" => this_arg.clone(),
    _ => arg.clone(),
  }
}

fn remap_inst(
  mut inst: Inst,
  temp_map: &BTreeMap<u32, u32>,
  param_map: &BTreeMap<u32, Arg>,
  this_arg: &Arg,
  label_map: &BTreeMap<u32, u32>,
) -> Inst {
  for tgt in inst.tgts.iter_mut() {
    if let Some(new) = remap_var_to_u32(*tgt, temp_map, param_map) {
      *tgt = new;
    } else {
      // Should never happen (SSA params are not assigned to).
      *tgt = temp_map[tgt];
    }
  }

  for arg in inst.args.iter_mut() {
    *arg = remap_arg(arg, temp_map, param_map, this_arg);
  }

  if !inst.labels.is_empty() {
    for label in inst.labels.iter_mut() {
      if let Some(new) = label_map.get(label).copied() {
        *label = new;
      }
    }
  }

  if let Some(narrowing) = inst.meta.nullability_narrowing.as_mut() {
    if let Some(new_var) = remap_var_to_u32(narrowing.var, temp_map, param_map) {
      narrowing.var = new_var;
    } else {
      inst.meta.nullability_narrowing = None;
    }
  }

  if let Some(hint) = inst.meta.in_place_hint {
    let new_hint = match hint {
      InPlaceHint::MoveNoClone { src, tgt } => match (
        remap_var_to_u32(src, temp_map, param_map),
        remap_var_to_u32(tgt, temp_map, param_map),
      ) {
        (Some(src), Some(tgt)) => Some(InPlaceHint::MoveNoClone { src, tgt }),
        _ => None,
      },
    };
    inst.meta.in_place_hint = new_hint;
  }

  inst
}

fn inline_callsite(
  caller_cfg: &mut Cfg,
  ctx: &mut InlineCfgCtx,
  call_label: u32,
  call_inst_idx: usize,
  callee_id: FnId,
  callee: &CalleeSnapshot,
) {
  let call_stack = ctx
    .block_stack
    .get(&call_label)
    .cloned()
    .unwrap_or_default();

  // Split the call block into:
  //   call_label:  [...before_call]
  //   cont_label:  [...after_call]
  let cont_label = ctx.c_label.bump();
  let call_inst = {
    let block = caller_cfg.bblocks.get_mut(call_label);
    let tail = block.split_off(call_inst_idx + 1);
    let call_inst = block.pop().expect("call instruction exists");
    caller_cfg.bblocks.add(cont_label, tail);
    call_inst
  };
  ctx.insert_block(cont_label, call_stack.clone());
  caller_cfg.graph.ensure_label(cont_label);

  // Move outgoing edges from the call block to the continuation block.
  let orig_children = caller_cfg.graph.children_sorted(call_label);
  for child in orig_children.iter().copied() {
    caller_cfg.graph.disconnect(call_label, child);
    caller_cfg.graph.connect(cont_label, child);
    update_phi_pred(caller_cfg, child, call_label, cont_label);
  }

  // Build param substitutions. JS "missing arg" => undefined.
  let (_call_tgt, _callee_arg, this_arg, args, spreads) = call_inst.as_call();
  debug_assert!(
    spreads.is_empty(),
    "inline_callsite should not be called for spread calls"
  );
  let mut param_map = BTreeMap::<u32, Arg>::new();
  for (idx, &param) in callee.params.iter().enumerate() {
    let arg = args.get(idx).cloned().unwrap_or(Arg::Const(Const::Undefined));
    param_map.insert(param, arg);
  }

  let new_stack = {
    let mut s = call_stack.clone();
    s.push(callee_id);
    s
  };

  // Only inline blocks reachable from the callee entry (avoid copying dead code).
  let mut callee_labels = callee.cfg.reverse_postorder();
  callee_labels.sort_unstable();

  let mut label_map = BTreeMap::<u32, u32>::new();
  for old in callee_labels.iter().copied() {
    label_map.insert(old, ctx.c_label.bump());
  }

  let temp_map = build_temp_map(&callee.cfg, ctx, &callee.params);

  // Clone bblocks.
  let mut return_blocks: Vec<(u32 /*new label*/, Arg /*return value*/)> = Vec::new();
  for old_label in callee_labels.iter().copied() {
    let new_label = label_map[&old_label];
    let mut new_insts = Vec::new();
    let mut return_value: Option<Arg> = None;
    for inst in callee.cfg.bblocks.get(old_label).iter().cloned() {
      if inst.t == InstTyp::Return {
        let value = inst
          .as_return()
          .cloned()
          .unwrap_or(Arg::Const(Const::Undefined));
        return_value = Some(remap_arg(&value, &temp_map, &param_map, this_arg));
        continue;
      }
      new_insts.push(remap_inst(
        inst,
        &temp_map,
        &param_map,
        this_arg,
        &label_map,
      ));
    }
    if let Some(value) = return_value {
      return_blocks.push((new_label, value));
    }
    caller_cfg.bblocks.add(new_label, new_insts);
    caller_cfg.graph.ensure_label(new_label);
    ctx.insert_block(new_label, new_stack.clone());
  }

  // Clone edges.
  for old_parent in callee_labels.iter().copied() {
    let new_parent = label_map[&old_parent];
    for old_child in callee.cfg.graph.children_sorted(old_parent) {
      let Some(&new_child) = label_map.get(&old_child) else {
        continue;
      };
      caller_cfg.graph.connect(new_parent, new_child);
    }
  }

  // Wire call site -> callee entry.
  let callee_entry = label_map[&callee.cfg.entry];
  caller_cfg.graph.connect(call_label, callee_entry);

  // Replace returns with a join block that flows into the continuation.
  if !return_blocks.is_empty() {
    let join_label = ctx.c_label.bump();
    caller_cfg.bblocks.add(join_label, Vec::new());
    caller_cfg.graph.ensure_label(join_label);
    ctx.insert_block(join_label, new_stack.clone());

    // Connect all return blocks into the join.
    return_blocks.sort_by_key(|(label, _)| *label);
    for &(ret_label, _) in &return_blocks {
      caller_cfg.graph.connect(ret_label, join_label);
    }

    // If the call produced a value, materialize it in the join block (Phi when needed).
    if let Some(call_tgt) = call_inst.tgts.get(0).copied() {
      let join_block = caller_cfg.bblocks.get_mut(join_label);
      if return_blocks.len() == 1 {
        let value = return_blocks[0].1.clone();
        let mut assign = Inst::var_assign(call_tgt, value);
        assign.meta.copy_result_var_metadata_from(&call_inst.meta);
        if !call_inst.value_type.is_unknown() {
          assign.value_type = call_inst.value_type;
        }
        join_block.push(assign);
      } else {
        let mut phi = Inst::phi_empty(call_tgt);
        phi.meta.copy_result_var_metadata_from(&call_inst.meta);
        if !call_inst.value_type.is_unknown() {
          phi.value_type = call_inst.value_type;
        }
        for (label, value) in return_blocks.iter() {
          phi.insert_phi(*label, value.clone());
        }
        join_block.push(phi);
      }
    }

    // Join -> continuation.
    caller_cfg.graph.connect(join_label, cont_label);
  }

  // SSA/phi invariants may have been disturbed by predecessor rewrites.
}

fn should_inline(
  callee: FnId,
  call_counts: &[usize],
  fn_summaries: &[FnSummary],
  options: InlineOptions,
) -> bool {
  let called_once = call_counts.get(callee).copied().unwrap_or(0) == 1;
  if called_once {
    return true;
  }
  let Some(summary) = fn_summaries.get(callee) else {
    return false;
  };
  summary.inst_count <= options.threshold
}

fn inline_cfg(
  cfg: &mut Cfg,
  body_fn: Option<FnId>,
  callees: &[CalleeSnapshot],
  foreign_fns: &BTreeMap<SymbolId, FnId>,
  call_counts: &[usize],
  fn_summaries: &[FnSummary],
  options: InlineOptions,
) -> bool {
  if !options.enabled || options.max_depth == 0 {
    return false;
  }

  let mut ctx = InlineCfgCtx::new(cfg);
  let mut changed = false;

  loop {
    let defs = build_callee_var_defs(cfg, foreign_fns);
    let mut next: Option<(u32, usize, FnId)> = None;

    for label in cfg.reverse_postorder() {
      let stack = ctx.stack_for(label);
      if stack.len() >= options.max_depth {
        continue;
      }
      let block = cfg.bblocks.get(label);
      for (inst_idx, inst) in block.iter().enumerate() {
        if inst.t != InstTyp::Call {
          continue;
        }
        if !inst.spreads.is_empty() {
          continue;
        }

        let mut visiting = Vec::new();
        let Some(callee_id) = resolve_fn_id(&inst.args[0], &defs, &mut visiting) else {
          continue;
        };
        if body_fn.is_some_and(|id| id == callee_id) {
          // Do not inline direct self-recursion; it leads to unbounded growth and does not help call
          // graph simplification.
          continue;
        }
        if callee_id >= callees.len() {
          continue;
        }
        if stack.contains(&callee_id) {
          // Cycle: A -> ... -> A. Respect stack-based cycle detection for determinism and to avoid
          // unbounded recursion.
          continue;
        }
        if !should_inline(callee_id, call_counts, fn_summaries, options) {
          continue;
        }

        next = Some((label, inst_idx, callee_id));
        break;
      }
      if next.is_some() {
        break;
      }
    }

    let Some((label, inst_idx, callee_id)) = next else {
      break;
    };

    let callee = &callees[callee_id];
    inline_callsite(cfg, &mut ctx, label, inst_idx, callee_id, callee);
    changed = true;

    // Remove any unreachable blocks introduced by inlining e.g. when inlining a never-returning
    // callee.
    let did_prune = prune_unreachable(cfg, &mut ctx);
    if did_prune {
      // `prune_unreachable` can remove entire predecessor subgraphs; phi nodes must be cleaned up.
      let _ = simplify_phis(cfg);
    } else {
      let _ = simplify_phis(cfg);
    }
  }

  changed
}

pub fn optpass_inline(program: &mut Program, options: InlineOptions, keep_ssa: bool) -> PassResult {
  let mut result = PassResult::default();
  if !options.enabled || options.max_depth == 0 {
    return result;
  }

  // Global, deterministic helpers.
  let foreign_fns = collect_constant_foreign_fns(program);
  let call_counts = compute_callsite_counts(program, &foreign_fns);
  let fn_summaries = compute_fn_summaries(program);

  let callees: Vec<CalleeSnapshot> = program
    .functions
    .iter()
    .map(|func| CalleeSnapshot {
      cfg: cfg_for_inlining(func).clone(),
      params: func.params.clone(),
    })
    .collect();

  // Inline into top-level first, then functions in FnId order.
  let top_changed = {
    let cfg = cfg_for_inlining_mut(&mut program.top_level);
    inline_cfg(
      cfg,
      None,
      &callees,
      &foreign_fns,
      &call_counts,
      &fn_summaries,
      options,
    )
  };
  if top_changed {
    if program.top_level.debug.is_some() {
      let snapshot = cfg_for_inlining(&program.top_level).clone();
      if let Some(dbg) = program.top_level.debug.as_mut() {
        dbg.add_step("after_inline", &snapshot);
      }
    }
    result.mark_cfg_changed();
  }

  for (id, func) in program.functions.iter_mut().enumerate() {
    let changed = {
      let cfg = cfg_for_inlining_mut(func);
      inline_cfg(
        cfg,
        Some(id),
        &callees,
        &foreign_fns,
        &call_counts,
        &fn_summaries,
        options,
      )
    };
    if changed {
      if func.debug.is_some() {
        let snapshot = cfg_for_inlining(func).clone();
        if let Some(dbg) = func.debug.as_mut() {
          dbg.add_step("after_inline", &snapshot);
        }
      }
      result.mark_cfg_changed();
    }
  }

  // Re-sync `body` with the inlined SSA CFGs.
  if keep_ssa {
    // SSA is the primary output.
    if let Some(ssa) = program.top_level.ssa_body.as_ref() {
      program.top_level.body = ssa.clone();
    }
    for func in program.functions.iter_mut() {
      if let Some(ssa) = func.ssa_body.as_ref() {
        func.body = ssa.clone();
      }
    }
  } else {
    // Preserve the default pipeline behaviour (`body` is SSA-deconstructed).
    if let Some(ssa) = program.top_level.ssa_body.as_ref() {
      let mut deconstructed = ssa.clone();
      let mut c_label = Counter::new(cfg_next_label(&deconstructed));
      deconstruct_ssa(&mut deconstructed, &mut c_label);
      program.top_level.body = deconstructed;
    }
    for func in program.functions.iter_mut() {
      if let Some(ssa) = func.ssa_body.as_ref() {
        let mut deconstructed = ssa.clone();
        let mut c_label = Counter::new(cfg_next_label(&deconstructed));
        deconstruct_ssa(&mut deconstructed, &mut c_label);
        func.body = deconstructed;
      }
    }
  }

  result
}
use crate::ssa::ssa_deconstruct::deconstruct_ssa;
