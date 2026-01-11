//! Program-wide analysis driver.
//!
//! The `optimize-js` compilation pipeline intentionally avoids running most
//! semantic analyses by default. Downstream codegen can opt into a consolidated
//! analysis pass by calling [`annotate_program`] (to attach metadata directly to
//! the IR) or [`analyze_program`] (to only collect results in a side table).

use crate::cfg::cfg::Cfg;
use crate::il::inst::{Arg, BinOp, Const, EffectSet, Inst, InstMeta, InstTyp, Nullability};
use crate::il::inst::NullabilityNarrowing;
use crate::{FnId, Program};
use ahash::HashMap;
use ahash::HashMapExt;

use super::{alias, effect, escape, nullability, ownership, range};

/// Stable identifier for a function in a [`Program`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FunctionKey {
  /// The top-level "function" (the program body).
  TopLevel,
  /// A nested function referenced by [`Arg::Fn`].
  Fn(FnId),
}

#[derive(Clone, Debug, Default)]
pub struct EncodingAnalysisResult {}

/// Program-wide analysis results.
///
/// This is returned by both [`analyze_program`] and [`annotate_program`]. The
/// `annotate_*` variant additionally writes relevant information into
/// [`InstMeta`] on each instruction.
#[derive(Debug, Default)]
pub struct ProgramAnalyses {
  pub effects_summary: HashMap<FunctionKey, EffectSet>,
  /// `true` when the function has no observable effects.
  pub purity: HashMap<FunctionKey, bool>,

  pub alias: HashMap<FunctionKey, alias::AliasResult>,
  pub escape: HashMap<FunctionKey, escape::EscapeResult>,
  pub ownership: HashMap<FunctionKey, ownership::OwnershipResult>,

  pub range: HashMap<FunctionKey, range::RangeResult>,
  pub nullability: HashMap<FunctionKey, nullability::NullabilityResult>,
  pub encoding: HashMap<FunctionKey, EncodingAnalysisResult>,
}

fn sorted_fn_ids(program: &Program) -> Vec<FnId> {
  let mut ids: Vec<FnId> = (0..program.functions.len()).collect();
  ids.sort_unstable();
  ids
}

fn function_keys(program: &Program) -> Vec<FunctionKey> {
  let mut keys = Vec::with_capacity(program.functions.len() + 1);
  keys.push(FunctionKey::TopLevel);
  keys.extend(sorted_fn_ids(program).into_iter().map(FunctionKey::Fn));
  keys
}

fn cfg_for_key(program: &Program, key: FunctionKey) -> &Cfg {
  match key {
    FunctionKey::TopLevel => &program.top_level.body,
    FunctionKey::Fn(id) => &program.functions[id].body,
  }
}

fn cfg_for_key_mut(program: &mut Program, key: FunctionKey) -> &mut Cfg {
  match key {
    FunctionKey::TopLevel => &mut program.top_level.body,
    FunctionKey::Fn(id) => &mut program.functions[id].body,
  }
}

fn cfg_block_labels_sorted(cfg: &Cfg) -> Vec<u32> {
  let mut labels: Vec<u32> = cfg.bblocks.all().map(|(label, _)| label).collect();
  labels.sort_unstable();
  labels
}

fn reset_cfg_meta(cfg: &mut Cfg) {
  for label in cfg_block_labels_sorted(cfg) {
    for inst in cfg.bblocks.get_mut(label).iter_mut() {
      inst.meta = InstMeta::default();
    }
  }
}

fn extract_nullish_test(inst: &Inst) -> Option<(u32, bool)> {
  if inst.t != InstTyp::Bin {
    return None;
  }
  let op = inst.bin_op;
  let is_eq = match op {
    BinOp::LooseEq => true,
    BinOp::NotLooseEq => false,
    _ => return None,
  };

  let (left, right) = (&inst.args[0], &inst.args[1]);
  match (left, right) {
    (Arg::Var(v), Arg::Const(Const::Null)) | (Arg::Const(Const::Null), Arg::Var(v)) => {
      Some((*v, is_eq))
    }
    _ => None,
  }
}

