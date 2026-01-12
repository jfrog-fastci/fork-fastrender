use crate::cfg::cfg::Cfg;
use crate::il::inst::{Arg, BinOp, InstTyp};
use crate::analysis::call_summary::{FnSummary, ReturnKind};
use crate::analysis::interproc_escape::ProgramEscapeSummaries;
use crate::symbol::semantics::SymbolId;
use crate::FnId;
use std::collections::{BTreeMap, BTreeSet};

/// Escape classification for allocations local to a function.
///
/// This analysis is intraprocedural and conservative. It focuses on allocations created by a small
/// set of internal marker builtins (e.g. `__optimize_js_object`) and determines whether they remain
/// local to the function or may become reachable outside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum EscapeState {
  /// No observed escape.
  NoEscape,
  /// The allocation becomes reachable from an input/parameter object.
  ///
  /// The payload is the index into the function's parameter list.
  ArgEscape(usize),
  /// Escapes by being returned from the current function.
  ///
  /// This includes values returned via [`InstTyp::Return`] or thrown via [`InstTyp::Throw`]. We do
  /// not model `try`/`catch` explicitly in the CFG, so both are treated as escaping to the caller.
  ReturnEscape,
  /// Escapes to global/outer scope storage (e.g. `ForeignStore`/`UnknownStore`).
  GlobalEscape,
  /// Escapes in an unknown way.
  Unknown,
}

impl EscapeState {
  /// Deterministic, conservative join ("worse wins").
  pub fn join(self, other: Self) -> Self {
    use EscapeState::*;
    match (self, other) {
      (Unknown, _) | (_, Unknown) => Unknown,
      (GlobalEscape, _) | (_, GlobalEscape) => GlobalEscape,
      (ReturnEscape, ReturnEscape) => ReturnEscape,
      (ReturnEscape, NoEscape) | (NoEscape, ReturnEscape) => ReturnEscape,
      (ReturnEscape, ArgEscape(_)) | (ArgEscape(_), ReturnEscape) => Unknown,
      (ArgEscape(a), ArgEscape(b)) => {
        if a == b {
          ArgEscape(a)
        } else {
          Unknown
        }
      }
      (NoEscape, x) | (x, NoEscape) => x,
    }
  }

  pub fn escapes(self) -> bool {
    self != EscapeState::NoEscape
  }
}

/// Escape results keyed by SSA/temp variable ID.
///
/// This is the (legacy) API used by other intraprocedural analyses. It contains entries for all
/// allocation-defining temps, plus any temps that may alias an escaping allocation.
pub type EscapeResult = BTreeMap<u32, EscapeState>;

/// Allocation-only escape analysis results.
///
/// This exposes the escape state for each allocation-defining temp, without including intermediate
/// temps that may alias those allocations.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EscapeResults {
  alloc_states: BTreeMap<u32, EscapeState>,
}

impl EscapeResults {
  pub fn escape_state(&self, alloc: u32) -> Option<EscapeState> {
    self.alloc_states.get(&alloc).copied()
  }

  pub fn iter(&self) -> impl Iterator<Item = (u32, EscapeState)> + '_ {
    self
      .alloc_states
      .iter()
      .map(|(&alloc, &state)| (alloc, state))
  }

  pub fn len(&self) -> usize {
    self.alloc_states.len()
  }

  pub fn is_empty(&self) -> bool {
    self.alloc_states.is_empty()
  }
}

fn cfg_labels_sorted(cfg: &Cfg) -> Vec<u32> {
  let mut labels = cfg.graph.labels_sorted();
  labels.extend(cfg.bblocks.all().map(|(label, _)| label));
  labels.sort_unstable();
  labels.dedup();
  labels
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkerCall {
  Object,
  Array,
  Regex,
  Template,
  New,
}

fn marker_call(callee: &Arg) -> Option<MarkerCall> {
  let Arg::Builtin(path) = callee else {
    return None;
  };
  Some(match path.as_str() {
    "__optimize_js_object" => MarkerCall::Object,
    "__optimize_js_array" => MarkerCall::Array,
    "__optimize_js_regex" => MarkerCall::Regex,
    "__optimize_js_template" => MarkerCall::Template,
    "__optimize_js_new" => MarkerCall::New,
    _ => return None,
  })
}

fn marker_call_is_safe(callee: &Arg) -> bool {
  matches!(
    marker_call(callee),
    Some(MarkerCall::Object | MarkerCall::Array | MarkerCall::Regex | MarkerCall::Template)
  )
}

#[derive(Clone, Debug)]
enum CalleeVarDef {
  Alias(u32),
  Fn(FnId),
  Phi(Vec<Arg>),
  Unknown,
}

fn build_callee_var_defs(
  cfg: &Cfg,
  foreign_fns: &BTreeMap<SymbolId, FnId>,
) -> BTreeMap<u32, CalleeVarDef> {
  let mut defs = BTreeMap::<u32, CalleeVarDef>::new();
  for label in cfg_labels_sorted(cfg) {
    let Some(block) = cfg.bblocks.maybe_get(label) else {
      continue;
    };
    for inst in block.iter() {
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
          // Non-SSA CFGs may assign the same temp multiple times. Only keep
          // definitions when we can prove the value is constant.
          if !matches!(existing, CalleeVarDef::Unknown) {
            *existing = CalleeVarDef::Unknown;
          }
        })
        .or_insert(def);
    }
  }
  defs
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

fn resolve_fn_id(arg: &Arg, defs: &BTreeMap<u32, CalleeVarDef>, visiting: &mut Vec<u32>) -> Option<FnId> {
  match arg {
    Arg::Fn(id) => Some(*id),
    Arg::Var(v) => resolve_var_fn_id(*v, defs, visiting),
    _ => None,
  }
}

