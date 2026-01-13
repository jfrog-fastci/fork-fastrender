use crate::execution_context::ModuleId;
use crate::fallible_alloc::arc_try_new_vm;
use crate::module_graph::ModuleGraph;
use crate::ImportAttribute;
use crate::LoadedModuleRequest;
use crate::ModuleRequest;
use crate::SourceText;
use crate::CompiledScript;
use crate::{
  EnvRootId, ExternalMemoryToken, GcObject, Heap, PromiseCapability, RealmId, RootId, Scope, Value, Vm, VmError,
};
use diagnostics::{Diagnostic, FileId};
use parse_js::ast::class_or_object::{
  ClassMember, ClassOrObjKey, ClassOrObjVal, ObjMember, ObjMemberType,
};
use parse_js::ast::expr::Expr;
use parse_js::ast::expr::pat::Pat;
use parse_js::ast::expr::lit::{LitArrElem, LitTemplatePart};
use parse_js::ast::import_export::ExportNames;
use parse_js::ast::import_export::ImportNames;
use parse_js::ast::import_export::ModuleExportImportName as AstModuleExportImportName;
use parse_js::ast::node::{
  literal_string_code_units, module_export_import_name_code_units, module_specifier_code_units, Node,
};
use parse_js::ast::stmt::Stmt;
use parse_js::ast::stmt::ForInOfLhs;
use parse_js::ast::stmt::decl::VarDeclMode;
use parse_js::ast::stx::TopLevel;
use parse_js::lex::KEYWORDS_MAPPING;
use parse_js::operator::OperatorName;
use parse_js::token::TT;
use parse_js::{parse_with_options, Dialect, ParseOptions, SourceType};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Module linking/loading status.
///
/// This is a subset of ECMA-262's `ModuleStatus` enum.
///
/// `vm-js` models the states needed for linking/evaluation (including async evaluation for
/// top-level `await`). Additional states may be added as more of the module specification is
/// implemented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ModuleStatus {
  #[default]
  New,
  Unlinked,
  Linking,
  Linked,
  Evaluating,
  /// Spec `~evaluating-async~` (`ModuleStatus::evaluating-async`).
  EvaluatingAsync,
  Evaluated,
  Errored,
}

/// Spec `[[AsyncEvaluationOrder]]` internal slot of Cyclic Module Records.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub(crate) enum AsyncEvaluationOrder {
  /// Spec `~unset~`.
  #[default]
  Unset,
  /// Concrete order index.
  Order(usize),
  /// Spec `~done~`.
  Done,
}

/// Persistent roots for an ECMAScript PromiseCapability record.
///
/// This is used for module loading/evaluation state that can live in host memory across async
/// boundaries. Callers MUST explicitly tear down these roots when the capability is no longer
/// needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PromiseCapabilityRoots {
  promise: RootId,
  resolve: RootId,
  reject: RootId,
}

impl PromiseCapabilityRoots {
  pub(crate) fn new(scope: &mut Scope<'_>, cap: PromiseCapability) -> Result<Self, VmError> {
    // Root the capability values while creating persistent roots: `Heap::add_root` can trigger GC.
    let values = [cap.promise, cap.resolve, cap.reject];
    scope.push_roots(&values)?;

    let mut roots: Vec<RootId> = Vec::new();
    roots
      .try_reserve_exact(values.len())
      .map_err(|_| VmError::OutOfMemory)?;
    for &value in &values {
      match scope.heap_mut().add_root(value) {
        Ok(id) => roots.push(id),
        Err(e) => {
          for root in roots.drain(..) {
            scope.heap_mut().remove_root(root);
          }
          return Err(e);
        }
      }
    }

    Ok(Self {
      promise: roots[0],
      resolve: roots[1],
      reject: roots[2],
    })
  }

  #[inline]
  #[allow(dead_code)]
  pub(crate) fn promise_root(&self) -> RootId {
    self.promise
  }

  #[inline]
  #[allow(dead_code)]
  pub(crate) fn resolve_root(&self) -> RootId {
    self.resolve
  }

  #[inline]
  #[allow(dead_code)]
  pub(crate) fn reject_root(&self) -> RootId {
    self.reject
  }

  pub(crate) fn capability(&self, heap: &Heap) -> Option<PromiseCapability> {
    Some(PromiseCapability {
      promise: heap.get_root(self.promise)?,
      resolve: heap.get_root(self.resolve)?,
      reject: heap.get_root(self.reject)?,
    })
  }

  pub(crate) fn teardown(self, heap: &mut Heap) {
    heap.remove_root(self.promise);
    heap.remove_root(self.resolve);
    heap.remove_root(self.reject);
  }
}

/// Persistent root for a single JS value stored in host-owned, non-traced memory.
///
/// Callers MUST explicitly tear down the root when the value is no longer needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ValueRoot(RootId);

#[allow(dead_code)]
impl ValueRoot {
  pub(crate) fn new(scope: &mut Scope<'_>, value: Value) -> Result<Self, VmError> {
    // Root `value` during persistent root registration in case it triggers a GC.
    scope.push_root(value)?;
    let id = scope.heap_mut().add_root(value)?;
    Ok(Self(id))
  }

  pub(crate) fn get(self, heap: &Heap) -> Option<Value> {
    heap.get_root(self.0)
  }

