use crate::analysis::effect::FnEffectMap;
use crate::analysis::purity::FnPurityMap;
use crate::cfg::cfg::Cfg;
use crate::il::inst::{Arg, BinOp, EffectLocation, EffectSet, Inst, InstTyp, ParallelPlan, ParallelReason, Purity};
use crate::symbol::semantics::SymbolId;
use crate::{FnId, Program};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Default)]
pub struct CallbackInfo {
  /// Whether the callback uses its second declared parameter.
  ///
  /// For array iteration callbacks (`map`/`filter`), the second parameter is the element index.
  pub uses_second_param: bool,
  /// Whether this callback is a known associative+commutative reducer we can safely parallelize.
  pub associative_reduce: bool,
}

fn cfg_uses_var(cfg: &Cfg, var: u32) -> bool {
  for label in cfg.reverse_postorder() {
    let Some(bb) = cfg.bblocks.maybe_get(label) else {
      continue;
    };
    for inst in bb.iter() {
      if inst.args.iter().any(|arg| matches!(arg, Arg::Var(v) if *v == var)) {
        return true;
      }
    }
  }
  false
}

fn resolve_alias(mut var: u32, aliases: &BTreeMap<u32, u32>) -> u32 {
  let mut visiting = Vec::new();
  while let Some(&src) = aliases.get(&var) {
    if visiting.contains(&var) {
      break;
    }
    visiting.push(var);
    var = src;
  }
  var
}

fn detect_associative_reduce_callback(cfg: &Cfg, params: &[u32]) -> bool {
  if params.len() < 2 {
    return false;
  }

  let mut aliases = BTreeMap::<u32, u32>::new();
  let mut bin: Option<&Inst> = None;
  let mut ret: Option<u32> = None;

  for label in cfg.reverse_postorder() {
    let Some(bb) = cfg.bblocks.maybe_get(label) else {
      continue;
    };
    for inst in bb.iter() {
      match inst.t {
        InstTyp::VarAssign => {
          let Some(&tgt) = inst.tgts.get(0) else {
            continue;
          };
          if let Some(Arg::Var(src)) = inst.args.get(0) {
            aliases.insert(tgt, *src);
          }
        }
        InstTyp::Bin => {
          if bin.is_some() {
            return false;
          }
          if !matches!(inst.bin_op, BinOp::Add | BinOp::Mul) {
            return false;
          }
          // Only accept numeric-only contexts. This avoids string concatenation for `+` and other
          // dynamic coercions.
          if inst.value_type != crate::types::ValueTypeSummary::NUMBER {
            return false;
          }
          bin = Some(inst);
        }
        InstTyp::Return => {
          if ret.is_some() {
            return false;
          }
          let Some(Arg::Var(v)) = inst.args.get(0) else {
            return false;
          };
          ret = Some(*v);
        }
        // Any control-flow, calls, or other ops are treated as unknown/non-associative for now.
        _ => return false,
      }
    }
  }

  let Some(bin) = bin else {
    return false;
  };
  let Some(ret) = ret else {
    return false;
  };
  let Some(&tgt) = bin.tgts.get(0) else {
    return false;
  };

  let Some(Arg::Var(left)) = bin.args.get(0) else {
    return false;
  };
  let Some(Arg::Var(right)) = bin.args.get(1) else {
    return false;
  };

  let root = |v: u32| resolve_alias(v, &aliases);
  let (left, right) = (root(*left), root(*right));
  let (tgt, ret) = (root(tgt), root(ret));

  if tgt != ret {
    return false;
  }

  let p0 = params[0];
  let p1 = params[1];
  // `+` and `*` are commutative, so accept either order.
  (left == p0 && right == p1) || (left == p1 && right == p0)
}

/// Precompute per-function callback properties used by the parallelization inference.
///
/// This is separate from [`annotate_cfg_parallelize`] so `analysis::driver` can compute this once
/// (with an immutable borrow of the program) and then annotate CFGs mutably without borrow
/// conflicts.
pub fn compute_callback_infos(program: &Program) -> Vec<CallbackInfo> {
  let mut out = vec![CallbackInfo::default(); program.functions.len()];
  for (id, func) in program.functions.iter().enumerate() {
    let params = &func.params;
    let cfg = func.cfg_deconstructed();
    let uses_second_param = params
      .get(1)
      .copied()
      .is_some_and(|p1| cfg_uses_var(cfg, p1));
    let associative_reduce = detect_associative_reduce_callback(cfg, params);
    out[id] = CallbackInfo {
      uses_second_param,
      associative_reduce,
    };
  }
  out
}

#[derive(Clone, Debug)]
enum CalleeVarDef {
  Alias(u32),
  Fn(FnId),
  Phi(Vec<Arg>),
  Unknown,
}

