use crate::analysis::effect::FnEffectMap;
use crate::cfg::cfg::Cfg;
use crate::il::inst::EffectSet;
use crate::il::inst::{Arg, InstTyp, ValueTypeSummary};
use crate::symbol::semantics::SymbolId;
use crate::{FnId, Program};
pub use crate::il::meta::Purity;
use std::collections::BTreeMap;

use super::value_types::ValueTypeSummaries;

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

fn builtin_call_purity(path: &str, args: &[Arg], value_types: &ValueTypeSummaries) -> Purity {
  // Keep in sync with `eval/consteval.rs:maybe_eval_const_builtin_call` for calls we can safely
  // treat as pure in our current model.
  //
  // NOTE: Many JS builtins coerce their arguments with `ToNumber`, which can either:
  // - throw (BigInt/Symbol), or
  // - invoke user code (`ToPrimitive` on objects/functions via `valueOf`/`toString`).
  //
  // We only mark these calls as pure when all *used* arguments are known to be safe for
  // `ToNumber` without invoking user code:
  // - no BigInt/Symbol (TypeError)
  // - no object/function (may run user-defined `valueOf`/`toString`)
  //
  // In untyped builds this generally means "literal constants"; typed builds can propagate more
  // precise summaries through `Inst.value_type`.
  let safe_to_number_arg = |ty: ValueTypeSummary| {
    !ty.is_unknown()
      && !ty.contains(ValueTypeSummary::BIGINT)
      && !ty.contains(ValueTypeSummary::SYMBOL)
      && !ty.contains(ValueTypeSummary::OBJECT)
      && !ty.contains(ValueTypeSummary::FUNCTION)
  };

  let arg_type = |idx: usize| match args.get(idx) {
    Some(arg) => value_types.arg(arg).unwrap_or(ValueTypeSummary::UNKNOWN),
    None => ValueTypeSummary::UNDEFINED,
  };
  let all_args_safe_to_number = || {
    args.iter().all(|arg| {
      let ty = value_types.arg(arg).unwrap_or(ValueTypeSummary::UNKNOWN);
      safe_to_number_arg(ty)
    })
  };

  match path {
    "Math.abs"
    | "Math.acos"
    | "Math.acosh"
    | "Math.asin"
    | "Math.asinh"
    | "Math.atan"
    | "Math.atanh"
    | "Math.cbrt"
    | "Math.ceil"
    | "Math.clz32"
    | "Math.cos"
    | "Math.cosh"
    | "Math.exp"
    | "Math.expm1"
    | "Math.floor"
    | "Math.fround"
    | "Math.log"
    | "Math.log10"
    | "Math.log1p"
    | "Math.log2"
    | "Math.round"
    | "Math.sign"
    | "Math.sin"
    | "Math.sinh"
    | "Math.sqrt"
    | "Math.tan"
    | "Math.tanh"
    | "Math.trunc"
    | "Number" => {
      if safe_to_number_arg(arg_type(0)) {
        Purity::Pure
      } else {
        Purity::Impure
      }
    }
    "Math.atan2" | "Math.imul" | "Math.pow" => {
      if safe_to_number_arg(arg_type(0)) && safe_to_number_arg(arg_type(1)) {
        Purity::Pure
      } else {
        Purity::Impure
      }
    }
    "Math.hypot" | "Math.max" | "Math.min" => {
      if all_args_safe_to_number() {
        Purity::Pure
      } else {
        Purity::Impure
      }
    }

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
    Arg::Builtin(_) => Purity::Impure,
    _ => Purity::Impure,
  }
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
  for label in cfg.reverse_postorder() {
    let Some(bb) = cfg.bblocks.maybe_get(label) else {
      continue;
    };
    for inst in bb.iter() {
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
          if !matches!(existing, CalleeVarDef::Unknown) {
            *existing = CalleeVarDef::Unknown;
          }
        })
        .or_insert(def);
    }
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

fn callee_purity_resolved(
  callee: &Arg,
  purities: &FnPurityMap,
  defs: &BTreeMap<u32, CalleeVarDef>,
) -> Purity {
  match callee {
    Arg::Var(_) => resolve_fn_id(callee, defs, &mut Vec::new())
      .map(|id| purities.for_fn(id))
      .unwrap_or(Purity::Impure),
    _ => callee_purity(callee, purities),
  }
}

