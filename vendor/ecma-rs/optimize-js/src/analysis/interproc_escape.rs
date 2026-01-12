use crate::analysis::escape::EscapeState;
use crate::cfg::cfg::Cfg;
use crate::il::inst::{Arg, BinOp, InstTyp};
use crate::symbol::semantics::SymbolId;
use crate::{FnId, Program};
use std::collections::{BTreeMap, BTreeSet};

/// How a function parameter's *value* may escape from a callee.
///
/// This is used to refine `analysis::escape` for direct `Arg::Fn` calls.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FnEscapeSummary {
  /// For each parameter `i`, the escape state of the value passed for that parameter.
  pub param_escape: Vec<EscapeState>,
  /// Parameter indices that may be returned by the function (i.e. returned by alias).
  pub returns_param: BTreeSet<usize>,
  /// Parameter indices that may be thrown by the function (i.e. thrown by alias).
  pub throws_param: BTreeSet<usize>,
}

impl FnEscapeSummary {
  pub fn new(param_count: usize) -> Self {
    Self {
      param_escape: vec![EscapeState::NoEscape; param_count],
      returns_param: BTreeSet::new(),
      throws_param: BTreeSet::new(),
    }
  }

  pub fn join(&mut self, other: &Self) -> bool {
    let mut changed = false;
    let len = self.param_escape.len().min(other.param_escape.len());
    for i in 0..len {
      let next = self.param_escape[i].join(other.param_escape[i]);
      if next != self.param_escape[i] {
        self.param_escape[i] = next;
        changed = true;
      }
    }
    let before = self.returns_param.len();
    self.returns_param.extend(other.returns_param.iter().copied());
    changed |= self.returns_param.len() != before;

    let before = self.throws_param.len();
    self.throws_param.extend(other.throws_param.iter().copied());
    changed |= self.throws_param.len() != before;
    changed
  }
}

/// Escape summaries for every function in a program.
///
/// `functions` is index-aligned with `Program::functions` and `FnId`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgramEscapeSummaries {
  pub top_level: FnEscapeSummary,
  pub functions: Vec<FnEscapeSummary>,
  pub(crate) constant_foreign_fns: BTreeMap<SymbolId, FnId>,
}

impl ProgramEscapeSummaries {
  pub fn get(&self, id: FnId) -> Option<&FnEscapeSummary> {
    self.functions.get(id)
  }

  pub(crate) fn constant_foreign_fns(&self) -> &BTreeMap<SymbolId, FnId> {
    &self.constant_foreign_fns
  }
}

fn cfg_labels_sorted(cfg: &Cfg) -> Vec<u32> {
  let mut labels = cfg.bblocks.all().map(|(label, _)| label).collect::<Vec<_>>();
  labels.sort_unstable();
  labels
}

fn collect_insts(cfg: &Cfg) -> Vec<&crate::il::inst::Inst> {
  // Only consider instructions reachable from the CFG entry. The optimizer may
  // leave behind unreachable blocks (e.g. implicit `return undefined` after an
  // explicit return), and including them would pessimistically degrade
  // summaries.
  cfg
    .reverse_postorder()
    .into_iter()
    .flat_map(|label| cfg.bblocks.maybe_get(label).into_iter().flat_map(|bb| bb.iter()))
    .collect()
}

fn marker_call_is_safe(callee: &Arg) -> bool {
  matches!(
    callee,
    Arg::Builtin(path)
      if matches!(
        path.as_str(),
        "__optimize_js_object"
          | "__optimize_js_array"
          | "__optimize_js_regex"
          | "__optimize_js_template"
      )
  )
}

type VarParams = BTreeMap<u32, BTreeSet<usize>>;

fn params_for_arg(var_params: &VarParams, arg: &Arg) -> BTreeSet<usize> {
  match arg {
    Arg::Var(v) => var_params.get(v).cloned().unwrap_or_default(),
    _ => BTreeSet::new(),
  }
}