  pub(crate) fn teardown(self, heap: &mut Heap) {
    heap.remove_root(self.0);
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalExportEntry {
  pub export_name: String,
  pub local_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImportName {
  Name(String),
  /// Corresponds to ECMA-262 `ImportName = all`, used by `export * as ns from "m"`.
  All,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportEntry {
  pub module_request: ModuleRequest,
  pub import_name: ImportName,
  pub local_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndirectExportEntry {
  pub export_name: String,
  pub module_request: ModuleRequest,
  pub import_name: ImportName,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StarExportEntry {
  pub module_request: ModuleRequest,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BindingName {
  Name(String),
  Namespace,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedBinding {
  pub module: ModuleId,
  pub binding_name: BindingName,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolveExportResult {
  Resolved(ResolvedBinding),
  NotFound,
  Ambiguous,
}

/// Cached data for a module's namespace object (`module.[[Namespace]]` in ECMA-262).
#[derive(Clone, Debug)]
pub(crate) struct ModuleNamespaceCache {
  pub object: RootId,
  pub exports: Vec<String>,
  #[allow(dead_code)]
  pub external_memory: Option<Arc<ExternalMemoryToken>>,
}

/// Source Text Module Record (ECMA-262).
#[derive(Clone, Debug, Default)]
pub struct SourceTextModuleRecord {
  // === Executable code ===
  //
  // A `SourceTextModuleRecord` can be executed through one of two evaluators:
  //
  // - **AST interpreter** (`exec.rs`): requires `source` + `ast` to both be `Some`.
  // - **HIR executor** (`hir_exec.rs`): requires `compiled` to be `Some`.
  //
  // These fields are optional so a `ModuleGraph` can be populated in stages (e.g. module loading
  // before compilation) and so embeddings can choose their preferred execution strategy.
  //
  // Note: [`crate::CompiledScript`] owns an [`ExternalMemoryToken`] that charges the heap for
  // off-heap compiled code/HIR. Storing it in this record ensures the token stays alive (and its
  // bytes stay accounted for) for as long as the module record is reachable from the graph.

  /// Source text and metadata for this module (URL, name, etc).
  pub source: Option<Arc<SourceText>>,
  /// Parsed `parse-js` AST for this module.
  pub ast: Option<Arc<Node<TopLevel>>>,
  /// External-memory token for retained module ASTs (`ast`).
  ///
  /// Module ASTs live outside the GC heap. When a module is compiled to HIR but must retain/parse
  /// an AST for a fallback execution path (e.g. top-level await or async-generator fallback), the
  /// host must charge the additional memory usage via [`Heap::charge_external`].
  ///
  /// This token is stored in an [`Arc`] so `SourceTextModuleRecord` remains `Clone` without
  /// duplicating the external-memory charge.
  pub(crate) ast_external_memory: Option<Arc<ExternalMemoryToken>>,
  /// Compiled module code (source text + lowered HIR).
  ///
  /// When present, module bodies may be executed via the compiled (HIR) executor instead of the
  /// AST interpreter.
  ///
  /// Note: async module evaluation (top-level await) is supported by the compiled executor only
  /// for a conservative subset of `await` shapes. When unsupported patterns are present,
  /// [`crate::ModuleGraph`] falls back to the async AST evaluator.
  ///
  /// When executing via the HIR path, `source`/`ast` may be `None` as the compiled script already
  /// owns its own [`SourceText`] reference.
  pub compiled: Option<Arc<CompiledScript>>,
  pub requested_modules: Vec<ModuleRequest>,
  pub import_entries: Vec<ImportEntry>,
  pub status: ModuleStatus,
  /// Cached thrown value for modules in the [`ModuleStatus::Errored`] state.
  ///
  /// Stored as a persistent GC root so that subsequent linking/evaluation attempts can fail
  /// deterministically with the **same** thrown value (object identity is preserved).
  pub(crate) error: Option<RootId>,
  /// `[[HasTLA]]` – whether this module contains top-level `await`.
  pub has_tla: bool,
  pub local_export_entries: Vec<LocalExportEntry>,
  pub indirect_export_entries: Vec<IndirectExportEntry>,
  pub star_export_entries: Vec<StarExportEntry>,

  /// `[[LoadedModules]]` – a host-populated mapping from module requests to resolved module ids.
  pub loaded_modules: Vec<LoadedModuleRequest<ModuleId>>,

  /// `[[Namespace]]` – cached module namespace object + sorted `[[Exports]]` list.
  ///
  /// Note: the namespace object is rooted in the heap via a persistent [`RootId`] so it survives GC.
  pub(crate) namespace: Option<ModuleNamespaceCache>,

  /// `[[Environment]]` – module environment record (rooted in the heap).
  pub(crate) environment: Option<EnvRootId>,

  /// `[[ImportMeta]]` – cached `import.meta` object (rooted in the heap).
  pub(crate) import_meta: Option<RootId>,

  // === Cyclic Module Record evaluation state (ECMA-262) ===
  //
  // These fields mirror the spec's internal slots for cyclic modules.
  //
  // Note: `vm-js` stores some async-evaluation bookkeeping out-of-line in [`ModuleGraph`] (SCC
  // caches, async dependency counters, etc), so not every slot below is used directly by the
  // evaluator yet. We keep them for spec alignment and for slots that must be cached on the module
  // record itself (promise capability roots, cached errors, etc).

  /// `[[CycleRoot]]` – SCC root module record (or empty).
  #[allow(dead_code)]
  pub(crate) cycle_root: Option<ModuleId>,
  /// `[[DFSAncestorIndex]]` – DFS ancestor index (or empty).
  #[allow(dead_code)]
  pub(crate) dfs_ancestor_index: Option<usize>,
  /// `[[AsyncEvaluationOrder]]` – `~unset~ | integer | ~done~`.
  #[allow(dead_code)]
  pub(crate) async_evaluation_order: AsyncEvaluationOrder,
  /// `[[TopLevelCapability]]` – module evaluation promise capability (or empty).
  #[allow(dead_code)]
  pub(crate) top_level_capability: Option<PromiseCapabilityRoots>,
  /// `[[AsyncParentModules]]` – async parent module set.
  #[allow(dead_code)]
  pub(crate) async_parent_modules: Vec<ModuleId>,
  /// `[[PendingAsyncDependencies]]` – async dependency count (or empty).
  #[allow(dead_code)]
  pub(crate) pending_async_dependencies: Option<usize>,
  /// `[[EvaluationError]]` – thrown value during evaluation (or empty).
  pub(crate) evaluation_error: Option<ValueRoot>,

  /// Async continuation id produced by top-level await execution, if this module suspended at an
  /// `await` boundary.
  ///
  /// This is used by `ModuleGraph::abort_tla_evaluation` to tear down module-owned async
  /// continuations so their persistent roots do not leak when an embedder does not drive the event
  /// loop.
  #[allow(dead_code)]
  pub(crate) async_continuation_id: Option<u32>,

  /// Realm/global object used for module execution.
  ///
  /// These are recorded when module evaluation begins so async module execution microtasks
  /// (`AsyncModuleExecutionFulfilled` / `AsyncModuleExecutionRejected`) can execute dependent modules
  /// later, after `ModuleGraph::evaluate_with_scope` has returned.
  #[allow(dead_code)]
  pub(crate) eval_realm_id: Option<RealmId>,
  #[allow(dead_code)]
  pub(crate) eval_global_object: Option<GcObject>,
}

impl SourceTextModuleRecord {
  /// Clears the retained module AST, dropping any associated external-memory charge.
  pub(crate) fn clear_ast(&mut self) {
    // Drop the AST first so its memory is freed before releasing the external-memory charge.
    self.ast = None;
    self.ast_external_memory = None;
  }

  #[allow(dead_code)]
  pub(crate) fn set_top_level_capability(
    &mut self,
    scope: &mut Scope<'_>,
    cap: PromiseCapability,
  ) -> Result<(), VmError> {
    if self.top_level_capability.is_some() {
      return Err(VmError::InvariantViolation(
        "module already has a top-level promise capability",
      ));
    }
    self.top_level_capability = Some(PromiseCapabilityRoots::new(scope, cap)?);
    Ok(())
  }

  #[allow(dead_code)]
  pub(crate) fn teardown_top_level_capability(&mut self, heap: &mut Heap) {
    if let Some(roots) = self.top_level_capability.take() {
      roots.teardown(heap);
    }
  }

  pub(crate) fn set_evaluation_error(
    &mut self,
    scope: &mut Scope<'_>,
    value: Value,
  ) -> Result<(), VmError> {
    if self.evaluation_error.is_some() {
      return Err(VmError::InvariantViolation(
        "module already has an evaluation error value",
      ));
    }
    self.evaluation_error = Some(ValueRoot::new(scope, value)?);
    Ok(())
  }

  pub(crate) fn teardown_evaluation_error(&mut self, heap: &mut Heap) {
    if let Some(root) = self.evaluation_error.take() {
      root.teardown(heap);
    }
  }

  /// Returns the cached namespace export list (`[[Exports]]`) if a namespace object has been
  /// created.
  pub fn namespace_exports(&self) -> Option<&[String]> {
    self.namespace.as_ref().map(|ns| ns.exports.as_slice())
  }

  /// Parses a source text module using the `parse-js` front-end and extracts the module record
  /// fields needed by `GetExportedNames` and `ResolveExport`.
  ///
  /// This corresponds to the spec's `ParseModule` abstract operation, but only models the export
  /// entry lists and `[[RequestedModules]]`.
  pub fn parse(heap: &mut Heap, source: &str) -> Result<Self, VmError> {
    let source = arc_try_new_vm(SourceText::new_charged(heap, "<inline>", source)?)?;
    Self::parse_source(source)
  }

  /// Parse a module and additionally compile it to HIR so `ModuleGraph` can execute it via the
  /// compiled (HIR) execution engine.
  pub fn parse_compiled<'a>(
    heap: &mut Heap,
    name: impl Into<crate::SourceTextInput<'a>>,
    text: impl Into<crate::SourceTextInput<'a>>,
  ) -> Result<Self, VmError> {
    let script = CompiledScript::compile_module(heap, name, text)?;
    // Parse the module record fields from the same `SourceText` so error spans and `import.meta`
    // use consistent source metadata.
    let mut record = Self::parse_source(script.source.clone())?;
    record.compiled = Some(script);
    Ok(record)
  }

  /// Parse a module and capture its [`SourceText`] + parsed AST for later evaluation.
  pub fn parse_source(source: Arc<SourceText>) -> Result<Self, VmError> {
    let opts = ParseOptions {
      dialect: Dialect::Ecma,
      source_type: SourceType::Module,
    };
    let top = parse_with_options(&source.text, opts)
      .map_err(|err| {
        let diag = crate::parse_diagnostics::parse_js_error_to_diagnostic(&err, FileId(0));
        VmError::Syntax(vec![diag])
      })?;
    {
      let mut tick = || Ok(());
      crate::early_errors::validate_top_level(
        &top.stx.body,
        crate::early_errors::EarlyErrorOptions::module(),
        Some(source.text.as_ref()),
        &mut tick,
      )?;
    }
    let mut cancel = || Ok(());
    let mut record = module_record_from_top_level(&top, &mut cancel)?;
    record.source = Some(source);
    record.ast = Some(arc_try_new_vm(top)?);
    Ok(record)
  }

  /// Parse, validate, and compile a source text module.
  ///
  /// This is a convenience API for embeddings and unit tests that want to execute modules through
  /// the compiled HIR engine. The returned [`SourceTextModuleRecord`] is identical to
  /// [`SourceTextModuleRecord::parse_source`], but has `compiled` populated so
  /// [`crate::ModuleGraph`] can execute it via `hir_exec`.
  pub fn compile_source(heap: &mut Heap, source: Arc<SourceText>) -> Result<Self, VmError> {
    let mut record = Self::parse_source(source.clone())?;
    let ast = record
      .ast
      .as_deref()
      .ok_or(VmError::InvariantViolation("module AST missing after successful parse"))?;
    let compiled = crate::CompiledScript::compile_module_from_parsed(heap, source, ast)?;
    record.compiled = Some(compiled);
    Ok(record)
  }

  /// Parses a source text module using VM budget/interrupt state.
  pub fn parse_with_vm(heap: &mut Heap, vm: &mut Vm, source: &str) -> Result<Self, VmError> {
    let source = arc_try_new_vm(SourceText::new_charged(heap, "<inline>", source)?)?;
    let opts = ParseOptions {
      dialect: Dialect::Ecma,
      source_type: SourceType::Module,
    };
    let top = vm.parse_top_level_with_budget(&source.text, opts)?;
    let mut cancel = || vm.tick();
    crate::early_errors::validate_top_level(
      &top.stx.body,
      crate::early_errors::EarlyErrorOptions::module(),
      Some(source.text.as_ref()),
      &mut cancel,
    )?;
    let mut record = module_record_from_top_level(&top, &mut cancel)?;
    record.source = Some(source);
    record.ast = Some(arc_try_new_vm(top)?);
    Ok(record)
  }

  /// Parses a source text module using VM budget/interrupt state, preserving the provided source
  /// metadata (URL/name/etc).
  pub fn parse_source_with_vm(vm: &mut Vm, source: Arc<SourceText>) -> Result<Self, VmError> {
    let opts = ParseOptions {
      dialect: Dialect::Ecma,
      source_type: SourceType::Module,
    };
    let top = vm.parse_top_level_with_budget(&source.text, opts)?;
    let mut cancel = || vm.tick();
    crate::early_errors::validate_top_level(
      &top.stx.body,
      crate::early_errors::EarlyErrorOptions::module(),
      Some(source.text.as_ref()),
      &mut cancel,
    )?;
    let mut record = module_record_from_top_level(&top, &mut cancel)?;
    record.source = Some(source);
    record.ast = Some(arc_try_new_vm(top)?);
    Ok(record)
  }

  /// Budget-aware variant of [`SourceTextModuleRecord::compile_source`].
  pub fn compile_source_with_vm(
    vm: &mut Vm,
    heap: &mut Heap,
    source: Arc<SourceText>,
  ) -> Result<Self, VmError> {
    let mut record = Self::parse_source_with_vm(vm, source.clone())?;
    let ast = record
      .ast
      .as_deref()
      .ok_or(VmError::InvariantViolation("module AST missing after successful parse"))?;
    let compiled = crate::CompiledScript::compile_module_from_parsed(heap, source, ast)?;
    record.compiled = Some(compiled);
    Ok(record)
  }

  /// Implements ECMA-262 `GetExportedNames([exportStarSet])`.
  pub fn get_exported_names(&self, graph: &ModuleGraph, module: ModuleId) -> Vec<String> {
    self.get_exported_names_with_star_set(graph, module, &mut Vec::new())
  }

  /// Budget-aware variant of [`SourceTextModuleRecord::get_exported_names`].
  ///
  /// This mirrors [`Vm::parse_top_level_with_budget`] and other VM entrypoints by periodically
  /// calling `vm.tick()` while traversing module exports. Large export lists and `export *` graphs
  /// must still observe fuel/deadline/interrupt budgets even when modules contain little-to-no code.
  pub fn get_exported_names_with_vm(
    &self,
    vm: &mut Vm,
    graph: &ModuleGraph,
    module: ModuleId,
  ) -> Result<Vec<String>, VmError> {
    let mut cancel = || vm.tick();
    let mut ctx = ModuleRecordParseCtx::new(&mut cancel);
    ctx.cancel_now()?;
    self.get_exported_names_with_star_set_budgeted(graph, module, &mut Vec::new(), &mut ctx)
  }

  pub fn get_exported_names_with_star_set(
    &self,
    graph: &ModuleGraph,
    module: ModuleId,
    export_star_set: &mut Vec<ModuleId>,
  ) -> Vec<String> {
    // Best-effort wrapper around the budgeted implementation.
    //
    // The spec algorithms for module exports can be driven by attacker-controlled export lists and
    // `export *` graphs. Ensure the unbudgeted convenience wrapper does not abort the process on
    // allocator OOM by using fallible allocations and returning an empty list on `OutOfMemory`.
    let mut cancel = || Ok(());
    let mut ctx = ModuleRecordParseCtx::new(&mut cancel);
    match self.get_exported_names_with_star_set_budgeted(graph, module, export_star_set, &mut ctx) {
      Ok(names) => names,
      Err(VmError::OutOfMemory) => Vec::new(),
      // No other errors are expected without VM cancellation, but keep this wrapper infallible.
      Err(_) => Vec::new(),
    }
  }

  fn get_exported_names_with_star_set_budgeted(
    &self,
    graph: &ModuleGraph,
    module: ModuleId,
    export_star_set: &mut Vec<ModuleId>,
    ctx: &mut ModuleRecordParseCtx<'_>,
  ) -> Result<Vec<String>, VmError> {
    ctx.budget_tick()?;

    // 1. If exportStarSet contains module, then
    for existing in export_star_set.iter() {
      ctx.budget_tick()?;
      if *existing == module {
        // a. Return a new empty List.
        return Ok(Vec::new());
      }
    }

    // 2. Append module to exportStarSet.
    export_star_set
      .try_reserve_exact(1)
      .map_err(|_| VmError::OutOfMemory)?;
    export_star_set.push(module);

    // 3. Let exportedNames be a new empty List.
    let mut exported_names = Vec::<String>::new();

    // 4. For each ExportEntry Record e of module.[[LocalExportEntries]], do
    for entry in &self.local_export_entries {
      ctx.budget_tick()?;
      // a. Append e.[[ExportName]] to exportedNames.
      exported_names
        .try_reserve_exact(1)
        .map_err(|_| VmError::OutOfMemory)?;
      exported_names.push(try_string_from_str(entry.export_name.as_str())?);
    }

    // 5. For each ExportEntry Record e of module.[[IndirectExportEntries]], do
    for entry in &self.indirect_export_entries {
      ctx.budget_tick()?;
      // a. Append e.[[ExportName]] to exportedNames.
      exported_names
        .try_reserve_exact(1)
        .map_err(|_| VmError::OutOfMemory)?;
      exported_names.push(try_string_from_str(entry.export_name.as_str())?);
    }

    // 6. For each ExportEntry Record e of module.[[StarExportEntries]], do
    for entry in &self.star_export_entries {
      ctx.budget_tick()?;
      // a. Let requestedModule be GetImportedModule(module, e.[[ModuleRequest]]).
      let Some(requested_module) = graph.get_imported_module(module, &entry.module_request) else {
        continue;
      };
      // b. Let starNames be requestedModule.GetExportedNames(exportStarSet).
      let star_names = graph
        .module(requested_module)
        .get_exported_names_with_star_set_budgeted(graph, requested_module, export_star_set, ctx)?;

      // c. For each element n of starNames, do
      for name in star_names {
        ctx.budget_tick()?;
        // i. If SameValue(n, "default") is false, then
        if name == "default" {
          continue;
        }
        // 1. If exportedNames does not contain n, then
        if !vec_contains_string_with_budget(&exported_names, &name, ctx)? {
          // a. Append n to exportedNames.
          exported_names
            .try_reserve_exact(1)
            .map_err(|_| VmError::OutOfMemory)?;
          exported_names.push(name);
        }
      }
    }

    // 7. Return exportedNames.
    Ok(exported_names)
  }

  /// Implements ECMA-262 `ResolveExport(exportName[, resolveSet])`.
  pub fn resolve_export(
    &self,
    graph: &ModuleGraph,
    module: ModuleId,
    export_name: &str,
  ) -> ResolveExportResult {
    self.resolve_export_with_set(graph, module, export_name, &mut Vec::new())
  }

  /// Budget-aware variant of [`SourceTextModuleRecord::resolve_export`].
  ///
  /// `ResolveExport` can recurse through `export * from ...` graphs. In pathological cases (large
  /// graphs with many star exports), the spec algorithm can do substantial work without executing
  /// any user code. Periodically ticking the VM ensures those graphs still observe fuel/deadline
  /// budgets.
  pub fn resolve_export_with_vm(
    &self,
    vm: &mut Vm,
    graph: &ModuleGraph,
    module: ModuleId,
    export_name: &str,
  ) -> Result<ResolveExportResult, VmError> {
    let mut cancel = || vm.tick();
    let mut ctx = ModuleRecordParseCtx::new(&mut cancel);
    ctx.cancel_now()?;
    self.resolve_export_with_set_budgeted(graph, module, export_name, &mut Vec::new(), &mut ctx)
  }

  pub fn resolve_export_with_set(
    &self,
    graph: &ModuleGraph,
    module: ModuleId,
    export_name: &str,
    resolve_set: &mut Vec<(ModuleId, String)>,
  ) -> ResolveExportResult {
    // Best-effort wrapper around the budgeted implementation.
    //
    // Hostile module graphs and large export names can drive allocator OOM in the spec algorithm.
    // Ensure this infallible wrapper does not abort the process by mapping `OutOfMemory` to
    // `NotFound`.
    let mut cancel = || Ok(());
    let mut ctx = ModuleRecordParseCtx::new(&mut cancel);
    match self.resolve_export_with_set_budgeted(graph, module, export_name, resolve_set, &mut ctx) {
      Ok(result) => result,
      Err(VmError::OutOfMemory) => ResolveExportResult::NotFound,
      // No other errors are expected without VM cancellation, but keep this wrapper infallible.
      Err(_) => ResolveExportResult::NotFound,
    }
  }

  fn resolve_export_with_set_budgeted(
    &self,
    graph: &ModuleGraph,
    module: ModuleId,
    export_name: &str,
    resolve_set: &mut Vec<(ModuleId, String)>,
    ctx: &mut ModuleRecordParseCtx<'_>,
  ) -> Result<ResolveExportResult, VmError> {
    ctx.budget_tick()?;

    // 1. For each Record { [[Module]], [[ExportName]] } r of resolveSet, do
    //    a. If module and r.[[Module]] are the same Module Record and SameValue(exportName, r.[[ExportName]]) is true, then
    //       i. Return null.
    for (m, name) in resolve_set.iter() {
      ctx.budget_tick()?;
      if *m == module && name == export_name {
        return Ok(ResolveExportResult::NotFound);
      }
    }

    // 2. Append the Record { [[Module]]: module, [[ExportName]]: exportName } to resolveSet.
    //
    // `export_name` can be attacker-controlled (e.g. extremely long export names), so perform host
    // allocations fallibly to avoid aborting on allocator OOM.
    resolve_set
      .try_reserve_exact(1)
      .map_err(|_| VmError::OutOfMemory)?;
    let mut name = String::new();
    name
      .try_reserve_exact(export_name.len())
      .map_err(|_| VmError::OutOfMemory)?;
    name.push_str(export_name);
    resolve_set.push((module, name));

    // 3. For each ExportEntry Record e of module.[[LocalExportEntries]], do
    for entry in &self.local_export_entries {
      ctx.budget_tick()?;
      // a. If SameValue(exportName, e.[[ExportName]]) is true, then
      if entry.export_name == export_name {
        // i. Assert: module provides the direct binding for this export.
        // ii. Return ResolvedBinding Record { [[Module]]: module, [[BindingName]]: e.[[LocalName]] }.
        return Ok(ResolveExportResult::Resolved(ResolvedBinding {
          module,
          binding_name: BindingName::Name(try_string_from_str(entry.local_name.as_str())?),
        }));
      }
    }

    // 4. For each ExportEntry Record e of module.[[IndirectExportEntries]], do
    for entry in &self.indirect_export_entries {
      ctx.budget_tick()?;
      // a. If SameValue(exportName, e.[[ExportName]]) is true, then
      if entry.export_name == export_name {
        // i. Let importedModule be GetImportedModule(module, e.[[ModuleRequest]]).
        let Some(imported_module) = graph.get_imported_module(module, &entry.module_request) else {
          return Ok(ResolveExportResult::NotFound);
        };
        // ii. If e.[[ImportName]] is all, then
        if entry.import_name == ImportName::All {
          // 1. Return ResolvedBinding Record { [[Module]]: importedModule, [[BindingName]]: namespace }.
          return Ok(ResolveExportResult::Resolved(ResolvedBinding {
            module: imported_module,
            binding_name: BindingName::Namespace,
          }));
        }

        // iii. Else,
        // 1. Assert: e.[[ImportName]] is a String.
        // 2. Return importedModule.ResolveExport(e.[[ImportName]], resolveSet).
        let import_name = match &entry.import_name {
          ImportName::Name(name) => name,
          ImportName::All => {
            debug_assert!(false, "ImportName::All handled above");
            return Ok(ResolveExportResult::NotFound);
          }
        };
        return graph
          .module(imported_module)
          .resolve_export_with_set_budgeted(graph, imported_module, import_name, resolve_set, ctx);
      }
    }

    // 5. If SameValue(exportName, "default") is true, then
    if export_name == "default" {
      // a. Return null.
      return Ok(ResolveExportResult::NotFound);
    }

    // 6. Let starResolution be null.
    let mut star_resolution: Option<ResolvedBinding> = None;

    // 7. For each ExportEntry Record e of module.[[StarExportEntries]], do
    for entry in &self.star_export_entries {
      ctx.budget_tick()?;
      // a. Let importedModule be GetImportedModule(module, e.[[ModuleRequest]]).
      let Some(imported_module) = graph.get_imported_module(module, &entry.module_request) else {
        continue;
      };
      // b. Let resolution be importedModule.ResolveExport(exportName, resolveSet).
      let resolution = graph
        .module(imported_module)
        .resolve_export_with_set_budgeted(graph, imported_module, export_name, resolve_set, ctx)?;

      // c. If resolution is ambiguous, return ambiguous.
      if resolution == ResolveExportResult::Ambiguous {
        return Ok(ResolveExportResult::Ambiguous);
      }

      // d. If resolution is not null, then
      let ResolveExportResult::Resolved(resolution) = resolution else {
        continue;
      };

      // i. If starResolution is null, then
      let Some(existing) = &star_resolution else {
        // 1. Set starResolution to resolution.
        star_resolution = Some(resolution);
        continue;
      };

      // ii. Else,
      // 1. If resolution.[[Module]] and starResolution.[[Module]] are not the same Module Record, return ambiguous.
      // 2. If resolution.[[BindingName]] is not the same as starResolution.[[BindingName]], return ambiguous.
      if existing != &resolution {
        return Ok(ResolveExportResult::Ambiguous);
      }
    }

    // 8. Return starResolution.
    Ok(match star_resolution {
      Some(binding) => ResolveExportResult::Resolved(binding),
      None => ResolveExportResult::NotFound,
    })
  }
}

fn vec_contains_string_with_budget(
  list: &[String],
  needle: &str,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<bool, VmError> {
  for item in list {
    ctx.budget_tick()?;
    if item == needle {
      return Ok(true);
    }
  }
  Ok(false)
}

const MODULE_RECORD_TICK_EVERY: u64 = 256;

struct ModuleRecordParseCtx<'a> {
  steps: u64,
  cancel: &'a mut dyn FnMut() -> Result<(), VmError>,
}

impl<'a> ModuleRecordParseCtx<'a> {
  fn new(cancel: &'a mut dyn FnMut() -> Result<(), VmError>) -> Self {
    Self { steps: 0, cancel }
  }

  fn cancel_now(&mut self) -> Result<(), VmError> {
    (self.cancel)()
  }

  fn budget_tick(&mut self) -> Result<(), VmError> {
    self.steps = self.steps.wrapping_add(1);
    if self.steps % MODULE_RECORD_TICK_EVERY == 0 {
      (self.cancel)()?;
    }
    Ok(())
  }
}

/// Runs module-specific static semantics early errors on a parsed `SourceType::Module` AST.
///
/// This reuses module record extraction so module compilation (`CompiledScript::compile_module*`)
/// rejects the same invalid module programs as [`SourceTextModuleRecord::parse_source`], without
/// parsing the source text twice.
pub(crate) fn validate_module_static_semantics_early_errors(
  top: &Node<TopLevel>,
  cancel: &mut impl FnMut() -> Result<(), VmError>,
) -> Result<(), VmError> {
  // `module_record_from_top_level` performs module record extraction and runs
  // `module_static_semantics_early_errors` as part of the process. The extracted record is not
  // needed here; we only care about surfacing parse-time syntax errors.
  let _ = module_record_from_top_level(top, cancel)?;
  Ok(())
}

fn module_record_from_top_level(
  top: &Node<TopLevel>,
  cancel: &mut impl FnMut() -> Result<(), VmError>,
) -> Result<SourceTextModuleRecord, VmError> {
  let mut ctx = ModuleRecordParseCtx::new(cancel);
  ctx.cancel_now()?;

  let mut record = SourceTextModuleRecord::default();
  record.has_tla = module_contains_top_level_await(top, &mut ctx)?;

  // `export { ... }` (without `from "mod"`) export entries are parsed as `LocalExportEntry` records,
  // but later need to be reclassified as `IndirectExportEntry` records when they re-export an
  // imported binding (including imported module namespace objects).
  //
  // Spec: `ParseModule`, step 10 (`importedBoundNames` re-export conversion).
  let mut pending_export_entries_without_from: Vec<LocalExportEntry> = Vec::new();

  for stmt in &top.stx.body {
    ctx.budget_tick()?;

    match &*stmt.stx {
      Stmt::Import(import_stmt) => {
        if import_stmt.stx.type_only {
          continue;
        }
        let req = module_request_from_specifier(
          &import_stmt.stx.module,
          module_specifier_code_units(&import_stmt.assoc),
          import_stmt.stx.attributes.as_ref(),
          &mut ctx,
        )?;
        // `[[RequestedModules]]`
        push_requested_module(
          &mut record.requested_modules,
          clone_module_request(&req, &mut ctx)?,
          &mut ctx,
        )?;

        // `[[ImportEntries]]`
        //
        // Note: `import "m"` (side-effect only) produces no import entries.
        let mut import_entry_count: usize = 0;
        if import_stmt.stx.default.is_some() {
          import_entry_count = import_entry_count.saturating_add(1);
        }
        if let Some(names) = import_stmt.stx.names.as_ref() {
          match names {
            ImportNames::All(_) => {
              import_entry_count = import_entry_count.saturating_add(1);
            }
            ImportNames::Specific(list) => {
              for name in list {
                ctx.budget_tick()?;
                if name.stx.type_only {
                  continue;
                }
                import_entry_count = import_entry_count.saturating_add(1);
              }
            }
          }
        }

        if import_entry_count != 0 {
          record
            .import_entries
            .try_reserve(import_entry_count)
            .map_err(|_| VmError::OutOfMemory)?;

          // Reuse the parsed module request for one entry to avoid an extra clone.
          let mut req_for_entries = Some(req);
          let mut remaining = import_entry_count;

          let next_req = |ctx: &mut ModuleRecordParseCtx<'_>,
                               remaining: &mut usize,
                               req_for_entries: &mut Option<ModuleRequest>|
           -> Result<ModuleRequest, VmError> {
            debug_assert!(*remaining > 0);
            let is_last = *remaining == 1;
            *remaining = remaining.saturating_sub(1);
            if is_last {
              Ok(req_for_entries
                .take()
                .ok_or(VmError::InvariantViolation("missing module request for import entry"))?)
            } else {
              clone_module_request(
                req_for_entries
                  .as_ref()
                  .ok_or(VmError::InvariantViolation("missing module request for import entry"))?,
                ctx,
              )
            }
          };

            if let Some(default) = &import_stmt.stx.default {
              ctx.budget_tick()?;
              let Pat::Id(id) = &*default.stx.pat.stx else {
                return Err(syntax_error(default.loc, "invalid import binding"));
              };
              record.import_entries.push(ImportEntry {
                module_request: next_req(&mut ctx, &mut remaining, &mut req_for_entries)?,
                import_name: ImportName::Name(try_string_from_str("default")?),
                local_name: try_string_from_identifier_name(&id.stx.name)?,
              });
            }

          if let Some(names) = import_stmt.stx.names.as_ref() {
            match names {
              ImportNames::All(pat_decl) => {
                ctx.budget_tick()?;
                let Pat::Id(id) = &*pat_decl.stx.pat.stx else {
                  return Err(syntax_error(pat_decl.loc, "invalid import binding"));
                };
                record.import_entries.push(ImportEntry {
                  module_request: next_req(&mut ctx, &mut remaining, &mut req_for_entries)?,
                  import_name: ImportName::All,
                  local_name: try_string_from_identifier_name(&id.stx.name)?,
                });
              }
              ImportNames::Specific(list) => {
                for name in list {
                  ctx.budget_tick()?;
                  if name.stx.type_only {
                    continue;
                  }
                  let Pat::Id(id) = &*name.stx.alias.stx.pat.stx else {
                    return Err(syntax_error(name.stx.alias.loc, "invalid import binding"));
                  };
                record.import_entries.push(ImportEntry {
                  module_request: next_req(&mut ctx, &mut remaining, &mut req_for_entries)?,
                  import_name: ImportName::Name(try_string_from_module_export_import_name(&name.stx.importable)?),
                  local_name: try_string_from_identifier_name(&id.stx.name)?,
                });
              }
            }
          }
          }

          debug_assert_eq!(remaining, 0, "import entry count mismatch");
        }
      }

      Stmt::ExportDefaultExpr(_) => {
        record
          .local_export_entries
          .try_reserve(1)
          .map_err(|_| VmError::OutOfMemory)?;
        record.local_export_entries.push(LocalExportEntry {
          export_name: try_string_from_str("default")?,
          local_name: try_string_from_str("*default*")?,
        });
      }

      Stmt::ExportList(export_stmt) => {
        if export_stmt.stx.type_only {
          continue;
        }

        let from = match export_stmt.stx.from.as_ref() {
          Some(specifier) => Some(module_request_from_specifier(
            specifier,
            module_specifier_code_units(&export_stmt.assoc),
            export_stmt.stx.attributes.as_ref(),
            &mut ctx,
          )?),
          None => None,
        };

        if let Some(req) = &from {
          let mut exists = false;
          for existing in &record.requested_modules {
            ctx.budget_tick()?;
            if existing == req {
              exists = true;
              break;
            }
          }
          if !exists {
            record
              .requested_modules
              .try_reserve(1)
              .map_err(|_| VmError::OutOfMemory)?;
            record
              .requested_modules
              .push(clone_module_request(req, &mut ctx)?);
          }
        }

        match (&export_stmt.stx.names, from) {
          (ExportNames::All(None), Some(req)) => {
            record
              .star_export_entries
              .try_reserve(1)
              .map_err(|_| VmError::OutOfMemory)?;
            record.star_export_entries.push(StarExportEntry { module_request: req });
          }
          (ExportNames::All(Some(alias)), Some(req)) => {
            record
              .indirect_export_entries
              .try_reserve(1)
              .map_err(|_| VmError::OutOfMemory)?;
            record.indirect_export_entries.push(IndirectExportEntry {
              export_name: try_string_from_export_name(alias)?,
              module_request: req,
              import_name: ImportName::All,
            });
          }
          (ExportNames::Specific(names), Some(req)) => {
            record
              .indirect_export_entries
              .try_reserve(names.len())
              .map_err(|_| VmError::OutOfMemory)?;
            if let Some((last, rest)) = names.split_last() {
              for name in rest {
                ctx.budget_tick()?;
                record.indirect_export_entries.push(IndirectExportEntry {
                  export_name: try_string_from_export_alias(&name.stx.exportable, name.loc, &name.stx.alias)?,
                  module_request: clone_module_request(&req, &mut ctx)?,
                  import_name: ImportName::Name(try_string_from_module_export_import_name(&name.stx.exportable)?),
                });
              }
              ctx.budget_tick()?;
              record.indirect_export_entries.push(IndirectExportEntry {
                export_name: try_string_from_export_alias(&last.stx.exportable, last.loc, &last.stx.alias)?,
                module_request: req,
                import_name: ImportName::Name(try_string_from_module_export_import_name(&last.stx.exportable)?),
              });
            }
          }
          (ExportNames::Specific(names), None) => {
            pending_export_entries_without_from
              .try_reserve(names.len())
              .map_err(|_| VmError::OutOfMemory)?;
            for name in names {
              ctx.budget_tick()?;
              pending_export_entries_without_from.push(LocalExportEntry {
                export_name: try_string_from_export_alias(&name.stx.exportable, name.loc, &name.stx.alias)?,
                local_name: try_string_from_module_export_import_name(&name.stx.exportable)?,
              });
            }
          }
          (ExportNames::All(_), None) => {}
        }
      }

      Stmt::VarDecl(var_decl) if var_decl.stx.export => {
        record
          .local_export_entries
          .try_reserve(var_decl.stx.declarators.len())
          .map_err(|_| VmError::OutOfMemory)?;

        for declarator in &var_decl.stx.declarators {
          ctx.budget_tick()?;
          push_local_export_entries_from_binding_pat(
            &declarator.pattern.stx.pat,
            &mut record.local_export_entries,
            &mut ctx,
          )?;
        }
      }

      Stmt::FunctionDecl(func) if func.stx.export || func.stx.export_default => {
        record
          .local_export_entries
          .try_reserve(1)
          .map_err(|_| VmError::OutOfMemory)?;
        let local_name = match func.stx.name.as_ref() {
          Some(n) => try_string_from_identifier_name(&n.stx.name)?,
          None => try_string_from_str("*default*")?,
        };
        record.local_export_entries.push(LocalExportEntry {
          export_name: if func.stx.export_default {
            try_string_from_str("default")?
          } else {
            try_string_from_str(&local_name)?
          },
          local_name,
        });
      }

      Stmt::ClassDecl(class) if class.stx.export || class.stx.export_default => {
        record
          .local_export_entries
          .try_reserve(1)
          .map_err(|_| VmError::OutOfMemory)?;
        let local_name = match class.stx.name.as_ref() {
          Some(n) => try_string_from_identifier_name(&n.stx.name)?,
          None => try_string_from_str("*default*")?,
        };
        record.local_export_entries.push(LocalExportEntry {
          export_name: if class.stx.export_default {
            try_string_from_str("default")?
          } else {
            try_string_from_str(&local_name)?
          },
          local_name,
        });
      }

      _ => {}
    }
  }

  // Convert `export { local as exported }` entries that re-export imported bindings (including
  // imported namespace objects) into `[[IndirectExportEntries]]`. This must happen after we have
  // parsed the full `[[ImportEntries]]` list, because import/export declarations can appear in any
  // order within a module.
  if !pending_export_entries_without_from.is_empty() && !record.import_entries.is_empty() {
    // Map `[[LocalName]]` -> import entry for fast lookup.
    let mut import_by_local_name: HashMap<&str, &ImportEntry> = HashMap::new();
    import_by_local_name
      .try_reserve(record.import_entries.len())
      .map_err(|_| VmError::OutOfMemory)?;
    for entry in &record.import_entries {
      ctx.budget_tick()?;
      import_by_local_name.insert(entry.local_name.as_str(), entry);
    }

    for entry in pending_export_entries_without_from {
      ctx.budget_tick()?;
      if let Some(import_entry) = import_by_local_name.get(entry.local_name.as_str()) {
        record
          .indirect_export_entries
          .try_reserve(1)
          .map_err(|_| VmError::OutOfMemory)?;

        // The indirect export entry references the original import, not the local binding.
        let module_request = clone_module_request(&import_entry.module_request, &mut ctx)?;
        let import_name = match &import_entry.import_name {
          ImportName::All => ImportName::All,
          ImportName::Name(name) => ImportName::Name(try_string_from_str(name)?),
        };
        record.indirect_export_entries.push(IndirectExportEntry {
          export_name: entry.export_name,
          module_request,
          import_name,
        });
      } else {
        record
          .local_export_entries
          .try_reserve(1)
          .map_err(|_| VmError::OutOfMemory)?;
        record.local_export_entries.push(entry);
      }
    }
  } else {
    // No imports (or no pending entries): all `export { ... }` entries are local exports.
    if !pending_export_entries_without_from.is_empty() {
      record
        .local_export_entries
        .try_reserve(pending_export_entries_without_from.len())
        .map_err(|_| VmError::OutOfMemory)?;
      record
        .local_export_entries
        .extend(pending_export_entries_without_from);
    }
  }

  module_static_semantics_early_errors(top, &record, &mut ctx)?;

  Ok(record)
}

fn push_local_export_entries_from_binding_pat(
  pat: &Node<Pat>,
  out: &mut Vec<LocalExportEntry>,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<(), VmError> {
  ctx.budget_tick()?;

  match &*pat.stx {
    Pat::Id(id) => {
      let local_name = try_string_from_identifier_name(&id.stx.name)?;
      out.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
      out.push(LocalExportEntry {
        export_name: try_string_from_str(&local_name)?,
        local_name,
      });
      Ok(())
    }
    Pat::Obj(obj) => {
      for prop in &obj.stx.properties {
        ctx.budget_tick()?;
        push_local_export_entries_from_binding_pat(&prop.stx.target, out, ctx)?;
      }
      if let Some(rest) = obj.stx.rest.as_ref() {
        push_local_export_entries_from_binding_pat(rest, out, ctx)?;
      }
      Ok(())
    }
    Pat::Arr(arr) => {
      for elem in &arr.stx.elements {
        ctx.budget_tick()?;
        let Some(elem) = elem.as_ref() else {
          continue;
        };
        push_local_export_entries_from_binding_pat(&elem.target, out, ctx)?;
      }
      if let Some(rest) = arr.stx.rest.as_ref() {
        push_local_export_entries_from_binding_pat(rest, out, ctx)?;
      }
      Ok(())
    }
    Pat::AssignTarget(_) => Err(syntax_error(
      pat.loc,
      "invalid binding pattern in export declaration",
    )),
  }
}

/// Minimal module static-semantics early errors needed for test262 `negative.phase: parse`
/// module-code coverage.
///
/// These checks intentionally run during module record extraction so failures are surfaced as
/// `VmError::Syntax` (parse phase), not as runtime exceptions during evaluation/instantiation.
fn module_static_semantics_early_errors(
  top: &Node<TopLevel>,
  record: &SourceTextModuleRecord,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<(), VmError> {
  ctx.budget_tick()?;

  // `ModuleExportName : StringLiteral` and `ModuleImportName : StringLiteral`
  // require `IsStringWellFormedUnicode` to be true.
  module_export_import_name_string_literals_well_formed_unicode(top, ctx)?;

  let var_declared_names = module_var_declared_names(top, ctx)?;
  let lex_declared_names = module_lex_declared_names(top, ctx)?;

  // It is a Syntax Error if any element of the LexicallyDeclaredNames of ModuleItemList also
  // occurs in the VarDeclaredNames of ModuleItemList.
  module_lex_var_declared_names_do_not_intersect(
    &var_declared_names,
    &lex_declared_names,
    top.loc,
    ctx,
  )?;

  // It is a Syntax Error if ContainsDuplicateLabels of ModuleItemList with argument « » is true.
  if module_contains_duplicate_labels(top, ctx)? {
    return Err(syntax_error(top.loc, "duplicate label"));
  }

  // Import-bound names (+ strict-mode restrictions + collisions).
  let import_bound_names =
    module_import_bound_names(record, &var_declared_names, &lex_declared_names, top.loc, ctx)?;

  // ExportedNames uniqueness.
  module_exported_names_unique(record, top.loc, ctx)?;

  // ExportedBindings must be declared.
  module_exported_bindings_declared(
    record,
    &var_declared_names,
    &lex_declared_names,
    &import_bound_names,
    top.loc,
    ctx,
  )?;

  Ok(())
}

fn module_var_declared_names(
  top: &Node<TopLevel>,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<HashSet<String>, VmError> {
  ctx.budget_tick()?;

  let mut names = HashSet::<String>::new();
  // Heuristic reserve: at least one name per top-level statement.
  names
    .try_reserve(top.stx.body.len())
    .map_err(|_| VmError::OutOfMemory)?;

  // `var` is module-scoped even when nested inside blocks/loops/etc.
  module_var_declared_names_from_stmt_list(&top.stx.body, &mut names, ctx)?;
  Ok(names)
}

fn module_var_declared_names_from_stmt_list(
  stmts: &[Node<Stmt>],
  out: &mut HashSet<String>,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<(), VmError> {
  for stmt in stmts {
    ctx.budget_tick()?;
    module_var_declared_names_from_stmt(stmt, out, ctx)?;
  }
  Ok(())
}

fn module_var_declared_names_from_stmt(
  stmt: &Node<Stmt>,
  out: &mut HashSet<String>,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<(), VmError> {
  ctx.budget_tick()?;

  match &*stmt.stx {
    Stmt::VarDecl(decl) if decl.stx.mode == VarDeclMode::Var => {
      for declarator in &decl.stx.declarators {
        ctx.budget_tick()?;
        pat_decl_bound_names(&declarator.pattern.stx, out, ctx)?;
      }
    }

    // Control-flow / statement-list containers.
    Stmt::Block(block) => {
      module_var_declared_names_from_stmt_list(&block.stx.body, out, ctx)?;
    }
    Stmt::DoWhile(stmt) => module_var_declared_names_from_stmt(&stmt.stx.body, out, ctx)?,
    Stmt::If(stmt) => {
      module_var_declared_names_from_stmt(&stmt.stx.consequent, out, ctx)?;
      if let Some(alt) = stmt.stx.alternate.as_ref() {
        module_var_declared_names_from_stmt(alt, out, ctx)?;
      }
    }
    Stmt::While(stmt) => module_var_declared_names_from_stmt(&stmt.stx.body, out, ctx)?,
    Stmt::ForTriple(stmt) => {
      match &stmt.stx.init {
        parse_js::ast::stmt::ForTripleStmtInit::Decl(decl) if decl.stx.mode == VarDeclMode::Var => {
          for declarator in &decl.stx.declarators {
            ctx.budget_tick()?;
            pat_decl_bound_names(&declarator.pattern.stx, out, ctx)?;
          }
        }
        _ => {}
      }
      module_var_declared_names_from_stmt_list(&stmt.stx.body.stx.body, out, ctx)?;
    }
    Stmt::ForIn(stmt) => {
      if let ForInOfLhs::Decl((mode, pat_decl)) = &stmt.stx.lhs {
        if *mode == VarDeclMode::Var {
          pat_decl_bound_names(&pat_decl.stx, out, ctx)?;
        }
      }
      module_var_declared_names_from_stmt_list(&stmt.stx.body.stx.body, out, ctx)?;
    }
    Stmt::ForOf(stmt) => {
      if let ForInOfLhs::Decl((mode, pat_decl)) = &stmt.stx.lhs {
        if *mode == VarDeclMode::Var {
          pat_decl_bound_names(&pat_decl.stx, out, ctx)?;
        }
      }
      module_var_declared_names_from_stmt_list(&stmt.stx.body.stx.body, out, ctx)?;
    }
    Stmt::Label(stmt) => {
      // Labels do not introduce a new scope.
      module_var_declared_names_from_stmt(&stmt.stx.statement, out, ctx)?;
    }
    Stmt::Switch(stmt) => {
      for branch in &stmt.stx.branches {
        ctx.budget_tick()?;
        module_var_declared_names_from_stmt_list(&branch.stx.body, out, ctx)?;
      }
    }
    Stmt::Try(stmt) => {
      module_var_declared_names_from_stmt_list(&stmt.stx.wrapped.stx.body, out, ctx)?;
      if let Some(catch) = stmt.stx.catch.as_ref() {
        module_var_declared_names_from_stmt_list(&catch.stx.body, out, ctx)?;
      }
      if let Some(finally) = stmt.stx.finally.as_ref() {
        module_var_declared_names_from_stmt_list(&finally.stx.body, out, ctx)?;
      }
    }
    Stmt::With(stmt) => module_var_declared_names_from_stmt(&stmt.stx.body, out, ctx)?,

    // Class bodies are boundaries: do not descend.
    Stmt::ClassDecl(_) => {}

    // Everything else either contains no statements or only expressions.
    _ => {}
  }

  Ok(())
}

fn module_lex_declared_names(
  top: &Node<TopLevel>,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<HashSet<String>, VmError> {
  ctx.budget_tick()?;

  let mut names = HashSet::<String>::new();
  names
    .try_reserve(top.stx.body.len())
    .map_err(|_| VmError::OutOfMemory)?;

  for stmt in &top.stx.body {
    ctx.budget_tick()?;

    match &*stmt.stx {
      Stmt::VarDecl(decl)
        if matches!(
          decl.stx.mode,
          VarDeclMode::Let | VarDeclMode::Const | VarDeclMode::Using | VarDeclMode::AwaitUsing
        ) =>
      {
        for declarator in &decl.stx.declarators {
          ctx.budget_tick()?;
          pat_decl_bound_names_no_dupes(&declarator.pattern.stx, &mut names, ctx)?;
        }
      }

      Stmt::FunctionDecl(decl) => {
        let Some(name) = decl.stx.name.as_ref() else {
          continue;
        };
        ctx.budget_tick()?;
        names.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
        if !names.insert(try_string_from_str(&name.stx.name)?) {
          return Err(syntax_error(name.loc, "duplicate lexically declared name"));
        }
      }

      Stmt::ClassDecl(decl) => {
        let Some(name) = decl.stx.name.as_ref() else {
          continue;
        };
        ctx.budget_tick()?;
        names.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
        if !names.insert(try_string_from_str(&name.stx.name)?) {
          return Err(syntax_error(name.loc, "duplicate lexically declared name"));
        }
      }

      _ => {}
    }
  }

  Ok(names)
}

fn pat_decl_bound_names(
  pat: &parse_js::ast::stmt::decl::PatDecl,
  out: &mut HashSet<String>,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<(), VmError> {
  pat_bound_names(&pat.pat, out, ctx)
}

fn pat_bound_names(
  pat: &Node<Pat>,
  out: &mut HashSet<String>,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<(), VmError> {
  ctx.budget_tick()?;

  match &*pat.stx {
    Pat::Id(id) => {
      out.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
      out.insert(try_string_from_str(&id.stx.name)?);
    }
    Pat::Arr(arr) => {
      for elem in &arr.stx.elements {
        let Some(elem) = elem.as_ref() else {
          // Array patterns can contain arbitrarily many elisions (`[,,,,x]`); ensure the traversal
          // can't do `O(N)` work without ticking.
          ctx.budget_tick()?;
          continue;
        };
        pat_bound_names(&elem.target, out, ctx)?;
      }
      if let Some(rest) = arr.stx.rest.as_ref() {
        pat_bound_names(rest, out, ctx)?;
      }
    }
    Pat::Obj(obj) => {
      for prop in &obj.stx.properties {
        ctx.budget_tick()?;
        pat_bound_names(&prop.stx.target, out, ctx)?;
      }
      if let Some(rest) = obj.stx.rest.as_ref() {
        pat_bound_names(rest, out, ctx)?;
      }
    }
    // Assignment targets are not valid binding patterns, but can appear in the AST for recovery.
    Pat::AssignTarget(_) => {}
  }

  Ok(())
}

fn pat_decl_bound_names_no_dupes(
  pat: &parse_js::ast::stmt::decl::PatDecl,
  out: &mut HashSet<String>,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<(), VmError> {
  pat_bound_names_no_dupes(&pat.pat, out, ctx)
}

fn pat_bound_names_no_dupes(
  pat: &Node<Pat>,
  out: &mut HashSet<String>,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<(), VmError> {
  ctx.budget_tick()?;

  match &*pat.stx {
    Pat::Id(id) => {
      out.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
      if !out.insert(try_string_from_str(&id.stx.name)?) {
        return Err(syntax_error(pat.loc, "duplicate lexically declared name"));
      }
    }
    Pat::Arr(arr) => {
      for elem in &arr.stx.elements {
        let Some(elem) = elem.as_ref() else {
          // Array patterns can contain arbitrarily many elisions (`[,,,,x]`); ensure the traversal
          // can't do `O(N)` work without ticking.
          ctx.budget_tick()?;
          continue;
        };
        pat_bound_names_no_dupes(&elem.target, out, ctx)?;
      }
      if let Some(rest) = arr.stx.rest.as_ref() {
        pat_bound_names_no_dupes(rest, out, ctx)?;
      }
    }
    Pat::Obj(obj) => {
      for prop in &obj.stx.properties {
        ctx.budget_tick()?;
        pat_bound_names_no_dupes(&prop.stx.target, out, ctx)?;
      }
      if let Some(rest) = obj.stx.rest.as_ref() {
        pat_bound_names_no_dupes(rest, out, ctx)?;
      }
    }
    // Assignment targets are not valid binding patterns, but can appear in the AST for recovery.
    Pat::AssignTarget(_) => {}
  }

  Ok(())
}

fn module_lex_var_declared_names_do_not_intersect(
  var_declared_names: &HashSet<String>,
  lex_declared_names: &HashSet<String>,
  loc: parse_js::loc::Loc,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<(), VmError> {
  for name in lex_declared_names {
    ctx.budget_tick()?;
    if var_declared_names.contains(name.as_str()) {
      return Err(syntax_error(
        loc,
        "lexical declaration collides with var declaration",
      ));
    }
  }
  Ok(())
}

fn module_contains_duplicate_labels(
  top: &Node<TopLevel>,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<bool, VmError> {
  let mut stack: Vec<&str> = Vec::new();
  stmt_list_contains_duplicate_labels(&top.stx.body, &mut stack, ctx)
}

fn stmt_list_contains_duplicate_labels<'a>(
  stmts: &'a [Node<Stmt>],
  label_stack: &mut Vec<&'a str>,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<bool, VmError> {
  for stmt in stmts {
    if stmt_contains_duplicate_labels(stmt, label_stack, ctx)? {
      return Ok(true);
    }
  }
  Ok(false)
}

fn stmt_contains_duplicate_labels<'a>(
  stmt: &'a Node<Stmt>,
  label_stack: &mut Vec<&'a str>,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<bool, VmError> {
  ctx.budget_tick()?;

  Ok(match &*stmt.stx {
    Stmt::Label(labelled) => {
      let name = labelled.stx.name.as_str();
      for existing in label_stack.iter() {
        ctx.budget_tick()?;
        if *existing == name {
          return Ok(true);
        }
      }
      label_stack.push(name);
      let found = stmt_contains_duplicate_labels(&labelled.stx.statement, label_stack, ctx)?;
      label_stack.pop();
      found
    }

    Stmt::Block(block) => stmt_list_contains_duplicate_labels(&block.stx.body, label_stack, ctx)?,
    Stmt::DoWhile(stmt) => stmt_contains_duplicate_labels(&stmt.stx.body, label_stack, ctx)?,
    Stmt::If(stmt) => {
      if stmt_contains_duplicate_labels(&stmt.stx.consequent, label_stack, ctx)? {
        true
      } else if let Some(alt) = stmt.stx.alternate.as_ref() {
        stmt_contains_duplicate_labels(alt, label_stack, ctx)?
      } else {
        false
      }
    }
    Stmt::While(stmt) => stmt_contains_duplicate_labels(&stmt.stx.body, label_stack, ctx)?,
    Stmt::ForTriple(stmt) => {
      stmt_list_contains_duplicate_labels(&stmt.stx.body.stx.body, label_stack, ctx)?
    }
    Stmt::ForIn(stmt) => {
      stmt_list_contains_duplicate_labels(&stmt.stx.body.stx.body, label_stack, ctx)?
    }
    Stmt::ForOf(stmt) => {
      stmt_list_contains_duplicate_labels(&stmt.stx.body.stx.body, label_stack, ctx)?
    }
    Stmt::Switch(stmt) => {
      let mut found = false;
      for branch in &stmt.stx.branches {
        ctx.budget_tick()?;
        if stmt_list_contains_duplicate_labels(&branch.stx.body, label_stack, ctx)? {
          found = true;
          break;
        }
      }
      found
    }
    Stmt::Try(stmt) => {
      if stmt_list_contains_duplicate_labels(&stmt.stx.wrapped.stx.body, label_stack, ctx)? {
        true
      } else if let Some(catch) = stmt.stx.catch.as_ref() {
        if stmt_list_contains_duplicate_labels(&catch.stx.body, label_stack, ctx)? {
          true
        } else if let Some(finally) = stmt.stx.finally.as_ref() {
          stmt_list_contains_duplicate_labels(&finally.stx.body, label_stack, ctx)?
        } else {
          false
        }
      } else if let Some(finally) = stmt.stx.finally.as_ref() {
        stmt_list_contains_duplicate_labels(&finally.stx.body, label_stack, ctx)?
      } else {
        false
      }
    }
    Stmt::With(stmt) => stmt_contains_duplicate_labels(&stmt.stx.body, label_stack, ctx)?,

    // Function-like boundaries: do not descend.
    Stmt::FunctionDecl(_) => false,

    // Everything else is leaf-like for label scanning.
    _ => false,
  })
}

fn module_import_bound_names<'a>(
  record: &'a SourceTextModuleRecord,
  var_declared_names: &HashSet<String>,
  lex_declared_names: &HashSet<String>,
  loc: parse_js::loc::Loc,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<HashSet<&'a str>, VmError> {
  ctx.budget_tick()?;

  let mut names = HashSet::<&'a str>::new();
  names
    .try_reserve(record.import_entries.len())
    .map_err(|_| VmError::OutOfMemory)?;

  for entry in &record.import_entries {
    ctx.budget_tick()?;

    let local_name = entry.local_name.as_str();

    // Modules are strict: `eval`/`arguments` are RestrictedIdentifiers in binding positions.
    if local_name == "eval" || local_name == "arguments" {
      return Err(syntax_error(
        loc,
        "imported bindings must not be eval or arguments in modules",
      ));
    }

    // `import` introduces module-scope bindings; they must not collide with local declarations.
    if var_declared_names.contains(local_name) || lex_declared_names.contains(local_name) {
      return Err(syntax_error(loc, "imported binding collides with local declaration"));
    }

    // Import-bound names must be unique.
    if !names.insert(local_name) {
      return Err(syntax_error(loc, "duplicate import binding name"));
    }
  }

  Ok(names)
}

fn module_exported_names_unique(
  record: &SourceTextModuleRecord,
  loc: parse_js::loc::Loc,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<(), VmError> {
  ctx.budget_tick()?;

  let mut names = HashSet::<&str>::new();
  names
    .try_reserve(record.local_export_entries.len() + record.indirect_export_entries.len())
    .map_err(|_| VmError::OutOfMemory)?;

  for entry in &record.local_export_entries {
    ctx.budget_tick()?;
    if !names.insert(entry.export_name.as_str()) {
      return Err(syntax_error(loc, "duplicate exported name"));
    }
  }
  for entry in &record.indirect_export_entries {
    ctx.budget_tick()?;
    if !names.insert(entry.export_name.as_str()) {
      return Err(syntax_error(loc, "duplicate exported name"));
    }
  }

  Ok(())
}

fn module_exported_bindings_declared(
  record: &SourceTextModuleRecord,
  var_declared_names: &HashSet<String>,
  lex_declared_names: &HashSet<String>,
  import_bound_names: &HashSet<&str>,
  loc: parse_js::loc::Loc,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<(), VmError> {
  ctx.budget_tick()?;

  for entry in &record.local_export_entries {
    ctx.budget_tick()?;

    // `export default <expr>` and unnamed default export declarations do not create a binding name.
    if entry.local_name == "*default*" {
      continue;
    }

    let local_name = entry.local_name.as_str();
    if var_declared_names.contains(local_name)
      || lex_declared_names.contains(local_name)
      || import_bound_names.contains(local_name)
    {
      continue;
    }

    return Err(syntax_error(loc, "exported binding must refer to a declared name"));
  }

  Ok(())
}

fn module_export_import_name_string_literals_well_formed_unicode(
  top: &Node<TopLevel>,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<(), VmError> {
  ctx.budget_tick()?;

  for stmt in &top.stx.body {
    ctx.budget_tick()?;

    match &*stmt.stx {
      Stmt::Import(import_stmt) => {
        if import_stmt.stx.type_only {
          continue;
        }
        let Some(names) = import_stmt.stx.names.as_ref() else {
          continue;
        };
        if let ImportNames::Specific(list) = names {
          for name in list {
            ctx.budget_tick()?;
            if name.stx.type_only {
              continue;
            }
            let parse_js::ast::stmt::decl::PatDecl { pat } = &*name.stx.alias.stx;
            let Pat::Id(id) = &*pat.stx else {
              continue;
            };
            if let Some(code_units) = module_export_import_name_code_units(&id.assoc) {
              validate_string_well_formed_unicode(code_units, id.loc, ctx)?;
            }
          }
        }
      }

      Stmt::ExportList(export_stmt) => {
        if export_stmt.stx.type_only {
          continue;
        }

        match &export_stmt.stx.names {
          ExportNames::Specific(list) => {
            for name in list {
              ctx.budget_tick()?;
              if name.stx.type_only {
                continue;
              }

              // Validate the `ModuleExportName : StringLiteral` target (left-hand side).
              if let Some(code_units) = module_export_import_name_code_units(&name.stx.alias.assoc) {
                validate_string_well_formed_unicode(code_units, name.stx.alias.loc, ctx)?;
              }
              // Validate the exported alias (`as "name"`).
              if let Some(code_units) = literal_string_code_units(&name.stx.alias.assoc) {
                validate_string_well_formed_unicode(code_units, name.stx.alias.loc, ctx)?;
              }
            }
          }
          ExportNames::All(Some(alias)) => {
            if let Some(code_units) = literal_string_code_units(&alias.assoc) {
              validate_string_well_formed_unicode(code_units, alias.loc, ctx)?;
            }
          }
          ExportNames::All(None) => {}
        }
      }

      _ => {}
    }
  }

  Ok(())
}

fn validate_string_well_formed_unicode(
  code_units: &[u16],
  loc: parse_js::loc::Loc,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<(), VmError> {
  if !string_is_well_formed_unicode(code_units, ctx)? {
    return Err(syntax_error(
      loc,
      "module import/export string literals must be well-formed unicode",
    ));
  }
  Ok(())
}

fn string_is_well_formed_unicode(
  code_units: &[u16],
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<bool, VmError> {
  let mut i: usize = 0;
  while i < code_units.len() {
    ctx.budget_tick()?;

    let cu = code_units[i];
    if (0xD800..=0xDBFF).contains(&cu) {
      // High surrogate must be followed by a low surrogate.
      if i + 1 >= code_units.len() {
        return Ok(false);
      }
      let next = code_units[i + 1];
      if !(0xDC00..=0xDFFF).contains(&next) {
        return Ok(false);
      }
      i = i.saturating_add(2);
      continue;
    }
    if (0xDC00..=0xDFFF).contains(&cu) {
      // Low surrogate without a preceding high surrogate.
      return Ok(false);
    }
    i = i.saturating_add(1);
  }
  Ok(true)
}

fn module_contains_top_level_await(
  top: &Node<TopLevel>,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<bool, VmError> {
  stmt_list_contains_top_level_await(&top.stx.body, ctx)
}

fn stmt_list_contains_top_level_await(
  stmts: &[Node<Stmt>],
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<bool, VmError> {
  for stmt in stmts {
    if stmt_contains_top_level_await(stmt, ctx)? {
      return Ok(true);
    }
  }
  Ok(false)
}

fn stmt_contains_top_level_await(
  stmt: &Node<Stmt>,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<bool, VmError> {
  ctx.budget_tick()?;

  Ok(match &*stmt.stx {
    Stmt::Expr(expr_stmt) => expr_contains_top_level_await(&expr_stmt.stx.expr, ctx)?,
    Stmt::Block(block) => stmt_list_contains_top_level_await(&block.stx.body, ctx)?,
    Stmt::DoWhile(stmt) => {
      expr_contains_top_level_await(&stmt.stx.condition, ctx)?
        || stmt_contains_top_level_await(&stmt.stx.body, ctx)?
    }
    Stmt::If(stmt) => {
      if expr_contains_top_level_await(&stmt.stx.test, ctx)? {
        true
      } else if stmt_contains_top_level_await(&stmt.stx.consequent, ctx)? {
        true
      } else if let Some(alt) = stmt.stx.alternate.as_ref() {
        stmt_contains_top_level_await(alt, ctx)?
      } else {
        false
      }
    }
    Stmt::While(stmt) => {
      expr_contains_top_level_await(&stmt.stx.condition, ctx)?
        || stmt_contains_top_level_await(&stmt.stx.body, ctx)?
    }
    Stmt::ForTriple(stmt) => {
      let init_has = match &stmt.stx.init {
        parse_js::ast::stmt::ForTripleStmtInit::None => false,
        parse_js::ast::stmt::ForTripleStmtInit::Expr(expr) => expr_contains_top_level_await(expr, ctx)?,
        parse_js::ast::stmt::ForTripleStmtInit::Decl(decl) => {
          var_decl_contains_top_level_await(&decl.stx, ctx)?
        }
      };
      let cond_has = match stmt.stx.cond.as_ref() {
        Some(expr) => expr_contains_top_level_await(expr, ctx)?,
        None => false,
      };
      let post_has = match stmt.stx.post.as_ref() {
        Some(expr) => expr_contains_top_level_await(expr, ctx)?,
        None => false,
      };
      init_has || cond_has || post_has || stmt_list_contains_top_level_await(&stmt.stx.body.stx.body, ctx)?
    }
    Stmt::ForIn(stmt) => {
      for_in_of_lhs_contains_top_level_await(&stmt.stx.lhs, ctx)?
        || expr_contains_top_level_await(&stmt.stx.rhs, ctx)?
        || stmt_list_contains_top_level_await(&stmt.stx.body.stx.body, ctx)?
    }
    Stmt::ForOf(stmt) => {
      if stmt.stx.await_ {
        true
      } else {
        for_in_of_lhs_contains_top_level_await(&stmt.stx.lhs, ctx)?
          || expr_contains_top_level_await(&stmt.stx.rhs, ctx)?
          || stmt_list_contains_top_level_await(&stmt.stx.body.stx.body, ctx)?
      }
    }
    Stmt::Label(stmt) => stmt_contains_top_level_await(&stmt.stx.statement, ctx)?,
    Stmt::Switch(stmt) => {
      if expr_contains_top_level_await(&stmt.stx.test, ctx)? {
        true
      } else {
        let mut found = false;
        for branch in &stmt.stx.branches {
          if let Some(case) = branch.stx.case.as_ref() {
            if expr_contains_top_level_await(case, ctx)? {
              found = true;
              break;
            }
          }
          if stmt_list_contains_top_level_await(&branch.stx.body, ctx)? {
            found = true;
            break;
          }
        }
        found
      }
    }
    Stmt::Throw(stmt) => expr_contains_top_level_await(&stmt.stx.value, ctx)?,
    Stmt::Try(stmt) => {
      if stmt_list_contains_top_level_await(&stmt.stx.wrapped.stx.body, ctx)? {
        true
      } else {
        if let Some(catch) = stmt.stx.catch.as_ref() {
          if let Some(param) = catch.stx.parameter.as_ref() {
            if pat_contains_top_level_await(&param.stx.pat, ctx)? {
              return Ok(true);
            }
          }
          if stmt_list_contains_top_level_await(&catch.stx.body, ctx)? {
            return Ok(true);
          }
        }
        if let Some(finally) = stmt.stx.finally.as_ref() {
          if stmt_list_contains_top_level_await(&finally.stx.body, ctx)? {
            return Ok(true);
          }
        }
        false
      }
    }
    Stmt::With(stmt) => {
      expr_contains_top_level_await(&stmt.stx.object, ctx)?
        || stmt_contains_top_level_await(&stmt.stx.body, ctx)?
    }

    // Import/export statements.
    Stmt::ExportDefaultExpr(stmt) => expr_contains_top_level_await(&stmt.stx.expression, ctx)?,
    Stmt::ExportList(stmt) => match stmt.stx.attributes.as_ref() {
      Some(attributes) => expr_contains_top_level_await(attributes, ctx)?,
      None => false,
    },
    Stmt::Import(stmt) => match stmt.stx.attributes.as_ref() {
      Some(attributes) => expr_contains_top_level_await(attributes, ctx)?,
      None => false,
    },

    // Declarations.
    Stmt::ClassDecl(stmt) => class_decl_contains_top_level_await(&stmt.stx, ctx)?,
    Stmt::VarDecl(stmt) => var_decl_contains_top_level_await(&stmt.stx, ctx)?,

    // Function-like boundaries: do not descend.
    Stmt::FunctionDecl(_) => false,

    // Everything else cannot contain `await` (or is syntax-error in modules).
    _ => false,
  })
}

fn var_decl_contains_top_level_await(
  decl: &parse_js::ast::stmt::decl::VarDecl,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<bool, VmError> {
  ctx.budget_tick()?;

  for d in &decl.declarators {
    if pat_contains_top_level_await(&d.pattern.stx.pat, ctx)? {
      return Ok(true);
    }
    if let Some(init) = d.initializer.as_ref() {
      if expr_contains_top_level_await(init, ctx)? {
        return Ok(true);
      }
    }
  }
  Ok(false)
}

fn for_in_of_lhs_contains_top_level_await(
  lhs: &ForInOfLhs,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<bool, VmError> {
  ctx.budget_tick()?;

  match lhs {
    ForInOfLhs::Assign(pat) => pat_contains_top_level_await(pat, ctx),
    ForInOfLhs::Decl((_mode, pat_decl)) => pat_contains_top_level_await(&pat_decl.stx.pat, ctx),
  }
}

fn expr_contains_top_level_await(
  expr: &Node<Expr>,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<bool, VmError> {
  ctx.budget_tick()?;

  Ok(match &*expr.stx {
    Expr::Unary(unary) => {
      if unary.stx.operator == OperatorName::Await {
        true
      } else {
        expr_contains_top_level_await(&unary.stx.argument, ctx)?
      }
    }
    Expr::UnaryPostfix(unary) => expr_contains_top_level_await(&unary.stx.argument, ctx)?,
    Expr::Binary(binary) => {
      expr_contains_top_level_await(&binary.stx.left, ctx)?
        || expr_contains_top_level_await(&binary.stx.right, ctx)?
    }
    Expr::Call(call) => {
      if expr_contains_top_level_await(&call.stx.callee, ctx)? {
        true
      } else {
        let mut found = false;
        for arg in &call.stx.arguments {
          if expr_contains_top_level_await(&arg.stx.value, ctx)? {
            found = true;
            break;
          }
        }
        found
      }
    }
    Expr::ComputedMember(member) => {
      expr_contains_top_level_await(&member.stx.object, ctx)?
        || expr_contains_top_level_await(&member.stx.member, ctx)?
    }
    Expr::Cond(cond) => {
      expr_contains_top_level_await(&cond.stx.test, ctx)?
        || expr_contains_top_level_await(&cond.stx.consequent, ctx)?
        || expr_contains_top_level_await(&cond.stx.alternate, ctx)?
    }
    Expr::Import(expr) => {
      if expr_contains_top_level_await(&expr.stx.module, ctx)? {
        true
      } else if let Some(attrs) = expr.stx.attributes.as_ref() {
        expr_contains_top_level_await(attrs, ctx)?
      } else {
        false
      }
    }
    Expr::Member(member) => expr_contains_top_level_await(&member.stx.left, ctx)?,
    Expr::TaggedTemplate(template) => {
      if expr_contains_top_level_await(&template.stx.function, ctx)? {
        true
      } else {
        let mut found = false;
        for part in &template.stx.parts {
          if let LitTemplatePart::Substitution(expr) = part {
            if expr_contains_top_level_await(expr, ctx)? {
              found = true;
              break;
            }
          }
        }
        found
      }
    }

    Expr::LitArr(arr) => {
      let mut found = false;
      for elem in &arr.stx.elements {
        match elem {
          LitArrElem::Single(expr) | LitArrElem::Rest(expr) => {
            if expr_contains_top_level_await(expr, ctx)? {
              found = true;
              break;
            }
          }
          LitArrElem::Empty => {
            // Array literals can contain arbitrarily many elisions (`[,,,,]`) without any nested
            // expressions. Budget traversal so module record parsing can't do `O(N)` work without
            // calling the cancel/budget hook.
            ctx.budget_tick()?;
          }
        }
      }
      found
    }
    Expr::LitObj(obj) => {
      let mut found = false;
      for member in &obj.stx.members {
        if obj_member_contains_top_level_await(member, ctx)? {
          found = true;
          break;
        }
      }
      found
    }
    Expr::LitTemplate(template) => {
      let mut found = false;
      for part in &template.stx.parts {
        match part {
          LitTemplatePart::Substitution(expr) => {
            if expr_contains_top_level_await(expr, ctx)? {
              found = true;
              break;
            }
          }
          LitTemplatePart::String(_) => {}
        }
      }
      found
    }

    // Class expressions are not function boundaries: only method bodies are.
    Expr::Class(class) => class_expr_contains_top_level_await(&class.stx, ctx)?,

    // Patterns (can contain expressions via default values).
    Expr::ArrPat(arr) => arr_pat_contains_top_level_await(&arr.stx, ctx)?,
    Expr::IdPat(_) => false,
    Expr::ObjPat(obj) => obj_pat_contains_top_level_await(&obj.stx, ctx)?,

    // TypeScript wrappers around expressions.
    Expr::Instantiation(expr) => expr_contains_top_level_await(&expr.stx.expression, ctx)?,
    Expr::TypeAssertion(expr) => expr_contains_top_level_await(&expr.stx.expression, ctx)?,
    Expr::NonNullAssertion(expr) => expr_contains_top_level_await(&expr.stx.expression, ctx)?,
    Expr::SatisfiesExpr(expr) => expr_contains_top_level_await(&expr.stx.expression, ctx)?,

    // Function-like boundaries: do not descend.
    Expr::ArrowFunc(_) | Expr::Func(_) => false,

    // Everything else is leaf-like for our purposes.
    _ => false,
  })
}

fn pat_contains_top_level_await(
  pat: &Node<Pat>,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<bool, VmError> {
  ctx.budget_tick()?;

  match &*pat.stx {
    Pat::Arr(arr) => arr_pat_contains_top_level_await(&arr.stx, ctx),
    Pat::Obj(obj) => obj_pat_contains_top_level_await(&obj.stx, ctx),
    Pat::AssignTarget(expr) => expr_contains_top_level_await(expr, ctx),
    Pat::Id(_) => Ok(false),
  }
}

fn arr_pat_contains_top_level_await(
  pat: &parse_js::ast::expr::pat::ArrPat,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<bool, VmError> {
  ctx.budget_tick()?;

  for elem in &pat.elements {
    let Some(elem) = elem.as_ref() else {
      // Array patterns can contain arbitrarily many elisions (`[,,,,x]`) without any nested
      // patterns/expressions. Budget traversal so top-level-await scanning can't do `O(N)` work
      // without calling the cancel/budget hook.
      ctx.budget_tick()?;
      continue;
    };
    if pat_contains_top_level_await(&elem.target, ctx)? {
      return Ok(true);
    }
    if let Some(default) = elem.default_value.as_ref() {
      if expr_contains_top_level_await(default, ctx)? {
        return Ok(true);
      }
    }
  }

  if let Some(rest) = pat.rest.as_ref() {
    return pat_contains_top_level_await(rest, ctx);
  }
  Ok(false)
}

fn obj_pat_contains_top_level_await(
  pat: &parse_js::ast::expr::pat::ObjPat,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<bool, VmError> {
  ctx.budget_tick()?;

  for prop in &pat.properties {
    if class_or_obj_key_contains_top_level_await(&prop.stx.key, ctx)? {
      return Ok(true);
    }
    if pat_contains_top_level_await(&prop.stx.target, ctx)? {
      return Ok(true);
    }
    if let Some(default) = prop.stx.default_value.as_ref() {
      if expr_contains_top_level_await(default, ctx)? {
        return Ok(true);
      }
    }
  }

  if let Some(rest) = pat.rest.as_ref() {
    return pat_contains_top_level_await(rest, ctx);
  }
  Ok(false)
}

fn class_decl_contains_top_level_await(
  class: &parse_js::ast::stmt::decl::ClassDecl,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<bool, VmError> {
  ctx.budget_tick()?;

  for d in &class.decorators {
    if expr_contains_top_level_await(&d.stx.expression, ctx)? {
      return Ok(true);
    }
  }
  if let Some(extends) = class.extends.as_ref() {
    if expr_contains_top_level_await(extends, ctx)? {
      return Ok(true);
    }
  }
  for imp in &class.implements {
    if expr_contains_top_level_await(imp, ctx)? {
      return Ok(true);
    }
  }
  for member in &class.members {
    if class_member_contains_top_level_await(member, ctx)? {
      return Ok(true);
    }
  }
  Ok(false)
}

fn class_expr_contains_top_level_await(
  class: &parse_js::ast::expr::ClassExpr,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<bool, VmError> {
  ctx.budget_tick()?;

  for d in &class.decorators {
    if expr_contains_top_level_await(&d.stx.expression, ctx)? {
      return Ok(true);
    }
  }
  if let Some(extends) = class.extends.as_ref() {
    if expr_contains_top_level_await(extends, ctx)? {
      return Ok(true);
    }
  }
  for member in &class.members {
    if class_member_contains_top_level_await(member, ctx)? {
      return Ok(true);
    }
  }
  Ok(false)
}

fn class_member_contains_top_level_await(
  member: &Node<ClassMember>,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<bool, VmError> {
  ctx.budget_tick()?;

  for d in &member.stx.decorators {
    if expr_contains_top_level_await(&d.stx.expression, ctx)? {
      return Ok(true);
    }
  }
  Ok(
    class_or_obj_key_contains_top_level_await(&member.stx.key, ctx)?
      || class_or_obj_val_contains_top_level_await(&member.stx.val, ctx)?,
  )
}

fn obj_member_contains_top_level_await(
  member: &Node<ObjMember>,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<bool, VmError> {
  ctx.budget_tick()?;

  match &member.stx.typ {
    ObjMemberType::Valued { key, val } => Ok(
      class_or_obj_key_contains_top_level_await(key, ctx)?
        || class_or_obj_val_contains_top_level_await(val, ctx)?,
    ),
    ObjMemberType::Shorthand { .. } => Ok(false),
    ObjMemberType::Rest { val } => expr_contains_top_level_await(val, ctx),
  }
}

fn class_or_obj_key_contains_top_level_await(
  key: &ClassOrObjKey,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<bool, VmError> {
  ctx.budget_tick()?;

  match key {
    ClassOrObjKey::Direct(_) => Ok(false),
    ClassOrObjKey::Computed(expr) => expr_contains_top_level_await(expr, ctx),
  }
}

fn class_or_obj_val_contains_top_level_await(
  val: &ClassOrObjVal,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<bool, VmError> {
  ctx.budget_tick()?;

  match val {
    // Function-like boundaries: do not descend.
    ClassOrObjVal::Getter(_) | ClassOrObjVal::Setter(_) | ClassOrObjVal::Method(_) => Ok(false),
    ClassOrObjVal::Prop(expr) => match expr.as_ref() {
      Some(expr) => expr_contains_top_level_await(expr, ctx),
      None => Ok(false),
    },
    ClassOrObjVal::IndexSignature(_) => Ok(false),
    // Class static blocks are syntax-errors for `await`; don't scan them.
    ClassOrObjVal::StaticBlock(_) => Ok(false),
  }
}

fn module_request_from_specifier(
  specifier: &str,
  specifier_code_units: Option<&[u16]>,
  attributes: Option<&Node<Expr>>,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<ModuleRequest, VmError> {
  ctx.budget_tick()?;
  let specifier = match specifier_code_units {
    Some(units) => crate::JsString::from_code_units(units)?,
    None => crate::JsString::from_str(specifier)?,
  };
  let attrs = with_clause_to_attributes(attributes, ctx)?;
  ModuleRequest::try_new(specifier, attrs, || ctx.cancel_now())
}

fn push_requested_module(
  out: &mut Vec<ModuleRequest>,
  request: ModuleRequest,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<(), VmError> {
  for existing in out.iter() {
    ctx.budget_tick()?;
    // All `ModuleRequest`s we create here are canonicalized (attribute list sorting), so a direct
    // equality check is equivalent to `ModuleRequestsEqual` while being much cheaper than the
    // spec-shaped order-insensitive comparison.
    if existing == &request {
      return Ok(());
    }
  }
  out.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
  out.push(request);
  Ok(())
}

/// Implements `WithClauseToAttributes` (ECMA-262) for static import/export declarations.
fn with_clause_to_attributes(
  attributes: Option<&Node<Expr>>,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<Vec<ImportAttribute>, VmError> {
  let Some(attributes) = attributes else {
    return Ok(Vec::new());
  };

  ctx.budget_tick()?;

  let Expr::LitObj(obj) = &*attributes.stx else {
    return Err(syntax_error(
      attributes.loc,
      "import attributes must be an object literal",
    ));
  };

  let mut out = Vec::<ImportAttribute>::new();

  out
    .try_reserve(obj.stx.members.len())
    .map_err(|_| VmError::OutOfMemory)?;

  for member in &obj.stx.members {
    ctx.budget_tick()?;

    let (key_node, key_loc, value_expr) = match &member.stx.typ {
      ObjMemberType::Valued { key, val } => {
        let key_node = match key {
          ClassOrObjKey::Direct(direct) => direct,
          ClassOrObjKey::Computed(_) => {
            return Err(syntax_error(
              member.loc,
              "computed import attribute keys are not allowed",
            ));
          }
        };

        let is_ident_or_keyword =
          key_node.stx.tt == TT::Identifier || KEYWORDS_MAPPING.contains_key(&key_node.stx.tt);
        let is_string = key_node.stx.tt == TT::LiteralString;
        if !is_ident_or_keyword && !is_string {
          return Err(syntax_error(
            key_node.loc,
            "import attribute keys must be identifiers, keywords, or string literals",
          ));
        }

        let value_expr = match val {
          ClassOrObjVal::Prop(Some(expr)) => expr,
          _ => {
            return Err(syntax_error(
              member.loc,
              "import attribute entries must be simple key/value properties",
            ));
          }
        };

        (key_node, key_node.loc, value_expr)
      }
      ObjMemberType::Shorthand { .. } => {
        return Err(syntax_error(
          member.loc,
          "shorthand properties are not allowed in import attributes",
        ));
      }
      ObjMemberType::Rest { .. } => {
        return Err(syntax_error(
          member.loc,
          "spread properties are not allowed in import attributes",
        ));
      }
    };

    let key = if key_node.stx.tt == TT::LiteralString {
      match literal_string_code_units(&key_node.assoc) {
        Some(units) => crate::JsString::from_code_units(units)?,
        None => crate::JsString::from_str(key_node.stx.key.as_str())?,
      }
    } else {
      crate::JsString::from_str(key_node.stx.key.as_str())?
    };

    // Detect duplicate keys by code-unit equality.
    //
    // Avoid a `HashSet<JsString>` here: we don't want to clone keys using `JsString`'s infallible
    // derived `Clone` impl (it can abort the process on allocator OOM).
    for existing in &out {
      ctx.budget_tick()?;
      if existing.key == key {
        return Err(syntax_error(key_loc, "duplicate import attribute key"));
      }
    }

    let value = match &*value_expr.stx {
      Expr::LitStr(str_lit) => match literal_string_code_units(&str_lit.assoc) {
        Some(units) => crate::JsString::from_code_units(units)?,
        None => crate::JsString::from_str(&str_lit.stx.value)?,
      },
      _ => {
        return Err(syntax_error(
          value_expr.loc,
          "import attribute values must be string literals",
        ));
      }
    };

    out.push(ImportAttribute { key, value });
  }

  // `ModuleRequest::new` canonicalizes attribute order for storage/comparison, so callers that turn
  // this into a `ModuleRequest` do not need to pre-sort here.
  Ok(out)
}

fn try_string_from_str(value: &str) -> Result<String, VmError> {
  let mut out = String::new();
  out.try_reserve(value.len()).map_err(|_| VmError::OutOfMemory)?;
  out.push_str(value);
  Ok(out)
}

fn try_string_from_module_export_import_name(name: &AstModuleExportImportName) -> Result<String, VmError> {
  match name {
    AstModuleExportImportName::Ident(s) => try_string_from_identifier_name(s),
    AstModuleExportImportName::Str(s) => try_string_from_str(s),
  }
}

fn try_string_from_export_name(alias: &Node<parse_js::ast::expr::pat::IdPat>) -> Result<String, VmError> {
  // Exported names can be identifiers/keywords *or* string literals. `parse-js` represents them all
  // as `IdPat` nodes; use associated data to distinguish string-literal spellings so we don't
  // misinterpret backslashes in string values as identifier escape sequences.
  if literal_string_code_units(&alias.assoc).is_some() {
    return try_string_from_str(&alias.stx.name);
  }
  try_string_from_identifier_name(&alias.stx.name)
}

fn try_string_from_export_alias(
  exportable: &AstModuleExportImportName,
  exportable_loc: parse_js::loc::Loc,
  alias: &Node<parse_js::ast::expr::pat::IdPat>,
) -> Result<String, VmError> {
  // `parse-js` always populates `alias`, even when no explicit `as` clause is present. Detect the
  // implicit alias case via location: the exported name's span starts at the exportable name, while
  // an explicit alias must begin later in the source.
  //
  // For implicit aliases, the export name is the same as the exported binding name, and therefore
  // must be interpreted using the binding name's syntactic form (identifier vs string literal).
  if alias.loc.0 == exportable_loc.0 {
    return try_string_from_module_export_import_name(exportable);
  }

  try_string_from_export_name(alias)
}

/// Decodes `\uXXXX` and `\u{...}` escape sequences in identifier names.
///
/// `parse-js` preserves the original source spelling for identifiers (including Unicode escape
/// sequences) and validates them in the lexer, but the VM needs the *cooked* identifier name for
/// module record algorithms (`GetExportedNames`, namespace export keys, etc.).
fn try_string_from_identifier_name(value: &str) -> Result<String, VmError> {
  if !value.contains('\\') {
    return try_string_from_str(value);
  }

  let mut out = String::new();
  out.try_reserve(value.len()).map_err(|_| VmError::OutOfMemory)?;

  let mut chars = value.chars().peekable();
  while let Some(ch) = chars.next() {
    if ch != '\\' {
      out.push(ch);
      continue;
    }

    match chars.next() {
      Some('u') => {}
      _ => {
        return Err(VmError::InvariantViolation(
          "identifier escape sequence was not a Unicode escape",
        ))
      }
    }

    if chars.peek() == Some(&'{') {
      // `\u{...}`
      let _ = chars.next();
      let mut value_u32: u32 = 0;
      let mut saw_digit = false;
      let mut closed = false;
      while let Some(next) = chars.next() {
        if next == '}' {
          closed = true;
          break;
        }
        let Some(digit) = next.to_digit(16) else {
          return Err(VmError::InvariantViolation(
            "invalid hex digit in identifier escape sequence",
          ));
        };
        saw_digit = true;
        value_u32 = value_u32
          .checked_mul(16)
          .and_then(|v| v.checked_add(digit))
          .ok_or(VmError::InvariantViolation(
            "identifier escape sequence value overflow",
          ))?;
      }
      if !closed || !saw_digit {
        return Err(VmError::InvariantViolation(
          "unterminated identifier escape sequence",
        ));
      }
      let decoded = char::from_u32(value_u32).ok_or(VmError::InvariantViolation(
        "identifier escape sequence produced an invalid code point",
      ))?;
      out.push(decoded);
      continue;
    }

    // `\uXXXX`
    let mut value_u32: u32 = 0;
    for _ in 0..4 {
      let next = chars.next().ok_or(VmError::InvariantViolation(
        "truncated identifier escape sequence",
      ))?;
      let digit = next.to_digit(16).ok_or(VmError::InvariantViolation(
        "invalid hex digit in identifier escape sequence",
      ))?;
      value_u32 = (value_u32 << 4) | digit;
    }
    let decoded = char::from_u32(value_u32).ok_or(VmError::InvariantViolation(
      "identifier escape sequence produced an invalid code point",
    ))?;
    out.push(decoded);
  }
  Ok(out)
}

fn clone_module_request(
  req: &ModuleRequest,
  ctx: &mut ModuleRecordParseCtx<'_>,
) -> Result<ModuleRequest, VmError> {
  ctx.budget_tick()?;
  let mut attrs = Vec::<ImportAttribute>::new();
  attrs
    .try_reserve(req.attributes.len())
    .map_err(|_| VmError::OutOfMemory)?;
  for attr in &req.attributes {
    ctx.budget_tick()?;
    attrs.push(ImportAttribute {
      key: crate::JsString::from_code_units(attr.key.as_code_units())?,
      value: crate::JsString::from_code_units(attr.value.as_code_units())?,
    });
  }

  let specifier = crate::JsString::from_code_units(req.specifier.as_code_units())?;
  // `req` was created via `ModuleRequest::try_new`, so its attributes are already canonicalized.
  // Preserve that ordering while cloning to avoid an additional uninterruptible sort.
  Ok(ModuleRequest::new_with_canonicalized_attributes(specifier, attrs))
}

fn syntax_error(loc: parse_js::loc::Loc, message: &str) -> VmError {
  let span = loc.to_diagnostics_span(FileId(0));
  VmError::Syntax(vec![Diagnostic::error("VMJS0001", message, span)])
}

#[cfg(test)]
mod tests {
  use super::{ImportEntry, ImportName, IndirectExportEntry, LocalExportEntry, SourceTextModuleRecord};
  use crate::{Heap, HeapLimits, SourceText, TerminationReason, Vm, VmError, VmOptions};

  fn assert_syntax(result: Result<SourceTextModuleRecord, VmError>) {
    match result {
      Err(VmError::Syntax(_)) => {}
      other => panic!("expected VmError::Syntax, got {other:?}"),
    }
  }

  fn parse(src: &str) -> Result<SourceTextModuleRecord, VmError> {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    SourceTextModuleRecord::parse(&mut heap, src)
  }

  #[test]
  fn parse_source_with_vm_respects_fuel_budget() {
    let mut opts = VmOptions::default();
    opts.default_fuel = Some(0);
    let mut vm = Vm::new(opts);
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let source = SourceText::new_charged_arc(
      &mut heap,
      "https://example.invalid/module.js",
      "export const x = 1;",
    )
    .expect("SourceText::new_charged");

    let err = SourceTextModuleRecord::parse_source_with_vm(&mut vm, source)
      .expect_err("expected fuel budget to terminate parsing");
    match err {
      VmError::Termination(term) => assert_eq!(term.reason, TerminationReason::OutOfFuel),
      other => panic!("expected OutOfFuel termination, got {other:?}"),
    }
  }

  #[test]
  fn export_object_destructuring_exports_bound_names() {
    let record = parse("export const { a, b: c, ...d } = obj;").unwrap();
    assert_eq!(
      record.local_export_entries,
      vec![
        LocalExportEntry {
          export_name: String::from("a"),
          local_name: String::from("a"),
        },
        LocalExportEntry {
          export_name: String::from("c"),
          local_name: String::from("c"),
        },
        LocalExportEntry {
          export_name: String::from("d"),
          local_name: String::from("d"),
        },
      ]
    );
  }

  #[test]
  fn export_array_destructuring_exports_bound_names() {
    let record = parse("export let [a, , b, ...rest] = arr;").unwrap();
    assert_eq!(
      record.local_export_entries,
      vec![
        LocalExportEntry {
          export_name: String::from("a"),
          local_name: String::from("a"),
        },
        LocalExportEntry {
          export_name: String::from("b"),
          local_name: String::from("b"),
        },
        LocalExportEntry {
          export_name: String::from("rest"),
          local_name: String::from("rest"),
        },
      ]
    );
  }

  #[test]
  fn export_list_reexports_namespace_import_as_indirect_export() {
    let record = parse("import * as foo from \"m\"; export { foo };").unwrap();
    assert!(record.local_export_entries.is_empty());
    assert_eq!(
      record.indirect_export_entries,
      vec![IndirectExportEntry {
        export_name: String::from("foo"),
        module_request: crate::ModuleRequest::new(crate::JsString::from_str("m").unwrap(), vec![]),
        import_name: ImportName::All,
      }]
    );
  }

  #[test]
  fn export_list_reexports_named_import_as_indirect_export() {
    let record =
      parse("import { foo as bar } from \"m\"; export { bar as baz };").unwrap();
    assert!(record.local_export_entries.is_empty());
    assert_eq!(
      record.indirect_export_entries,
      vec![IndirectExportEntry {
        export_name: String::from("baz"),
        module_request: crate::ModuleRequest::new(crate::JsString::from_str("m").unwrap(), vec![]),
        import_name: ImportName::Name(String::from("foo")),
      }]
    );
  }

  #[test]
  fn export_nested_destructuring_exports_bound_names() {
    let record = parse("export const { a: { b } } = obj;").unwrap();
    assert_eq!(
      record.local_export_entries,
      vec![LocalExportEntry {
        export_name: String::from("b"),
        local_name: String::from("b"),
      }]
    );
  }

  #[test]
  fn export_multiple_declarators_exports_all_bound_names() {
    let record = parse("export const { a } = obj, [b] = arr, c = 1;").unwrap();
    assert_eq!(
      record.local_export_entries,
      vec![
        LocalExportEntry {
          export_name: String::from("a"),
          local_name: String::from("a"),
        },
        LocalExportEntry {
          export_name: String::from("b"),
          local_name: String::from("b"),
        },
        LocalExportEntry {
          export_name: String::from("c"),
          local_name: String::from("c"),
        },
      ]
    );
  }

  #[test]
  fn module_early_error_duplicate_exported_name() {
    assert_syntax(parse(
      r#"
      var x;
      export { x };
      export { x };
    "#,
    ));
  }

  #[test]
  fn module_early_error_duplicate_exported_name_default() {
    assert_syntax(parse(
      r#"
      var x;
      export default 1;
      export { x as default };
    "#,
    ));
  }

  #[test]
  fn module_early_error_export_unresolvable() {
    assert_syntax(parse("export { unresolvable };"));
  }

  #[test]
  fn module_early_error_export_global() {
    assert_syntax(parse("export { Number };"));
  }

  #[test]
  fn module_early_error_import_eval() {
    assert_syntax(parse("import { x as eval } from 'm';"));
  }

  #[test]
  fn module_early_error_import_arguments() {
    assert_syntax(parse("import arguments from 'm';"));
  }

  #[test]
  fn module_early_error_import_collides_with_var() {
    assert_syntax(parse(
      r#"
      import { x } from 'm';
      var x;
    "#,
    ));
  }

  #[test]
  fn module_early_error_duplicate_import_binding() {
    assert_syntax(parse(
      r#"
      import { x } from 'm';
      import { x } from 'n';
    "#,
    ));
  }

  #[test]
  fn module_early_error_duplicate_lex_declared_name() {
    assert_syntax(parse(
      r#"
      let x;
      const x = 0;
    "#,
    ));
  }

  #[test]
  fn module_early_error_lex_and_var_collision() {
    assert_syntax(parse(
      r#"
      let x;
      var x;
    "#,
    ));
  }

  #[test]
  fn module_early_error_duplicate_top_level_function_decl() {
    assert_syntax(parse(
      r#"
      function x() {}
      function x() {}
    "#,
    ));
  }

  #[test]
  fn module_early_error_duplicate_labels() {
    assert_syntax(parse(
      r#"
      label: {
        label: 0;
      }
    "#,
    ));
  }

  #[test]
  fn module_early_error_undefined_break_label() {
    assert_syntax(parse(
      r#"
      while (false) {
        break undef;
      }
    "#,
    ));
  }

  #[test]
  fn module_early_error_undefined_continue_label() {
    assert_syntax(parse(
      r#"
      while (false) {
        continue undef;
      }
    "#,
    ));
  }

  #[test]
  fn module_early_error_export_alias_string_literal_invalid_unicode() {
    assert_syntax(parse(
      r#"export { Moon as "\uD83C" } from "./m.js";"#,
    ));
  }

  #[test]
  fn module_early_error_export_target_string_literal_invalid_unicode() {
    assert_syntax(parse(
      r#"export { "\uD83C" as Moon } from "./m.js";"#,
    ));
  }

  #[test]
  fn module_early_error_import_target_string_literal_invalid_unicode() {
    assert_syntax(parse(
      r#"import { "\uD83C" as Moon } from "./m.js";"#,
    ));
  }

  #[test]
  fn module_early_error_export_star_alias_string_literal_invalid_unicode() {
    assert_syntax(parse(
      r#"export * as "\uD83C" from "./m.js";"#,
    ));
  }

  #[test]
  fn module_allows_well_formed_unicode_string_literal_export_names() {
    parse(r#"export { Moon as "\uD83C\uDF19" } from "./m.js";"#)
      .expect("well-formed unicode string literals should be valid module export names");
  }

  #[test]
  fn module_allows_shorthand_string_literal_export_names() {
    parse(r#"export { "☿" } from "./m.js";"#)
      .expect("string-literal module export names should be allowed in re-exports");
  }

  #[test]
  fn module_preserves_backslashes_in_string_literal_export_names() {
    let record = parse(
      r#"
      const a = 1;
      export { a as "\\u0061" };
    "#,
    )
    .unwrap();
    assert_eq!(
      record.local_export_entries,
      vec![LocalExportEntry {
        export_name: String::from("\\u0061"),
        local_name: String::from("a"),
      }]
    );
  }

  #[test]
  fn module_preserves_backslashes_in_string_literal_reexport_names() {
    let record = parse(r#"export { "\\u0061" } from "./m.js";"#).unwrap();
    assert_eq!(
      record.indirect_export_entries,
      vec![IndirectExportEntry {
        export_name: String::from("\\u0061"),
        module_request: crate::ModuleRequest::new(crate::JsString::from_str("./m.js").unwrap(), vec![]),
        import_name: ImportName::Name(String::from("\\u0061")),
      }]
    );
  }

  #[test]
  fn module_preserves_backslashes_in_string_literal_import_names() {
    let record = parse(r#"import { "\\u0061" as a } from "./m.js";"#).unwrap();
    assert_eq!(
      record.import_entries,
      vec![ImportEntry {
        module_request: crate::ModuleRequest::new(crate::JsString::from_str("./m.js").unwrap(), vec![]),
        import_name: ImportName::Name(String::from("\\u0061")),
        local_name: String::from("a"),
      }]
    );
  }

  #[test]
  fn module_allows_exporting_imported_binding() {
    parse(
      r#"
      import { x } from 'm';
      export { x };
    "#,
    )
    .expect("exporting an imported binding should be valid");
  }
}
