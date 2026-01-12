//! Stable, versioned "program dump" format for downstream tooling.
//!
//! This module intentionally defines a schema that is:
//! - **stable** (explicit, versioned, deterministic)
//! - **tooling-friendly** (JSON/msgpack, avoids hash iteration order)
//! - **decoupled** from `Inst`'s `serde` representation (which skips `InstMeta`)
//!
//! Note: most instruction metadata (effects/ownership/escape/purity/typed IDs)
//! is populated by running [`crate::analysis::annotate_program`]. `dump_program`
//! will include whatever metadata is already present on the CFGs.

use crate::analysis::{self, analyze_cfg};
use crate::cfg::cfg::Cfg;
use crate::il::inst::{
  Arg, ArgUseMode, AwaitBehavior, BinOp, Const, EffectSet, InPlaceHint, Inst, InstTyp, Nullability,
  NullabilityNarrowing, OwnershipState, ParallelPlan, Purity, StringEncoding, UnOp,
};
use crate::il::meta::{EscapeState as ValueEscapeState, ValueFacts};
use crate::symbol::semantics::{ScopeId, SymbolId};
use crate::{compile_source, Program, TopLevelMode};
use diagnostics::{Diagnostic, FileId, Span, TextRange};
use std::collections::BTreeMap;

/// Version number for the [`ProgramDump`] schema.
pub type DumpVersion = u32;

/// Current [`ProgramDump`] schema version.
pub const DUMP_VERSION: DumpVersion = 2;

#[derive(Clone, Copy, Debug, Default)]
pub struct DumpOptions {
  /// Include the optional symbol table dump.
  pub include_symbols: bool,
  /// Include the optional program-wide analysis summary.
  pub include_analyses: bool,
}

impl DumpOptions {
  pub fn minimal() -> Self {
    Self {
      include_symbols: false,
      include_analyses: false,
    }
  }
}

/// Options for [`compile_source_to_dump`].
#[derive(Clone, Copy, Debug)]
pub struct CompileDumpOptions {
  /// Enable type checking when `optimize-js` is built with feature `"typed"`.
  pub typed: bool,
  /// Require that `optimize-js` is built with feature `"semantic-ops"`.
  ///
  /// Semantic ops are enabled at compile time; this flag exists so downstream
  /// tooling can surface a clear error message when a user requests semantic ops
  /// but the server binary was built without that feature.
  pub semantic_ops: bool,
  /// Run [`crate::analysis::annotate_program`] before dumping.
  pub run_analyses: bool,
  /// Include the optional symbol table dump.
  pub include_symbols: bool,
  /// Include the optional program-wide analysis summary.
  pub include_analyses: bool,
  /// Preserve optimizer debug checkpoints (`OptimizerDebug`) during compilation.
  ///
  /// Note: the dump format does not currently include optimizer steps, but
  /// retaining them can be useful for tools that also consume the compiled
  /// [`crate::Program`] separately.
  pub debug: bool,
}

impl Default for CompileDumpOptions {
  fn default() -> Self {
    Self {
      typed: false,
      semantic_ops: false,
      run_analyses: true,
      include_symbols: true,
      include_analyses: false,
      debug: false,
    }
  }
}

fn compile_dump_option_error(code: &'static str, message: impl Into<String>) -> Vec<Diagnostic> {
  vec![Diagnostic::error(
    code,
    message,
    Span::new(FileId(0), TextRange::new(0, 0)),
  )]
}

/// Compile a source string to a [`ProgramDump`].
///
/// This is a convenience wrapper around [`crate::compile_source`] (or its typed
/// variant) + optional [`crate::analysis::annotate_program`] + [`dump_program`].
pub fn compile_source_to_dump(
  source: &str,
  mode: TopLevelMode,
  options: CompileDumpOptions,
) -> Result<ProgramDump, Vec<Diagnostic>> {
  if options.semantic_ops && !cfg!(feature = "semantic-ops") {
    return Err(compile_dump_option_error(
      "OPTDBG0002",
      "semantic ops requested but optimize-js was built without feature \"semantic-ops\"",
    ));
  }

  let mut program = if options.typed {
    #[cfg(feature = "typed")]
    {
      crate::compile_source_typed(source, mode, options.debug)?
    }
    #[cfg(not(feature = "typed"))]
    {
      return Err(compile_dump_option_error(
        "OPTDBG0001",
        "typed compilation requested but optimize-js was built without feature \"typed\"",
      ));
    }
  } else {
    compile_source(source, mode, options.debug)?
  };

  if options.run_analyses {
    analysis::annotate_program(&mut program);
  }

  Ok(dump_program(
    &program,
    DumpOptions {
      include_symbols: options.include_symbols,
      include_analyses: options.include_analyses,
    },
  ))
}

