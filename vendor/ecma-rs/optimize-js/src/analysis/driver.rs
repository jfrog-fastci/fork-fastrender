//! Program-wide analysis driver.
//!
//! The `optimize-js` compilation pipeline intentionally avoids running most
//! semantic analyses by default. Downstream codegen can opt into a consolidated
//! analysis pass by calling [`annotate_program`] (to attach metadata directly to
//! the IR) or [`analyze_program`] (to only collect results in a side table).

use crate::cfg::cfg::Cfg;
use crate::il::inst::{Arg, BinOp, Const, EffectSet, Inst, InstMeta, InstTyp, Nullability, UnOp};
use crate::il::inst::NullabilityNarrowing;
use crate::{FnId, Program};
use ahash::HashMap;
use ahash::HashMapExt;

use super::{
  alias, consume, effect, encoding, escape, interproc_escape, nullability, ownership, purity,
  range,
};

/// Per-function analysis bundle.
///
/// This is a convenience wrapper used by downstream passes/codegen that only
/// need the core dataflow analyses (range/nullability/encoding) for a single
/// [`Cfg`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct FunctionAnalyses {
  pub nullability: nullability::NullabilityResult,
  pub range: range::RangeResult,
  pub encoding: encoding::EncodingResult,
}

/// Compute the core analyses (range/nullability/encoding) for a single function CFG.
pub fn analyze_cfg(cfg: &Cfg) -> FunctionAnalyses {
  FunctionAnalyses {
    nullability: nullability::calculate_nullability(cfg),
    range: range::analyze_ranges(cfg),
    encoding: encoding::analyze_cfg_encoding(cfg),
  }
}

/// Typed entry point for [`analyze_cfg`].
///
/// The current core analyses are driven entirely by IL metadata, so the typed
/// and untyped entry points are identical.
#[cfg(feature = "typed")]
pub fn analyze_cfg_typed(cfg: &Cfg, _types: &crate::types::TypeContext) -> FunctionAnalyses {
  analyze_cfg(cfg)
}

/// Stable identifier for a function in a [`Program`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FunctionKey {
  /// The top-level "function" (the program body).
  TopLevel,
  /// A nested function referenced by [`Arg::Fn`].
  Fn(FnId),
}

/// Program-wide analysis results.
///
/// This is returned by both [`analyze_program`] and [`annotate_program`]. The
/// `annotate_*` variant additionally writes relevant information into
/// [`InstMeta`] on each instruction.
#[derive(Debug, Default)]
pub struct ProgramAnalyses {
  pub effects_summary: HashMap<FunctionKey, EffectSet>,
  /// Conservative purity classification derived from the function-level effect summary.
  pub purity: HashMap<FunctionKey, purity::Purity>,

  pub alias: HashMap<FunctionKey, alias::AliasResult>,
  pub escape: HashMap<FunctionKey, escape::EscapeResult>,
  pub ownership: HashMap<FunctionKey, ownership::OwnershipResult>,

  pub range: HashMap<FunctionKey, range::RangeResult>,
  pub nullability: HashMap<FunctionKey, nullability::NullabilityResult>,
  pub encoding: HashMap<FunctionKey, encoding::EncodingResult>,
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

fn params_for_key(program: &Program, key: FunctionKey) -> &[u32] {
  match key {
    FunctionKey::TopLevel => &program.top_level.params,
    FunctionKey::Fn(id) => &program.functions[id].params,
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
      // Preserve metadata produced during lowering/typechecking. These fields are
      // orthogonal to analysis results and are expected to remain stable even
      // when we re-run analysis annotations.
      let type_id = inst.meta.type_id;
      let hir_expr = inst.meta.hir_expr;
      let type_summary = inst.meta.type_summary;
      let excludes_nullish = inst.meta.excludes_nullish;
      inst.meta = InstMeta::default();
      inst.meta.type_id = type_id;
      inst.meta.hir_expr = hir_expr;
      inst.meta.type_summary = type_summary;
      inst.meta.excludes_nullish = excludes_nullish;
    }
  }
}

