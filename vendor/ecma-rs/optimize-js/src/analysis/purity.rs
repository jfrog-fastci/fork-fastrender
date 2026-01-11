use crate::analysis::effect::FnEffectMap;
use crate::cfg::cfg::Cfg;
use crate::il::inst::EffectSet;
use crate::il::inst::{Arg, InstTyp};
use crate::{FnId, Program};
pub use crate::il::meta::Purity;

pub fn purity_of_effects(effects: &EffectSet) -> Purity {
  purity_from_effects(effects)
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
  Purity::from_effects(effects)
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
    | "__optimize_js_template" => Purity::Allocating,

    // Helpers that can invoke user code or consult observable state.
    "__optimize_js_tagged_template" | "__optimize_js_in" | "__optimize_js_instanceof" => Purity::Impure,

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
  use crate::il::inst::{Const, EffectLocation, EffectSet, Inst};
  use crate::OptimizationStats;
  use crate::ProgramFunction;
  use crate::TopLevelMode;
  use effect_model::{EffectFlags, ThrowBehavior};

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
      ssa_body: None,
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
  fn tagged_template_call_is_impure() {
    let mut cfg = cfg_with_single_call(Arg::Builtin("__optimize_js_tagged_template".to_string()));
    annotate_cfg_purity(&mut cfg, &FnPurityMap::default());
    assert_eq!(cfg.bblocks.get(0)[0].meta.callee_purity, Purity::Impure);
  }

  #[test]
  fn unknown_call_is_impure() {
    let mut cfg = cfg_with_single_call(Arg::Var(0));
    annotate_cfg_purity(&mut cfg, &FnPurityMap::default());
    assert_eq!(cfg.bblocks.get(0)[0].meta.callee_purity, Purity::Impure);
  }

  #[test]
  fn purity_of_effects_matches_expected_categories() {
    let pure = EffectSet::default();
    assert_eq!(purity_of_effects(&pure), Purity::Pure);
    assert!(pure.is_pure());

    let mut read_only = EffectSet::default();
    read_only.reads.insert(EffectLocation::Heap);
    assert_eq!(purity_of_effects(&read_only), Purity::ReadOnly);

    let mut alloc_only = EffectSet::default();
    alloc_only.summary.flags = EffectFlags::ALLOCATES;
    assert_eq!(purity_of_effects(&alloc_only), Purity::Allocating);

    let mut io = EffectSet::default();
    io.summary.flags = EffectFlags::IO;
    assert_eq!(purity_of_effects(&io), Purity::Impure);

    let mut throws = EffectSet::default();
    throws.summary.throws = ThrowBehavior::Maybe;
    assert_eq!(purity_of_effects(&throws), Purity::Impure);

    let mut alloc_and_read = EffectSet::default();
    alloc_and_read.summary.flags = EffectFlags::ALLOCATES;
    alloc_and_read.reads.insert(EffectLocation::Heap);
    assert_eq!(purity_of_effects(&alloc_and_read), Purity::Impure);
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