fn collect_param_vars(cfg: &Cfg) -> BTreeSet<u32> {
  let mut defs = BTreeSet::<u32>::new();
  let mut uses = BTreeSet::<u32>::new();

  for label in cfg_labels_sorted(cfg) {
    let Some(block) = cfg.bblocks.maybe_get(label) else {
      continue;
    };
    for inst in block.iter() {
      defs.extend(inst.tgts.iter().copied());
      for arg in inst.args.iter() {
        if let Arg::Var(v) = arg {
          uses.insert(*v);
        }
      }
    }
  }

  uses.into_iter().filter(|v| !defs.contains(v)).collect()
}

fn array_alloc_init_values(args: &[Arg], spreads: &[usize]) -> Vec<Arg> {
  let mut out = Vec::new();
  for (idx, arg) in args.iter().enumerate() {
    if matches!(arg, Arg::Builtin(path) if path == "__optimize_js_array_hole") {
      continue;
    }
    // Conservatively treat spreads as storing the spread source into the container. This
    // over-approximates reachability, but ensures that values reachable from the spread source are
    // also considered reachable from the new container (e.g. `[...a]` may copy references stored
    // inside `a`).
    let _is_spread = spreads.contains(&(idx + 2));
    out.push(arg.clone());
  }
  out
}

fn object_alloc_init_values(args: &[Arg]) -> Vec<Arg> {
  let mut out = Vec::new();
  for chunk in args.chunks(3) {
    if chunk.len() != 3 {
      continue;
    }
    let Arg::Builtin(marker) = &chunk[0] else {
      continue;
    };
    match marker.as_str() {
      "__optimize_js_object_prop" | "__optimize_js_object_prop_computed" => {
        out.push(chunk[2].clone());
      }
      "__optimize_js_object_spread" => {
        // Conservatively treat object spread as storing the spread source into the container so
        // that values reachable from the source are treated as reachable from the result.
        out.push(chunk[1].clone());
      }
      _ => {}
    }
  }
  out
}

#[derive(Default)]
struct LocalAllocFlowFacts {
  /// Allocation-defining SSA temps (allocation id = temp).
  alloc_vars: BTreeSet<u32>,
  /// SSA temps that may refer to non-local objects (e.g. parameters, global loads, unknown call
  /// results).
  ///
  /// This is used to conservatively treat `PropAssign` into such values as an escape sink even when
  /// they may also alias a local allocation (e.g. via `Phi`).
  external_defs: BTreeSet<u32>,
  /// `tgt = src`
  var_assigns: Vec<(u32, Arg)>,
  /// `tgt = phi(args...)`
  phis: Vec<(u32, Vec<Arg>)>,
  /// Values stored into the newly allocated container at allocation time.
  alloc_inits: Vec<(u32, Vec<Arg>)>,
  /// `obj[prop] = val` (prop ignored; field-insensitive)
  prop_assigns: Vec<(Arg, Arg)>,
  /// `tgt = obj[prop]` (prop ignored; field-insensitive)
  getprops: Vec<(u32, Arg)>,
}

fn collect_local_alloc_flow_facts(
  cfg: &Cfg,
  call_summaries: Option<&[FnSummary]>,
) -> LocalAllocFlowFacts {
  let mut facts = LocalAllocFlowFacts::default();

  for label in cfg_labels_sorted(cfg) {
    let Some(block) = cfg.bblocks.maybe_get(label) else {
      continue;
    };
    for inst in block.iter() {
      match inst.t {
        InstTyp::Call => {
          let (tgt, callee, _this, args, spreads) = inst.as_call();
          let Some(tgt) = tgt else {
            continue;
          };
          if let Some(marker) = marker_call(callee) {
            facts.alloc_vars.insert(tgt);
            match marker {
              MarkerCall::Object => {
                let init = object_alloc_init_values(args);
                if !init.is_empty() {
                  facts.alloc_inits.push((tgt, init));
                }
              }
              MarkerCall::Array => {
                let init = array_alloc_init_values(args, spreads);
                if !init.is_empty() {
                  facts.alloc_inits.push((tgt, init));
                }
              }
              MarkerCall::Regex | MarkerCall::Template | MarkerCall::New => {}
            }
            continue;
          }

          // Direct nested-function calls that always return a fresh allocation
          // are treated as local allocation sites, even when the call uses
          // spread arguments (spreads do not change the "fresh allocation"
          // property).
          if let (Some(call_summaries), Arg::Fn(id)) = (call_summaries, callee) {
            if let Some(summary) = call_summaries.get(*id) {
              if matches!(summary.return_kind, ReturnKind::FreshAlloc) {
                facts.alloc_vars.insert(tgt);
                continue;
              }
            }
          }

          facts.external_defs.insert(tgt);
        }
        #[cfg(feature = "native-async-ops")]
        InstTyp::Await | InstTyp::PromiseAll | InstTyp::PromiseRace => {
          // These ops go through builtin/VM machinery (thenables / promises), so treat their
          // results as external objects rather than local allocations.
          let Some(&tgt) = inst.tgts.get(0) else {
            continue;
          };
          facts.external_defs.insert(tgt);
        }
        InstTyp::VarAssign => {
          let (tgt, arg) = inst.as_var_assign();
          facts.var_assigns.push((tgt, arg.clone()));
        }
        InstTyp::Phi => {
          let tgt = inst.tgts[0];
          facts.phis.push((tgt, inst.args.clone()));
        }
        InstTyp::ForeignLoad => {
          let (tgt, _) = inst.as_foreign_load();
          facts.external_defs.insert(tgt);
        }
        InstTyp::UnknownLoad => {
          let (tgt, _) = inst.as_unknown_load();
          facts.external_defs.insert(tgt);
        }
        InstTyp::PropAssign => {
          let (obj, _prop, val) = inst.as_prop_assign();
          facts.prop_assigns.push((obj.clone(), val.clone()));
        }
        InstTyp::Bin if inst.bin_op == BinOp::GetProp => {
          let (tgt, obj, _op, _prop) = inst.as_bin();
          facts.getprops.push((tgt, obj.clone()));
        }
        _ => {}
      }
    }
  }

  facts
}