fn ext_for_arg(var_ext: &BTreeMap<u32, EscapeState>, arg: &Arg) -> EscapeState {
  match arg {
    Arg::Var(v) => var_ext.get(v).copied().unwrap_or(EscapeState::NoEscape),
    // Builtins and nested functions are not local values we can reason about.
    Arg::Builtin(_) | Arg::Fn(_) => EscapeState::GlobalEscape,
    Arg::Const(_) => EscapeState::NoEscape,
  }
}

#[derive(Default)]
struct LocalSummaryFacts {
  var_assigns: Vec<(u32, Arg)>,
  phis: Vec<(u32, Vec<Arg>)>,
  external_defs: BTreeSet<u32>,
  /// Direct-call candidates, recorded so we can add return-alias edges.
  calls: Vec<CallFact>,
}

#[derive(Clone)]
struct CallFact {
  tgt: u32,
  callee: Arg,
  args: Vec<Arg>,
  first_spread_arg: Option<usize>,
}

fn collect_local_summary_facts(cfg: &Cfg) -> LocalSummaryFacts {
  let mut facts = LocalSummaryFacts::default();

  for label in cfg_labels_sorted(cfg) {
    let block = cfg.bblocks.get(label);
    for inst in block.iter() {
      match inst.t {
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
        InstTyp::Bin if inst.bin_op == BinOp::GetProp => {
          let (tgt, _obj, _op, _prop) = inst.as_bin();
          facts.external_defs.insert(tgt);
        }
        InstTyp::FieldLoad => {
          let (tgt, _obj, _field) = inst.as_field_load();
          facts.external_defs.insert(tgt);
        }
        InstTyp::Call => {
          let (tgt, callee, _this, args, spreads) = inst.as_call();
          let Some(tgt) = tgt else {
            continue;
          };
          // Call results are conservatively treated as external objects unless they are one of the
          // internal marker allocators.
          if !marker_call_is_safe(callee) {
            facts.external_defs.insert(tgt);
          }
          facts.calls.push(CallFact {
            tgt,
            callee: callee.clone(),
            args: args.to_vec(),
            // Spreads make argument position mapping ambiguous after the first spread.
            first_spread_arg: spreads.iter().copied().min().map(|idx| idx.saturating_sub(2)),
          });
        }
        #[cfg(feature = "semantic-ops")]
        InstTyp::KnownApiCall { .. } => {
          let (tgt, _api, _args) = inst.as_known_api_call();
          if let Some(tgt) = tgt {
            facts.external_defs.insert(tgt);
          }
        }
        #[cfg(feature = "native-async-ops")]
        InstTyp::Await | InstTyp::PromiseAll | InstTyp::PromiseRace => {
          // These ops conceptually go through builtin/VM machinery (thenables / promises), so treat
          // their results as external values for summary purposes.
          if let Some(&tgt) = inst.tgts.get(0) {
            facts.external_defs.insert(tgt);
          }
        }
        _ => {}
      }
    }
  }

  facts
}

#[derive(Clone, Debug)]
enum VarDef {
  Alias(u32),
  Fn(FnId),
  Phi(Vec<Arg>),
  Unknown,
}