#[derive(Clone, Copy, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum SourceModeDump {
  Module,
  Script,
  Global,
}

impl From<TopLevelMode> for SourceModeDump {
  fn from(value: TopLevelMode) -> Self {
    match value {
      TopLevelMode::Module => Self::Module,
      TopLevelMode::Script => Self::Script,
      TopLevelMode::Global => Self::Global,
    }
  }
}

/// Top-level program dump.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct ProgramDump {
  pub version: DumpVersion,
  pub source_mode: SourceModeDump,
  pub top_level: FunctionDump,
  pub functions: Vec<FunctionDump>,
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  pub symbols: Option<ProgramSymbolsDump>,
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  pub analyses: Option<ProgramAnalysesDump>,
}

impl ProgramDump {
  pub fn to_json_value(&self) -> serde_json::Value {
    serde_json::to_value(self).expect("serialize ProgramDump to JSON value")
  }

  pub fn to_json_string(&self) -> String {
    serde_json::to_string_pretty(self).expect("serialize ProgramDump to JSON string")
  }

  pub fn to_msgpack(&self) -> Vec<u8> {
    rmp_serde::to_vec_named(self).expect("serialize ProgramDump to msgpack")
  }
}

#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct FunctionDump {
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  pub id: Option<u32>,
  pub params: Vec<u32>,
  pub cfg: CfgDump,
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  pub cfg_deconstructed: Option<CfgDump>,
}

#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct CfgDump {
  pub entry: u32,
  pub bblock_order: Vec<u32>,
  pub bblocks: BTreeMap<u32, Vec<InstDump>>,
  pub cfg_edges: BTreeMap<u32, Vec<u32>>,
}

#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case", tag = "kind", content = "value"))]
pub enum ConstDump {
  Null,
  Undefined,
  BigInt(String),
  Bool(bool),
  Num(f64),
  Str(String),
}

fn dump_const(value: &Const) -> ConstDump {
  match value {
    Const::Null => ConstDump::Null,
    Const::Undefined => ConstDump::Undefined,
    Const::Bool(v) => ConstDump::Bool(*v),
    Const::Num(num) => ConstDump::Num(num.0),
    Const::Str(s) => ConstDump::Str(s.clone()),
    Const::BigInt(v) => ConstDump::BigInt(v.to_string()),
  }
}

#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case", tag = "kind"))]
pub enum ArgDump {
  Builtin { value: String },
  Const { value: ConstDump },
  Fn { value: u32 },
  Var { value: u32 },
}

fn dump_arg(arg: &Arg) -> ArgDump {
  match arg {
    Arg::Builtin(path) => ArgDump::Builtin { value: path.clone() },
    Arg::Const(value) => ArgDump::Const {
      value: dump_const(value),
    },
    Arg::Fn(idx) => ArgDump::Fn { value: *idx as u32 },
    Arg::Var(id) => ArgDump::Var { value: *id },
  }
}

#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct InstDump {
  pub t: String,
  pub tgts: Vec<u32>,
  pub args: Vec<ArgDump>,
  pub spreads: Vec<u32>,
  pub labels: Vec<u32>,
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  pub bin_op: Option<String>,
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  pub un_op: Option<String>,
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  pub foreign: Option<u64>,
  /// Stringified copy of [`InstDump::foreign`].
  ///
  /// `SymbolId` values (used for `ForeignLoad`/`ForeignStore`) are `u64` and can
  /// exceed JavaScript's safe integer range, which breaks tooling that decodes
  /// MessagePack into JS numbers. Tooling should prefer this field when present.
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  pub foreign_str: Option<String>,
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  pub unknown: Option<String>,
  pub meta: InstMetaDump,
}

#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct NullabilityFactDump {
  pub may_be_null: bool,
  pub may_be_undefined: bool,
  pub may_be_other: bool,
  pub is_bottom: bool,
}

#[derive(Clone, Copy, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum ValueEscapeStateDump {
  Unknown,
  NoEscape,
  Escapes,
}

impl From<ValueEscapeState> for ValueEscapeStateDump {
  fn from(value: ValueEscapeState) -> Self {
    match value {
      ValueEscapeState::Unknown => Self::Unknown,
      ValueEscapeState::NoEscape => Self::NoEscape,
      ValueEscapeState::Escapes => Self::Escapes,
    }
  }
}

