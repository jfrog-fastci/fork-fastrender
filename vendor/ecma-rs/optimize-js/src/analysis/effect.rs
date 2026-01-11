use crate::cfg::cfg::Cfg;
use crate::il::inst::{Arg, BinOp, EffectLocation, EffectSet, Inst, InstTyp};
use crate::{FnId, Program};
use effect_model::{EffectFlags, ThrowBehavior};
use std::collections::BTreeSet;

/// Function-level effect summaries for every function in a [`crate::Program`].
///
/// `functions` is index-aligned with `Program::functions` and `FnId`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FnEffectMap {
  pub top_level: EffectSet,
  pub functions: Vec<EffectSet>,
}

impl FnEffectMap {
  pub fn get(&self, id: FnId) -> Option<&EffectSet> {
    self.functions.get(id)
  }
}

fn is_pure_consteval_builtin_call(path: &str) -> bool {
  matches!(
    path,
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
      | "Number"
  )
}

/// Classify the local effects of a single IL instruction.
///
/// This excludes interprocedural callee summaries for direct `Arg::Fn` calls;
/// those are incorporated by [`compute_program_effects`] (function summaries)
/// and [`annotate_cfg_effects`] (per-instruction metadata).
pub fn inst_local_effect(inst: &Inst) -> EffectSet {
  let mut effects = EffectSet::default();

  match inst.t {
    InstTyp::Bin => {
      if inst.bin_op == BinOp::GetProp {
        effects.reads.insert(EffectLocation::Heap);
        effects.summary.throws = ThrowBehavior::Maybe;
      }
    }
    InstTyp::Throw => {
      effects.summary.throws = ThrowBehavior::Always;
    }
    InstTyp::PropAssign => {
      effects.writes.insert(EffectLocation::Heap);
      effects.summary.throws = ThrowBehavior::Maybe;
    }
    InstTyp::ForeignLoad => {
      effects
        .reads
        .insert(EffectLocation::Foreign(inst.foreign));
    }
    InstTyp::ForeignStore => {
      effects
        .writes
        .insert(EffectLocation::Foreign(inst.foreign));
    }
    InstTyp::UnknownLoad => {
      effects
        .reads
        .insert(EffectLocation::Unknown(inst.unknown.clone()));
      effects.summary.throws = ThrowBehavior::Maybe;
    }
    InstTyp::UnknownStore => {
      effects
        .writes
        .insert(EffectLocation::Unknown(inst.unknown.clone()));
      effects.summary.throws = ThrowBehavior::Maybe;
    }
    InstTyp::Call => {
      let (_, callee, _, _, _) = inst.as_call();
      match callee {
        Arg::Fn(_) => {
          // The callee effects are accounted for interprocedurally.
        }
        Arg::Builtin(path) => match path.as_str() {
          // Internal lowering helpers that construct literals / perform pure allocations.
          "__optimize_js_array" | "__optimize_js_object" | "__optimize_js_regex" | "__optimize_js_template" => {
            effects.summary.flags |= EffectFlags::ALLOCATES;
          }
          // Tagged templates call the tag function; we conservatively treat them as unknown.
          "__optimize_js_tagged_template" => {
            effects.summary.flags |= EffectFlags::ALLOCATES;
            effects.mark_unknown();
          }
          "__optimize_js_in" => {
            // Property existence checks read heap state and can throw on nullish RHS.
            effects.reads.insert(EffectLocation::Heap);
            effects.summary.throws = ThrowBehavior::Maybe;
          }
          "__optimize_js_instanceof" => {
            // `instanceof` can consult `Symbol.hasInstance` and invoke user code.
            effects.reads.insert(EffectLocation::Heap);
            effects.mark_unknown();
          }
          "__optimize_js_delete" => {
            effects.writes.insert(EffectLocation::Heap);
            effects.mark_unknown();
          }
          "__optimize_js_new" | "__optimize_js_await" | "import" => {
            effects.mark_unknown();
          }
          _ if is_pure_consteval_builtin_call(path) => {}
          _ => {
            effects.mark_unknown();
          }
        },
        _ => {
          effects.mark_unknown();
        }
      }
    }
    InstTyp::CondGoto
    | InstTyp::Return
    | InstTyp::Un
    | InstTyp::VarAssign
    | InstTyp::Phi
    | InstTyp::_Label => {}
    // These should not exist after CFG construction but are treated as no-ops for analysis.
    InstTyp::_Goto | InstTyp::_Dummy => {}
  }

  effects
}

