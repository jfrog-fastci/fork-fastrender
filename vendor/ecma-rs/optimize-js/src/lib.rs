//! SSA-based JavaScript optimizer and decompiler.
//!
//! The optimizer lowers `parse-js` source into `hir-js`, builds an SSA-based
//! intermediate representation, runs optimization passes, and can decompile the
//! result back to a `parse-js` AST or emitted JavaScript.
//!
//! The public entry point for one-shot compilation is [`compile_source`]. Use
//! [`compile_source_with_cfg_options`] with [`CompileCfgOptions`] to opt into
//! retaining SSA form (including `Phi`) in the returned CFGs for downstream
//! analyses/backends.
//!
//! See [`program_to_js`] / [`program_to_ast`] for decompilation. Note that the
//! decompiler expects non-SSA CFGs, so callers must disable `keep_ssa` (default)
//! or deconstruct SSA before decompiling.
//!
//! # Example
//! ```no_run
//! use optimize_js::{compile_source, program_to_js, DecompileOptions, TopLevelMode};
//!
//! let program = compile_source("let x = 1;", TopLevelMode::Module, false).unwrap();
//! let bytes = program_to_js(
//!   &program,
//!   &DecompileOptions::default(),
//!   emit_js::EmitOptions::minified(),
//! )
//! .unwrap();
//! println!("{}", String::from_utf8_lossy(&bytes));
//! ```
//!
//! # Runnable example
//!
//! ```bash
//! bash scripts/cargo_agent.sh run -p optimize-js --example basic
//! ```

pub mod analysis;
pub mod cfg;
pub mod decompile;
pub mod dom;
pub mod eval;
pub mod graph;
pub mod il;
pub mod opt;
pub mod ssa;
pub mod symbol;
pub mod types;
pub mod util;

pub use crate::decompile::program_to_js;
pub use crate::decompile::ProgramToJsError;
pub use crate::decompile::{program_to_ast, DecompileOptions};
use crate::il::inst::Inst;
use crate::util::counter::Counter;
use ahash::HashMap;
use ahash::HashSet;
use analysis::defs::calculate_defs;
use cfg::bblock::convert_insts_to_bblocks;
use cfg::cfg::Cfg;
use dashmap::DashMap;
use dom::Dom;
use hir_js::Body;
use hir_js::BodyId;
use hir_js::DefId;
use hir_js::ExprId;
use hir_js::FileKind as HirFileKind;
use hir_js::LowerResult;
use hir_js::NameId;
use hir_js::NameInterner;
use hir_js::PatId;
use opt::optpass_cfg_prune::optpass_cfg_prune;
use opt::optpass_dvn::optpass_dvn;
use opt::optpass_impossible_branches::optpass_impossible_branches;
use opt::optpass_redundant_assigns::optpass_redundant_assigns;
use opt::optpass_trivial_dce::optpass_trivial_dce;
use opt::PassResult;
use parse_js::ast::node::Node;
use parse_js::ast::node::NodeAssocData;
use parse_js::ast::stx::TopLevel;
use parse_js::loc::Loc;
use parse_js::{parse_with_options, Dialect, ParseOptions, SourceType};
use semantic_js::js::ScopeKind;
pub use semantic_js::js::TopLevelMode;
use ssa::ssa_deconstruct::deconstruct_ssa;
use ssa::ssa_insert_phis::insert_phis_for_ssa_construction;
use ssa::ssa_rename::rename_targets_for_ssa_construction;
use std::collections::BTreeMap;
use std::ops::Deref;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use symbol::semantics::{
  assoc_declared_symbol, assoc_resolved_symbol, JsSymbols, ScopeId, SymbolId,
};
use symbol::var_analysis::VarAnalysis;
use util::debug::OptimizerDebug;

pub use diagnostics::{Diagnostic, FileId, Span, TextRange};

const SOURCE_FILE: FileId = FileId(0);

pub type OptimizeResult<T> = Result<T, Vec<Diagnostic>>;

/// Options controlling the CFG/IL pipeline during compilation.
///
/// The default behaviour matches the existing `compile_source` pipeline: build
/// SSA, run optimisation passes, then deconstruct SSA back into a non-SSA CFG
/// stored in [`ProgramFunction::body`]. The SSA form is still preserved in
/// [`ProgramFunction::ssa_body`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompileCfgOptions {
  /// Retain SSA form (including `InstTyp::Phi`) in [`ProgramFunction::body`].
  ///
  /// When this is `false` (default), SSA is always deconstructed before it is
  /// stored in the returned program's `body`. The preserved SSA CFG (annotated
  /// with ownership/escape metadata) is still available via
  /// [`ProgramFunction::ssa_body`]/[`ProgramFunction::cfg_ssa`].
  pub keep_ssa: bool,
  /// Run optimisation passes after SSA construction.
  ///
  /// This is enabled by default. Turning it off can be useful for experimenting
  /// with downstream backends/analyses on the unoptimised SSA graph.
  pub run_opt_passes: bool,
}

impl Default for CompileCfgOptions {
  fn default() -> Self {
    Self {
      keep_ssa: false,
      run_opt_passes: true,
    }
  }
}

fn parse_source(source: &str, file: FileId, mode: TopLevelMode) -> OptimizeResult<Node<TopLevel>> {
  let source_type = match mode {
    TopLevelMode::Module => SourceType::Module,
    TopLevelMode::Global | TopLevelMode::Script => SourceType::Script,
  };
  let opts = ParseOptions {
    // `optimize-js` uses the TypeScript parser dialect by default because it must accept TS syntax
    // for typed pipelines (and most TS syntax is also valid JS).
    dialect: Dialect::Ts,
    source_type,
  };
  parse_with_options(source, opts).map_err(|err| vec![err.to_diagnostic(file)])
}

fn diagnostic_with_span(
  file: FileId,
  code: &'static str,
  message: impl Into<String>,
  loc: Loc,
) -> Diagnostic {
  let (range, note) = loc.to_diagnostics_range_with_note();
  let mut diagnostic = Diagnostic::error(code, message, Span::new(file, range));
  if let Some(note) = note {
    diagnostic = diagnostic.with_note(note);
  }
  diagnostic
}

fn unsupported_syntax(file: FileId, loc: Loc, message: impl Into<String>) -> Vec<Diagnostic> {
  vec![diagnostic_with_span(file, "OPT0002", message, loc)]
}

fn diagnostic_with_range(
  file: FileId,
  code: &'static str,
  message: impl Into<String>,
  range: TextRange,
) -> Diagnostic {
  Diagnostic::error(code, message, Span::new(file, range))
}