#[derive(Clone, Copy, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum ValueNullabilityDump {
  Unknown,
  Nullish,
  NonNullish,
}

impl From<Nullability> for ValueNullabilityDump {
  fn from(value: Nullability) -> Self {
    match value {
      Nullability::Unknown => Self::Unknown,
      Nullability::Nullish => Self::Nullish,
      Nullability::NonNullish => Self::NonNullish,
    }
  }
}

#[derive(Clone, Copy, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct NullabilityNarrowingDump {
  pub var: u32,
  pub when_true: ValueNullabilityDump,
  pub when_false: ValueNullabilityDump,
}

fn dump_nullability_narrowing(value: NullabilityNarrowing) -> NullabilityNarrowingDump {
  NullabilityNarrowingDump {
    var: value.var,
    when_true: value.when_true.into(),
    when_false: value.when_false.into(),
  }
}

#[derive(Clone, Copy, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct SourceSpanDump {
  pub start: u32,
  pub end: u32,
}

fn dump_span(span: diagnostics::TextRange, source_len: u32) -> SourceSpanDump {
  let mut start = span.start.min(source_len);
  let mut end = span.end.min(source_len);
  if end < start {
    std::mem::swap(&mut start, &mut end);
  }
  SourceSpanDump { start, end }
}

#[derive(Clone, Copy, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct ValueIntRangeDump {
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  pub min: Option<i64>,
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  pub max: Option<i64>,
}

#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct ValueFactsDump {
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  pub purity: Option<Purity>,
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  pub escape: Option<ValueEscapeStateDump>,
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  pub ownership: Option<OwnershipState>,
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  pub encoding: Option<StringEncoding>,
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  pub int_range: Option<ValueIntRangeDump>,
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  pub nullability: Option<ValueNullabilityDump>,
}

fn dump_value_facts(value: &ValueFacts) -> Option<ValueFactsDump> {
  let int_range = value.int_range.and_then(|range| {
    (range.min.is_some() || range.max.is_some()).then_some(ValueIntRangeDump {
      min: range.min,
      max: range.max,
    })
  });

  let out = ValueFactsDump {
    purity: value.purity,
    escape: value.escape.map(Into::into),
    ownership: value.ownership,
    encoding: value.encoding,
    int_range,
    nullability: value.nullability.map(Into::into),
  };

  let is_empty = out.purity.is_none()
    && out.escape.is_none()
    && out.ownership.is_none()
    && out.encoding.is_none()
    && out.int_range.is_none()
    && out.nullability.is_none();

  (!is_empty).then_some(out)
}

#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct InstMetaDump {
  pub effects: EffectSet,
  pub purity: Purity,
  pub callee_purity: Purity,
  pub ownership: OwnershipState,
  pub arg_use_modes: Vec<ArgUseMode>,
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  pub in_place_hint: Option<InPlaceHint>,
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  pub result_escape: Option<analysis::escape::EscapeState>,
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  pub range: Option<analysis::range::IntRange>,
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  pub nullability: Option<NullabilityFactDump>,
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  pub encoding: Option<StringEncoding>,
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  pub type_id: Option<String>,
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  pub type_summary: Option<String>,
  pub excludes_nullish: bool,
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  pub native_layout: Option<String>,
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  pub span: Option<SourceSpanDump>,
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  pub preserve_var_assign: Option<bool>,
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  pub stack_alloc_candidate: Option<bool>,
  #[cfg(feature = "native-async-ops")]
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  pub await_known_resolved: Option<bool>,
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  pub await_behavior: Option<AwaitBehavior>,
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  pub parallel: Option<ParallelPlan>,
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  pub nullability_narrowing: Option<NullabilityNarrowingDump>,
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  pub value: Option<ValueFactsDump>,
  /// Reserved for legacy tooling. Prefer [`InstMetaDump::native_layout`].
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  pub layout_id: Option<u32>,
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  pub hir_expr: Option<u32>,
}

