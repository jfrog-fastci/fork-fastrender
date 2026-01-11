use crate::analysis::effect::FnEffectMap;
use crate::cfg::cfg::Cfg;
use crate::il::inst::EffectSet;
use crate::il::inst::{Arg, InstTyp};
use crate::{FnId, Program};
pub use effect_model::Purity;
use effect_model::{EffectFlags, ThrowBehavior};

/// `optimize-js` uses the canonical purity taxonomy from `effect-model`.
///
/// Note: We treat "unknown purity" as [`Purity::Impure`] in this pass (i.e. we
/// stay conservative when we can't prove purity).
pub(crate) fn is_default_purity(purity: &Purity) -> bool {
  matches!(purity, Purity::Impure)
}

/// Purity summaries for every function in a [`crate::Program`].
///
/// `functions` is index-aligned with `Program::functions` and `FnId`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct FnPurityMap {
  pub top_level: Purity,
  pub functions: Vec<Purity>,
}

impl Default for FnPurityMap {
  fn default() -> Self {
    Self {
      top_level: Purity::Impure,
      functions: Vec::new(),
    }
  }
}

impl FnPurityMap {
  pub fn new(top_level: Purity, functions: Vec<Purity>) -> Self {
    Self { top_level, functions }
  }

  pub fn for_fn(&self, id: FnId) -> Purity {
    self.functions.get(id).copied().unwrap_or(Purity::Impure)
  }
}

fn purity_from_effects(effects: &EffectSet) -> Purity {
  if effects.unknown
    || !effects.writes.is_empty()
    || !matches!(effects.summary.throws, ThrowBehavior::Never)
  {
    return Purity::Impure;
  }

  if !effects.reads.is_empty() {
    return Purity::ReadOnly;
  }

  if effects.summary.flags.contains(EffectFlags::ALLOCATES) {
    // Allocating is only used when allocation is the *only* tracked effect.
    if effects.summary.flags != EffectFlags::ALLOCATES {
      return Purity::Impure;
    }
    return Purity::Allocating;
  }

  if !effects.summary.flags.is_empty() {
    return Purity::Impure;
  }

  Purity::Pure
}

pub fn compute_program_purity(program: &Program, effects: &FnEffectMap) -> FnPurityMap {
  assert_eq!(
    program.functions.len(),
    effects.functions.len(),
    "FnEffectMap must be index-aligned with Program::functions"
  );

  FnPurityMap {
    top_level: purity_from_effects(&effects.top_level),
    functions: effects.functions.iter().map(purity_from_effects).collect(),
  }
}

fn builtin_callee_purity(path: &str) -> Purity {
  // Keep in sync with `eval/consteval.rs:maybe_eval_const_builtin_call` for calls we treat as pure.
  match path {
    "Math.abs"
    | "Math.acos"
    | "Math.asin"
    | "Math.atan"
    | "Math.ceil"
    | "Math.cos"
    | "Math.floor"
    | "Math.log"
    | "Math.log10"
    | "Math.log1p"
    | "Math.log2"
    | "Math.round"
    | "Math.sin"
    | "Math.sqrt"
    | "Math.tan"
    | "Math.trunc"
    | "Number" => Purity::Pure,

    // Internal lowering helpers that construct literals / allocate.
    "__optimize_js_array"
    | "__optimize_js_object"
    | "__optimize_js_regex"
    | "__optimize_js_template"
    | "__optimize_js_tagged_template" => Purity::Allocating,

    _ => Purity::Impure,
  }
}

fn callee_purity(callee: &Arg, purities: &FnPurityMap) -> Purity {
  match callee {
    Arg::Fn(id) => purities.for_fn(*id),
    Arg::Builtin(path) => builtin_callee_purity(path),
    _ => Purity::Impure,
  }
}

pub fn annotate_cfg_purity(cfg: &mut Cfg, purities: &FnPurityMap) {
  for label in cfg.graph.labels_sorted() {
    for inst in cfg.bblocks.get_mut(label).iter_mut() {
      if inst.t != InstTyp::Call {
        continue;
      }

      let (_, callee, _, _, _) = inst.as_call();
      inst.meta.callee_purity = callee_purity(callee, purities);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::cfg::cfg::{CfgBBlocks, CfgGraph};
  use crate::il::inst::{Const, Inst};
  use crate::OptimizationStats;
  use crate::ProgramFunction;
  use crate::TopLevelMode;

  fn cfg_with_single_call(callee: Arg) -> Cfg {
    let mut graph = CfgGraph::default();
    // Ensure label 0 exists in the CFG graph.
    graph.connect(0, 0);
    let mut bblocks = CfgBBlocks::default();
    bblocks.add(
      0,
      vec![Inst::call(
        None::<u32>,
        callee,
        Arg::Const(Const::Undefined),
        Vec::new(),
        Vec::new(),
      )],
    );
    Cfg {
      graph,
      bblocks,
      entry: 0,
    }
  }

  fn empty_program_function() -> ProgramFunction {
    let mut graph = CfgGraph::default();
    graph.connect(0, 0);
    let mut bblocks = CfgBBlocks::default();
    bblocks.add(0, Vec::new());
    ProgramFunction {
      debug: None,
      body: Cfg {
        graph,
        bblocks,
        entry: 0,
      },
      params: Vec::new(),
      stats: OptimizationStats::default(),
    }
  }

  #[test]
  fn pure_builtin_call_is_pure() {
    let mut cfg = cfg_with_single_call(Arg::Builtin("Math.abs".to_string()));
    annotate_cfg_purity(&mut cfg, &FnPurityMap::default());
    assert_eq!(cfg.bblocks.get(0)[0].meta.callee_purity, Purity::Pure);
  }

  #[test]
  fn internal_array_literal_call_is_allocating() {
    let mut cfg = cfg_with_single_call(Arg::Builtin("__optimize_js_array".to_string()));
    annotate_cfg_purity(&mut cfg, &FnPurityMap::default());
    assert_eq!(
      cfg.bblocks.get(0)[0].meta.callee_purity,
      Purity::Allocating
    );
  }

  #[test]
  fn unknown_call_is_impure() {
    let mut cfg = cfg_with_single_call(Arg::Var(0));
    annotate_cfg_purity(&mut cfg, &FnPurityMap::default());
    assert_eq!(cfg.bblocks.get(0)[0].meta.callee_purity, Purity::Impure);
  }

  #[test]
  fn fn_call_uses_computed_purity() {
    let program = Program {
      functions: vec![empty_program_function()],
      top_level: empty_program_function(),
      top_level_mode: TopLevelMode::Module,
      symbols: None,
    };

    let effects = FnEffectMap {
      top_level: EffectSet::default(),
      functions: vec![EffectSet::default()],
    };

    let purities = compute_program_purity(&program, &effects);

    let mut cfg = cfg_with_single_call(Arg::Fn(0));
    annotate_cfg_purity(&mut cfg, &purities);
    assert_eq!(cfg.bblocks.get(0)[0].meta.callee_purity, Purity::Pure);
  }
}