fn unsupported_syntax_range(
  file: FileId,
  range: TextRange,
  message: impl Into<String>,
) -> Vec<Diagnostic> {
  vec![diagnostic_with_range(file, "OPT0002", message, range)]
}

fn use_before_declaration(file: FileId, name: &str, loc: Loc) -> Diagnostic {
  diagnostic_with_span(
    file,
    "OPT0001",
    format!("use of `{name}` before declaration"),
    loc,
  )
}

fn sort_diagnostics(diagnostics: &mut [Diagnostic]) {
  diagnostics.sort_by(|a, b| {
    a.primary
      .file
      .cmp(&b.primary.file)
      .then(a.primary.range.start.cmp(&b.primary.range.start))
      .then(a.primary.range.end.cmp(&b.primary.range.end))
      .then(a.code.cmp(&b.code))
      .then(a.message.cmp(&b.message))
  });
}

// The top level is considered a function (the optimizer concept, not parser or symbolizer).
#[derive(Clone, Copy, Debug, Default)]
pub struct OptimizationStats {
  /// Number of times dominance was recomputed for this function, including SSA construction.
  pub dom_calculations: usize,
  /// Number of iterations through the optimization fixpoint loop.
  pub fixpoint_iterations: usize,
}

impl OptimizationStats {
  fn record_dom_calculation(&mut self) {
    self.dom_calculations += 1;
  }

  fn record_iteration(&mut self) {
    self.fixpoint_iterations += 1;
  }
}

#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct ProgramFunction {
  pub debug: Option<OptimizerDebug>,
  pub body: Cfg,
  #[cfg_attr(
    feature = "serde",
    serde(default, skip_serializing_if = "Vec::is_empty")
  )]
  pub params: Vec<u32>,
  /// SSA-form CFG annotated with escape/ownership/consumption metadata.
  ///
  /// This is produced after SSA construction and optimization passes converge, and
  /// before SSA deconstruction. Native backends should prefer this over
  /// [`ProgramFunction::body`] when consuming ownership/escape/consumption metadata.
  #[cfg_attr(feature = "serde", serde(skip_serializing))]
  pub ssa_body: Option<Cfg>,
  #[cfg_attr(feature = "serde", serde(skip_serializing))]
  pub stats: OptimizationStats,
}

impl ProgramFunction {
  pub fn param_index_of(&self, var: u32) -> Option<usize> {
    self.params.iter().position(|&param| param == var)
  }

  /// Returns the CFG annotated with escape/ownership metadata when available.
  ///
  /// Native backends should prefer `analyzed_cfg()` to access ownership/escape metadata.
  pub fn analyzed_cfg(&self) -> &Cfg {
    self.ssa_body.as_ref().unwrap_or(&self.body)
  }

  /// Returns the SSA-form CFG (with phi nodes) when available.
  pub fn cfg_ssa(&self) -> Option<&Cfg> {
    self.ssa_body.as_ref()
  }

  /// Returns the primary CFG stored on this function (usually SSA-deconstructed).
  ///
  /// In the default compilation pipeline (`CompileCfgOptions::keep_ssa == false`),
  /// [`ProgramFunction::body`] is always SSA-deconstructed (no phi nodes). When
  /// `keep_ssa == true`, the body is in SSA form and may still contain phi nodes.
  pub fn cfg_deconstructed(&self) -> &Cfg {
    &self.body
  }
}

#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct ProgramSymbol {
  pub id: SymbolId,
  pub name: String,
  pub scope: ScopeId,
  pub captured: bool,
}

#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct ProgramFreeSymbols {
  pub top_level: Vec<SymbolId>,
  pub functions: Vec<Vec<SymbolId>>, // Index aligned with Program::functions.
}

#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum ProgramScopeKind {
  Global,
  Module,
  Class,
  StaticBlock,
  NonArrowFunction,
  ArrowFunction,
  Block,
  FunctionExpressionName,
}

impl From<ScopeKind> for ProgramScopeKind {
  fn from(kind: ScopeKind) -> Self {
    match kind {
      ScopeKind::Global => ProgramScopeKind::Global,
      ScopeKind::Module => ProgramScopeKind::Module,
      ScopeKind::Class => ProgramScopeKind::Class,
      ScopeKind::StaticBlock => ProgramScopeKind::StaticBlock,
      ScopeKind::NonArrowFunction => ProgramScopeKind::NonArrowFunction,
      ScopeKind::ArrowFunction => ProgramScopeKind::ArrowFunction,
      ScopeKind::Block => ProgramScopeKind::Block,
      ScopeKind::FunctionExpressionName => ProgramScopeKind::FunctionExpressionName,
    }
  }
}

#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct ProgramScope {
  pub id: ScopeId,
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  pub parent: Option<ScopeId>,
  pub kind: ProgramScopeKind,
  #[cfg_attr(
    feature = "serde",
    serde(default, skip_serializing_if = "Vec::is_empty")
  )]
  pub symbols: Vec<SymbolId>,
  #[cfg_attr(
    feature = "serde",
    serde(default, skip_serializing_if = "Vec::is_empty")
  )]
  pub children: Vec<ScopeId>,
  #[cfg_attr(
    feature = "serde",
    serde(default, skip_serializing_if = "Vec::is_empty")
  )]
  pub tdz_bindings: Vec<SymbolId>,
  pub is_dynamic: bool,
  pub has_direct_eval: bool,
}

#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct ProgramSymbols {
  pub symbols: Vec<ProgramSymbol>,
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  pub free_symbols: Option<ProgramFreeSymbols>,
  #[cfg_attr(
    feature = "serde",
    serde(default, skip_serializing_if = "Vec::is_empty")
  )]
  pub names: Vec<String>,
  #[cfg_attr(
    feature = "serde",
    serde(default, skip_serializing_if = "Vec::is_empty")
  )]
  pub scopes: Vec<ProgramScope>,
}

#[derive(Clone, Default, Debug)]
pub(crate) struct HirSymbolBindings {
  exprs: HashMap<BodyId, HashMap<ExprId, Option<SymbolId>>>,
  pats: HashMap<BodyId, HashMap<PatId, Option<SymbolId>>>,
  defs: HashMap<DefId, Option<SymbolId>>,
}

impl HirSymbolBindings {
  fn symbol_for_expr(&self, body: BodyId, expr: ExprId) -> Option<SymbolId> {
    self
      .exprs
      .get(&body)
      .and_then(|m| m.get(&expr))
      .and_then(|s| *s)
  }

  fn symbol_for_pat(&self, body: BodyId, pat: PatId) -> Option<SymbolId> {
    self
      .pats
      .get(&body)
      .and_then(|m| m.get(&pat))
      .and_then(|s| *s)
  }