fn inst_total_effect(inst: &Inst, fn_summaries: &FnEffectMap) -> EffectSet {
  let mut effects = inst_local_effect(inst);
  if inst.t == InstTyp::Call {
    let (_, callee, _, _, _) = inst.as_call();
    if let Arg::Fn(id) = callee {
      if let Some(summary) = fn_summaries.get(*id) {
        effects.merge(summary);
      } else {
        // An `Arg::Fn` with no corresponding summary should be impossible, but if it happens we
        // must stay conservative.
        effects.mark_unknown();
      }
    }
  }
  effects
}

fn cfg_labels_sorted(cfg: &Cfg) -> Vec<u32> {
  let mut labels = cfg.bblocks.all().map(|(label, _)| label).collect::<Vec<_>>();
  labels.sort_unstable();
  labels
}

fn cfg_local_effects(cfg: &Cfg) -> EffectSet {
  let mut effects = EffectSet::default();
  for label in cfg_labels_sorted(cfg) {
    for inst in cfg.bblocks.get(label) {
      effects.merge(&inst_local_effect(inst));
    }
  }
  effects
}

fn cfg_direct_calls(cfg: &Cfg) -> BTreeSet<FnId> {
  let mut callees = BTreeSet::new();
  for label in cfg_labels_sorted(cfg) {
    for inst in cfg.bblocks.get(label) {
      if inst.t == InstTyp::Call {
        let (_, callee, _, _, _) = inst.as_call();
        if let Arg::Fn(id) = callee {
          callees.insert(*id);
        }
      }
    }
  }
  callees
}

/// Whole-program effect analysis over the current IL.
///
/// This computes a fixpoint of function summaries so that direct `Arg::Fn`
/// calls incorporate callee summaries (including recursion/cycles).
pub fn compute_program_effects(program: &Program) -> FnEffectMap {
  let locals = FnEffectMap {
    top_level: cfg_local_effects(&program.top_level.body),
    functions: program
      .functions
      .iter()
      .map(|f| cfg_local_effects(&f.body))
      .collect(),
  };

  let top_level_calls = cfg_direct_calls(&program.top_level.body);
  let function_calls: Vec<_> = program
    .functions
    .iter()
    .map(|f| cfg_direct_calls(&f.body))
    .collect();

  // Start with purely local effects; iteratively fold in callee summaries until a fixpoint.
  let mut summaries = locals.clone();

  loop {
    let mut changed = false;

    let mut new_top = locals.top_level.clone();
    for callee in top_level_calls.iter().copied() {
      if let Some(summary) = summaries.get(callee) {
        new_top.merge(summary);
      } else {
        new_top.mark_unknown();
      }
    }
    if new_top != summaries.top_level {
      summaries.top_level = new_top;
      changed = true;
    }

    for fn_id in 0..program.functions.len() {
      let mut new_summary = locals.functions[fn_id].clone();
      for callee in function_calls[fn_id].iter().copied() {
        if let Some(summary) = summaries.get(callee) {
          new_summary.merge(summary);
        } else {
          new_summary.mark_unknown();
        }
      }
      if new_summary != summaries.functions[fn_id] {
        summaries.functions[fn_id] = new_summary;
        changed = true;
      }
    }

    if !changed {
      break;
    }
  }

  summaries
}

