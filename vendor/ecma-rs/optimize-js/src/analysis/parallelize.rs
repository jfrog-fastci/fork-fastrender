use crate::analysis::effect::FnEffectMap;
use crate::analysis::purity::FnPurityMap;
use crate::cfg::cfg::Cfg;
use crate::il::inst::{Arg, BinOp, EffectLocation, EffectSet, Inst, InstTyp, ParallelPlan, ParallelReason, Purity};
#[cfg(any(feature = "native-fusion", feature = "native-array-ops"))]
use crate::il::inst::ArrayChainOp;
use crate::symbol::semantics::SymbolId;
use crate::{FnId, Program};
#[cfg(feature = "native-async-ops")]
use effect_model::ThrowBehavior;
use std::collections::BTreeMap;
#[cfg(feature = "native-async-ops")]
use std::collections::BTreeSet;

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

#[cfg(feature = "native-async-ops")]
#[derive(Clone, Debug)]
struct ValueDefInfo {
  uses: Vec<u32>,
  effects: EffectSet,
}

#[cfg(feature = "native-async-ops")]
#[derive(Clone, Debug)]
enum ValueDef {
  Known(ValueDefInfo),
  Unknown,
}

#[cfg(feature = "native-async-ops")]
fn inst_used_vars(inst: &Inst) -> Vec<u32> {
  let mut out = Vec::new();
  for arg in &inst.args {
    if let Arg::Var(v) = arg {
      out.push(*v);
    }
  }
  out
}

#[cfg(feature = "native-async-ops")]
fn build_value_defs(cfg: &Cfg) -> BTreeMap<u32, ValueDef> {
  let mut defs = BTreeMap::<u32, ValueDef>::new();
  for label in cfg.graph.labels_sorted() {
    let bb = cfg.bblocks.maybe_get(label).into_iter().flatten();
    for inst in bb {
      for &tgt in &inst.tgts {
        let def = ValueDef::Known(ValueDefInfo {
          uses: inst_used_vars(inst),
          effects: inst.meta.effects.clone(),
        });
        defs
          .entry(tgt)
          .and_modify(|existing| {
            if !matches!(existing, ValueDef::Unknown) {
              *existing = ValueDef::Unknown;
            }
          })
          .or_insert(def);
      }
    }
  }
  defs
}

#[cfg(feature = "native-async-ops")]
fn unknown_effects() -> EffectSet {
  let mut e = EffectSet::default();
  e.mark_unknown();
  e
}

#[cfg(feature = "native-async-ops")]
fn var_total_effect(
  var: u32,
  defs: &BTreeMap<u32, ValueDef>,
  memo: &mut BTreeMap<u32, EffectSet>,
  visiting: &mut Vec<u32>,
) -> EffectSet {
  if let Some(e) = memo.get(&var) {
    return e.clone();
  }
  if visiting.contains(&var) {
    return unknown_effects();
  }
  visiting.push(var);

  let mut out = EffectSet::default();
  match defs.get(&var) {
    None => {
      // Variables without a defining instruction are treated as effect-free when used as an input
      // value (e.g. parameters).
    }
    Some(ValueDef::Unknown) => {
      out.mark_unknown();
    }
    Some(ValueDef::Known(info)) => {
      out.merge(&info.effects);
      for &u in &info.uses {
        let e = var_total_effect(u, defs, memo, visiting);
        out.merge(&e);
      }
    }
  }

  visiting.pop();
  memo.insert(var, out.clone());
  out
}

#[cfg(feature = "native-async-ops")]
fn arg_total_effect(arg: &Arg, defs: &BTreeMap<u32, ValueDef>, memo: &mut BTreeMap<u32, EffectSet>) -> EffectSet {
  match arg {
    Arg::Var(v) => var_total_effect(*v, defs, memo, &mut Vec::new()),
    _ => EffectSet::default(),
  }
}

#[cfg(feature = "native-async-ops")]
fn var_depends_on(var: u32, target: u32, defs: &BTreeMap<u32, ValueDef>, visiting: &mut Vec<u32>) -> bool {
  if visiting.contains(&var) {
    return true;
  }
  visiting.push(var);

  let mut out = false;
  match defs.get(&var) {
    None => {}
    Some(ValueDef::Unknown) => {
      out = true;
    }
    Some(ValueDef::Known(info)) => {
      for &u in &info.uses {
        if u == target || var_depends_on(u, target, defs, visiting) {
          out = true;
          break;
        }
      }
    }
  }

  visiting.pop();
  out
}