  fn symbol_for_def(&self, def: DefId) -> Option<SymbolId> {
    self.defs.get(&def).and_then(|s| *s)
  }
}

fn collect_hir_symbol_bindings(ast: &mut Node<TopLevel>, lower: &LowerResult) -> HirSymbolBindings {
  use derive_visitor::{DriveMut, VisitorMut};
  use parse_js::ast::expr::pat::ClassOrFuncName;
  use parse_js::ast::expr::pat::IdPat;
  use parse_js::ast::expr::IdExpr;

  #[derive(VisitorMut)]
  #[visitor(IdExprNode(enter), IdPatNode(enter), ClassOrFuncNameNode(enter))]
  struct Collector<'a> {
    span_map: &'a hir_js::span_map::SpanMap,
    expr_spans: BTreeMap<TextRange, Option<SymbolId>>,
    pat_spans: BTreeMap<TextRange, Option<SymbolId>>,
    def_spans: BTreeMap<TextRange, Option<SymbolId>>,
  }

  type IdExprNode = Node<IdExpr>;
  type IdPatNode = Node<IdPat>;
  type ClassOrFuncNameNode = Node<ClassOrFuncName>;

  impl<'a> Collector<'a> {
    fn map_symbol(&self, assoc: &NodeAssocData) -> Option<SymbolId> {
      assoc_declared_symbol(assoc).or_else(|| assoc_resolved_symbol(assoc))
    }

    fn offsets(span: TextRange) -> impl Iterator<Item = u32> {
      let mut offsets = vec![span.start];
      if span.end > span.start {
        let mid = span.start + (span.end - span.start) / 2;
        let end = span.end.saturating_sub(1);
        offsets.push(mid);
        offsets.push(end);
      }
      offsets.sort_unstable();
      offsets.dedup();
      offsets.into_iter()
    }

    fn expr_for_span(&self, span: TextRange) -> Option<(BodyId, ExprId)> {
      for offset in Self::offsets(span) {
        if let Some(span) = self.span_map.expr_span_at_offset(offset) {
          return Some(span.id);
        }
      }
      None
    }

    fn pat_for_span(&self, span: TextRange) -> Option<(BodyId, PatId)> {
      for offset in Self::offsets(span) {
        if let Some(span) = self.span_map.pat_span_at_offset(offset) {
          return Some(span.id);
        }
      }
      None
    }

    fn def_for_span(&self, span: TextRange) -> Option<DefId> {
      for offset in Self::offsets(span) {
        if let Some(span) = self.span_map.def_span_at_offset(offset) {
          return Some(span.id);
        }
      }
      None
    }

    fn record_expr(&mut self, span: TextRange, sym: Option<SymbolId>) {
      self.expr_spans.insert(span, sym);
      self.pat_spans.insert(span, sym);
    }

    fn record_pat(&mut self, span: TextRange, sym: Option<SymbolId>) {
      self.pat_spans.insert(span, sym);
    }

    fn record_def(&mut self, span: TextRange, sym: Option<SymbolId>) {
      self.def_spans.insert(span, sym);
    }

    fn enter_id_expr_node(&mut self, node: &mut IdExprNode) {
      let span = TextRange::new(node.loc.start_u32(), node.loc.end_u32());
      let sym = self.map_symbol(&node.assoc);
      self.record_expr(span, sym);
    }

    fn enter_id_pat_node(&mut self, node: &mut IdPatNode) {
      let span = TextRange::new(node.loc.start_u32(), node.loc.end_u32());
      let sym = self.map_symbol(&node.assoc);
      self.record_pat(span, sym);
    }

    fn enter_class_or_func_name_node(&mut self, node: &mut ClassOrFuncNameNode) {
      let span = TextRange::new(node.loc.start_u32(), node.loc.end_u32());
      let sym = self.map_symbol(&node.assoc);
      self.record_def(span, sym);
    }
  }

  let span_map = &lower.hir.span_map;
  let mut bindings = HirSymbolBindings::default();
  let mut collector = Collector {
    span_map,
    expr_spans: BTreeMap::new(),
    pat_spans: BTreeMap::new(),
    def_spans: BTreeMap::new(),
  };
  ast.drive_mut(&mut collector);

  for (span, sym) in collector.expr_spans.iter() {
    if let Some((body, expr_id)) = collector.expr_for_span(*span) {
      bindings
        .exprs
        .entry(body)
        .or_default()
        .insert(expr_id, *sym);
    }
  }
  for (span, sym) in collector.pat_spans.iter() {
    if let Some((body, pat_id)) = collector.pat_for_span(*span) {
      bindings.pats.entry(body).or_default().insert(pat_id, *sym);
    }
  }
  for (span, sym) in collector.def_spans.iter() {
    if let Some(def_id) = collector.def_for_span(*span) {
      // Prefer a concrete symbol id when multiple spans map to the same def.
      bindings
        .defs
        .entry(def_id)
        .and_modify(|existing| {
          if existing.is_none() {
            *existing = *sym;
          }
        })
        .or_insert(*sym);
    }
  }

  for (body_id, idx) in &lower.body_index {
    let body = &lower.bodies[*idx];
    let exprs = bindings.exprs.entry(*body_id).or_default();
    for (idx, expr) in body.exprs.iter().enumerate() {
      let id = ExprId(idx as u32);
      exprs.entry(id).or_insert_with(|| {
        collector
          .expr_spans
          .get(&expr.span)
          .copied()
          .unwrap_or(None)
      });
    }
    let pats = bindings.pats.entry(*body_id).or_default();
    for (idx, pat) in body.pats.iter().enumerate() {
      let id = PatId(idx as u32);
      pats
        .entry(id)
        .or_insert_with(|| collector.pat_spans.get(&pat.span).copied().unwrap_or(None));
    }
  }

  bindings
}

struct DomCache {
  dom: Option<Dom>,
  dirty: bool,
}

impl DomCache {
  fn new() -> Self {
    Self {
      dom: None,
      dirty: true,
    }
  }

  fn ensure<'a>(&'a mut self, cfg: &Cfg, stats: &mut OptimizationStats) -> &'a Dom {
    if self.dirty || self.dom.is_none() {
      self.dom = Some(Dom::calculate(cfg));
      self.dirty = false;
      stats.record_dom_calculation();
    }
    self.dom.as_ref().unwrap()
  }

  fn maybe_invalidate(&mut self, result: &PassResult) {
    if result.cfg_changed {
      self.dirty = true;
    }
  }
}