fn dump_inst(
  inst: &Inst,
  range: Option<analysis::range::IntRange>,
  nullability: Option<analysis::nullability::NullabilityMask>,
  encoding: Option<StringEncoding>,
  source_len: u32,
) -> InstDump {
  // Canonicalize `spreads` (order doesn't matter; it's a set of indices).
  let mut spreads: Vec<u32> = inst.spreads.iter().map(|s| *s as u32).collect();
  spreads.sort_unstable();
  spreads.dedup();

  // Canonicalize phi operands by sorting by incoming label.
  let (labels, args) = if inst.t == InstTyp::Phi && inst.labels.len() == inst.args.len() {
    let mut pairs: Vec<(u32, ArgDump)> = inst
      .labels
      .iter()
      .copied()
      .zip(inst.args.iter().map(dump_arg))
      .collect();
    pairs.sort_by_key(|(label, _)| *label);
    let (labels, args): (Vec<u32>, Vec<ArgDump>) = pairs.into_iter().unzip();
    (labels, args)
  } else {
    (inst.labels.clone(), inst.args.iter().map(dump_arg).collect())
  };

  let bin_op = match inst.t {
    InstTyp::Bin if !matches!(inst.bin_op, BinOp::_Dummy) => Some(format!("{:?}", inst.bin_op)),
    _ => None,
  };
  let un_op = match inst.t {
    InstTyp::Un if !matches!(inst.un_op, UnOp::_Dummy) => Some(format!("{:?}", inst.un_op)),
    _ => None,
  };
  let foreign = match inst.t {
    InstTyp::ForeignLoad | InstTyp::ForeignStore => Some(inst.foreign.raw_id()),
    _ => None,
  };
  let foreign_str = foreign.map(|id| id.to_string());
  let unknown = match inst.t {
    InstTyp::UnknownLoad | InstTyp::UnknownStore if !inst.unknown.is_empty() => {
      Some(inst.unknown.clone())
    }
    _ => None,
  };

  let arg_use_modes = (0..inst.args.len())
    .map(|idx| inst.meta.arg_use_mode(idx))
    .collect::<Vec<_>>();

  let nullability = nullability.map(|mask| NullabilityFactDump {
    may_be_null: mask.may_be_null(),
    may_be_undefined: mask.may_be_undefined(),
    may_be_other: mask.contains(analysis::nullability::NullabilityMask::OTHER),
    is_bottom: mask.is_bottom(),
  });

  let type_id = {
    #[cfg(feature = "typed")]
    {
      inst.meta.type_id.map(|id| id.0.to_string())
    }
    #[cfg(not(feature = "typed"))]
    {
      let _ = inst.meta.type_id;
      None
    }
  };

  let native_layout = {
    #[cfg(feature = "typed")]
    {
      inst
        .meta
        .native_layout
        .map(|layout| format!("0x{:032x}", layout.0))
    }
    #[cfg(not(feature = "typed"))]
    {
      None
    }
  };

  let span = inst.meta.span.map(|span| dump_span(span, source_len));

  let preserve_var_assign = inst.meta.preserve_var_assign.then_some(true);
  let stack_alloc_candidate = inst.meta.stack_alloc_candidate.then_some(true);

  #[cfg(feature = "native-async-ops")]
  let await_known_resolved = inst.meta.await_known_resolved.then_some(true);

  let nullability_narrowing = inst
    .meta
    .nullability_narrowing
    .map(dump_nullability_narrowing);

  let value = inst.meta.value.as_ref().and_then(dump_value_facts);

  InstDump {
    t: format!("{:?}", inst.t),
    tgts: inst.tgts.clone(),
    args,
    spreads,
    labels,
    bin_op,
    un_op,
    foreign,
    foreign_str,
    unknown,
    meta: InstMetaDump {
      effects: inst.meta.effects.clone(),
      purity: Purity::from_effects(&inst.meta.effects),
      callee_purity: inst.meta.callee_purity,
      ownership: inst.meta.ownership,
      arg_use_modes,
      in_place_hint: inst.meta.in_place_hint,
      result_escape: inst.meta.result_escape,
      range,
      nullability,
      encoding,
      type_id,
      type_summary: inst.meta.type_summary.map(|s| format!("{s:?}")),
      excludes_nullish: inst.meta.excludes_nullish,
      native_layout,
      span,
      preserve_var_assign,
      stack_alloc_candidate,
      #[cfg(feature = "native-async-ops")]
      await_known_resolved,
      await_behavior: inst.meta.await_behavior,
      parallel: inst.meta.parallel,
      nullability_narrowing,
      value,
      layout_id: None,
      hir_expr: inst.meta.hir_expr.map(|id| id.0),
    },
  }
}