fn collect_constant_foreign_fns(program: &Program) -> BTreeMap<SymbolId, FnId> {
  // Recover direct calls through captured constant function bindings.
  //
  // Inside a nested function, references to captured variables lower to:
  //   %tmp = ForeignLoad(sym)
  // and calls go through the loaded temp. If the captured symbol is only ever
  // assigned a single `Arg::Fn(id)`, treat loads from that symbol as that
  // function ID for interprocedural summaries.
  let mut candidates = BTreeMap::<SymbolId, FnId>::new();
  let mut invalid = BTreeSet::<SymbolId>::new();

  let mut scan_cfg = |cfg: &Cfg| {
    for inst in collect_insts(cfg) {
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

  scan_cfg(program.top_level.analyzed_cfg());
  for func in &program.functions {
    scan_cfg(func.analyzed_cfg());
  }

  candidates
}

fn build_var_defs(cfg: &Cfg, foreign_fns: &BTreeMap<SymbolId, FnId>) -> BTreeMap<u32, VarDef> {
  let mut defs = BTreeMap::<u32, VarDef>::new();
  for inst in collect_insts(cfg) {
    let Some(&tgt) = inst.tgts.get(0) else {
      continue;
    };

    let def = match inst.t {
      InstTyp::VarAssign => match &inst.args[0] {
        Arg::Var(src) => VarDef::Alias(*src),
        Arg::Fn(id) => VarDef::Fn(*id),
        _ => VarDef::Unknown,
      },
      InstTyp::Phi => VarDef::Phi(inst.args.clone()),
      InstTyp::ForeignLoad => foreign_fns
        .get(&inst.foreign)
        .copied()
        .map(VarDef::Fn)
        .unwrap_or(VarDef::Unknown),
      _ => VarDef::Unknown,
    };

    defs
      .entry(tgt)
      .and_modify(|existing| {
        if !matches!(existing, VarDef::Unknown) {
          *existing = VarDef::Unknown;
        }
      })
      .or_insert(def);
  }
  defs
}

fn resolve_fn_id(arg: &Arg, defs: &BTreeMap<u32, VarDef>, visiting: &mut Vec<u32>) -> Option<FnId> {
  match arg {
    Arg::Fn(id) => Some(*id),
    Arg::Var(v) => resolve_var_fn_id(*v, defs, visiting),
    _ => None,
  }
}

fn resolve_var_fn_id(var: u32, defs: &BTreeMap<u32, VarDef>, visiting: &mut Vec<u32>) -> Option<FnId> {
  if visiting.contains(&var) {
    return None;
  }
  visiting.push(var);

  let out = match defs.get(&var) {
    Some(VarDef::Fn(id)) => Some(*id),
    Some(VarDef::Alias(src)) => resolve_var_fn_id(*src, defs, visiting),
    Some(VarDef::Phi(args)) => {
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

fn compute_cfg_escape_summary(
  cfg: &Cfg,
  params: &[u32],
  program_summaries: &ProgramEscapeSummaries,
  foreign_fns: &BTreeMap<SymbolId, FnId>,
) -> FnEscapeSummary {
  let mut summary = FnEscapeSummary::new(params.len());
  let facts = collect_local_summary_facts(cfg);
  let defs = build_var_defs(cfg, foreign_fns);

  // Parameter alias tracking (VarAssign + Phi + direct-call return aliases).
  let mut var_params: VarParams = BTreeMap::new();
  for (idx, &param) in params.iter().enumerate() {
    var_params.insert(param, BTreeSet::from([idx]));
  }

  let mut var_ext: BTreeMap<u32, EscapeState> = BTreeMap::new();
  for (idx, &param) in params.iter().enumerate() {
    var_ext.insert(param, EscapeState::ArgEscape(idx));
  }
  for &v in facts.external_defs.iter() {
    let entry = var_ext.entry(v).or_insert(EscapeState::NoEscape);
    *entry = entry.join(EscapeState::GlobalEscape);
  }

  // Build additional var-assign edges for call results that may alias an argument.
  let mut var_assigns = facts.var_assigns.clone();
  for call in facts.calls.iter() {
    let Some(callee) = resolve_fn_id(&call.callee, &defs, &mut Vec::new()) else {
      continue;
    };
    let Some(callee_summary) = program_summaries.get(callee) else {
      continue;
    };
    for &k in callee_summary.returns_param.iter() {
      if call.first_spread_arg.is_some_and(|first| k >= first) {
        continue;
      }
      if let Some(arg) = call.args.get(k) {
        var_assigns.push((call.tgt, arg.clone()));
      }
    }
  }

  let mut changed = true;
  while changed {
    changed = false;

    for (tgt, arg) in var_assigns.iter() {
      let src_params = params_for_arg(&var_params, arg);
      if !src_params.is_empty() {
        let entry = var_params.entry(*tgt).or_default();
        let before = entry.len();
        entry.extend(src_params);
        if entry.len() != before {
          changed = true;
        }
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
      let mut merged_params = BTreeSet::new();
      let mut merged_ext = EscapeState::NoEscape;
      for arg in args.iter() {
        merged_params.extend(params_for_arg(&var_params, arg));
        merged_ext = merged_ext.join(ext_for_arg(&var_ext, arg));
      }

      if !merged_params.is_empty() {
        let entry = var_params.entry(*tgt).or_default();
        let before = entry.len();
        entry.extend(merged_params);
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
  }

  // Find escape sinks and update per-param summary.
  for label in cfg_labels_sorted(cfg) {
    let block = cfg.bblocks.get(label);
    for inst in block.iter() {
      match inst.t {
        InstTyp::Return => {
          if let Some(value) = inst.as_return() {
            for idx in params_for_arg(&var_params, value) {
              summary.param_escape[idx] = summary.param_escape[idx].join(EscapeState::ReturnEscape);
              summary.returns_param.insert(idx);
            }
          }
        }
        InstTyp::Throw => {
          for idx in params_for_arg(&var_params, inst.as_throw()) {
            summary.param_escape[idx] = summary.param_escape[idx].join(EscapeState::ReturnEscape);
            summary.throws_param.insert(idx);
          }
        }
        InstTyp::ForeignStore | InstTyp::UnknownStore => {
          for idx in params_for_arg(&var_params, &inst.args[0]) {
            summary.param_escape[idx] = summary.param_escape[idx].join(EscapeState::GlobalEscape);
          }
        }
        InstTyp::PropAssign | InstTyp::FieldStore => {
          let (obj, val) = match inst.t {
            InstTyp::PropAssign => {
              let (obj, _prop, val) = inst.as_prop_assign();
              (obj, val)
            }
            InstTyp::FieldStore => {
              let (obj, _field, val) = inst.as_field_store();
              (obj, val)
            }
            _ => unreachable!(),
          };
          let value_params = params_for_arg(&var_params, val);
          if value_params.is_empty() {
            continue;
          }
          let obj_ext = ext_for_arg(&var_ext, obj);
          if obj_ext == EscapeState::NoEscape {
            continue;
          }
          for idx in value_params {
            summary.param_escape[idx] = summary.param_escape[idx].join(obj_ext);
          }
        }
        InstTyp::Call => {
          let (_tgt, callee, this, args, spreads) = inst.as_call();
          if marker_call_is_safe(callee) {
            continue;
          }

          let first_spread_arg = spreads.iter().copied().min().map(|idx| idx.saturating_sub(2));

          // Conservatively treat passing a param as the `this` value as escaping; we do not model
          // `this` in summaries (only formal parameters).
          for idx in params_for_arg(&var_params, this) {
            summary.param_escape[idx] = summary.param_escape[idx].join(EscapeState::GlobalEscape);
          }

          if let Some(id) = resolve_fn_id(callee, &defs, &mut Vec::new()) {
            let Some(callee_summary) = program_summaries.get(id) else {
              // Missing summary should be impossible; stay conservative.
              for arg in args.iter() {
                for idx in params_for_arg(&var_params, arg) {
                  summary.param_escape[idx] = summary.param_escape[idx].join(EscapeState::GlobalEscape);
                }
              }
              continue;
            };

            for (k, arg) in args.iter().enumerate() {
              let passed_params = params_for_arg(&var_params, arg);
              if passed_params.is_empty() {
                continue;
              }

              // When a call site contains spreads, argument indexing is ambiguous after the first
              // spread. Only trust per-parameter summaries for arguments that appear before the
              // first spread; conservatively treat the rest as escaping via an unknown call.
              if first_spread_arg.is_some_and(|first| k >= first) {
                for idx in passed_params {
                  summary.param_escape[idx] =
                    summary.param_escape[idx].join(EscapeState::GlobalEscape);
                }
                continue;
              }

              let callee_state = callee_summary
                .param_escape
                .get(k)
                .copied()
                .unwrap_or(EscapeState::Unknown);

              // `ReturnEscape` in a callee summary means the argument may be returned *or thrown*
              // to the callee's caller.
              //
              // - Return-by-alias: does not by itself cause the value to escape *from this
              //   wrapper* (the wrapper would also have to return it, which is handled by the
              //   return-alias var-assign edges built above).
              // - Throw-by-alias: does escape from this wrapper, because we don't model
              //   `try`/`catch` explicitly and treat throws as escaping to the caller.
              let thrown_by_call = callee_state == EscapeState::ReturnEscape
                && callee_summary.throws_param.contains(&k);
              let mapped = match callee_state {
                EscapeState::NoEscape => EscapeState::NoEscape,
                EscapeState::ReturnEscape => {
                  if thrown_by_call {
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
                    // Map into this function's parameter index space when possible.
                    match args.get(j) {
                      Some(receiver_arg) => {
                        let receiver_params = params_for_arg(&var_params, receiver_arg);
                        if receiver_params.len() == 1 {
                          EscapeState::ArgEscape(*receiver_params.iter().next().unwrap())
                        } else {
                          EscapeState::GlobalEscape
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
              for idx in passed_params {
                summary.param_escape[idx] = summary.param_escape[idx].join(mapped);
                if thrown_by_call {
                  summary.throws_param.insert(idx);
                }
              }
            }
          } else {
            // Unknown/impure call: conservatively treat any parameter passed as escaping.
            for arg in inst.args.iter() {
              for idx in params_for_arg(&var_params, arg) {
                summary.param_escape[idx] = summary.param_escape[idx].join(EscapeState::GlobalEscape);
              }
            }
          }
        }
        #[cfg(feature = "semantic-ops")]
        InstTyp::KnownApiCall { .. } => {
          // Conservatively treat known-api calls as unknown calls until we have
          // `knowledge-base`-aware modeling for them.
          for arg in inst.args.iter() {
            for idx in params_for_arg(&var_params, arg) {
              summary.param_escape[idx] =
                summary.param_escape[idx].join(EscapeState::GlobalEscape);
            }
          }
        }
        #[cfg(feature = "native-async-ops")]
        InstTyp::Await | InstTyp::PromiseAll | InstTyp::PromiseRace => {
          // Async semantic ops may retain references to their inputs (e.g. awaiting thenables or
          // Promise.all attaching handlers), so conservatively treat any parameter passed as
          // escaping.
          for arg in inst.args.iter() {
            for idx in params_for_arg(&var_params, arg) {
              summary.param_escape[idx] =
                summary.param_escape[idx].join(EscapeState::GlobalEscape);
            }
          }
        }
        _ => {}
      }
    }
  }

  summary
}

/// Compute deterministic interprocedural parameter escape summaries for the whole program.
pub fn compute_program_escape_summaries(program: &Program) -> ProgramEscapeSummaries {
  let foreign_fns = collect_constant_foreign_fns(program);
  let mut summaries = ProgramEscapeSummaries {
    top_level: FnEscapeSummary::new(program.top_level.params.len()),
    functions: program
      .functions
      .iter()
      .map(|f| FnEscapeSummary::new(f.params.len()))
      .collect(),
    constant_foreign_fns: foreign_fns.clone(),
  };

  loop {
    let prev = summaries.clone();
    let mut changed = false;

    let new_top =
      compute_cfg_escape_summary(
        program.top_level.analyzed_cfg(),
        &program.top_level.params,
        &prev,
        &foreign_fns,
      );
    if new_top != prev.top_level {
      summaries.top_level = new_top;
      changed = true;
    }

    for id in 0..program.functions.len() {
      let func = &program.functions[id];
      let new_summary = compute_cfg_escape_summary(func.analyzed_cfg(), &func.params, &prev, &foreign_fns);
      if new_summary != prev.functions[id] {
        summaries.functions[id] = new_summary;
        changed = true;
      }
    }

    if !changed {
      break;
    }
  }

  summaries
}