fn annotate_cfg_nullability_narrowings(cfg: &mut Cfg) {
  for label in cfg_block_labels_sorted(cfg) {
    let block = cfg.bblocks.get_mut(label);
    let mut cond_to_test: HashMap<u32, (u32, bool)> = HashMap::new();
    for inst in block.iter_mut() {
      if let Some((tested_var, is_eq)) = extract_nullish_test(inst) {
        cond_to_test.insert(inst.tgts[0], (tested_var, is_eq));
      }

      if inst.t != InstTyp::CondGoto {
        continue;
      }
      let Arg::Var(cond_var) = inst.args[0] else {
        continue;
      };
      let Some(&(tested_var, is_eq)) = cond_to_test.get(&cond_var) else {
        continue;
      };
      let (when_true, when_false) = if is_eq {
        (Nullability::Nullish, Nullability::NonNullish)
      } else {
        (Nullability::NonNullish, Nullability::Nullish)
      };
      let narrowing = NullabilityNarrowing {
        var: tested_var,
        when_true,
        when_false,
      };
      inst.meta.nullability_narrowing = Some(narrowing);
    }
  }
}

/// Compute all analyses for `program` without mutating it.
pub fn analyze_program(program: &Program) -> ProgramAnalyses {
  let keys = function_keys(program);
  let mut analyses = ProgramAnalyses::default();

  // 1) effects
  let effects = effect::compute_program_effects(program);
  analyses
    .effects_summary
    .insert(FunctionKey::TopLevel, effects.top_level.clone());
  for id in sorted_fn_ids(program) {
    analyses
      .effects_summary
      .insert(FunctionKey::Fn(id), effects.functions[id].clone());
  }

  // 2) purity
  for &key in &keys {
    let is_pure = analyses
      .effects_summary
      .get(&key)
      .map(|eff| eff.is_pure())
      .unwrap_or(false);
    analyses.purity.insert(key, is_pure);
  }

  // 3) alias
  for &key in &keys {
    analyses
      .alias
      .insert(key, alias::calculate_alias(cfg_for_key(program, key)));
  }

  // 4) escape
  for &key in &keys {
    analyses
      .escape
      .insert(key, escape::analyze_cfg_escapes(cfg_for_key(program, key)));
  }

  // 5) ownership
  for &key in &keys {
    let cfg = cfg_for_key(program, key);
    let escapes = analyses
      .escape
      .get(&key)
      .expect("escape results should be populated before ownership");
    analyses
      .ownership
      .insert(key, ownership::analyze_cfg_ownership_with_escapes(cfg, escapes));
  }

  // 6) nullability/range/encoding
  for &key in &keys {
    analyses.nullability.insert(
      key,
      nullability::calculate_nullability(cfg_for_key(program, key)),
    );
    analyses
      .range
      .insert(key, range::analyze_ranges(cfg_for_key(program, key)));
    analyses.encoding.insert(key, EncodingAnalysisResult::default());
  }

  analyses
}