fn dump_cfg(cfg: &Cfg, analyses: &analysis::driver::FunctionAnalyses, source_len: u32) -> CfgDump {
  let entry = cfg.entry;
  let bblock_order = cfg.graph.calculate_postorder(cfg.entry).0;

  let mut cfg_edges = BTreeMap::new();
  for label in cfg.graph.labels_sorted() {
    let children = cfg.graph.children_sorted(label);
    if !children.is_empty() {
      cfg_edges.insert(label, children);
    }
  }

  let mut labels: Vec<u32> = cfg.bblocks.all().map(|(label, _)| label).collect();
  labels.sort_unstable();

  let mut bblocks = BTreeMap::new();
  for label in labels {
    let insts = cfg.bblocks.get(label);
    let inst_len = insts.len();

    let mut range_facts: Vec<Option<analysis::range::IntRange>> = vec![None; inst_len];
    analyses
      .range
      .visit_states_after_each_inst_in_block(cfg, label, |idx, inst, state| {
        let Some(&tgt) = inst.tgts.get(0) else {
          return;
        };
        range_facts[idx] = Some(state.range_of_var(tgt));
      });

    let mut nullability_facts: Vec<Option<analysis::nullability::NullabilityMask>> =
      vec![None; inst_len];
    analyses.nullability.visit_states_after_each_inst_in_block(
      cfg,
      label,
      |idx, inst, state| {
        let Some(&tgt) = inst.tgts.get(0) else {
          return;
        };
        nullability_facts[idx] = Some(state.mask_of_var(tgt));
      },
    );

    let mut encoding_facts: Vec<Option<StringEncoding>> = vec![None; inst_len];
    analyses
      .encoding
      .visit_states_after_each_inst_in_block(cfg, label, |idx, inst, state| {
        let Some(&tgt) = inst.tgts.get(0) else {
          return;
        };
        let enc = state
          .get(tgt as usize)
          .copied()
          .unwrap_or(StringEncoding::Unknown);
        encoding_facts[idx] = Some(enc);
      });

    let dumped = insts
      .iter()
      .enumerate()
      .map(|(idx, inst)| {
        dump_inst(
          inst,
          range_facts[idx],
          nullability_facts[idx],
          encoding_facts[idx],
          source_len,
        )
      })
      .collect::<Vec<_>>();
    bblocks.insert(label, dumped);
  }

  CfgDump {
    entry,
    bblock_order,
    bblocks,
    cfg_edges,
  }
}

/// A serde-friendly snapshot of [`crate::ProgramSymbols`].
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct ProgramSymbolsDump {
  pub symbols: Vec<ProgramSymbolDump>,
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  pub free_symbols: Option<ProgramFreeSymbolsDump>,
  #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "Vec::is_empty"))]
  pub names: Vec<String>,
  #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "Vec::is_empty"))]
  pub scopes: Vec<ProgramScopeDump>,
}

#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct ProgramSymbolDump {
  pub id: u64,
  pub name: String,
  pub scope: u64,
  pub captured: bool,
}

#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct ProgramFreeSymbolsDump {
  pub top_level: Vec<u64>,
  pub functions: Vec<Vec<u64>>,
}

#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct ProgramScopeDump {
  pub id: u64,
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  pub parent: Option<u64>,
  pub kind: String,
  #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "Vec::is_empty"))]
  pub symbols: Vec<u64>,
  #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "Vec::is_empty"))]
  pub children: Vec<u64>,
  #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "Vec::is_empty"))]
  pub tdz_bindings: Vec<u64>,
  pub is_dynamic: bool,
  pub has_direct_eval: bool,
}

fn dump_symbol_id(id: SymbolId) -> u64 {
  id.raw_id()
}

fn dump_scope_id(id: ScopeId) -> u64 {
  id.raw_id()
}

fn dump_scope_kind(kind: &crate::ProgramScopeKind) -> &'static str {
  match kind {
    crate::ProgramScopeKind::Global => "global",
    crate::ProgramScopeKind::Module => "module",
    crate::ProgramScopeKind::Class => "class",
    crate::ProgramScopeKind::StaticBlock => "static_block",
    crate::ProgramScopeKind::NonArrowFunction => "non_arrow_function",
    crate::ProgramScopeKind::ArrowFunction => "arrow_function",
    crate::ProgramScopeKind::Block => "block",
    crate::ProgramScopeKind::FunctionExpressionName => "function_expression_name",
  }
}