/// Write per-instruction effects into [`crate::il::inst::InstMeta`].
///
/// This is intended to run on the finalized CFG (after `build_program_function`)
/// and does not attempt to preserve metadata through subsequent opt passes.
pub fn annotate_cfg_effects(cfg: &mut Cfg, fn_summaries: &FnEffectMap) {
  for label in cfg_labels_sorted(cfg) {
    for inst in cfg.bblocks.get_mut(label) {
      inst.meta.effects = inst_total_effect(inst, fn_summaries);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::cfg::cfg::{Cfg, CfgBBlocks, CfgGraph};
  use crate::il::inst::Const;
  use crate::symbol::semantics::SymbolId;
  use crate::{OptimizationStats, ProgramFunction, TopLevelMode};

  const EXIT: u32 = u32::MAX;

  fn cfg_single_block(insts: Vec<Inst>) -> Cfg {
    let mut graph = CfgGraph::default();
    graph.connect(0, EXIT);
    let mut bblocks = CfgBBlocks::default();
    bblocks.add(0, insts);
    bblocks.add(EXIT, Vec::new());
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
  fn var_assign_is_pure() {
    let inst = Inst::var_assign(0, Arg::Var(1));
    let eff = inst_local_effect(&inst);
    assert!(eff.is_pure());
  }

  #[test]
  fn return_is_pure_and_never_throws() {
    let inst = Inst::ret(None);
    let eff = inst_local_effect(&inst);
    assert!(eff.is_pure());
    assert_eq!(eff.summary.throws, ThrowBehavior::Never);
    assert!(eff.reads.is_empty());
    assert!(eff.writes.is_empty());
    assert!(!eff.summary.flags.contains(EffectFlags::ALLOCATES));
    assert!(!eff.unknown);
  }

  #[test]
  fn prop_assign_writes_heap_and_may_throw() {
    let inst = Inst::prop_assign(
      Arg::Var(0),
      Arg::Const(Const::Str("k".to_string())),
      Arg::Var(1),
    );
    let eff = inst_local_effect(&inst);
    assert!(eff.writes.contains(&EffectLocation::Heap));
    assert_eq!(eff.summary.throws, ThrowBehavior::Maybe);
  }

  #[test]
  fn unknown_load_reads_global_and_may_throw() {
    let inst = Inst::unknown_load(0, "mystery".to_string());
    let eff = inst_local_effect(&inst);
    assert!(eff
      .reads
      .contains(&EffectLocation::Unknown("mystery".to_string())));
    assert_eq!(eff.summary.throws, ThrowBehavior::Maybe);
  }

  #[test]
  fn foreign_load_store_are_classified() {
    let sym = SymbolId(1);

    let load = Inst::foreign_load(0, sym);
    let load_eff = inst_local_effect(&load);
    assert!(load_eff.reads.contains(&EffectLocation::Foreign(sym)));
    assert!(load_eff.writes.is_empty());
    assert!(!load_eff.summary.flags.contains(EffectFlags::ALLOCATES));
    assert!(!load_eff.unknown);

    let store = Inst::foreign_store(sym, Arg::Const(Const::Undefined));
    let store_eff = inst_local_effect(&store);
    assert!(store_eff.writes.contains(&EffectLocation::Foreign(sym)));
    assert!(store_eff.reads.is_empty());
    assert!(!store_eff.summary.flags.contains(EffectFlags::ALLOCATES));
    assert!(!store_eff.unknown);
  }

  #[test]
  fn internal_literal_builtins_allocate_without_unknown() {
    for builtin in [
      "__optimize_js_array",
      "__optimize_js_object",
      "__optimize_js_regex",
      "__optimize_js_template",
    ] {
      let call = Inst::call(
        0,
        Arg::Builtin(builtin.to_string()),
        Arg::Const(Const::Undefined),
        Vec::new(),
        Vec::new(),
      );
      let eff = inst_local_effect(&call);
      assert!(
        eff.summary.flags.contains(EffectFlags::ALLOCATES),
        "{builtin} should allocate but got {eff:?}"
      );
      assert!(
        !eff.unknown,
        "{builtin} should not be marked unknown but got {eff:?}"
      );
      assert!(
        eff.summary.throws == ThrowBehavior::Never,
        "{builtin} should not be marked as throwing but got {eff:?}"
      );
      assert!(eff.reads.is_empty());
      assert!(eff.writes.is_empty());
    }
  }

  #[test]
  fn tagged_template_is_unknown() {
    let call = Inst::call(
      0,
      Arg::Builtin("__optimize_js_tagged_template".to_string()),
      Arg::Const(Const::Undefined),
      Vec::new(),
      Vec::new(),
    );
    let eff = inst_local_effect(&call);
    assert!(eff.summary.flags.contains(EffectFlags::ALLOCATES));
    assert!(eff.unknown);
    assert_eq!(eff.summary.throws, ThrowBehavior::Maybe);
  }

  #[test]
  fn unknown_call_is_unknown() {
    let call = Inst::call(
      None::<u32>,
      Arg::Var(0),
      Arg::Const(Const::Undefined),
      Vec::new(),
      Vec::new(),
    );
    let eff = inst_local_effect(&call);
    assert!(eff.unknown);
    assert_eq!(eff.summary.throws, ThrowBehavior::Maybe);
  }

  #[test]
  fn throw_is_always_throwing() {
    let inst = Inst::throw(Arg::Const(Const::Undefined));
    let eff = inst_local_effect(&inst);
    assert_eq!(eff.summary.throws, ThrowBehavior::Always);
    assert!(eff.reads.is_empty());
    assert!(eff.writes.is_empty());
    assert!(!eff.summary.flags.contains(EffectFlags::ALLOCATES));
    assert!(!eff.unknown);
  }

  #[test]
  fn getprop_reads_heap_and_may_throw() {
    let inst = Inst::bin(
      0,
      Arg::Var(0),
      BinOp::GetProp,
      Arg::Const(Const::Str("prop".to_string())),
    );
    let eff = inst_local_effect(&inst);
    assert!(eff.reads.contains(&EffectLocation::Heap));
    assert_eq!(eff.summary.throws, ThrowBehavior::Maybe);
  }

  #[test]
  fn interprocedural_propagation_includes_direct_callee_effects() {
    let sym = SymbolId(7);

    // Fn0 writes a foreign symbol.
    let callee = func(cfg_single_block(vec![Inst::foreign_store(
      sym,
      Arg::Const(Const::Undefined),
    )]));

    // Fn1 calls Fn0 directly.
    let call_inst = Inst::call(
      None::<u32>,
      Arg::Fn(0),
      Arg::Const(Const::Undefined),
      Vec::new(),
      Vec::new(),
    );
    let caller = func(cfg_single_block(vec![call_inst]));

    let mut program = Program {
      functions: vec![callee, caller],
      top_level: func(cfg_single_block(Vec::new())),
      top_level_mode: TopLevelMode::Module,
      symbols: None,
    };

    let summaries = compute_program_effects(&program);
    assert!(summaries.functions[0]
      .writes
      .contains(&EffectLocation::Foreign(sym)));
    assert!(summaries.functions[1]
      .writes
      .contains(&EffectLocation::Foreign(sym)));

    // Per-instruction annotation should reflect the callee's summary on the call instruction.
    annotate_cfg_effects(&mut program.functions[1].body, &summaries);
    let call_effects = &program.functions[1].body.bblocks.get(0)[0].meta.effects;
    assert!(call_effects
      .writes
      .contains(&EffectLocation::Foreign(sym)));
  }

  #[test]
  fn interprocedural_propagation_includes_may_throw() {
    // Fn0 reads an unknown global, which can throw (e.g. ReferenceError in global mode).
    let callee = func(cfg_single_block(vec![Inst::unknown_load(
      0,
      "missingGlobal".to_string(),
    )]));

    // Fn1 calls Fn0 directly.
    let call_inst = Inst::call(
      None::<u32>,
      Arg::Fn(0),
      Arg::Const(Const::Undefined),
      Vec::new(),
      Vec::new(),
    );
    let mut program = Program {
      functions: vec![callee, func(cfg_single_block(vec![call_inst]))],
      top_level: func(cfg_single_block(Vec::new())),
      top_level_mode: TopLevelMode::Module,
      symbols: None,
    };

    let summaries = compute_program_effects(&program);
    assert_eq!(summaries.functions[0].summary.throws, ThrowBehavior::Maybe);
    assert_eq!(summaries.functions[1].summary.throws, ThrowBehavior::Maybe);

    annotate_cfg_effects(&mut program.functions[1].body, &summaries);
    let call_effects = &program.functions[1].body.bblocks.get(0)[0].meta.effects;
    assert_eq!(call_effects.summary.throws, ThrowBehavior::Maybe);
  }
}