/// Compute all analyses for `program`, annotating per-instruction metadata.
///
/// This resets all existing [`InstMeta`] on the program and rewrites it from
/// scratch, so it is safe to call repeatedly.
pub fn annotate_program(program: &mut Program) -> ProgramAnalyses {
  let keys = function_keys(program);

  // Clear any stale metadata before annotating.
  for &key in &keys {
    reset_cfg_meta(cfg_for_key_mut(program, key));
  }

  let mut analyses = ProgramAnalyses::default();

  // 1) effects
  let effects = effect::compute_program_effects(program);
  for &key in &keys {
    match key {
      FunctionKey::TopLevel => effect::annotate_cfg_effects(&mut program.top_level.body, &effects),
      FunctionKey::Fn(id) => effect::annotate_cfg_effects(&mut program.functions[id].body, &effects),
    }
  }
  analyses
    .effects_summary
    .insert(FunctionKey::TopLevel, effects.top_level.clone());
  for id in sorted_fn_ids(program) {
    analyses
      .effects_summary
      .insert(FunctionKey::Fn(id), effects.functions[id].clone());
  }

  // 2) purity
  for &key in &keys {
    let is_pure = analyses
      .effects_summary
      .get(&key)
      .map(|eff| eff.is_pure())
      .unwrap_or(false);
    analyses.purity.insert(key, is_pure);
  }

  // 3) alias
  for &key in &keys {
    analyses
      .alias
      .insert(key, alias::calculate_alias(cfg_for_key(program, key)));
  }

  // 4) escape
  for &key in &keys {
    analyses
      .escape
      .insert(key, escape::analyze_cfg_escapes(cfg_for_key(program, key)));
  }

  // 5) ownership
  for &key in &keys {
    let ownership_result = {
      let cfg = cfg_for_key(program, key);
      let escapes = analyses
        .escape
        .get(&key)
        .expect("escape results should be populated before ownership");
      ownership::analyze_cfg_ownership_with_escapes(cfg, escapes)
    };
    ownership::annotate_cfg_ownership(cfg_for_key_mut(program, key), &ownership_result);
    analyses.ownership.insert(key, ownership_result);
  }

  // 6) nullability/range/encoding
  for &key in &keys {
    analyses.nullability.insert(
      key,
      nullability::calculate_nullability(cfg_for_key(program, key)),
    );
    analyses
      .range
      .insert(key, range::analyze_ranges(cfg_for_key(program, key)));
    annotate_cfg_nullability_narrowings(cfg_for_key_mut(program, key));
    analyses.encoding.insert(key, EncodingAnalysisResult::default());
  }

  analyses
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::compile_source;
  use crate::il::inst::{Arg, InstTyp, OwnershipState};
  use crate::TopLevelMode;

  fn any_inst(program: &Program, pred: impl Fn(&Inst) -> bool) -> bool {
    let mut keys = function_keys(program);
    // Deterministic ordering in case this gets debugged.
    keys.sort();
    for key in keys {
      let cfg = cfg_for_key(program, key);
      for label in cfg.graph.labels_sorted() {
        for inst in cfg.bblocks.get(label).iter() {
          if pred(inst) {
            return true;
          }
        }
      }
    }
    false
  }

  #[test]
  fn annotate_program_smoke() {
    let source = r#"
      // Nested functions + optional chaining.
      const out = ((o) => {
        return ((x) => x + 1)(o?.x);
      })({ x: 1 });
      void out;
    "#;

    let mut program = compile_source(source, TopLevelMode::Module, false).expect("compile");
    let analyses = annotate_program(&mut program);

    assert!(
      any_inst(&program, |inst| !inst.meta.effects.is_default()),
      "expected at least one instruction to have non-default InstMeta.effects"
    );

    assert!(
      any_inst(&program, |inst| inst.t == InstTyp::Call && !inst.meta.effects.is_pure()),
      "expected at least one call site to have call purity information"
    );

    assert!(
      any_inst(&program, |inst| inst.t == InstTyp::Call
        && matches!(inst.args.get(0), Some(Arg::Builtin(name)) if name.starts_with("__optimize_js_"))
        && inst.meta.ownership != OwnershipState::Unknown),
      "expected at least one allocation call to have non-Unknown InstMeta.ownership"
    );

    assert!(
      any_inst(&program, |inst| inst.t == InstTyp::CondGoto && inst.meta.nullability_narrowing.is_some()),
      "expected at least one CondGoto to record nullability narrowing"
    );

    let top_cfg = &program.top_level.body;
    assert!(
      analyses.range.get(&FunctionKey::TopLevel).is_some_and(|r| r.entry(top_cfg.entry).is_some()),
      "expected range analysis results for top-level entry block"
    );
    assert!(
      analyses
        .nullability
        .get(&FunctionKey::TopLevel)
        .is_some_and(|r| r.entry_state(top_cfg.entry).is_reachable()),
      "expected nullability analysis results for top-level entry block"
    );
  }
}