pub(crate) fn build_program_function(
  program: &ProgramCompiler,
  insts: Vec<Inst>,
  c_label: Counter,
  c_temp: Counter,
  params: Vec<u32>,
) -> ProgramFunction {
  build_program_function_with_options(
    program,
    insts,
    c_label,
    c_temp,
    params,
    CompileCfgOptions::default(),
  )
}

pub(crate) fn build_program_function_with_options(
  program: &ProgramCompiler,
  insts: Vec<Inst>,
  mut c_label: Counter,
  mut c_temp: Counter,
  params: Vec<u32>,
  options: CompileCfgOptions,
) -> ProgramFunction {
  let mut dbg = program.debug.then(|| OptimizerDebug::new());
  let mut dbg_checkpoint = |name: &str, cfg: &Cfg| {
    dbg.as_mut().map(|dbg| dbg.add_step(name, cfg));
  };

  let (bblocks, bblock_order) = convert_insts_to_bblocks(insts, &mut c_label);
  let mut cfg = Cfg::from_bblocks(bblocks, bblock_order);
  // Prune unreachable blocks from 0. This is necessary for dominance calculation to be correct (basic example: every block should be dominated by 0, but if there's an unreachable block it'll make all its descendants not dominated by 0).
  // This can happen due to user code (unreachable code) or by us, because we split after a `goto` which makes the new other-split-half block unreachable (this block is usually empty).
  cfg.find_and_pop_unreachable();

  let mut defs = calculate_defs(&cfg);
  dbg_checkpoint("source", &cfg);

  let mut stats = OptimizationStats::default();
  let mut dom_cache = DomCache::new();

  // Construct SSA.
  let dom = dom_cache.ensure(&cfg, &mut stats);
  insert_phis_for_ssa_construction(&mut defs, &mut cfg, dom);
  dbg_checkpoint("ssa_insert_phis", &cfg);
  rename_targets_for_ssa_construction(&mut cfg, dom, &mut c_temp);
  dbg_checkpoint("ssa_rename_targets", &cfg);

  // Optimisation passes:
  // - Dominator-based value numbering.
  // - Trivial dead code elimination.
  // Drop defs as it likely will be invalid after even one pass.
  drop(defs);
  if options.run_opt_passes {
    for i in 1.. {
      stats.record_iteration();
      let dom = dom_cache.ensure(&cfg, &mut stats);
      let mut iteration_result = PassResult::default();

      iteration_result.merge(optpass_dvn(&mut cfg, dom));
      dbg_checkpoint(&format!("opt{}_dvn", i), &cfg);
      iteration_result.merge(optpass_trivial_dce(&mut cfg));
      dbg_checkpoint(&format!("opt{}_dce", i), &cfg);
      // TODO Isn't this really const/copy propagation to child Phi insts?
      iteration_result.merge(optpass_redundant_assigns(&mut cfg));
      dbg_checkpoint(&format!("opt{}_redundant_assigns", i), &cfg);
      let impossible_result = optpass_impossible_branches(&mut cfg);
      dom_cache.maybe_invalidate(&impossible_result);
      iteration_result.merge(impossible_result);
      dbg_checkpoint(&format!("opt{}_impossible_branches", i), &cfg);
      let cfg_prune_result = optpass_cfg_prune(&mut cfg);
      dom_cache.maybe_invalidate(&cfg_prune_result);
      iteration_result.merge(cfg_prune_result);
      dbg_checkpoint(&format!("opt{}_cfg_prune", i), &cfg);

      if !iteration_result.any_change() {
        break;
      }
    }
  }

  // Preserve an SSA-form CFG for downstream consumers (e.g. native codegen backends). We
  // intentionally do not propagate escape/ownership/consumption metadata onto the deconstructed
  // CFG; consumers should use `ProgramFunction::analyzed_cfg()` when they need those results.
  //
  // Escape/ownership/consumption metadata is annotated at the end of compilation using
  // whole-program interprocedural summaries so direct `Arg::Fn` calls can be handled precisely.
  let ssa_cfg = cfg.clone();

  if !options.keep_ssa {
    // It's safe to calculate liveliness before removing Phi insts; after deconstructing, they
    // always lie exactly between all parent bblocks and the head of the bblock, so their
    // lifetimes are identical.
    deconstruct_ssa(&mut cfg, &mut c_label);
    dbg_checkpoint("ssa_deconstruct", &cfg);
  }

  ProgramFunction {
    debug: dbg,
    body: cfg,
    params,
    ssa_body: Some(ssa_cfg),
    stats,
  }
}

fn annotate_ssa_cfg_escape_and_ownership(
  cfg: &mut Cfg,
  params: &[u32],
  summaries: &analysis::interproc_escape::ProgramEscapeSummaries,
  call_summaries: &[analysis::call_summary::FnSummary],
) {
  let escapes = analysis::escape::analyze_cfg_escapes_with_params_and_summaries(
    cfg,
    params,
    Some(summaries),
    Some(call_summaries),
  );
  let ownership = analysis::ownership::analyze_cfg_ownership_with_escapes_and_params_and_summaries(
    cfg,
    params,
    &escapes,
    Some(call_summaries),
  );
  analysis::ownership::annotate_cfg_ownership(cfg, &ownership);
  analysis::consume::annotate_cfg_consumption(cfg, &ownership);
  // Deterministic traversal over bblocks so SSA metadata annotation stays stable.
  let mut labels: Vec<u32> = cfg.bblocks.all().map(|(label, _)| label).collect();
  labels.sort_unstable();
  for label in labels {
    for inst in cfg.bblocks.get_mut(label).iter_mut() {
      let Some(tgt) = inst.tgts.get(0).copied() else {
        continue;
      };
      inst.meta.result_escape = escapes.get(&tgt).copied();
    }
  }
}

fn annotate_program_ssa_metadata(program: &mut Program) {
  let call_summaries = analysis::call_summary::summarize_program(program);
  let summaries = analysis::interproc_escape::compute_program_escape_summaries(program);

  if let Some(cfg) = program.top_level.ssa_body.as_mut() {
    annotate_ssa_cfg_escape_and_ownership(
      cfg,
      &program.top_level.params,
      &summaries,
      &call_summaries,
    );
  }
  for func in program.functions.iter_mut() {
    if let Some(cfg) = func.ssa_body.as_mut() {
      annotate_ssa_cfg_escape_and_ownership(cfg, &func.params, &summaries, &call_summaries);
    }
  }
}

