use crate::cfg::cfg::Cfg;
use crate::il::inst::{Arg, InstTyp};
use crate::analysis::escape::EscapeState;
use crate::Program;
use crate::symbol::semantics::SymbolId;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum ReturnKind {
  /// Conservative fallback.
  Unknown,
  /// Returns a fresh allocation produced within the function.
  FreshAlloc,
  /// Returns (aliases) one of the function parameters.
  AliasParam(usize),
  /// Returns a constant value (including `undefined`).
  Const,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct FnSummary {
  /// Escape information for each parameter, in parameter index order.
  pub param_escape: Vec<EscapeState>,
  /// Escape information for the returned value.
  pub return_escape: EscapeState,
  /// Return classification used by ownership/alias inference.
  pub return_kind: ReturnKind,
}

impl FnSummary {
  fn conservative(params_len: usize) -> Self {
    Self {
      param_escape: vec![EscapeState::GlobalEscape; params_len],
      return_escape: EscapeState::GlobalEscape,
      return_kind: ReturnKind::Unknown,
    }
  }

  fn escape_rank(state: EscapeState) -> u8 {
    match state {
      EscapeState::NoEscape => 0,
      EscapeState::ReturnEscape => 1,
      EscapeState::ArgEscape(_) => 2,
      EscapeState::GlobalEscape => 3,
      EscapeState::Unknown => 4,
    }
  }

  /// Refine this summary in-place using `new`, returning whether anything changed.
  ///
  /// Refinement is intentionally one-way: we only accept strictly more precise
  /// information. This keeps the program-level fixpoint deterministic and
  /// prevents oscillation in the presence of recursion.
  fn refine_with(&mut self, new: FnSummary) -> bool {
    let mut changed = false;

    if self.param_escape.len() == new.param_escape.len() {
      for (cur, next) in self.param_escape.iter_mut().zip(new.param_escape) {
        if Self::escape_rank(next) < Self::escape_rank(*cur) {
          *cur = next;
          changed = true;
        }
      }
    }

    if Self::escape_rank(new.return_escape) < Self::escape_rank(self.return_escape) {
      self.return_escape = new.return_escape;
      changed = true;
    }

    if matches!(self.return_kind, ReturnKind::Unknown) && !matches!(new.return_kind, ReturnKind::Unknown)
    {
      self.return_kind = new.return_kind;
      changed = true;
    }

    changed
  }
}

#[derive(Clone, Debug)]
enum VarDef {
  Alias(u32),
  Const,
  Fn(usize),
  FreshAlloc,
  CallFn {
    id: usize,
    args: Vec<Arg>,
    /// Index (within `args`) of the first spread argument, if any.
    ///
    /// Spread arguments make parameter→argument mapping ambiguous after the
    /// first spread, so we only model `ReturnKind::AliasParam(i)` through
    /// call results when `i < first_spread_arg`.
    first_spread_arg: Option<usize>,
  },
  Phi(Vec<Arg>),
  Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Origin {
  Param(usize),
  Const,
  FreshAlloc,
  Unknown,
}

fn is_alloc_builtin(callee: &Arg) -> bool {
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

fn collect_constant_foreign_fns(program: &Program) -> HashMap<SymbolId, usize> {
  // This is used to recover direct `Arg::Fn` calls through captured constant
  // bindings, e.g.:
  //
  //   const g = () => ({x:1});
  //   const f = (...args) => g(...args);
  //
  // Inside `f` the callee is a `ForeignLoad` of the captured symbol. If the
  // symbol is only ever assigned a single function ID, treat loads from that
  // symbol as that function for summary purposes.
  let mut candidates = HashMap::<SymbolId, usize>::new();
  let mut invalid = HashSet::<SymbolId>::new();

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
        Some(Arg::Fn(id)) => match candidates.get(&sym) {
          None => {
            candidates.insert(sym, *id);
          }
          Some(existing) if *existing == *id => {}
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

fn resolve_var_fn_id(var: u32, defs: &HashMap<u32, VarDef>, visiting: &mut Vec<u32>) -> Option<usize> {
  if visiting.contains(&var) {
    return None;
  }
  visiting.push(var);

  let out = match defs.get(&var) {
    Some(VarDef::Fn(id)) => Some(*id),
    Some(VarDef::Alias(src)) => resolve_var_fn_id(*src, defs, visiting),
    Some(VarDef::Phi(args)) => {
      let mut merged: Option<usize> = None;
      for arg in args {
        let Arg::Var(v) = arg else {
          return None;
        };
        let Some(id) = resolve_var_fn_id(*v, defs, visiting) else {
          return None;
        };
        merged = match merged {
          None => Some(id),
          Some(prev) if prev == id => Some(prev),
          _ => return None,
        };
      }
      merged
    }
    _ => None,
  };

  visiting.pop();
  out
}

fn resolve_fn_id(arg: &Arg, defs: &HashMap<u32, VarDef>) -> Option<usize> {
  match arg {
    Arg::Fn(id) => Some(*id),
    Arg::Var(v) => resolve_var_fn_id(*v, defs, &mut Vec::new()),
    _ => None,
  }
}

fn build_var_defs(
  cfg: &Cfg,
  callee_summaries: &[FnSummary],
  foreign_fns: &HashMap<SymbolId, usize>,
) -> HashMap<u32, VarDef> {
  let mut defs = HashMap::<u32, VarDef>::new();
  for inst in collect_insts(cfg) {
    let Some(&tgt) = inst.tgts.get(0) else {
      continue;
    };

    let def = match inst.t {
      InstTyp::VarAssign => match &inst.args[0] {
        Arg::Var(src) => VarDef::Alias(*src),
        Arg::Const(_) => VarDef::Const,
        Arg::Fn(id) => VarDef::Fn(*id),
        _ => VarDef::Unknown,
      },
      InstTyp::ForeignLoad => foreign_fns
        .get(&inst.foreign)
        .copied()
        .map(VarDef::Fn)
        .unwrap_or(VarDef::Unknown),
      InstTyp::Phi => VarDef::Phi(inst.args.clone()),
      InstTyp::Call => {
        let (tgt, callee, _this, args, spreads) = inst.as_call();
        if tgt.is_none() {
          continue;
        }
        if is_alloc_builtin(callee) {
          VarDef::FreshAlloc
        } else if let Some(id) = resolve_fn_id(callee, &defs) {
          // If callee isn't in-bounds, treat as unknown.
          if id < callee_summaries.len() {
            VarDef::CallFn {
              id,
              args: args.to_vec(),
              first_spread_arg: spreads.iter().copied().min().map(|idx| idx.saturating_sub(2)),
            }
          } else {
            VarDef::Unknown
          }
        } else {
          VarDef::Unknown
        }
      }
      // Everything else either produces a non-aliasable value (Bin/Un) or is
      // not tracked by this summary analysis yet.
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

fn resolve_arg_origin(
  arg: &Arg,
  param_to_index: &HashMap<u32, usize>,
  defs: &HashMap<u32, VarDef>,
  callee_summaries: &[FnSummary],
  visiting: &mut Vec<u32>,
) -> Origin {
  match arg {
    Arg::Var(v) => resolve_var_origin(*v, param_to_index, defs, callee_summaries, visiting),
    Arg::Const(_) => Origin::Const,
    _ => Origin::Unknown,
  }
}

fn resolve_var_origin(
  var: u32,
  param_to_index: &HashMap<u32, usize>,
  defs: &HashMap<u32, VarDef>,
  callee_summaries: &[FnSummary],
  visiting: &mut Vec<u32>,
) -> Origin {
  if let Some(&idx) = param_to_index.get(&var) {
    return Origin::Param(idx);
  }

  if visiting.contains(&var) {
    return Origin::Unknown;
  }
  visiting.push(var);

  let origin = match defs.get(&var) {
    Some(VarDef::Alias(src)) => {
      resolve_var_origin(*src, param_to_index, defs, callee_summaries, visiting)
    }
    Some(VarDef::Const) => Origin::Const,
    Some(VarDef::FreshAlloc) => Origin::FreshAlloc,
    Some(VarDef::Phi(args)) => {
      let mut merged: Option<Origin> = None;
      for arg in args {
        let origin = resolve_arg_origin(arg, param_to_index, defs, callee_summaries, visiting);
        merged = match merged {
          None => Some(origin),
          Some(prev) if prev == origin => Some(prev),
          _ => Some(Origin::Unknown),
        };
        if merged == Some(Origin::Unknown) {
          break;
        }
      }
      merged.unwrap_or(Origin::Unknown)
    }
    Some(VarDef::CallFn {
      id,
      args,
      first_spread_arg,
    }) => {
      match callee_summaries.get(*id) {
        Some(summary) => match &summary.return_kind {
          ReturnKind::FreshAlloc => Origin::FreshAlloc,
          ReturnKind::Const => Origin::Const,
          ReturnKind::AliasParam(i) => args
            .get(*i)
            .and_then(|arg| {
              // Only model param aliasing through calls when we can map the
              // parameter index to a concrete argument (i.e. the argument
              // position is before any spread).
              if first_spread_arg.is_some_and(|first| *i >= first) {
                None
              } else {
                Some(resolve_arg_origin(
                  arg,
                  param_to_index,
                  defs,
                  callee_summaries,
                  visiting,
                ))
              }
            })
            .unwrap_or(Origin::Unknown),
          ReturnKind::Unknown => Origin::Unknown,
        },
        None => Origin::Unknown,
      }
    }
    _ => Origin::Unknown,
  };

  visiting.pop();
  origin
}

fn max_escape(a: EscapeState, b: EscapeState) -> EscapeState {
  if FnSummary::escape_rank(a) >= FnSummary::escape_rank(b) {
    a
  } else {
    b
  }
}

fn summarize_function(
  func: &crate::ProgramFunction,
  callee_summaries: &[FnSummary],
  foreign_fns: &HashMap<SymbolId, usize>,
) -> FnSummary {
  let cfg = func.analyzed_cfg();
  let mut param_escape = vec![EscapeState::NoEscape; func.params.len()];
  let param_to_index: HashMap<u32, usize> = func
    .params
    .iter()
    .copied()
    .enumerate()
    .map(|(idx, var)| (var, idx))
    .collect();

  let defs = build_var_defs(cfg, callee_summaries, foreign_fns);

  let mut return_origins = Vec::<Origin>::new();

  for inst in collect_insts(cfg) {
    match inst.t {
      InstTyp::Call => {
        let (_tgt, callee, _this, args, spreads) = inst.as_call();
        let direct = matches!(callee, Arg::Fn(id) if *id < callee_summaries.len()) && spreads.is_empty();

        for (idx, arg) in args.iter().enumerate() {
          let origin = resolve_arg_origin(
            arg,
            &param_to_index,
            &defs,
            callee_summaries,
            &mut Vec::new(),
          );
          let Origin::Param(p) = origin else {
            continue;
          };

          if direct {
            let Arg::Fn(id) = callee else { unreachable!() };
            let summary = &callee_summaries[*id];
            let callee_escape = summary
              .param_escape
              .get(idx)
              .copied()
              .unwrap_or(EscapeState::GlobalEscape);
            param_escape[p] = max_escape(param_escape[p], callee_escape);
          } else {
            // Unknown call - conservatively assume any argument may globally escape.
            param_escape[p] = EscapeState::GlobalEscape;
          }
        }
      }
      #[cfg(feature = "semantic-ops")]
      InstTyp::KnownApiCall { .. } => {
        let (_tgt, _api, args) = inst.as_known_api_call();
        // Conservatively treat known-api calls as unknown calls until we have
        // `knowledge-base`-aware summaries for them.
        for arg in args.iter() {
          let origin = resolve_arg_origin(
            arg,
            &param_to_index,
            &defs,
            callee_summaries,
            &mut Vec::new(),
          );
          if let Origin::Param(p) = origin {
            param_escape[p] = EscapeState::GlobalEscape;
          }
        }
      }
      InstTyp::ForeignStore | InstTyp::UnknownStore => {
        let value = inst.args.get(0).expect("store has one arg");
        let origin = resolve_arg_origin(
          value,
          &param_to_index,
          &defs,
          callee_summaries,
          &mut Vec::new(),
        );
        if let Origin::Param(p) = origin {
          param_escape[p] = EscapeState::GlobalEscape;
        }
      }
      InstTyp::Return => {
        let origin = match inst.as_return() {
          Some(value) => resolve_arg_origin(
            value,
            &param_to_index,
            &defs,
            callee_summaries,
            &mut Vec::new(),
          ),
          // `return;` / falling off the end returns `undefined`.
          None => Origin::Const,
        };
        return_origins.push(origin);
        if let Origin::Param(p) = origin {
          param_escape[p] = max_escape(param_escape[p], EscapeState::ReturnEscape);
        }
      }
      InstTyp::Throw => {
        let value = inst.as_throw();
        let origin = resolve_arg_origin(
          value,
          &param_to_index,
          &defs,
          callee_summaries,
          &mut Vec::new(),
        );
        if let Origin::Param(p) = origin {
          param_escape[p] = max_escape(param_escape[p], EscapeState::ReturnEscape);
        }
      }
      _ => {}
    }
  }

  let return_kind = if return_origins.is_empty() {
    ReturnKind::Unknown
  } else if return_origins.iter().all(|o| *o == Origin::FreshAlloc) {
    ReturnKind::FreshAlloc
  } else if return_origins.iter().all(|o| matches!(o, Origin::Const)) {
    ReturnKind::Const
  } else if let Some(Origin::Param(p0)) = return_origins.first().copied() {
    if return_origins
      .iter()
      .all(|o| matches!(o, Origin::Param(p) if *p == p0))
    {
      ReturnKind::AliasParam(p0)
    } else {
      ReturnKind::Unknown
    }
  } else {
    ReturnKind::Unknown
  };

  let return_escape = match return_kind {
    ReturnKind::FreshAlloc | ReturnKind::AliasParam(_) => EscapeState::ReturnEscape,
    ReturnKind::Const => EscapeState::NoEscape,
    ReturnKind::Unknown => EscapeState::GlobalEscape,
  };

  FnSummary {
    param_escape,
    return_escape,
    return_kind,
  }
}

/// Compute call summaries for all nested functions in `program`.
///
/// The returned vector is aligned with `program.functions` (and therefore
/// `Arg::Fn(id)` indices). Summaries are computed to a fixpoint in a deterministic
/// order.
pub fn summarize_program(program: &Program) -> Vec<FnSummary> {
  let foreign_fns = collect_constant_foreign_fns(program);
  let mut summaries: Vec<_> = program
    .functions
    .iter()
    .map(|f| FnSummary::conservative(f.params.len()))
    .collect();

  // In the current lattice all fields only refine (move toward "more precise").
  // We cap iterations defensively to avoid infinite loops if invariants are
  // violated.
  let max_iters = std::cmp::max(1, program.functions.len() * 8);
  for _ in 0..max_iters {
    let mut changed = false;
    for (idx, func) in program.functions.iter().enumerate() {
      let computed = summarize_function(func, &summaries, &foreign_fns);
      changed |= summaries[idx].refine_with(computed);
    }
    if !changed {
      break;
    }
  }

  summaries
}