type VarAllocs = BTreeMap<u32, BTreeSet<u32>>;

fn allocs_for_arg(var_allocs: &VarAllocs, arg: &Arg) -> BTreeSet<u32> {
  match arg {
    Arg::Var(v) => var_allocs.get(v).cloned().unwrap_or_default(),
    _ => BTreeSet::new(),
  }
}

fn ext_for_arg(var_ext: &BTreeMap<u32, EscapeState>, arg: &Arg) -> EscapeState {
  match arg {
    Arg::Var(v) => var_ext.get(v).copied().unwrap_or(EscapeState::NoEscape),
    // Builtins and nested functions are not local allocations.
    Arg::Builtin(_) | Arg::Fn(_) => EscapeState::GlobalEscape,
    Arg::Const(_) => EscapeState::NoEscape,
  }
}

fn is_internal_literal_marker_builtin(name: &str) -> bool {
  matches!(
    name,
    "__optimize_js_array_hole"
      | "__optimize_js_object_prop"
      | "__optimize_js_object_prop_computed"
      | "__optimize_js_object_spread"
  )
}

fn ext_for_stored_value(var_ext: &BTreeMap<u32, EscapeState>, arg: &Arg) -> EscapeState {
  match arg {
    Arg::Var(v) => var_ext.get(v).copied().unwrap_or(EscapeState::NoEscape),
    Arg::Builtin(name) => {
      // Marker builtins are not real values stored into objects/arrays; ignore them here so we
      // don't falsely taint literal containers as "external".
      if is_internal_literal_marker_builtin(name) {
        EscapeState::NoEscape
      } else {
        EscapeState::GlobalEscape
      }
    }
    Arg::Fn(_) => EscapeState::GlobalEscape,
    Arg::Const(_) => EscapeState::NoEscape,
  }
}

fn join_escape(states: &mut BTreeMap<u32, EscapeState>, alloc: u32, esc: EscapeState) {
  let entry = states.entry(alloc).or_insert(EscapeState::NoEscape);
  let next = entry.join(esc);
  if next != *entry {
    *entry = next;
  }
}

pub fn analyze_cfg_escapes(cfg: &Cfg) -> EscapeResult {
  let mut params: Vec<u32> = collect_param_vars(cfg).into_iter().collect();
  params.sort_unstable();
  analyze_cfg_escapes_with_params_and_summaries(cfg, &params, None, None)
}

pub fn analyze_cfg_escapes_with_params(cfg: &Cfg, params: &[u32]) -> EscapeResult {
  analyze_cfg_escapes_with_params_and_summaries(cfg, params, None, None)
}