pub(crate) fn compile_hir_body(
  program: &ProgramCompiler,
  body: BodyId,
) -> OptimizeResult<ProgramFunction> {
  let (insts, c_label, c_temp, params) = crate::il::s2i::stmt::translate_body(program, body)?;
  let options = program.cfg_options;
  Ok(if options == CompileCfgOptions::default() {
    build_program_function(program, insts, c_label, c_temp, params)
  } else {
    build_program_function_with_options(program, insts, c_label, c_temp, params, options)
  })
}

pub type FnId = usize;

pub use decompile::structurer::{structure_cfg, BreakTarget, ControlTree, LoopLabel};

#[derive(Debug)]
pub struct ProgramCompilerInner {
  // Precomputed via VarAnalysis.
  pub foreign_vars: HashSet<SymbolId>,
  pub functions: DashMap<FnId, ProgramFunction>,
  pub next_fn_id: AtomicUsize,
  pub debug: bool,
  pub cfg_options: CompileCfgOptions,
  pub lower: Arc<LowerResult>,
  pub(crate) bindings: HirSymbolBindings,
  pub names: Arc<NameInterner>,
  pub(crate) types: crate::types::TypeContext,
}

/// Our internal compiler state for a program.
/// We have a separate struct instead of using the public-facing Program.
/// It means we can use pub instead of pub(crate) fields and methods everywhere.
#[derive(Clone)]
pub struct ProgramCompiler(Arc<ProgramCompilerInner>);

impl Deref for ProgramCompiler {
  type Target = ProgramCompilerInner;

  fn deref(&self) -> &Self::Target {
    &self.0
  }
}

impl ProgramCompiler {
  fn name_for(&self, name: NameId) -> String {
    self
      .names
      .resolve(name)
      .map(str::to_string)
      .unwrap_or_else(|| "<unknown>".to_string())
  }

  fn symbol_for_expr(&self, body: BodyId, expr: ExprId) -> Option<SymbolId> {
    self.bindings.symbol_for_expr(body, expr)
  }

  fn symbol_for_pat(&self, body: BodyId, pat: PatId) -> Option<SymbolId> {
    self.bindings.symbol_for_pat(body, pat)
  }

  fn symbol_for_def(&self, def: DefId) -> Option<SymbolId> {
    self.bindings.symbol_for_def(def)
  }

  fn body(&self, id: BodyId) -> &Body {
    self.lower.body(id).expect("body should exist")
  }
}

#[derive(Debug)]
pub struct Program {
  pub functions: Vec<ProgramFunction>,
  pub top_level: ProgramFunction,
  pub top_level_mode: TopLevelMode,
  pub symbols: Option<ProgramSymbols>,
}

/// Parse, symbolize, and compile source text in one step.
///
/// The provided source must be valid UTF-8; identifier handling and span math
/// operate on UTF-8 byte offsets. Validate and convert any raw byte buffers at
/// the I/O boundary before calling this helper.
pub fn compile_source(source: &str, mode: TopLevelMode, debug: bool) -> OptimizeResult<Program> {
  let top_level_node = parse_source(source, SOURCE_FILE, mode)?;
  Program::compile(top_level_node, mode, debug)
}

/// Parse, symbolize, and compile source text with explicit CFG pipeline options.
pub fn compile_source_with_cfg_options(
  source: &str,
  mode: TopLevelMode,
  debug: bool,
  options: CompileCfgOptions,
) -> OptimizeResult<Program> {
  let top_level_node = parse_source(source, SOURCE_FILE, mode)?;
  Program::compile_with_cfg_options(top_level_node, mode, debug, options)
}

/// Compile source text with optional `typecheck-ts` type information.
///
/// This helper is only available when the crate is built with the `typed`
/// feature. If the provided type program does not contain usable type
/// information for a particular expression, the optimizer falls back to the
/// existing untyped behaviour.
#[cfg(feature = "typed")]
pub fn compile_source_with_typecheck(
  source: &str,
  mode: TopLevelMode,
  debug: bool,
  type_program: std::sync::Arc<typecheck_ts::Program>,
  type_file: typecheck_ts::FileId,
) -> OptimizeResult<Program> {
  compile_source_with_typecheck_cfg_options(
    source,
    mode,
    debug,
    type_program,
    type_file,
    CompileCfgOptions::default(),
  )
}

/// Compile source text with optional `typecheck-ts` type information and explicit CFG options.
///
/// This helper is only available when the crate is built with the `typed`
/// feature. If the provided type program does not contain usable type
/// information for a particular expression, the optimizer falls back to the
/// existing untyped behaviour.
#[cfg(feature = "typed")]
pub fn compile_source_with_typecheck_cfg_options(
  source: &str,
  mode: TopLevelMode,
  debug: bool,
  type_program: std::sync::Arc<typecheck_ts::Program>,
  type_file: typecheck_ts::FileId,
  options: CompileCfgOptions,
) -> OptimizeResult<Program> {
  let matches_file_text = type_program
    .file_text(type_file)
    .map(|text| text.as_ref() == source)
    .unwrap_or(false);

  if matches_file_text {
    return compile_file_with_typecheck_cfg_options(type_program, type_file, mode, debug, options);
  }

  let top_level_node = parse_source(source, SOURCE_FILE, mode)?;
  Program::compile_with_cfg_options(top_level_node, mode, debug, options)
}

/// Compile a file from a `typecheck-ts` [`typecheck_ts::Program`].
///
/// This reuses the `hir-js` lowering cached inside the type program so `BodyId`
/// / `ExprId` values match the IDs used for type checking. If the type program
/// does not have a cached lowering for `file`, the optimizer falls back to
/// lowering the parsed AST itself.
#[cfg(feature = "typed")]
pub fn compile_file_with_typecheck(
  program: std::sync::Arc<typecheck_ts::Program>,
  file: typecheck_ts::FileId,
  mode: TopLevelMode,
  debug: bool,
) -> OptimizeResult<Program> {
  compile_file_with_typecheck_cfg_options(program, file, mode, debug, CompileCfgOptions::default())
}