fn annotate_cfg_escape_states(cfg: &mut Cfg, escapes: &escape::EscapeResult) {
  for (_label, insts) in cfg.bblocks.all_mut() {
    for inst in insts {
      let Some(&tgt) = inst.tgts.get(0) else {
        continue;
      };
      inst.meta.result_escape = escapes.get(&tgt).copied();
    }
  }
}

#[derive(Clone, Copy, Debug)]
struct NullishComparison {
  tested_var: u32,
  op: BinOp,
}

fn extract_nullish_comparison(inst: &Inst) -> Option<NullishComparison> {
  if inst.t != InstTyp::Bin {
    return None;
  }

  let op = match inst.bin_op {
    BinOp::LooseEq | BinOp::NotLooseEq | BinOp::StrictEq | BinOp::NotStrictEq => inst.bin_op,
    _ => return None,
  };

  let left = inst.args.get(0)?;
  let right = inst.args.get(1)?;
  let is_nullish = |arg: &Arg| {
    matches!(arg, Arg::Const(Const::Null | Const::Undefined))
      || matches!(arg, Arg::Builtin(name) if name == "undefined")
  };

  let tested_var = match (left, right) {
    (Arg::Var(v), other) if is_nullish(other) => *v,
    (other, Arg::Var(v)) if is_nullish(other) => *v,
    _ => return None,
  };

  Some(NullishComparison { tested_var, op })
}

#[derive(Clone, Copy, Debug)]
struct NullishTest {
  tested_var: u32,
  op: BinOp,
  negated: bool,
}

fn nullability_from_test(test: NullishTest) -> Option<NullabilityNarrowing> {
  let (mut when_true, mut when_false) = match test.op {
    BinOp::LooseEq => (Nullability::Nullish, Nullability::NonNullish),
    BinOp::NotLooseEq => (Nullability::NonNullish, Nullability::Nullish),
    BinOp::StrictEq => (Nullability::Nullish, Nullability::Unknown),
    BinOp::NotStrictEq => (Nullability::Unknown, Nullability::Nullish),
    _ => return None,
  };

  if test.negated {
    std::mem::swap(&mut when_true, &mut when_false);
  }

  Some(NullabilityNarrowing {
    var: test.tested_var,
    when_true,
    when_false,
  })
}