pub fn analyze_cfg_escapes_with_params_and_summaries(
  cfg: &Cfg,
  params: &[u32],
  summaries: Option<&ProgramEscapeSummaries>,
  call_summaries: Option<&[FnSummary]>,
) -> EscapeResult {
  let facts = collect_local_alloc_flow_facts(cfg, call_summaries);
  let callee_defs = summaries.map(|summaries| {
    build_callee_var_defs(cfg, summaries.constant_foreign_fns())
  });
  let alloc_vars = facts.alloc_vars.clone();

  // Infer which local allocations may flow through each SSA variable.
  //
  // This is field-insensitive: `GetProp` may read any allocation ever stored into the receiver.
  // This is necessary because alias analysis currently returns `Top` for `GetProp`, which would
  // otherwise let escapes slip through (e.g. store allocation into a local object, read it back,
  // then pass the loaded value to an unknown call).
  let mut var_allocs: VarAllocs = VarAllocs::new();
  for &alloc in alloc_vars.iter() {
    var_allocs.insert(alloc, BTreeSet::from([alloc]));
  }
  let mut var_ext: BTreeMap<u32, EscapeState> = BTreeMap::new();
  for (idx, &param) in params.iter().enumerate() {
    var_ext.insert(param, EscapeState::ArgEscape(idx));
  }
  for &v in facts.external_defs.iter() {
    join_escape(&mut var_ext, v, EscapeState::GlobalEscape);
  }
  let mut stored_into: BTreeMap<u32, BTreeSet<u32>> = BTreeMap::new();
  let mut stored_external: BTreeMap<u32, EscapeState> = BTreeMap::new();

  // Direct `Arg::Fn` return aliasing: if the callee may return an argument `k`, model the call
  // result as aliasing that argument so subsequent `return`/stores of the call result are tracked.
  let mut call_return_assigns: Vec<(u32, Arg)> = Vec::new();
  // Call-induced stores: if the callee may store argument `k` into argument `j` (modeled as the
  // callee reporting `ArgEscape(j)` for parameter `k`), model that as storing the caller's value
  // into the caller's receiver. This enables container-based escape propagation across helper
  // calls (e.g. `return ((x, obj) => { obj.p = x; return obj; })(x, obj)`).
  let mut call_store_edges: Vec<(Arg, Arg)> = Vec::new(); // (receiver, value)
  if let Some(summaries) = summaries {
    let callee_defs = callee_defs
      .as_ref()
      .expect("callee defs should be built when interprocedural summaries are provided");
    for label in cfg_labels_sorted(cfg) {
      let Some(block) = cfg.bblocks.maybe_get(label) else {
        continue;
      };
      for inst in block.iter() {
        if inst.t != InstTyp::Call {
          continue;
        }
        let (tgt, callee, _this, args, spreads) = inst.as_call();
        let Some(id) = resolve_fn_id(callee, callee_defs, &mut Vec::new()) else {
          continue;
        };
        let Some(callee_summary) = summaries.get(id) else {
          continue;
        };
        let first_spread_arg = spreads.iter().copied().min().map(|idx| idx.saturating_sub(2));

        if let Some(tgt) = tgt {
          for &k in callee_summary.returns_param.iter() {
            if first_spread_arg.is_some_and(|first| k >= first) {
              continue;
            }
            if let Some(arg) = args.get(k) {
              call_return_assigns.push((tgt, arg.clone()));
            }
          }
        }

        let max = callee_summary.param_escape.len().min(args.len());
        for k in 0..max {
          if first_spread_arg.is_some_and(|first| k >= first) {
            continue;
          }
          if let EscapeState::ArgEscape(j) = callee_summary.param_escape[k] {
            if first_spread_arg.is_some_and(|first| j >= first) {
              continue;
            }
            if let (Some(receiver), Some(value)) = (args.get(j), args.get(k)) {
              call_store_edges.push((receiver.clone(), value.clone()));
            }
          }
        }
      }
    }
  }

  let mut changed = true;
  while changed {
    changed = false;

    for (tgt, arg) in facts
      .var_assigns
      .iter()
      .chain(call_return_assigns.iter())
    {
      let src_allocs = allocs_for_arg(&var_allocs, arg);
      if src_allocs.is_empty() {
        // Still propagate externalness through copies.
        let src_ext = ext_for_arg(&var_ext, arg);
        if src_ext != EscapeState::NoEscape {
          let entry = var_ext.entry(*tgt).or_insert(EscapeState::NoEscape);
          let next = entry.join(src_ext);
          if next != *entry {
            *entry = next;
            changed = true;
          }
        }
        continue;
      }
      let entry = var_allocs.entry(*tgt).or_default();
      let before = entry.len();
      entry.extend(src_allocs);
      if entry.len() != before {
        changed = true;
      }

      let src_ext = ext_for_arg(&var_ext, arg);
      if src_ext != EscapeState::NoEscape {
        let entry = var_ext.entry(*tgt).or_insert(EscapeState::NoEscape);
        let next = entry.join(src_ext);
        if next != *entry {
          *entry = next;
          changed = true;
        }
      }
    }

    for (tgt, args) in facts.phis.iter() {
      let mut merged = BTreeSet::new();
      let mut merged_ext = EscapeState::NoEscape;
      for arg in args.iter() {
        merged.extend(allocs_for_arg(&var_allocs, arg));
        merged_ext = merged_ext.join(ext_for_arg(&var_ext, arg));
      }
      if !merged.is_empty() {
        let entry = var_allocs.entry(*tgt).or_default();
        let before = entry.len();
        entry.extend(merged);
        if entry.len() != before {
          changed = true;
        }
      }
      if merged_ext != EscapeState::NoEscape {
        let entry = var_ext.entry(*tgt).or_insert(EscapeState::NoEscape);
        let next = entry.join(merged_ext);
        if next != *entry {
          *entry = next;
          changed = true;
        }
      }
    }

    // Container initialization: treat internal literal builder values as stored into the result.
    for (container, args) in facts.alloc_inits.iter() {
      let entry = stored_into.entry(*container).or_default();
      let before = entry.len();
      for arg in args.iter() {
        entry.extend(allocs_for_arg(&var_allocs, arg));
      }
      if entry.len() != before {
        changed = true;
      }

      let mut ext = EscapeState::NoEscape;
      for arg in args.iter() {
        ext = ext.join(ext_for_stored_value(&var_ext, arg));
        if ext == EscapeState::Unknown {
          break;
        }
      }
      if ext != EscapeState::NoEscape {
        let entry_ext = stored_external.entry(*container).or_insert(EscapeState::NoEscape);
        let next = entry_ext.join(ext);
        if next != *entry_ext {
          *entry_ext = next;
          changed = true;
        }
      }
    }

    // Property stores: `obj[prop] = val` stores `val` into any possible container allocation.
    for (obj, val) in facts.prop_assigns.iter() {
      let value_allocs = allocs_for_arg(&var_allocs, val);
      let value_ext = ext_for_stored_value(&var_ext, val);
      let container_allocs = allocs_for_arg(&var_allocs, obj);
      if container_allocs.is_empty() {
        continue;
      }
      for container in container_allocs {
        if !value_allocs.is_empty() {
          let entry = stored_into.entry(container).or_default();
          let before = entry.len();
          entry.extend(value_allocs.iter().copied());
          if entry.len() != before {
            changed = true;
          }
        }

        if value_ext != EscapeState::NoEscape {
          let entry_ext = stored_external.entry(container).or_insert(EscapeState::NoEscape);
          let next = entry_ext.join(value_ext);
          if next != *entry_ext {
            *entry_ext = next;
            changed = true;
          }
        }
      }
    }

    // Call-induced stores: treat `ArgEscape(j)` on a direct `Arg::Fn` call as storing the value into
    // the receiver argument.
    for (receiver, value) in call_store_edges.iter() {
      let value_allocs = allocs_for_arg(&var_allocs, value);
      let value_ext = ext_for_stored_value(&var_ext, value);
      let container_allocs = allocs_for_arg(&var_allocs, receiver);
      if container_allocs.is_empty() {
        continue;
      }
      for container in container_allocs {
        if !value_allocs.is_empty() {
          let entry = stored_into.entry(container).or_default();
          let before = entry.len();
          entry.extend(value_allocs.iter().copied());
          if entry.len() != before {
            changed = true;
          }
        }

        if value_ext != EscapeState::NoEscape {
          let entry_ext = stored_external.entry(container).or_insert(EscapeState::NoEscape);
          let next = entry_ext.join(value_ext);
          if next != *entry_ext {
            *entry_ext = next;
            changed = true;
          }
        }
      }
    }

    // Property loads: `tgt = obj[prop]` may read any allocation stored into the receiver.
    for (tgt, obj) in facts.getprops.iter() {
      let container_allocs = allocs_for_arg(&var_allocs, obj);
      let mut loaded = BTreeSet::new();
      let mut loaded_ext = EscapeState::NoEscape;
      for container in container_allocs {
        if let Some(values) = stored_into.get(&container) {
          loaded.extend(values.iter().copied());
        }
        if let Some(ext) = stored_external.get(&container) {
          loaded_ext = loaded_ext.join(*ext);
        }
      }
      if !loaded.is_empty() {
        let entry = var_allocs.entry(*tgt).or_default();
        let before = entry.len();
        entry.extend(loaded);
        if entry.len() != before {
          changed = true;
        }
      }

      let receiver_ext = ext_for_arg(&var_ext, obj);
      loaded_ext = loaded_ext.join(receiver_ext);
      if loaded_ext != EscapeState::NoEscape {
        let entry = var_ext.entry(*tgt).or_insert(EscapeState::NoEscape);
        let next = entry.join(loaded_ext);
        if next != *entry {
          *entry = next;
          changed = true;
        }
      }
    }
  }

  let mut alloc_states: BTreeMap<u32, EscapeState> =
    alloc_vars.iter().copied().map(|v| (v, EscapeState::NoEscape)).collect();

  // Find escape sinks and record direct escapes.
  for label in cfg_labels_sorted(cfg) {
    let Some(block) = cfg.bblocks.maybe_get(label) else {
      continue;
    };
    for inst in block.iter() {
      match inst.t {
        InstTyp::Return | InstTyp::Throw => {
          // Returned/thrown values escape the current function but can still be treated as
          // ownership-transfer sites rather than forcing them to be globally shared.
          let Some(arg) = inst.args.get(0) else {
            continue;
          };
          for alloc in allocs_for_arg(&var_allocs, arg) {
            join_escape(&mut alloc_states, alloc, EscapeState::ReturnEscape);
          }
        }
        InstTyp::ForeignStore | InstTyp::UnknownStore => {
          for alloc in allocs_for_arg(&var_allocs, &inst.args[0]) {
            join_escape(&mut alloc_states, alloc, EscapeState::GlobalEscape);
          }
        }
        InstTyp::Call => {
          let (_tgt, callee, this, args, spreads) = inst.as_call();
          if marker_call_is_safe(callee) {
            continue;
          }

          let first_spread_arg = spreads.iter().copied().min().map(|idx| idx.saturating_sub(2));

          // If we have interprocedural summaries for nested functions, use them to avoid
          // conservatively forcing `GlobalEscape` for all passed allocations.
          let callee_id = summaries.and_then(|program_summaries| {
            let callee_defs = callee_defs
              .as_ref()
              .expect("callee defs should be built when interprocedural summaries are provided");
            resolve_fn_id(callee, callee_defs, &mut Vec::new())
              .and_then(|id| program_summaries.get(id).map(|summary| (id, summary)))
          });
          if let Some((_id, callee_summary)) = callee_id {

            // We don't model `this` in summaries; treat it conservatively.
            for alloc in allocs_for_arg(&var_allocs, this) {
              join_escape(&mut alloc_states, alloc, EscapeState::GlobalEscape);
            }

            for (k, arg) in args.iter().enumerate() {
              let allocs = allocs_for_arg(&var_allocs, arg);
              if allocs.is_empty() {
                continue;
              }

              // When a call site contains spreads, argument indexing is ambiguous after the first
              // spread. Only trust per-parameter summaries for arguments that appear before the
              // first spread; conservatively treat the rest as escaping via an unknown call.
              if first_spread_arg.is_some_and(|first| k >= first) {
                for alloc in allocs {
                  join_escape(&mut alloc_states, alloc, EscapeState::GlobalEscape);
                }
                continue;
              }

              let callee_state = callee_summary
                .param_escape
                .get(k)
                .copied()
                .unwrap_or(EscapeState::Unknown);

              let mapped = match callee_state {
                EscapeState::NoEscape => EscapeState::NoEscape,
                EscapeState::ReturnEscape => {
                  if callee_summary.throws_param.contains(&k) {
                    EscapeState::ReturnEscape
                  } else {
                    EscapeState::NoEscape
                  }
                }
                EscapeState::GlobalEscape | EscapeState::Unknown => callee_state,
                EscapeState::ArgEscape(j) => {
                  // If the receiver parameter index is past the first spread, we cannot map it to a
                  // concrete argument. Stay conservative.
                  if first_spread_arg.is_some_and(|first| j >= first) {
                    EscapeState::GlobalEscape
                  } else {
                  match args.get(j) {
                    Some(receiver_arg) => {
                      let receiver_ext = ext_for_arg(&var_ext, receiver_arg);
                      match receiver_ext {
                        EscapeState::ArgEscape(p) => EscapeState::ArgEscape(p),
                        EscapeState::GlobalEscape | EscapeState::Unknown => receiver_ext,
                        EscapeState::NoEscape | EscapeState::ReturnEscape => {
                          // If the receiver is a local allocation we can track, rely on
                          // container-edge propagation (via `stored_into`) instead of forcing an
                          // immediate escape.
                          if allocs_for_arg(&var_allocs, receiver_arg).is_empty() {
                            EscapeState::GlobalEscape
                          } else {
                            EscapeState::NoEscape
                          }
                        }
                      }
                    }
                    None => EscapeState::GlobalEscape,
                  }
                  }
                }
              };

              if mapped == EscapeState::NoEscape {
                continue;
              }
              for alloc in allocs {
                join_escape(&mut alloc_states, alloc, mapped);
              }
            }
          } else {
            // Unknown/impure call: conservatively treat any allocation passed as escaping.
            for arg in inst.args.iter() {
              for alloc in allocs_for_arg(&var_allocs, arg) {
                join_escape(&mut alloc_states, alloc, EscapeState::GlobalEscape);
              }
            }
          }
        }
        #[cfg(feature = "native-async-ops")]
        InstTyp::Await | InstTyp::PromiseAll | InstTyp::PromiseRace => {
          // Async semantic ops may retain references to their inputs (e.g. awaiting thenables or
          // Promise.all attaching handlers). Conservatively treat any allocation passed as
          // escaping.
          for arg in inst.args.iter() {
            for alloc in allocs_for_arg(&var_allocs, arg) {
              join_escape(&mut alloc_states, alloc, EscapeState::GlobalEscape);
            }
          }
        }
        InstTyp::PropAssign => {
          let (obj, _prop, val) = inst.as_prop_assign();
          let value_allocs = allocs_for_arg(&var_allocs, val);
          if value_allocs.is_empty() {
            continue;
          }
          let obj_ext = ext_for_arg(&var_ext, obj);
          if obj_ext != EscapeState::NoEscape {
            for value in value_allocs {
              join_escape(&mut alloc_states, value, obj_ext);
            }
            continue;
          }

          let container_allocs = allocs_for_arg(&var_allocs, obj);
          if container_allocs.is_empty() {
            // Receiver is not a local allocation we can reason about; conservatively treat as
            // escape to global/unknown memory.
            for value in value_allocs {
              join_escape(&mut alloc_states, value, EscapeState::GlobalEscape);
            }
          }
        }
        _ => {}
      }
    }
  }

  // Propagate escapes through container edges: if a container allocation escapes, any allocations
  // stored into it are also reachable outside the function.
  let mut changed = true;
  while changed {
    changed = false;
    for (container, values) in stored_into.iter() {
      let container_state = alloc_states.get(container).copied().unwrap_or(EscapeState::NoEscape);
      if container_state == EscapeState::NoEscape {
        continue;
      }
      for &value in values {
        let entry = alloc_states.entry(value).or_insert(EscapeState::NoEscape);
        let next = entry.join(container_state);
        if next != *entry {
          *entry = next;
          changed = true;
        }
      }
    }
  }

  // Materialize per-variable escape information for any temp that may alias an escaping allocation.
  let mut out: EscapeResult = EscapeResult::new();

  for &alloc in alloc_vars.iter() {
    out.insert(
      alloc,
      alloc_states.get(&alloc).copied().unwrap_or(EscapeState::NoEscape),
    );
  }

  for (&var, allocs) in var_allocs.iter() {
    let mut state = EscapeState::NoEscape;
    for alloc in allocs.iter() {
      let alloc_state = alloc_states
        .get(alloc)
        .copied()
        .unwrap_or(EscapeState::NoEscape);
      state = state.join(alloc_state);
      if state == EscapeState::Unknown {
        break;
      }
    }
    if state != EscapeState::NoEscape {
      out.insert(var, state);
    }
  }

  out
}