fn dump_symbols(symbols: &crate::ProgramSymbols) -> ProgramSymbolsDump {
  let mut out_symbols = symbols
    .symbols
    .iter()
    .map(|sym| ProgramSymbolDump {
      id: dump_symbol_id(sym.id),
      name: sym.name.clone(),
      scope: dump_scope_id(sym.scope),
      captured: sym.captured,
    })
    .collect::<Vec<_>>();

  // Deterministic order (matches `optimize-js-debugger` conventions).
  out_symbols.sort_by(|a, b| {
    (a.scope, &a.name, a.id, a.captured)
      .cmp(&(b.scope, &b.name, b.id, b.captured))
  });

  let free_symbols = symbols.free_symbols.as_ref().map(|free| ProgramFreeSymbolsDump {
    top_level: free.top_level.iter().copied().map(dump_symbol_id).collect(),
    functions: free
      .functions
      .iter()
      .map(|func| func.iter().copied().map(dump_symbol_id).collect())
      .collect(),
  });

  let mut scopes = symbols
    .scopes
    .iter()
    .map(|scope| ProgramScopeDump {
      id: dump_scope_id(scope.id),
      parent: scope.parent.map(dump_scope_id),
      kind: dump_scope_kind(&scope.kind).to_string(),
      symbols: scope.symbols.iter().copied().map(dump_symbol_id).collect(),
      children: scope.children.iter().copied().map(dump_scope_id).collect(),
      tdz_bindings: scope.tdz_bindings.iter().copied().map(dump_symbol_id).collect(),
      is_dynamic: scope.is_dynamic,
      has_direct_eval: scope.has_direct_eval,
    })
    .collect::<Vec<_>>();
  scopes.sort_by_key(|scope| scope.id);

  ProgramSymbolsDump {
    symbols: out_symbols,
    free_symbols,
    names: symbols.names.clone(),
    scopes,
  }
}

/// Serde-friendly, deterministic form of [`analysis::driver::ProgramAnalyses`].
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct ProgramAnalysesDump {
  pub effects_summary: FunctionAnalysesMap<EffectSet>,
  pub purity: FunctionAnalysesMap<Purity>,
  pub escape: FunctionAnalysesMap<analysis::escape::EscapeResult>,
  pub ownership: FunctionAnalysesMap<analysis::ownership::OwnershipResult>,
  pub range: FunctionAnalysesMap<RangeAnalysisDump>,
  pub nullability: FunctionAnalysesMap<NullabilityAnalysisDump>,
  pub encoding: FunctionAnalysesMap<analysis::encoding::EncodingResult>,
}

#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct FunctionAnalysesMap<T> {
  pub top_level: T,
  pub functions: Vec<T>,
}

/// Stable dump form for an edge-sensitive dataflow result.
///
/// `ForwardEdgeDataFlowResult` stores `edge_out` in a `HashMap<(u32, u32), T>`.
/// Serializing that directly to JSON fails because JSON map keys must be strings
/// (serde_json rejects tuple keys with "key must be a string").
///
/// We instead dump edge states as an ordered list.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
#[cfg_attr(feature = "serde", serde(bound(serialize = "T: serde::Serialize")))]
pub struct EdgeStateDump<T> {
  pub from: u32,
  pub to: u32,
  pub state: T,
}

#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct RangeStateDump {
  pub reachable: bool,
  pub ranges: Vec<analysis::range::IntRange>,
}

#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct RangeAnalysisDump {
  pub entry: u32,
  pub var_count: usize,
  pub block_entry: BTreeMap<u32, RangeStateDump>,
  pub block_exit: BTreeMap<u32, RangeStateDump>,
  pub edge_out: Vec<EdgeStateDump<RangeStateDump>>,
}

#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct NullabilityStateDump {
  pub reachable: bool,
  pub masks: Vec<analysis::nullability::NullabilityMask>,
}

#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct NullabilityAnalysisDump {
  pub entry: u32,
  pub var_count: usize,
  pub block_entry: BTreeMap<u32, NullabilityStateDump>,
  pub block_exit: BTreeMap<u32, NullabilityStateDump>,
  pub edge_out: Vec<EdgeStateDump<NullabilityStateDump>>,
}

fn cfg_labels_sorted(cfg: &Cfg) -> Vec<u32> {
  let mut labels = cfg.graph.labels_sorted();
  labels.extend(cfg.bblocks.all().map(|(label, _)| label));
  labels.push(cfg.entry);
  labels.sort_unstable();
  labels.dedup();
  labels
}