fn annotate_cfg_nullability_narrowings(cfg: &mut Cfg) {
  for label in cfg_block_labels_sorted(cfg) {
    let block = cfg.bblocks.get_mut(label);
    let mut cond_to_test: HashMap<u32, NullishTest> = HashMap::new();
    for inst in block.iter_mut() {
      if let Some(NullishComparison { tested_var, op }) = extract_nullish_comparison(inst) {
        if let Some(&tgt) = inst.tgts.get(0) {
          cond_to_test.insert(
            tgt,
            NullishTest {
              tested_var,
              op,
              negated: false,
            },
          );
        }
      }

      // Propagate through simple boolean negation.
      if inst.t == InstTyp::Un {
        let (tgt, op, arg) = inst.as_un();
        if op == UnOp::Not {
          if let Arg::Var(src) = arg {
            if let Some(mut test) = cond_to_test.get(src).copied() {
              test.negated = !test.negated;
              cond_to_test.insert(tgt, test);
            }
          }
        }
      }

      // Propagate through direct var assignments (`tgt = src`).
      if inst.t == InstTyp::VarAssign {
        let (tgt, arg) = inst.as_var_assign();
        if let Arg::Var(src) = arg {
          if let Some(test) = cond_to_test.get(src).copied() {
            cond_to_test.insert(tgt, test);
          }
        }
      }

      if inst.t != InstTyp::CondGoto {
        continue;
      }
      let Arg::Var(cond_var) = inst.args[0] else {
        continue;
      };
      let Some(test) = cond_to_test.get(&cond_var).copied() else {
        continue;
      };

      if let Some(narrowing) = nullability_from_test(test) {
        inst.meta.nullability_narrowing = Some(narrowing);
      }
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
  let purities = purity::compute_program_purity(program, &effects);
  analyses
    .purity
    .insert(FunctionKey::TopLevel, purities.top_level);
  for id in sorted_fn_ids(program) {
    analyses.purity.insert(FunctionKey::Fn(id), purities.for_fn(id));
  }

  // 3) alias
  for &key in &keys {
    analyses
      .alias
      .insert(key, alias::calculate_alias(cfg_for_key(program, key)));
  }

  // 4) escape
  let escape_summaries = interproc_escape::compute_program_escape_summaries(program);
  for &key in &keys {
    analyses
      .escape
      .insert(
        key,
        escape::analyze_cfg_escapes_with_params_and_summaries(
          cfg_for_key(program, key),
          params_for_key(program, key),
          Some(&escape_summaries),
        ),
      );
  }

  // 5) ownership
  for &key in &keys {
    let cfg = cfg_for_key(program, key);
    let params = params_for_key(program, key);
    let escapes = analyses
      .escape
      .get(&key)
      .expect("escape results should be populated before ownership");
    analyses
      .ownership
      .insert(key, ownership::analyze_cfg_ownership_with_escapes_and_params(cfg, params, escapes));
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
    analyses
      .encoding
      .insert(key, encoding::analyze_cfg_encoding(cfg_for_key(program, key)));
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
  let purities = purity::compute_program_purity(program, &effects);
  for &key in &keys {
    match key {
      FunctionKey::TopLevel => purity::annotate_cfg_purity(&mut program.top_level.body, &purities),
      FunctionKey::Fn(id) => purity::annotate_cfg_purity(&mut program.functions[id].body, &purities),
    }
  }
  analyses
    .purity
    .insert(FunctionKey::TopLevel, purities.top_level);
  for id in sorted_fn_ids(program) {
    analyses.purity.insert(FunctionKey::Fn(id), purities.for_fn(id));
  }

  // 3) alias
  for &key in &keys {
    analyses
      .alias
      .insert(key, alias::calculate_alias(cfg_for_key(program, key)));
  }

  // 4) escape
  let escape_summaries = interproc_escape::compute_program_escape_summaries(program);
  for &key in &keys {
    let escapes = escape::analyze_cfg_escapes_with_params_and_summaries(
      cfg_for_key(program, key),
      params_for_key(program, key),
      Some(&escape_summaries),
    );
    annotate_cfg_escape_states(cfg_for_key_mut(program, key), &escapes);
    analyses.escape.insert(key, escapes);
  }

  // 5) ownership
  for &key in &keys {
    let ownership_result = {
      let cfg = cfg_for_key(program, key);
      let params = params_for_key(program, key);
      let escapes = analyses
        .escape
        .get(&key)
        .expect("escape results should be populated before ownership");
      ownership::analyze_cfg_ownership_with_escapes_and_params(cfg, params, escapes)
    };
    ownership::annotate_cfg_ownership(cfg_for_key_mut(program, key), &ownership_result);
    consume::annotate_cfg_consumption(cfg_for_key_mut(program, key), &ownership_result);
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
    let encoding_result = encoding::analyze_cfg_encoding(cfg_for_key(program, key));
    encoding::annotate_cfg_encoding(cfg_for_key_mut(program, key), &encoding_result);
    analyses.encoding.insert(key, encoding_result);
  }

  analyses
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::cfg::cfg::{Cfg, CfgBBlocks, CfgGraph};
  use crate::compile_source;
  use crate::il::inst::{Arg, BinOp, Const, Inst, InstTyp, Nullability, OwnershipState, StringEncoding, UnOp};
  use crate::{OptimizationStats, Program, ProgramFunction};
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
      // `typeof` produces a string result, which should get encoding metadata.
      sink(typeof out);
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
      "expected at least one call site to have non-default InstMeta.effects"
    );

    assert!(
      any_inst(&program, |inst| inst.t == InstTyp::Call
        && inst.meta.callee_purity != crate::analysis::purity::Purity::Impure),
      "expected at least one call site to record non-Impure callee purity"
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

    assert!(
      any_inst(&program, |inst| inst.meta.result_escape.is_some()),
      "expected at least one instruction to record escape information in InstMeta.result_escape"
    );

    assert!(
      any_inst(&program, |inst| inst.meta.result_type.string_encoding.is_some()),
      "expected at least one instruction to record string encoding in InstMeta.result_type"
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
    assert!(
      analyses
        .encoding
        .get(&FunctionKey::TopLevel)
        .is_some_and(|r| r.block_entry(top_cfg.entry).is_some()),
      "expected encoding analysis results for top-level entry block"
    );
  }

  fn cfg_with_blocks(blocks: &[(u32, Vec<Inst>)], edges: &[(u32, u32)]) -> Cfg {
    let labels: Vec<u32> = blocks.iter().map(|(label, _)| *label).collect();
    let mut graph = CfgGraph::default();
    for &(from, to) in edges {
      graph.connect(from, to);
    }
    for &label in &labels {
      if !graph.contains(label) {
        // Ensure the node exists even if it has no edges.
        graph.connect(label, label);
        graph.disconnect(label, label);
      }
    }
    let mut bblocks = CfgBBlocks::default();
    for (label, insts) in blocks.iter() {
      bblocks.add(*label, insts.clone());
    }
    Cfg {
      graph,
      bblocks,
      entry: 0,
    }
  }

  #[test]
  fn analyze_program_computes_encoding_results() {
    let cfg = cfg_with_blocks(
      &[(
        0,
        vec![
          Inst::var_assign(0, Arg::Const(Const::Str("hello".to_string()))),
          Inst::var_assign(1, Arg::Const(Const::Str("π".to_string()))),
        ],
      )],
      &[],
    );
    let program = Program {
      functions: Vec::new(),
      top_level: ProgramFunction {
        debug: None,
        body: cfg,
        params: Vec::new(),
        ssa_body: None,
        stats: OptimizationStats::default(),
      },
      top_level_mode: TopLevelMode::Module,
      symbols: None,
    };

    let analyses = analyze_program(&program);
    let encoding = analyses
      .encoding
      .get(&FunctionKey::TopLevel)
      .expect("top-level encoding results missing");

    assert_eq!(
      encoding.encoding_at_exit(0, 0),
      StringEncoding::Ascii,
      "expected ASCII string literal to be classified as Ascii"
    );
    assert_eq!(
      encoding.encoding_at_exit(0, 1),
      StringEncoding::Utf8,
      "expected non-ASCII string literal to be classified as Utf8"
    );
  }

  #[test]
  fn annotate_program_writes_string_encoding_metadata() {
    let cfg = cfg_with_blocks(
      &[(
        0,
        vec![
          Inst::var_assign(0, Arg::Const(Const::Str("hello".to_string()))),
          Inst::var_assign(1, Arg::Const(Const::Str("π".to_string()))),
        ],
      )],
      &[],
    );
    let mut program = Program {
      functions: Vec::new(),
      top_level: ProgramFunction {
        debug: None,
        body: cfg,
        params: Vec::new(),
        ssa_body: None,
        stats: OptimizationStats::default(),
      },
      top_level_mode: TopLevelMode::Module,
      symbols: None,
    };

    let _analyses = annotate_program(&mut program);

    let insts = program.top_level.body.bblocks.get(0);
    assert_eq!(
      insts[0].meta.result_type.string_encoding,
      Some(StringEncoding::Ascii),
      "expected ASCII string literal to be annotated as Ascii"
    );
    assert_eq!(
      insts[1].meta.result_type.string_encoding,
      Some(StringEncoding::Utf8),
      "expected non-ASCII string literal to be annotated as Utf8"
    );
  }

  #[test]
  fn annotate_program_records_nullability_narrowing_through_not() {
    let cfg = cfg_with_blocks(
      &[
        (
          0,
          vec![
            Inst::unknown_load(0, "x".to_string()),
            Inst::bin(1, Arg::Var(0), BinOp::LooseEq, Arg::Const(Const::Null)),
            Inst::un(2, UnOp::Not, Arg::Var(1)),
            Inst::cond_goto(Arg::Var(2), 1, 2),
          ],
        ),
        (1, vec![]),
        (2, vec![]),
      ],
      &[(0, 1), (0, 2)],
    );
    let mut program = Program {
      functions: Vec::new(),
      top_level: ProgramFunction {
        debug: None,
        body: cfg,
        params: Vec::new(),
        ssa_body: None,
        stats: OptimizationStats::default(),
      },
      top_level_mode: TopLevelMode::Module,
      symbols: None,
    };

    let _analyses = annotate_program(&mut program);

    let insts = program.top_level.body.bblocks.get(0);
    let narrowing = insts
      .last()
      .and_then(|inst| inst.meta.nullability_narrowing)
      .expect("expected CondGoto to record nullability narrowing");
    assert_eq!(narrowing.var, 0);
    assert_eq!(narrowing.when_true, Nullability::NonNullish);
    assert_eq!(narrowing.when_false, Nullability::Nullish);
  }

  #[test]
  fn annotate_program_records_nullability_narrowing_through_var_assign() {
    let cfg = cfg_with_blocks(
      &[
        (
          0,
          vec![
            Inst::unknown_load(0, "x".to_string()),
            Inst::bin(1, Arg::Var(0), BinOp::LooseEq, Arg::Const(Const::Null)),
            Inst::var_assign(2, Arg::Var(1)),
            Inst::cond_goto(Arg::Var(2), 1, 2),
          ],
        ),
        (1, vec![]),
        (2, vec![]),
      ],
      &[(0, 1), (0, 2)],
    );
    let mut program = Program {
      functions: Vec::new(),
      top_level: ProgramFunction {
        debug: None,
        body: cfg,
        params: Vec::new(),
        ssa_body: None,
        stats: OptimizationStats::default(),
      },
      top_level_mode: TopLevelMode::Module,
      symbols: None,
    };

    let _analyses = annotate_program(&mut program);

    let insts = program.top_level.body.bblocks.get(0);
    let narrowing = insts
      .last()
      .and_then(|inst| inst.meta.nullability_narrowing)
      .expect("expected CondGoto to record nullability narrowing");
    assert_eq!(narrowing.var, 0);
    assert_eq!(narrowing.when_true, Nullability::Nullish);
    assert_eq!(narrowing.when_false, Nullability::NonNullish);
  }

  #[test]
  fn annotate_program_records_nullability_narrowing_for_builtin_undefined() {
    let cfg = cfg_with_blocks(
      &[
        (
          0,
          vec![
            Inst::unknown_load(0, "x".to_string()),
            Inst::bin(
              1,
              Arg::Var(0),
              BinOp::StrictEq,
              Arg::Builtin("undefined".to_string()),
            ),
            Inst::cond_goto(Arg::Var(1), 1, 2),
          ],
        ),
        (1, vec![]),
        (2, vec![]),
      ],
      &[(0, 1), (0, 2)],
    );
    let mut program = Program {
      functions: Vec::new(),
      top_level: ProgramFunction {
        debug: None,
        body: cfg,
        params: Vec::new(),
        ssa_body: None,
        stats: OptimizationStats::default(),
      },
      top_level_mode: TopLevelMode::Module,
      symbols: None,
    };

    let _analyses = annotate_program(&mut program);

    let insts = program.top_level.body.bblocks.get(0);
    let narrowing = insts
      .last()
      .and_then(|inst| inst.meta.nullability_narrowing)
      .expect("expected CondGoto to record nullability narrowing");
    assert_eq!(narrowing.var, 0);
    assert_eq!(narrowing.when_true, Nullability::Nullish);
    assert_eq!(narrowing.when_false, Nullability::Unknown);
  }
}