/// Compile a file from a `typecheck-ts` [`typecheck_ts::Program`] with explicit CFG options.
///
/// This reuses the `hir-js` lowering cached inside the type program so `BodyId`
/// / `ExprId` values match the IDs used for type checking. If the type program
/// does not have a cached lowering for `file`, the optimizer falls back to
/// lowering the parsed AST itself.
#[cfg(feature = "typed")]
pub fn compile_file_with_typecheck_cfg_options(
  program: std::sync::Arc<typecheck_ts::Program>,
  file: typecheck_ts::FileId,
  mode: TopLevelMode,
  debug: bool,
  cfg_options: CompileCfgOptions,
) -> OptimizeResult<Program> {
  let source = program.file_text(file).ok_or_else(|| {
    vec![Diagnostic::error(
      "OPT0003",
      format!("missing source text for {file:?}"),
      Span::new(file, TextRange::new(0, 0)),
    )]
  })?;
  let top_level_node = parse_source(&source, file, mode)?;

  if let Some(lowered) = program.hir_lowered(file) {
    let types = crate::types::TypeContext::from_typecheck_program_aligned(
      Arc::clone(&program),
      file,
      lowered.as_ref(),
    );
    Program::compile_with_lower(top_level_node, lowered, mode, debug, types, cfg_options)
  } else {
    let lower = hir_js::lower_file(file, HirFileKind::Ts, &top_level_node);
    let types =
      crate::types::TypeContext::from_typecheck_program(Arc::clone(&program), file, &lower);
    Program::compile_with_lower(
      top_level_node,
      Arc::new(lower),
      mode,
      debug,
      types,
      cfg_options,
    )
  }
}

/// Compile and type-check a single source string using the bundled
/// `typecheck-ts` memory host.
#[cfg(feature = "typed")]
pub fn compile_source_typed(
  source: &str,
  mode: TopLevelMode,
  debug: bool,
) -> OptimizeResult<Program> {
  compile_source_typed_cfg_options(source, mode, debug, CompileCfgOptions::default())
}

#[cfg(feature = "typed")]
const OPTIMIZE_JS_TYPED_BUILTINS_KEY: &str = "optimize-js:typed-builtins.d.ts";

#[cfg(feature = "typed")]
const OPTIMIZE_JS_TYPED_BUILTINS_D_TS: &str = r#"
// optimize-js builtins (minimal).
//
// We intentionally avoid loading TypeScript's default `dom` lib set for the
// `optimize_js::compile_source_typed*` helpers because it is large and slows down
// test/benchmark workloads. Most optimizer tests only need a typed `console.log`.

declare var console: {
  log(
    a0?: unknown,
    a1?: unknown,
    a2?: unknown,
    a3?: unknown,
    a4?: unknown,
    a5?: unknown,
    a6?: unknown,
    a7?: unknown,
  ): void;
};
"#;

#[cfg(feature = "typed")]
fn source_declares_console(source: &str) -> bool {
  // Conservative substring checks to avoid injecting a duplicate `console`
  // declaration when a test or caller provides its own stubs (common when using
  // `/// <reference no-default-lib="true" />`).
  source.contains("declare var console")
    || source.contains("declare const console")
    || source.contains("declare let console")
}

#[cfg(feature = "typed")]
fn typed_memory_host_for_source(source: &str) -> typecheck_ts::MemoryHost {
  use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, FileKind, LibFile, LibName};

  // Respect `/// <reference no-default-lib="true" />` on root files by leaving
  // the compiler options' `libs` empty so `typecheck-ts` can disable bundled lib
  // loading.
  let no_default_lib = typecheck_ts::triple_slash::scan_triple_slash_directives(source).no_default_lib;
  let mut host = if no_default_lib {
    typecheck_ts::MemoryHost::new()
  } else {
    typecheck_ts::MemoryHost::with_options(TsCompilerOptions {
      libs: vec![LibName::parse("es2015").expect("LibName::parse(es2015)")],
      ..Default::default()
    })
  };

  // Provide a tiny console definition for optimizer tests/benchmarks that
  // reference `console.log(...)`. Keep it free of `T[]` / `Array<T>` to avoid
  // depending on global `Array` types when `no-default-lib` is enabled.
  if source.contains("console") && !source_declares_console(source) {
    host.add_lib(LibFile {
      key: typecheck_ts::FileKey::new(OPTIMIZE_JS_TYPED_BUILTINS_KEY),
      name: Arc::from("optimize-js typed builtins"),
      kind: FileKind::Dts,
      text: Arc::from(OPTIMIZE_JS_TYPED_BUILTINS_D_TS),
    });
  }

  host
}

/// Compile and type-check a single source string using the bundled
/// `typecheck-ts` memory host with explicit CFG options.
#[cfg(feature = "typed")]
pub fn compile_source_typed_cfg_options(
  source: &str,
  mode: TopLevelMode,
  debug: bool,
  cfg_options: CompileCfgOptions,
) -> OptimizeResult<Program> {
  let mut host = typed_memory_host_for_source(source);
  let file = typecheck_ts::FileKey::new("input.ts");
  host.insert(file.clone(), source);
  let type_program = std::sync::Arc::new(typecheck_ts::Program::new(host, vec![file.clone()]));
  let _ = type_program.check();
  let type_file = type_program
    .file_id(&file)
    .expect("typecheck program should know the inserted file");
  compile_file_with_typecheck_cfg_options(type_program, type_file, mode, debug, cfg_options)
}

fn collect_symbol_table(symbols: &JsSymbols, captured: &HashSet<SymbolId>) -> ProgramSymbols {
  fn collect_scope_symbols(
    symbols: &JsSymbols,
    scope: ScopeId,
    captured: &HashSet<SymbolId>,
    out: &mut Vec<ProgramSymbol>,
  ) {
    for (id, name) in symbols.symbols_in_scope(scope) {
      out.push(ProgramSymbol {
        id,
        name,
        scope,
        captured: captured.contains(&id),
      });
    }

    let mut children: Vec<_> = symbols.children(scope).collect();
    children.sort_by_key(|scope| scope.raw_id());
    for child in children {
      collect_scope_symbols(symbols, child, captured, out);
    }
  }

  let mut out_symbols = Vec::new();
  collect_scope_symbols(symbols, symbols.top_scope(), captured, &mut out_symbols);
  let mut scopes = Vec::with_capacity(symbols.semantics.scopes.len());
  for (scope_id, scope) in symbols.semantics.scopes.iter() {
    let id = ScopeId::from(*scope_id);
    let mut scope_symbols: Vec<_> = scope
      .iter_symbols_sorted()
      .map(|(_, symbol)| SymbolId::from(symbol))
      .collect();
    scope_symbols.sort_by_key(|sym| sym.raw_id());
    let mut children: Vec<_> = scope.children.iter().copied().map(Into::into).collect();
    children.sort_by_key(|child: &ScopeId| child.raw_id());
    let mut tdz_bindings: Vec<_> = scope.tdz_bindings.iter().copied().map(Into::into).collect();
    tdz_bindings.sort_by_key(|sym: &SymbolId| sym.raw_id());
    scopes.push(ProgramScope {
      id,
      parent: scope.parent.map(Into::into),
      kind: scope.kind.into(),
      symbols: scope_symbols,
      children,
      tdz_bindings,
      is_dynamic: scope.is_dynamic,
      has_direct_eval: scope.has_direct_eval,
    });
  }
  scopes.sort_by_key(|scope| scope.id.raw_id());
  ProgramSymbols {
    symbols: out_symbols,
    free_symbols: None,
    names: symbols
      .semantics
      .names
      .iter()
      .map(|(_, name)| name.clone())
      .collect(),
    scopes,
  }
}