fn cfg_var_count(cfg: &Cfg) -> usize {
  let mut max: Option<u32> = None;
  for (_, block) in cfg.bblocks.all() {
    for inst in block.iter() {
      for &tgt in inst.tgts.iter() {
        max = Some(max.map_or(tgt, |m| m.max(tgt)));
      }
      for arg in inst.args.iter() {
        if let Arg::Var(v) = arg {
          max = Some(max.map_or(*v, |m| m.max(*v)));
        }
      }
    }
  }
  max.map(|m| m as usize + 1).unwrap_or(0)
}

fn dump_range_state(var_count: usize, state: Option<&analysis::range::State>) -> RangeStateDump {
  let Some(state) = state else {
    return RangeStateDump {
      reachable: false,
      ranges: vec![analysis::range::IntRange::Bottom; var_count],
    };
  };

  RangeStateDump {
    reachable: state.is_reachable(),
    ranges: (0..var_count)
      .map(|v| state.range_of_var(v as u32))
      .collect(),
  }
}

fn dump_range_analysis(cfg: &Cfg, result: &analysis::range::RangeResult) -> RangeAnalysisDump {
  let var_count = cfg_var_count(cfg);
  let labels = cfg_labels_sorted(cfg);

  let mut block_entry = BTreeMap::new();
  let mut block_exit = BTreeMap::new();
  for label in labels.iter().copied() {
    block_entry.insert(label, dump_range_state(var_count, result.entry(label)));
    block_exit.insert(label, dump_range_state(var_count, result.exit(label)));
  }

  let mut edge_out = Vec::new();
  for from in labels.iter().copied() {
    for to in cfg.graph.children_sorted(from) {
      edge_out.push(EdgeStateDump {
        from,
        to,
        state: dump_range_state(var_count, result.edge_state(from, to)),
      });
    }
  }
  edge_out.sort_by_key(|e| (e.from, e.to));

  RangeAnalysisDump {
    entry: cfg.entry,
    var_count,
    block_entry,
    block_exit,
    edge_out,
  }
}

fn dump_nullability_state(
  var_count: usize,
  state: Option<&analysis::nullability::State>,
) -> NullabilityStateDump {
  let Some(state) = state else {
    return NullabilityStateDump {
      reachable: false,
      masks: vec![analysis::nullability::NullabilityMask::BOTTOM; var_count],
    };
  };

  NullabilityStateDump {
    reachable: state.is_reachable(),
    masks: (0..var_count).map(|v| state.mask_of_var(v as u32)).collect(),
  }
}

fn dump_nullability_analysis(
  cfg: &Cfg,
  result: &analysis::nullability::NullabilityResult,
) -> NullabilityAnalysisDump {
  let var_count = cfg_var_count(cfg);
  let labels = cfg_labels_sorted(cfg);

  let mut block_entry = BTreeMap::new();
  let mut block_exit = BTreeMap::new();
  for label in labels.iter().copied() {
    block_entry.insert(
      label,
      dump_nullability_state(var_count, result.state_at_block_entry(label)),
    );
    block_exit.insert(
      label,
      dump_nullability_state(var_count, result.state_at_block_exit(label)),
    );
  }

  let mut edge_out = Vec::new();
  for from in labels.iter().copied() {
    for to in cfg.graph.children_sorted(from) {
      edge_out.push(EdgeStateDump {
        from,
        to,
        state: dump_nullability_state(
          var_count,
          result.state_at_edge_entry(analysis::facts::Edge { from, to }),
        ),
      });
    }
  }
  edge_out.sort_by_key(|e| (e.from, e.to));

  NullabilityAnalysisDump {
    entry: cfg.entry,
    var_count,
    block_entry,
    block_exit,
    edge_out,
  }
}

