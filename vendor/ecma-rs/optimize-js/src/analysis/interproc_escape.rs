use crate::analysis::escape::EscapeState;
use crate::cfg::cfg::Cfg;
use crate::il::inst::{Arg, BinOp, InstTyp};
use crate::{FnId, Program};
use std::collections::{BTreeMap, BTreeSet};

/// How a function parameter's *value* may escape from a callee.
///
/// This is used to refine `analysis::escape` for direct `Arg::Fn` calls.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FnEscapeSummary {
  /// For each parameter `i`, the escape state of the value passed for that parameter.
  pub param_escape: Vec<EscapeState>,
  /// Parameter indices that may be returned/thrown by the function (i.e. returned by alias).
  pub returns_param: BTreeSet<usize>,
}

impl FnEscapeSummary {
  pub fn new(param_count: usize) -> Self {
    Self {
      param_escape: vec![EscapeState::NoEscape; param_count],
      returns_param: BTreeSet::new(),
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
}

impl ProgramEscapeSummaries {
  pub fn get(&self, id: FnId) -> Option<&FnEscapeSummary> {
    self.functions.get(id)
  }
}

fn cfg_labels_sorted(cfg: &Cfg) -> Vec<u32> {
  let mut labels = cfg.bblocks.all().map(|(label, _)| label).collect::<Vec<_>>();
  labels.sort_unstable();
  labels
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
  /// Direct `Arg::Fn` call sites, recorded so we can add return-alias edges.
  direct_calls: Vec<DirectCallFact>,
}

#[derive(Clone)]
struct DirectCallFact {
  tgt: u32,
  callee: FnId,
  args: Vec<Arg>,
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
        InstTyp::Call => {
          let (tgt, callee, _this, args, _spreads) = inst.as_call();
          let Some(tgt) = tgt else {
            continue;
          };
          // Call results are conservatively treated as external objects unless they are one of the
          // internal marker allocators.
          if !marker_call_is_safe(callee) {
            facts.external_defs.insert(tgt);
          }
          if let Arg::Fn(id) = callee {
            facts.direct_calls.push(DirectCallFact {
              tgt,
              callee: *id,
              args: args.to_vec(),
            });
          }
        }
        _ => {}
      }
    }
  }

  facts
}

fn compute_cfg_escape_summary(
  cfg: &Cfg,
  params: &[u32],
  program_summaries: &ProgramEscapeSummaries,
) -> FnEscapeSummary {
  let mut summary = FnEscapeSummary::new(params.len());
  let facts = collect_local_summary_facts(cfg);

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
  for call in facts.direct_calls.iter() {
    let Some(callee_summary) = program_summaries.get(call.callee) else {
      continue;
    };
    for &k in callee_summary.returns_param.iter() {
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
              summary.param_escape[idx] =
                summary.param_escape[idx].join(EscapeState::ReturnEscape);
              summary.returns_param.insert(idx);
            }
          }
        }
        InstTyp::Throw => {
          for idx in params_for_arg(&var_params, inst.as_throw()) {
            summary.param_escape[idx] = summary.param_escape[idx].join(EscapeState::ReturnEscape);
            summary.returns_param.insert(idx);
          }
        }
        InstTyp::ForeignStore | InstTyp::UnknownStore => {
          for idx in params_for_arg(&var_params, &inst.args[0]) {
            summary.param_escape[idx] = summary.param_escape[idx].join(EscapeState::GlobalEscape);
          }
        }
        InstTyp::PropAssign => {
          let (obj, _prop, val) = inst.as_prop_assign();
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
          let (_tgt, callee, this, args, _spreads) = inst.as_call();
          if marker_call_is_safe(callee) {
            continue;
          }

          // Conservatively treat passing a param as the `this` value as escaping; we do not model
          // `this` in summaries (only formal parameters).
          for idx in params_for_arg(&var_params, this) {
            summary.param_escape[idx] = summary.param_escape[idx].join(EscapeState::GlobalEscape);
          }

          match callee {
            Arg::Fn(id) => {
              let Some(callee_summary) = program_summaries.get(*id) else {
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

                let callee_state = callee_summary
                  .param_escape
                  .get(k)
                  .copied()
                  .unwrap_or(EscapeState::Unknown);

                // `ReturnEscape` means "returned to the caller"; it does not by itself cause the
                // passed value to escape *from this function*.
                let mapped = match callee_state {
                  EscapeState::NoEscape | EscapeState::ReturnEscape => EscapeState::NoEscape,
                  EscapeState::GlobalEscape | EscapeState::Unknown => callee_state,
                  EscapeState::ArgEscape(j) => {
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
                };

                if mapped == EscapeState::NoEscape {
                  continue;
                }
                for idx in passed_params {
                  summary.param_escape[idx] = summary.param_escape[idx].join(mapped);
                }
              }
            }
            _ => {
              // Unknown/impure call: conservatively treat any parameter passed as escaping.
              for arg in inst.args.iter() {
                for idx in params_for_arg(&var_params, arg) {
                  summary.param_escape[idx] = summary.param_escape[idx].join(EscapeState::GlobalEscape);
                }
              }
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
  let mut summaries = ProgramEscapeSummaries {
    top_level: FnEscapeSummary::new(program.top_level.params.len()),
    functions: program
      .functions
      .iter()
      .map(|f| FnEscapeSummary::new(f.params.len()))
      .collect(),
  };

  loop {
    let prev = summaries.clone();
    let mut changed = false;

    let new_top =
      compute_cfg_escape_summary(program.top_level.analyzed_cfg(), &program.top_level.params, &prev);
    if new_top != prev.top_level {
      summaries.top_level = new_top;
      changed = true;
    }

    for id in 0..program.functions.len() {
      let func = &program.functions[id];
      let new_summary = compute_cfg_escape_summary(func.analyzed_cfg(), &func.params, &prev);
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