fn build_callee_var_defs(cfg: &Cfg, foreign_fns: &BTreeMap<SymbolId, FnId>) -> BTreeMap<u32, CalleeVarDef> {
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
          // Non-SSA CFGs may assign the same temp multiple times. Only keep definitions when we can
          // prove the value is constant.
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

fn callback_effects(fn_effects: &FnEffectMap, id: FnId) -> Option<&EffectSet> {
  fn_effects.get(id)
}

fn map_filter_plan(
  callback: FnId,
  fn_effects: &FnEffectMap,
  fn_purities: &FnPurityMap,
  callback_infos: &[CallbackInfo],
) -> ParallelPlan {
  let purity = fn_purities.for_fn(callback);
  if !matches!(purity, Purity::Pure | Purity::Allocating) {
    return ParallelPlan::NotParallelizable(ParallelReason::ImpureCallback);
  }

  let Some(effects) = callback_effects(fn_effects, callback) else {
    return ParallelPlan::NotParallelizable(ParallelReason::CallbackUnknownEffects);
  };

  if effects.unknown {
    return ParallelPlan::NotParallelizable(ParallelReason::CallbackUnknownEffects);
  }
  if !effects.writes.is_empty() {
    return ParallelPlan::NotParallelizable(ParallelReason::CallbackWrites);
  }
  if effects.reads.contains(&EffectLocation::Heap) || effects.writes.contains(&EffectLocation::Heap) {
    return ParallelPlan::NotParallelizable(ParallelReason::CallbackReadsHeap);
  }

  if callback_infos
    .get(callback)
    .is_some_and(|info| info.uses_second_param)
  {
    return ParallelPlan::NotParallelizable(ParallelReason::CallbackUsesIndex);
  }

  ParallelPlan::Parallelizable
}

fn reduce_plan(
  callback: FnId,
  fn_effects: &FnEffectMap,
  fn_purities: &FnPurityMap,
  callback_infos: &[CallbackInfo],
) -> ParallelPlan {
  let purity = fn_purities.for_fn(callback);
  if !matches!(purity, Purity::Pure) {
    return ParallelPlan::NotParallelizable(ParallelReason::ImpureCallback);
  }

  let Some(effects) = callback_effects(fn_effects, callback) else {
    return ParallelPlan::NotParallelizable(ParallelReason::CallbackUnknownEffects);
  };
  if effects.unknown {
    return ParallelPlan::NotParallelizable(ParallelReason::CallbackUnknownEffects);
  }
  if !effects.writes.is_empty() {
    return ParallelPlan::NotParallelizable(ParallelReason::CallbackWrites);
  }
  if effects.reads.contains(&EffectLocation::Heap) || effects.writes.contains(&EffectLocation::Heap) {
    return ParallelPlan::NotParallelizable(ParallelReason::CallbackReadsHeap);
  }

  if !callback_infos
    .get(callback)
    .is_some_and(|info| info.associative_reduce)
  {
    return ParallelPlan::NotParallelizable(ParallelReason::ReduceNotAssociative);
  }

  ParallelPlan::Parallelizable
}

pub fn annotate_cfg_parallelize(
  cfg: &mut Cfg,
  fn_effects: &FnEffectMap,
  fn_purities: &FnPurityMap,
  callback_infos: &[CallbackInfo],
  foreign_fns: &BTreeMap<SymbolId, FnId>,
) {
  let defs = build_callee_var_defs(cfg, foreign_fns);

  for label in cfg.graph.labels_sorted() {
    for inst in cfg.bblocks.get_mut(label).iter_mut() {
      if inst.t != InstTyp::Call {
        continue;
      }

      let (_, callee, _, args, _) = inst.as_call();
      let Arg::Builtin(path) = callee else {
        continue;
      };

      let plan = match path.as_str() {
        "Array.prototype.map" | "Array.prototype.filter" => match args.get(0) {
          None => ParallelPlan::NotParallelizable(ParallelReason::UnknownCallback),
          Some(callback_arg) => match resolve_fn_id(callback_arg, &defs, &mut Vec::new()) {
            None => ParallelPlan::NotParallelizable(ParallelReason::UnknownCallback),
            Some(id) => map_filter_plan(id, fn_effects, fn_purities, callback_infos),
          },
        },
        "Array.prototype.reduce" => match args.get(0) {
          None => ParallelPlan::NotParallelizable(ParallelReason::UnknownCallback),
          Some(callback_arg) => match resolve_fn_id(callback_arg, &defs, &mut Vec::new()) {
            None => ParallelPlan::NotParallelizable(ParallelReason::UnknownCallback),
            Some(id) => reduce_plan(id, fn_effects, fn_purities, callback_infos),
          },
        },
        "Promise.all" => ParallelPlan::SpawnAll,
        "Promise.race" => ParallelPlan::SpawnAllButRaceResult,
        "__optimize_js_await" => ParallelPlan::NotParallelizable(ParallelReason::Await),
        _ => continue,
      };

      inst.meta.parallel = Some(plan);
    }
  }
}