fn dump_program_analyses(program: &Program) -> ProgramAnalysesDump {
  use analysis::driver::{analyze_program, FunctionKey};
  let analyses = analyze_program(program);

  let fn_count = program.functions.len();

  ProgramAnalysesDump {
    effects_summary: FunctionAnalysesMap {
      top_level: analyses
        .effects_summary
        .get(&FunctionKey::TopLevel)
        .cloned()
        .unwrap_or_default(),
      functions: (0..fn_count)
        .map(|id| {
          analyses
            .effects_summary
            .get(&FunctionKey::Fn(id))
            .cloned()
            .unwrap_or_default()
        })
        .collect(),
    },
    purity: FunctionAnalysesMap {
      top_level: analyses
        .purity
        .get(&FunctionKey::TopLevel)
        .copied()
        .unwrap_or_default(),
      functions: (0..fn_count)
        .map(|id| {
          analyses
            .purity
            .get(&FunctionKey::Fn(id))
            .copied()
            .unwrap_or_default()
        })
        .collect(),
    },
    escape: FunctionAnalysesMap {
      top_level: analyses
        .escape
        .get(&FunctionKey::TopLevel)
        .cloned()
        .unwrap_or_default(),
      functions: (0..fn_count)
        .map(|id| {
          analyses
            .escape
            .get(&FunctionKey::Fn(id))
            .cloned()
            .unwrap_or_default()
        })
        .collect(),
    },
    ownership: FunctionAnalysesMap {
      top_level: analyses
        .ownership
        .get(&FunctionKey::TopLevel)
        .cloned()
        .unwrap_or_default(),
      functions: (0..fn_count)
        .map(|id| {
          analyses
            .ownership
            .get(&FunctionKey::Fn(id))
            .cloned()
            .unwrap_or_default()
        })
        .collect(),
    },
    range: FunctionAnalysesMap {
      top_level: dump_range_analysis(
        program.top_level.analyzed_cfg(),
        analyses
          .range
          .get(&FunctionKey::TopLevel)
          .expect("missing range results for top-level"),
      ),
      functions: (0..fn_count)
        .map(|id| {
          dump_range_analysis(
            program.functions[id].analyzed_cfg(),
            analyses
              .range
              .get(&FunctionKey::Fn(id))
              .expect("missing range results for function"),
          )
        })
        .collect(),
    },
    nullability: FunctionAnalysesMap {
      top_level: dump_nullability_analysis(
        program.top_level.analyzed_cfg(),
        analyses
          .nullability
          .get(&FunctionKey::TopLevel)
          .expect("missing nullability results for top-level"),
      ),
      functions: (0..fn_count)
        .map(|id| {
          dump_nullability_analysis(
            program.functions[id].analyzed_cfg(),
            analyses
              .nullability
              .get(&FunctionKey::Fn(id))
              .expect("missing nullability results for function"),
          )
        })
        .collect(),
    },
    encoding: FunctionAnalysesMap {
      top_level: analyses
        .encoding
        .get(&FunctionKey::TopLevel)
        .cloned()
        .unwrap_or_else(|| analysis::encoding::analyze_cfg_encoding(program.top_level.analyzed_cfg())),
      functions: (0..fn_count)
        .map(|id| {
          analyses
            .encoding
            .get(&FunctionKey::Fn(id))
            .expect("missing encoding results for function")
            .clone()
        })
        .collect(),
    },
  }
}

/// Build a deterministic dump of the given [`Program`].
pub fn dump_program(program: &Program, opts: DumpOptions) -> ProgramDump {
  let top_level_cfg = program.top_level.analyzed_cfg();
  let top_level_analyses = analyze_cfg(top_level_cfg);
  let top_level_deconstructed_cfg = &program.top_level.body;
  let top_level_deconstructed_analyses = analyze_cfg(top_level_deconstructed_cfg);

  let top_level = FunctionDump {
    id: None,
    params: program.top_level.params.clone(),
    cfg: dump_cfg(top_level_cfg, &top_level_analyses, program.source_len),
    cfg_deconstructed: Some(dump_cfg(
      top_level_deconstructed_cfg,
      &top_level_deconstructed_analyses,
      program.source_len,
    )),
  };

  let mut functions = Vec::with_capacity(program.functions.len());
  for (idx, func) in program.functions.iter().enumerate() {
    let cfg = func.analyzed_cfg();
    let analyses = analyze_cfg(cfg);
    let deconstructed_cfg = &func.body;
    let deconstructed_analyses = analyze_cfg(deconstructed_cfg);
    functions.push(FunctionDump {
      id: Some(idx as u32),
      params: func.params.clone(),
      cfg: dump_cfg(cfg, &analyses, program.source_len),
      cfg_deconstructed: Some(dump_cfg(
        deconstructed_cfg,
        &deconstructed_analyses,
        program.source_len,
      )),
    });
  }

  let symbols = opts
    .include_symbols
    .then(|| program.symbols.as_ref().map(dump_symbols))
    .flatten();

  let analyses = opts.include_analyses.then(|| dump_program_analyses(program));

  ProgramDump {
    version: DUMP_VERSION,
    source_mode: program.top_level_mode.into(),
    top_level,
    functions,
    symbols,
    analyses,
  }
}