fn collect_free_symbols(func: &ProgramFunction) -> Vec<SymbolId> {
  let mut free = HashSet::default();
  for (_, insts) in func.body.bblocks.all() {
    for inst in insts {
      match inst.t {
        il::inst::InstTyp::ForeignLoad | il::inst::InstTyp::ForeignStore => {
          free.insert(inst.foreign);
        }
        _ => {}
      }
    }
  }
  let mut out = free.into_iter().collect::<Vec<_>>();
  out.sort_by_key(|s| s.raw_id());
  out
}

impl Program {
  fn compile_with_lower(
    mut top_level_node: Node<TopLevel>,
    lower: Arc<LowerResult>,
    top_level_mode: TopLevelMode,
    debug: bool,
    types: crate::types::TypeContext,
    cfg_options: CompileCfgOptions,
  ) -> OptimizeResult<Self> {
    let source_file = lower.hir.file;
    let (semantics, diagnostics) =
      JsSymbols::bind(&mut top_level_node, top_level_mode, source_file);
    if !diagnostics.is_empty() {
      return Err(diagnostics);
    }
    let VarAnalysis {
      foreign,
      use_before_decl,
      dynamic_scope,
      ..
    } = VarAnalysis::analyze(&mut top_level_node, &semantics);
    if let Some(loc) = dynamic_scope {
      return Err(unsupported_syntax(
        source_file,
        loc,
        "with statements introduce dynamic scope and are not supported",
      ));
    }
    // SSA requires no use before declaration.
    if !use_before_decl.is_empty() {
      let mut diagnostics: Vec<_> = use_before_decl
        .into_iter()
        .map(|(_, (name, loc))| use_before_declaration(source_file, &name, loc))
        .collect();
      sort_diagnostics(&mut diagnostics);
      return Err(diagnostics);
    };
    let mut symbol_table = collect_symbol_table(&semantics, &foreign);

    let bindings = collect_hir_symbol_bindings(&mut top_level_node, lower.as_ref());
    let program = ProgramCompiler(Arc::new(ProgramCompilerInner {
      foreign_vars: foreign.clone(),
      functions: DashMap::new(),
      next_fn_id: AtomicUsize::new(0),
      debug,
      cfg_options,
      lower: Arc::clone(&lower),
      bindings,
      names: Arc::clone(&lower.names),
      types,
    }));

    let top_level = compile_hir_body(&program, lower.root_body())?;
    let ProgramCompilerInner {
      functions,
      next_fn_id,
      ..
    } = Arc::try_unwrap(program.0).unwrap();
    let fn_count = next_fn_id.load(Ordering::Relaxed);
    let functions: Vec<_> = (0..fn_count)
      .map(|i| functions.remove(&i).unwrap().1)
      .collect();

    let free_symbols = ProgramFreeSymbols {
      top_level: collect_free_symbols(&top_level),
      functions: functions.iter().map(collect_free_symbols).collect(),
    };

    let has_any_symbols = !symbol_table.symbols.is_empty()
      || !free_symbols.top_level.is_empty()
      || free_symbols.functions.iter().any(|f| !f.is_empty());

    let symbols = if has_any_symbols {
      symbol_table.free_symbols = Some(free_symbols);
      Some(symbol_table)
    } else {
      None
    };

    let mut program = Self {
      functions,
      top_level,
      top_level_mode,
      symbols,
    };

    // Annotate `ssa_body` CFGs with interprocedural escape/ownership metadata. This lets downstream
    // consumers (e.g. native backends) rely on `ProgramFunction::analyzed_cfg()` without separately
    // running the program-wide analysis driver.
    annotate_program_ssa_metadata(&mut program);

    Ok(program)
  }

  pub fn compile(
    top_level_node: Node<TopLevel>,
    top_level_mode: TopLevelMode,
    debug: bool,
  ) -> OptimizeResult<Self> {
    let lower = Arc::new(hir_js::lower_file(
      SOURCE_FILE,
      HirFileKind::Ts,
      &top_level_node,
    ));
    Self::compile_with_lower(
      top_level_node,
      lower,
      top_level_mode,
      debug,
      Default::default(),
      CompileCfgOptions::default(),
    )
  }

  pub fn compile_with_cfg_options(
    top_level_node: Node<TopLevel>,
    top_level_mode: TopLevelMode,
    debug: bool,
    cfg_options: CompileCfgOptions,
  ) -> OptimizeResult<Self> {
    let lower = Arc::new(hir_js::lower_file(
      SOURCE_FILE,
      HirFileKind::Ts,
      &top_level_node,
    ));
    Self::compile_with_lower(
      top_level_node,
      lower,
      top_level_mode,
      debug,
      Default::default(),
      cfg_options,
    )
  }

  pub fn compile_lowered(
    source: &str,
    lower: LowerResult,
    top_level_mode: TopLevelMode,
    debug: bool,
  ) -> OptimizeResult<Self> {
    Self::compile_lowered_with_cfg_options(
      source,
      lower,
      top_level_mode,
      debug,
      CompileCfgOptions::default(),
    )
  }

  pub fn compile_lowered_with_cfg_options(
    source: &str,
    lower: LowerResult,
    top_level_mode: TopLevelMode,
    debug: bool,
    cfg_options: CompileCfgOptions,
  ) -> OptimizeResult<Self> {
    let source_file = lower.hir.file;
    let top_level_node = parse_source(source, source_file, top_level_mode)?;
    Self::compile_with_lower(
      top_level_node,
      Arc::new(lower),
      top_level_mode,
      debug,
      Default::default(),
      cfg_options,
    )
  }
}

#[cfg(test)]
mod tests {
  use super::SOURCE_FILE;
  use crate::cfg::cfg::Cfg;
  use crate::compile_source;
  use crate::il::inst::Inst;
  use crate::il::inst::InstTyp;
  use crate::symbol::semantics::JsSymbols;
  use crate::symbol::var_analysis::VarAnalysis;
  use crate::Program;
  use crate::TopLevelMode;
  use parse_js::parse;
  #[cfg(feature = "serde")]
  use serde_json::to_string;
  use std::collections::HashSet;

  #[cfg(feature = "serde")]
  fn compile_with_debug_json(source: &str) -> String {
    let top_level_node = parse(source).expect("parse input");
    let Program { top_level, .. } =
      Program::compile(top_level_node, TopLevelMode::Module, true).expect("compile");
    let debug = top_level.debug.expect("debug enabled");
    to_string(&debug).expect("serialize debug output")
  }