/// Allocation-only escape analysis results.
///
/// This is a stable public entry point that returns the escape state for each allocation-defining
/// temp (as opposed to all temps that may alias an allocation).
pub fn analyze_escape(cfg: &Cfg) -> EscapeResults {
  let facts = collect_local_alloc_flow_facts(cfg, None);
  let all = analyze_cfg_escapes(cfg);

  let mut alloc_states = BTreeMap::new();
  for alloc in facts.alloc_vars.iter() {
    alloc_states.insert(
      *alloc,
      all.get(alloc).copied().unwrap_or(EscapeState::NoEscape),
    );
  }
  EscapeResults { alloc_states }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::cfg::cfg::{Cfg, CfgBBlocks, CfgGraph};
  use crate::il::inst::{Const, Inst};
  use crate::symbol::semantics::SymbolId;
  use parse_js::num::JsNumber;

  fn cfg_with_blocks(blocks: &[(u32, Vec<Inst>)], edges: &[(u32, u32)]) -> Cfg {
    let labels: Vec<u32> = blocks.iter().map(|(label, _)| *label).collect();
    let mut graph = CfgGraph::default();
    for &(from, to) in edges {
      graph.connect(from, to);
    }
    for &label in &labels {
      if !graph.contains(label) {
        // Ensure the node exists even if it has no edges.
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

  fn escape_of(result: &EscapeResult, var: u32) -> EscapeState {
    result.get(&var).copied().unwrap_or(EscapeState::NoEscape)
  }

  #[test]
  fn local_allocation_no_escape() {
    let cfg = cfg_with_blocks(
      &[(
        0,
        vec![
          Inst::call(
            0,
            Arg::Builtin("__optimize_js_array".to_string()),
            Arg::Const(Const::Undefined),
            vec![],
            vec![],
          ),
          Inst::prop_assign(
            Arg::Var(0),
            Arg::Const(Const::Str("x".to_string())),
            Arg::Const(Const::Num(JsNumber(1.0))),
          ),
        ],
      )],
      &[],
    );

    let escape = analyze_cfg_escapes(&cfg);
    assert_eq!(escape_of(&escape, 0), EscapeState::NoEscape);
  }

  #[test]
  fn allocation_passed_to_unknown_call_escapes() {
    let cfg = cfg_with_blocks(
      &[(
        0,
        vec![
          Inst::call(
            0,
            Arg::Builtin("__optimize_js_object".to_string()),
            Arg::Const(Const::Undefined),
            vec![],
            vec![],
          ),
          Inst::unknown_load(1, "f".to_string()),
          Inst::call(
            2,
            Arg::Var(1),
            Arg::Const(Const::Undefined),
            vec![Arg::Var(0)],
            vec![],
          ),
        ],
      )],
      &[],
    );

    let escape = analyze_cfg_escapes(&cfg);
    assert_eq!(escape_of(&escape, 0), EscapeState::GlobalEscape);
  }

  #[test]
  fn allocation_returned_is_return_escape() {
    let cfg = cfg_with_blocks(
      &[(
        0,
        vec![
          Inst::call(
            0,
            Arg::Builtin("__optimize_js_object".to_string()),
            Arg::Const(Const::Undefined),
            vec![],
            vec![],
          ),
          Inst::ret(Some(Arg::Var(0))),
        ],
      )],
      &[],
    );

    let escape = analyze_cfg_escapes(&cfg);
    assert_eq!(escape_of(&escape, 0), EscapeState::ReturnEscape);
  }

  #[test]
  fn allocation_thrown_is_return_escape() {
    let cfg = cfg_with_blocks(
      &[(
        0,
        vec![
          Inst::call(
            0,
            Arg::Builtin("__optimize_js_object".to_string()),
            Arg::Const(Const::Undefined),
            vec![],
            vec![],
          ),
          Inst::throw(Arg::Var(0)),
        ],
      )],
      &[],
    );

    let escape = analyze_cfg_escapes(&cfg);
    assert_eq!(escape_of(&escape, 0), EscapeState::ReturnEscape);
  }

  #[test]
  fn allocation_stored_to_foreign_store_escapes() {
    let cfg = cfg_with_blocks(
      &[(
        0,
        vec![
          Inst::call(
            0,
            Arg::Builtin("__optimize_js_object".to_string()),
            Arg::Const(Const::Undefined),
            vec![],
            vec![],
          ),
          Inst::var_assign(1, Arg::Var(0)),
          Inst::foreign_store(SymbolId(123), Arg::Var(1)),
        ],
      )],
      &[],
    );

    let escape = analyze_cfg_escapes(&cfg);
    assert_eq!(escape_of(&escape, 0), EscapeState::GlobalEscape);
  }

  #[test]
  fn allocation_stored_into_parameter_object_is_arg_escape() {
    // %99 has no definition, so the analysis treats it as a parameter/input.
    let cfg = cfg_with_blocks(
      &[(
        0,
        vec![
          Inst::call(
            0,
            Arg::Builtin("__optimize_js_array".to_string()),
            Arg::Const(Const::Undefined),
            vec![],
            vec![],
          ),
          Inst::prop_assign(
            Arg::Var(99),
            Arg::Const(Const::Str("x".to_string())),
            Arg::Var(0),
          ),
        ],
      )],
      &[],
    );

    let escape = analyze_cfg_escapes(&cfg);
    assert_eq!(escape_of(&escape, 0), EscapeState::ArgEscape(0));
  }

  #[test]
  fn allocation_returned_escapes() {
    let cfg = cfg_with_blocks(
      &[(
        0,
        vec![
          Inst::call(
            0,
            Arg::Builtin("__optimize_js_object".to_string()),
            Arg::Const(Const::Undefined),
            vec![],
            vec![],
          ),
          Inst::ret(Some(Arg::Var(0))),
        ],
      )],
      &[],
    );

    let escape = analyze_cfg_escapes(&cfg);
    assert_eq!(escape_of(&escape, 0), EscapeState::ReturnEscape);
  }

  #[test]
  fn allocation_thrown_escapes() {
    let cfg = cfg_with_blocks(
      &[(
        0,
        vec![
          Inst::call(
            0,
            Arg::Builtin("__optimize_js_object".to_string()),
            Arg::Const(Const::Undefined),
            vec![],
            vec![],
          ),
          Inst::throw(Arg::Var(0)),
        ],
      )],
      &[],
    );

    let escape = analyze_cfg_escapes(&cfg);
    assert_eq!(escape_of(&escape, 0), EscapeState::ReturnEscape);
  }

  #[test]
  fn stored_value_escapes_with_container() {
    let cfg = cfg_with_blocks(
      &[(
        0,
        vec![
          Inst::call(
            0,
            Arg::Builtin("__optimize_js_object".to_string()),
            Arg::Const(Const::Undefined),
            vec![],
            vec![],
          ),
          Inst::call(
            1,
            Arg::Builtin("__optimize_js_object".to_string()),
            Arg::Const(Const::Undefined),
            vec![],
            vec![],
          ),
          Inst::prop_assign(
            Arg::Var(0),
            Arg::Const(Const::Str("x".to_string())),
            Arg::Var(1),
          ),
          Inst::foreign_store(SymbolId(1), Arg::Var(0)),
        ],
      )],
      &[],
    );

    let escape = analyze_cfg_escapes(&cfg);
    assert_eq!(escape_of(&escape, 0), EscapeState::GlobalEscape);
    assert_eq!(escape_of(&escape, 1), EscapeState::GlobalEscape);
  }

  #[test]
  fn getprop_loaded_allocation_escapes_when_passed_to_call() {
    let cfg = cfg_with_blocks(
      &[(
        0,
        vec![
          Inst::call(
            0,
            Arg::Builtin("__optimize_js_object".to_string()),
            Arg::Const(Const::Undefined),
            vec![],
            vec![],
          ),
          Inst::call(
            1,
            Arg::Builtin("__optimize_js_object".to_string()),
            Arg::Const(Const::Undefined),
            vec![],
            vec![],
          ),
          Inst::prop_assign(
            Arg::Var(0),
            Arg::Const(Const::Str("x".to_string())),
            Arg::Var(1),
          ),
          Inst::bin(
            2,
            Arg::Var(0),
            BinOp::GetProp,
            Arg::Const(Const::Str("x".to_string())),
          ),
          Inst::unknown_load(3, "f".to_string()),
          Inst::call(
            4,
            Arg::Var(3),
            Arg::Const(Const::Undefined),
            vec![Arg::Var(2)],
            vec![],
          ),
        ],
      )],
      &[],
    );

    let escape = analyze_cfg_escapes(&cfg);
    assert_eq!(escape_of(&escape, 0), EscapeState::NoEscape);
    assert_eq!(escape_of(&escape, 1), EscapeState::GlobalEscape);
  }

  #[test]
  fn getprop_loaded_local_allocation_is_not_treated_as_external_receiver() {
    let cfg = cfg_with_blocks(
      &[(
        0,
        vec![
          Inst::call(
            0,
            Arg::Builtin("__optimize_js_object".to_string()),
            Arg::Const(Const::Undefined),
            vec![],
            vec![],
          ),
          Inst::call(
            1,
            Arg::Builtin("__optimize_js_object".to_string()),
            Arg::Const(Const::Undefined),
            vec![],
            vec![],
          ),
          Inst::prop_assign(
            Arg::Var(0),
            Arg::Const(Const::Str("x".to_string())),
            Arg::Var(1),
          ),
          Inst::bin(
            2,
            Arg::Var(0),
            BinOp::GetProp,
            Arg::Const(Const::Str("x".to_string())),
          ),
          Inst::call(
            3,
            Arg::Builtin("__optimize_js_object".to_string()),
            Arg::Const(Const::Undefined),
            vec![],
            vec![],
          ),
          Inst::prop_assign(
            Arg::Var(2),
            Arg::Const(Const::Str("y".to_string())),
            Arg::Var(3),
          ),
        ],
      )],
      &[],
    );

    let escape = analyze_cfg_escapes(&cfg);
    assert_eq!(escape_of(&escape, 0), EscapeState::NoEscape);
    assert_eq!(escape_of(&escape, 1), EscapeState::NoEscape);
    assert_eq!(escape_of(&escape, 3), EscapeState::NoEscape);
  }

  #[test]
  fn getprop_loaded_param_object_propagates_arg_escape() {
    // %99 has no definition, so the analysis treats it as a parameter/input.
    // ArgEscape payloads are parameter indices, so since it's the only input it is assigned
    // parameter index 0.
    let cfg = cfg_with_blocks(
      &[(
        0,
        vec![
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
            Arg::Var(99),
          ),
          Inst::bin(
            1,
            Arg::Var(0),
            BinOp::GetProp,
            Arg::Const(Const::Str("x".to_string())),
          ),
          Inst::call(
            2,
            Arg::Builtin("__optimize_js_object".to_string()),
            Arg::Const(Const::Undefined),
            vec![],
            vec![],
          ),
          Inst::prop_assign(
            Arg::Var(1),
            Arg::Const(Const::Str("y".to_string())),
            Arg::Var(2),
          ),
        ],
      )],
      &[],
    );
 
    let escape = analyze_cfg_escapes(&cfg);
    assert_eq!(escape_of(&escape, 2), EscapeState::ArgEscape(0));
  }

  #[test]
  fn prop_assign_to_phi_of_local_and_param_is_arg_escape() {
    let mut phi = Inst::phi_empty(1);
    phi.insert_phi(0, Arg::Var(0));
    phi.insert_phi(1, Arg::Var(99));

    let cfg = cfg_with_blocks(
      &[
        (
          0,
          vec![Inst::call(
            0,
            Arg::Builtin("__optimize_js_object".to_string()),
            Arg::Const(Const::Undefined),
            vec![],
            vec![],
          )],
        ),
        (1, vec![]),
        (
          2,
          vec![
            phi,
            Inst::call(
              2,
              Arg::Builtin("__optimize_js_object".to_string()),
              Arg::Const(Const::Undefined),
              vec![],
              vec![],
            ),
            Inst::prop_assign(
              Arg::Var(1),
              Arg::Const(Const::Str("x".to_string())),
              Arg::Var(2),
            ),
          ],
        ),
      ],
      &[(0, 2), (1, 2)],
    );

    let escape = analyze_cfg_escapes(&cfg);
    assert_eq!(escape_of(&escape, 2), EscapeState::ArgEscape(0));
  }

  #[test]
  fn builtin_initializers_propagate_escape() {
    let cfg = cfg_with_blocks(
      &[(
        0,
        vec![
          Inst::call(
            0,
            Arg::Builtin("__optimize_js_object".to_string()),
            Arg::Const(Const::Undefined),
            vec![],
            vec![],
          ),
          Inst::call(
            1,
            Arg::Builtin("__optimize_js_array".to_string()),
            Arg::Const(Const::Undefined),
            vec![Arg::Var(0)],
            vec![],
          ),
          Inst::foreign_store(SymbolId(2), Arg::Var(1)),
        ],
      )],
      &[],
    );

    let escape = analyze_cfg_escapes(&cfg);
    assert_eq!(escape_of(&escape, 1), EscapeState::GlobalEscape);
    assert_eq!(escape_of(&escape, 0), EscapeState::GlobalEscape);
  }
}