#[cfg(feature = "native-async-ops")]
fn promise_components_plan(
  args: &[Arg],
  defs: &BTreeMap<u32, ValueDef>,
  memo: &mut BTreeMap<u32, EffectSet>,
) -> Result<(), ParallelReason> {
  let arg_vars: Vec<u32> = args
    .iter()
    .filter_map(|a| match a {
      Arg::Var(v) => Some(*v),
      _ => None,
    })
    .collect();

  let mut effects_per_arg = Vec::<EffectSet>::with_capacity(args.len());
  for arg in args {
    match arg {
      Arg::Var(v) if !defs.contains_key(v) => {
        return Err(ParallelReason::PromiseUnknownEffects);
      }
      _ => {}
    }
    effects_per_arg.push(arg_total_effect(arg, defs, memo));
  }

  for eff in &effects_per_arg {
    if eff.unknown {
      return Err(ParallelReason::PromiseUnknownEffects);
    }
    if !matches!(eff.summary.throws, ThrowBehavior::Never) {
      return Err(ParallelReason::PromiseMayThrow);
    }
    if eff.writes.contains(&EffectLocation::Heap) {
      return Err(ParallelReason::PromiseWritesHeap);
    }
    if eff.writes.iter().any(|loc| matches!(loc, EffectLocation::Unknown(_))) {
      return Err(ParallelReason::PromiseWritesUnknown);
    }
  }

  // Conflicts: any read after another write, or any write after another read/write.
  let mut seen_reads = BTreeSet::<EffectLocation>::new();
  let mut seen_writes = BTreeSet::<EffectLocation>::new();
  for eff in &effects_per_arg {
    if eff
      .reads
      .iter()
      .any(|loc| seen_writes.iter().any(|w| loc.may_alias(w)))
    {
      return Err(ParallelReason::PromiseConflictingAccess);
    }
    if eff
      .writes
      .iter()
      .any(|loc| {
        seen_reads.iter().any(|r| loc.may_alias(r)) || seen_writes.iter().any(|w| loc.may_alias(w))
      })
    {
      return Err(ParallelReason::PromiseConflictingAccess);
    }
    seen_reads.extend(eff.reads.iter().cloned());
    seen_writes.extend(eff.writes.iter().cloned());
  }

  if arg_vars.len() >= 2 {
    for &v in &arg_vars {
      for &other in &arg_vars {
        if v == other {
          continue;
        }
        if var_depends_on(v, other, defs, &mut Vec::new()) {
          return Err(ParallelReason::PromiseDependsOnOther);
        }
      }
    }
  }

  Ok(())
}