  fn collect_insts(cfg: &Cfg) -> Vec<Inst> {
    let mut blocks: Vec<_> = cfg.bblocks.all().collect();
    blocks.sort_by_key(|(label, _)| *label);
    let mut insts = Vec::new();
    for (_, block) in blocks.into_iter() {
      insts.extend(block.iter().cloned());
    }
    insts
  }

  fn collect_all_insts(program: &Program) -> Vec<Inst> {
    let mut insts = collect_insts(&program.top_level.body);
    for func in &program.functions {
      insts.extend(collect_insts(&func.body));
    }
    insts
  }

  fn compile_with_mode(source: &str, mode: TopLevelMode) -> Program {
    let top_level_node = super::parse_source(source, SOURCE_FILE, mode).expect("parse input");
    Program::compile(top_level_node, mode, false).expect("compile input")
  }

  #[test]
  fn program_records_top_level_mode() {
    let program = compile_source("var x = 1;", TopLevelMode::Global, false).expect("compile");
    assert_eq!(program.top_level_mode, TopLevelMode::Global);
  }

  #[test]
  fn global_mode_rejects_module_syntax() {
    // Ensure `TopLevelMode` drives the parser source type selection so global/script programs do
    // not accept module-only constructs.
    let err = super::parse_source("export {};", SOURCE_FILE, TopLevelMode::Global)
      .expect_err("export should not be allowed in global scripts");
    assert!(
      err
        .iter()
        .any(|diag| diag.message.contains("export not allowed in scripts")),
      "expected parse diagnostic about export not allowed in scripts, got {err:?}"
    );

    super::parse_source("export {};", SOURCE_FILE, TopLevelMode::Module)
      .expect("module export should parse");
  }

  #[test]
  fn test_compile_js_statements() {
    let source = r#"
      (() => {
        a?.b?.c;
        let x = 1;
        if (x) {
          g();
          x += Math.round(1.1);
          for (;;) {
            x += 1;
            setTimeout(() => {
              h(x);
            }, 1000);
          }
        }
        f(x);
      })();
    "#;
    let top_level_node = parse(source).expect("parse input");
    let _bblocks = Program::compile(top_level_node, TopLevelMode::Module, false)
      .expect("compile")
      .top_level;
  }

  #[test]
  fn test_use_before_declaration_error() {
    let source = "function demo(){ a; let a = 1; }";
    let top_level_node = parse(source).expect("parse input");
    let err = Program::compile(top_level_node, TopLevelMode::Module, false)
      .expect_err("expected use-before-decl error");
    assert_eq!(err.len(), 1);
    let diagnostic = &err[0];
    assert_eq!(diagnostic.code, "BIND0003");
    let range = diagnostic.primary.range;
    assert!(range.start < range.end);
    assert_eq!(&source[range.start as usize..range.end as usize], "a");
  }

  #[cfg(feature = "serde")]
  #[test]
  fn optimizer_debug_output_is_deterministic() {
    let source = r#"
      let x = 1;
      if (x > 0) {
        x = x + 1;
      } else {
        x = x - 1;
      }
      while (x < 3) {
        x += 1;
      }
      x += 2;
    "#;

    let first = compile_with_debug_json(source);
    let second = compile_with_debug_json(source);

    assert_eq!(first, second, "debug output should be deterministic");
  }

  #[test]
  fn captured_reads_and_writes_use_foreign_insts() {
    let source = r#"
      let runner = () => {
        let x = 0;
        const bump = () => {
          x = x + 1;
          (() => {
            x = x + 1;
            x;
          })();
          x;
        };
        bump();
      };

      runner();
    "#;

    let program = compile_with_mode(source, TopLevelMode::Module);
    let insts = collect_all_insts(&program);

    let mut loads = HashSet::new();
    let mut stores = HashSet::new();
    for inst in insts {
      match inst.t {
        InstTyp::ForeignLoad => {
          loads.insert(inst.as_foreign_load().1);
        }
        InstTyp::ForeignStore => {
          stores.insert(inst.as_foreign_store().0);
        }
        _ => {}
      }
    }

    assert!(!loads.is_empty());
    assert!(!stores.is_empty());
    assert!(loads.intersection(&stores).next().is_some());
  }

  #[test]
  fn destructuring_declares_locals() {
    let source = r#"
      let {a} = obj;
      a = a + 1;
      call_me(a);
    "#;

    let mut top_level_node = parse(source).expect("parse input");
    let (symbols, _) = JsSymbols::bind(&mut top_level_node, TopLevelMode::Module, SOURCE_FILE);
    let analysis = VarAnalysis::analyze(&mut top_level_node, &symbols);
    assert_eq!(analysis.declared.len(), 1);
    assert!(analysis.unknown.contains("obj"));
    assert!(analysis.unknown.contains("call_me"));
    assert!(!analysis.unknown.contains("a"));

    let program =
      Program::compile(top_level_node, TopLevelMode::Module, false).expect("compile input");
    let insts = collect_all_insts(&program);

    #[cfg(test)]
    for inst in insts.iter() {
      if matches!(inst.t, InstTyp::UnknownLoad | InstTyp::UnknownStore) {
        eprintln!("unknown inst: {:?}", inst);
      }
    }
    assert!(insts.iter().all(|i| i.t != InstTyp::UnknownStore));
    let unknown_names: HashSet<_> = insts
      .iter()
      .filter(|i| matches!(i.t, InstTyp::UnknownLoad | InstTyp::UnknownStore))
      .map(|i| i.unknown.clone())
      .collect();
    assert!(!unknown_names.contains("a"));
  }

  #[test]
  fn global_mode_uses_unknown_memory_ops() {
    let source = r#"
      var a = 1;
      a = a + 2;
    "#;

    let program = compile_with_mode(source, TopLevelMode::Global);
    let insts = collect_insts(&program.top_level.body);

    assert!(insts.iter().any(|i| i.t == InstTyp::UnknownStore));
    assert!(insts.iter().any(|i| i.t == InstTyp::UnknownLoad));
  }

  #[test]
  fn optional_chaining_assignment_target_is_rejected() {
    let source = "a?.b = 1;";
    let top_level_node = parse(source).expect("parse input");
    let err =
      Program::compile(top_level_node, TopLevelMode::Module, false).expect_err("expected error");
    assert_eq!(err.len(), 1);
    assert_eq!(err[0].code, "OPT0002");
    assert!(err[0]
      .message
      .contains("optional chaining in assignment target"));
  }
}