pub fn annotate_cfg_purity(
  cfg: &mut Cfg,
  purities: &FnPurityMap,
  foreign_fns: &BTreeMap<SymbolId, FnId>,
) {
  let defs = build_callee_var_defs(cfg, foreign_fns);
  let value_types = ValueTypeSummaries::new(cfg);
  for label in cfg.graph.labels_sorted() {
    for inst in cfg.bblocks.get_mut(label).iter_mut() {
      if inst.t != InstTyp::Call {
        continue;
      }

      let (_, callee, _, args, _) = inst.as_call();
      inst.meta.callee_purity = match callee {
        Arg::Builtin(path) => builtin_call_purity(path, args, &value_types),
        _ => callee_purity_resolved(callee, purities, &defs),
      };
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::cfg::cfg::{CfgBBlocks, CfgGraph};
  use crate::il::inst::{Const, EffectLocation, EffectSet, Inst};
  use crate::symbol::semantics::SymbolId;
  use crate::OptimizationStats;
  use crate::ProgramFunction;
  use crate::TopLevelMode;
  use effect_model::{EffectFlags, ThrowBehavior};
  use parse_js::num::JsNumber as JN;

  fn cfg_with_single_call(callee: Arg) -> Cfg {
    let mut graph = CfgGraph::default();
    // Ensure label 0 exists in the CFG graph.
    graph.ensure_label(0);
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
    graph.ensure_label(0);
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

  fn func(cfg: Cfg) -> ProgramFunction {
    ProgramFunction {
      debug: None,
      body: cfg,
      params: Vec::new(),
      ssa_body: None,
      stats: OptimizationStats::default(),
    }
  }

  #[test]
  fn pure_builtin_call_is_pure() {
    let mut cfg = cfg_with_single_call(Arg::Builtin("Math.abs".to_string()));
    annotate_cfg_purity(&mut cfg, &FnPurityMap::default(), &std::collections::BTreeMap::new());
    assert_eq!(cfg.bblocks.get(0)[0].meta.callee_purity, Purity::Pure);
  }

  #[test]
  fn builtin_call_is_not_pure_when_first_arg_is_bigint() {
    let mut cfg = cfg_single_block(vec![Inst::call(
      None::<u32>,
      Arg::Builtin("Math.abs".to_string()),
      Arg::Const(Const::Undefined),
      vec![Arg::Const(Const::BigInt(num_bigint::BigInt::from(1)))],
      Vec::new(),
    )]);
    annotate_cfg_purity(&mut cfg, &FnPurityMap::default(), &std::collections::BTreeMap::new());
    assert_eq!(cfg.bblocks.get(0)[0].meta.callee_purity, Purity::Impure);
  }

  #[test]
  fn math_pow_builtin_call_is_pure() {
    let mut cfg = cfg_single_block(vec![Inst::call(
      None::<u32>,
      Arg::Builtin("Math.pow".to_string()),
      Arg::Const(Const::Undefined),
      vec![Arg::Const(Const::Num(JN(2.0))), Arg::Const(Const::Num(JN(3.0)))],
      Vec::new(),
    )]);
    annotate_cfg_purity(&mut cfg, &FnPurityMap::default(), &std::collections::BTreeMap::new());
    assert_eq!(cfg.bblocks.get(0)[0].meta.callee_purity, Purity::Pure);
  }

  #[test]
  fn math_pow_builtin_call_is_not_pure_when_any_arg_is_bigint() {
    let mut cfg = cfg_single_block(vec![Inst::call(
      None::<u32>,
      Arg::Builtin("Math.pow".to_string()),
      Arg::Const(Const::Undefined),
      vec![Arg::Const(Const::Num(JN(2.0))), Arg::Const(Const::BigInt(num_bigint::BigInt::from(3)))],
      Vec::new(),
    )]);
    annotate_cfg_purity(&mut cfg, &FnPurityMap::default(), &std::collections::BTreeMap::new());
    assert_eq!(cfg.bblocks.get(0)[0].meta.callee_purity, Purity::Impure);
  }

  #[test]
  fn internal_array_literal_call_is_allocating() {
    let mut cfg = cfg_with_single_call(Arg::Builtin("__optimize_js_array".to_string()));
    annotate_cfg_purity(&mut cfg, &FnPurityMap::default(), &std::collections::BTreeMap::new());
    assert_eq!(
      cfg.bblocks.get(0)[0].meta.callee_purity,
      Purity::Allocating
    );
  }

  #[test]
  fn tagged_template_call_is_impure() {
    let mut cfg = cfg_with_single_call(Arg::Builtin("__optimize_js_tagged_template".to_string()));
    annotate_cfg_purity(&mut cfg, &FnPurityMap::default(), &std::collections::BTreeMap::new());
    assert_eq!(cfg.bblocks.get(0)[0].meta.callee_purity, Purity::Impure);
  }

  #[test]
  fn unknown_call_is_impure() {
    let mut cfg = cfg_with_single_call(Arg::Var(0));
    annotate_cfg_purity(&mut cfg, &FnPurityMap::default(), &std::collections::BTreeMap::new());
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
      source_file: crate::FileId(0),
      source_len: 0,
      functions: vec![empty_program_function()],
      top_level: empty_program_function(),
      top_level_mode: TopLevelMode::Module,
      symbols: None,
    };

    let effects = FnEffectMap {
      top_level: EffectSet::default(),
      functions: vec![EffectSet::default()],
      constant_foreign_fns: Default::default(),
    };

    let purities = compute_program_purity(&program, &effects);

    let mut cfg = cfg_with_single_call(Arg::Fn(0));
    annotate_cfg_purity(&mut cfg, &purities, &std::collections::BTreeMap::new());
    assert_eq!(cfg.bblocks.get(0)[0].meta.callee_purity, Purity::Pure);
  }

  #[test]
  fn captured_constant_callee_call_uses_computed_purity() {
    let callee_sym = SymbolId(123);

    let callee = empty_program_function();
    let caller = func(cfg_single_block(vec![
      Inst::foreign_load(0, callee_sym),
      Inst::call(
        None::<u32>,
        Arg::Var(0),
        Arg::Const(Const::Undefined),
        Vec::new(),
        Vec::new(),
      ),
    ]));

    let mut program = Program {
      source_file: crate::FileId(0),
      source_len: 0,
      functions: vec![callee, caller],
      top_level: func(cfg_single_block(vec![Inst::foreign_store(callee_sym, Arg::Fn(0))])),
      top_level_mode: TopLevelMode::Module,
      symbols: None,
    };

    let effects = crate::analysis::effect::compute_program_effects(&program);
    let purities = compute_program_purity(&program, &effects);
    annotate_cfg_purity(
      &mut program.functions[1].body,
      &purities,
      effects.constant_foreign_fns(),
    );
    assert_eq!(
      program.functions[1].body.bblocks.get(0)[1].meta.callee_purity,
      Purity::Pure
    );
  }
}