#[cfg(any(feature = "native-fusion", feature = "native-array-ops"))]
fn array_chain_plan(
  args: &[Arg],
  ops: &[ArrayChainOp],
  fn_effects: &FnEffectMap,
  fn_purities: &FnPurityMap,
  callback_infos: &[CallbackInfo],
  defs: &BTreeMap<u32, CalleeVarDef>,
) -> ParallelPlan {
  for op in ops {
    match *op {
      ArrayChainOp::Map { callback } | ArrayChainOp::Filter { callback } => {
        let Some(callback_arg) = args.get(callback) else {
          return ParallelPlan::NotParallelizable(ParallelReason::UnknownCallback);
        };
        let Some(id) = resolve_fn_id(callback_arg, defs, &mut Vec::new()) else {
          return ParallelPlan::NotParallelizable(ParallelReason::UnknownCallback);
        };
        let plan = map_filter_plan(id, fn_effects, fn_purities, callback_infos);
        if !matches!(plan, ParallelPlan::Parallelizable) {
          return plan;
        }
      }
      ArrayChainOp::Reduce { callback, .. } => {
        let Some(callback_arg) = args.get(callback) else {
          return ParallelPlan::NotParallelizable(ParallelReason::UnknownCallback);
        };
        let Some(id) = resolve_fn_id(callback_arg, defs, &mut Vec::new()) else {
          return ParallelPlan::NotParallelizable(ParallelReason::UnknownCallback);
        };
        let plan = reduce_plan(id, fn_effects, fn_purities, callback_infos);
        if !matches!(plan, ParallelPlan::Parallelizable) {
          return plan;
        }
      }
      // Short-circuiting ops are not modeled yet.
      ArrayChainOp::Find { .. } | ArrayChainOp::Every { .. } | ArrayChainOp::Some { .. } => {
        return ParallelPlan::NotParallelizable(ParallelReason::ArrayChainUnsupportedOp);
      }
    }
  }
  ParallelPlan::Parallelizable
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
  #[cfg(feature = "native-async-ops")]
  let value_defs = build_value_defs(cfg);
  #[cfg(feature = "native-async-ops")]
  let mut value_memo = BTreeMap::<u32, EffectSet>::new();

  for label in cfg.graph.labels_sorted() {
    for inst in cfg.bblocks.get_mut(label).iter_mut() {
      #[cfg(any(feature = "native-fusion", feature = "native-array-ops"))]
      if inst.t == InstTyp::ArrayChain {
        let plan = {
          let (_tgt, _base, ops) = inst.as_array_chain();
          array_chain_plan(
            &inst.args,
            ops,
            fn_effects,
            fn_purities,
            callback_infos,
            &defs,
          )
        };
        inst.meta.parallel = Some(plan);
        continue;
      }

      #[cfg(feature = "native-async-ops")]
      match inst.t {
        InstTyp::Await => {
          inst.meta.parallel = Some(ParallelPlan::NotParallelizable(ParallelReason::Await));
          continue;
        }
        InstTyp::PromiseAll => {
          let plan = match promise_components_plan(&inst.args, &value_defs, &mut value_memo) {
            Ok(()) => ParallelPlan::SpawnAll,
            Err(reason) => ParallelPlan::NotParallelizable(reason),
          };
          inst.meta.parallel = Some(plan);
          continue;
        }
        InstTyp::PromiseRace => {
          let plan = match promise_components_plan(&inst.args, &value_defs, &mut value_memo) {
            Ok(()) => ParallelPlan::SpawnAllButRaceResult,
            Err(reason) => ParallelPlan::NotParallelizable(reason),
          };
          inst.meta.parallel = Some(plan);
          continue;
        }
        _ => {}
      }

      #[cfg(any(feature = "native-fusion", feature = "native-array-ops"))]
      if inst.t == InstTyp::ArrayChain {
        // Apply parallelization hints to the fused semantic ArrayChain representation.
        //
        // We currently only model `map`/`filter`/`reduce` because other array
        // ops (`find`/`some`/`every`) have short-circuit semantics and may need
        // more sophisticated lowering to parallelize.
        let mut chain_plan = ParallelPlan::Parallelizable;
        let mut should_annotate = true;

        for op in inst.array_chain.iter() {
          let op_plan = match op {
            ArrayChainOp::Map { callback } | ArrayChainOp::Filter { callback } => match inst.args.get(*callback) {
              None => ParallelPlan::NotParallelizable(ParallelReason::UnknownCallback),
              Some(callback_arg) => match resolve_fn_id(callback_arg, &defs, &mut Vec::new()) {
                None => ParallelPlan::NotParallelizable(ParallelReason::UnknownCallback),
                Some(id) => map_filter_plan(id, fn_effects, fn_purities, callback_infos),
              },
            },
            ArrayChainOp::Reduce { callback, .. } => match inst.args.get(*callback) {
              None => ParallelPlan::NotParallelizable(ParallelReason::UnknownCallback),
              Some(callback_arg) => match resolve_fn_id(callback_arg, &defs, &mut Vec::new()) {
                None => ParallelPlan::NotParallelizable(ParallelReason::UnknownCallback),
                Some(id) => reduce_plan(id, fn_effects, fn_purities, callback_infos),
              },
            },
            // For now, do not attach parallelization hints to unsupported ops.
            _ => {
              should_annotate = false;
              break;
            }
          };

          match op_plan {
            ParallelPlan::Parallelizable => {}
            ParallelPlan::NotParallelizable(_) => {
              chain_plan = op_plan;
              break;
            }
            // No other ParallelPlan variants should be produced by array semantic ops.
            _ => {
              chain_plan = op_plan;
              break;
            }
          }
        }

        if should_annotate {
          inst.meta.parallel = Some(chain_plan);
        }
        continue;
      }

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
