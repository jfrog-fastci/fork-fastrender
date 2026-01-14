use crate::exec::{
  instantiate_module_decls, resume_module_tla_evaluation, run_module, start_module_tla_evaluation,
  ModuleTlaStepResult,
};
use crate::fallible_alloc::arc_try_new_vm;
use crate::heap::{ModuleNamespaceExport, ModuleNamespaceExportValue};
use crate::hir_exec::instantiate_compiled_module_decls;
use crate::import_meta::{create_import_meta_object, VmImportMetaHostHooks};
use crate::module_loading::DynamicImportState;
use crate::module_record::ModuleNamespaceCache;
use crate::module_record::ModuleStatus;
use crate::module_record::ResolveExportResult;
use crate::module_record::SourceTextModuleRecord;
use crate::property::{PropertyDescriptor, PropertyKey, PropertyKind};
use crate::{
  cmp_utf16, ExecutionContext, GcEnv, GcObject, LoadedModuleRequest, ModuleId, ModuleRequest,
  RealmId, RootId, Scope, ScriptId, SourceText, StackFrame, Value, Vm, VmError,
};
use crate::{ExternalMemoryToken, Heap, VmHost, VmHostHooks};
use core::mem;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;
use parse_js::ast::node::Node;
use parse_js::ast::stx::TopLevel;
use parse_js::{Dialect, ParseOptions, SourceType};

const MAX_REJECTION_STACK_FRAMES: usize = 32;
const MAX_REJECTION_STACK_BYTES: usize = 16 * 1024;
const TLA_ABORT_REASON: &str = "asynchronous module loading/evaluation is not supported";

fn non_throw_vm_error_message(err: &VmError) -> &'static str {
  match err {
    VmError::OutOfMemory => "out of memory",
    VmError::InvariantViolation(msg) => msg,
    VmError::LimitExceeded(msg) => msg,
    VmError::InvalidHandle { .. } => "invalid handle",
    VmError::PrototypeCycle => "prototype cycle",
    VmError::PrototypeChainTooDeep => "prototype chain too deep",
    VmError::Unimplemented(msg) => msg,
    VmError::InvalidPropertyDescriptorPatch => "invalid property descriptor patch",
    VmError::PropertyNotFound => "property not found",
    VmError::PropertyNotData => "property is not a data property",
    VmError::TypeError(msg) => msg,
    VmError::RangeError(msg) => msg,
    VmError::NotCallable => "value is not callable",
    VmError::NotConstructable => "value is not a constructor",
    VmError::Throw(_) | VmError::ThrowWithStack { .. } => "exception",
    VmError::Termination(term) => match term.reason {
      crate::TerminationReason::OutOfFuel => "execution terminated: out of fuel",
      crate::TerminationReason::DeadlineExceeded => "execution terminated: deadline exceeded",
      crate::TerminationReason::Interrupted => "execution terminated: interrupted",
      crate::TerminationReason::OutOfMemory => "execution terminated: out of memory",
      crate::TerminationReason::StackOverflow => "execution terminated: stack overflow",
    },
    VmError::Syntax(_) => "syntax error",
  }
}

/// Value of a module's `[[AsyncEvaluationOrder]]` internal slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsyncEvaluationOrder {
  /// The module is not (currently) part of async module evaluation.
  Unset,
  /// The module's async execution completed (fulfilled or rejected).
  Done,
  /// The module has been assigned an evaluation order index.
  Order(u64),
}

impl Default for AsyncEvaluationOrder {
  fn default() -> Self {
    Self::Unset
  }
}

impl AsyncEvaluationOrder {
  #[inline]
  fn as_integer(self) -> Option<u64> {
    match self {
      Self::Order(n) => Some(n),
      _ => None,
    }
  }
}

#[derive(Debug, Default)]
struct AsyncModuleEvalState {
  async_evaluation_order: AsyncEvaluationOrder,
  /// Mirrors the spec's `[[PendingAsyncDependencies]]` slot:
  /// - `None` means `~empty~`,
  /// - `Some(n)` means an integer counter.
  pending_async_dependencies: Option<usize>,
  async_parent_modules: Vec<ModuleId>,
}

fn format_rejection_stack_trace_limited(frames: &[StackFrame]) -> String {
  let slice = &frames[..frames.len().min(MAX_REJECTION_STACK_FRAMES)];
  let mut out = crate::format_stack_trace(slice);
  if out.len() <= MAX_REJECTION_STACK_BYTES {
    return out;
  }

  let mut end = MAX_REJECTION_STACK_BYTES;
  while end > 0 && !out.is_char_boundary(end) {
    end -= 1;
  }
  out.truncate(end);
  out.push_str("...");
  out
}

fn attach_stack_property_for_promise_rejection(
  scope: &mut Scope<'_>,
  reason: Value,
  err: &VmError,
) {
  let Some(frames) = err.thrown_stack() else {
    return;
  };
  let Value::Object(obj) = reason else {
    return;
  };

  let stack_trace = format_rejection_stack_trace_limited(frames);
  if stack_trace.is_empty() {
    return;
  }

  // Best-effort: failure to attach stack data should not alter spec-visible module evaluation
  // semantics (the promise must still be rejected with the thrown value).
  let mut scope = scope.reborrow();
  if scope.push_root(Value::Object(obj)).is_err() {
    return;
  }

  let Ok(key_s) = scope.alloc_string("stack") else {
    return;
  };
  if scope.push_root(Value::String(key_s)).is_err() {
    return;
  }
  let key = PropertyKey::from_string(key_s);

  // Do not overwrite an existing own `stack` property; this mirrors browser behavior where
  // `Error.stack` can be customized by user code.
  match scope.heap().object_get_own_property(obj, &key) {
    Ok(Some(_)) => return,
    Ok(None) => {}
    Err(_) => return,
  }

  let Ok(stack_s) = scope.alloc_string(&stack_trace) else {
    return;
  };
  if scope.push_root(Value::String(stack_s)).is_err() {
    return;
  }

  let _ = scope.define_property(
    obj,
    key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::String(stack_s),
        writable: true,
      },
    },
  );
}

/// RAII helper for `vm.module_graph_ptr` install/restore.
///
/// `ModuleGraph::evaluate_with_scope` temporarily installs itself as the VM's active module graph so
/// dynamic `import()` can be evaluated from module code even when the embedding did not pre-attach a
/// graph via [`Vm::set_module_graph`]. For synchronous evaluation we restore the previous pointer on
/// return.
///
/// For async module evaluation (top-level await), the pointer must remain installed until the
/// module evaluation promise settles (fulfilled/rejected) or evaluation is aborted. In that case,
/// callers must [`ModuleGraphPtrGuard::disarm`] the guard and arrange restoration later via
/// [`ModuleGraph::retain_module_graph_ptr`] / [`ModuleGraph::release_module_graph_ptr`].
struct ModuleGraphPtrGuard {
  vm: *mut Vm,
  prev_graph: Option<*mut ModuleGraph>,
  restore_on_drop: bool,
}

impl ModuleGraphPtrGuard {
  fn install(vm: &mut Vm, graph: &mut ModuleGraph) -> Self {
    let prev_graph = vm.module_graph_ptr();
    vm.set_module_graph(graph);
    Self {
      vm: vm as *mut Vm,
      prev_graph,
      restore_on_drop: true,
    }
  }

  fn prev_graph(&self) -> Option<*mut ModuleGraph> {
    self.prev_graph
  }

  fn disarm(&mut self) {
    self.restore_on_drop = false;
  }

  fn restore(&mut self) {
    if !self.restore_on_drop {
      return;
    }
    // Safety: `vm` points to the live `&mut Vm` passed to `install` and this guard is only dropped
    // while that borrow remains active.
    let vm = unsafe { &mut *self.vm };
    match self.prev_graph {
      Some(ptr) => unsafe {
        vm.set_module_graph(&mut *ptr);
      },
      None => vm.clear_module_graph(),
    }
    self.restore_on_drop = false;
  }
}

impl Drop for ModuleGraphPtrGuard {
  fn drop(&mut self) {
    self.restore();
  }
}

/// An embedding-owned graph of ECMAScript module records.
///
/// `ModuleGraph` stores [`SourceTextModuleRecord`]s and their resolved `[[LoadedModules]]` edges,
/// and implements the core linking/evaluation algorithms (including dynamic `import()` and
/// top-level `await` bookkeeping).
///
/// The host environment is still responsible for *fetching and parsing* modules. Integrate module
/// loading via:
///
/// - the module-loading state machine ([`crate::load_requested_modules`] +
///   [`crate::Vm::finish_loading_imported_module`]), and
/// - [`crate::VmHostHooks::host_load_imported_module`] (host-defined module fetch/parse).
///
/// For a full embedder-facing guide, see [`crate::docs::modules`].
#[derive(Debug)]
pub struct ModuleGraph {
  modules: Vec<SourceTextModuleRecord>,
  host_resolve: Vec<(ModuleRequest, ModuleId)>,
  tla_states: Vec<Option<TlaEvaluationState>>,
  /// Per-module async module evaluation progress for each SCC (cycle root).
  ///
  /// Indexed by `module_index(scc_root)`. Non-root modules store `None`.
  scc_eval_states: Vec<Option<SccEvaluationState>>,
  /// Cached SCC membership list keyed by cycle root module index.
  ///
  /// Recomputed when `scc_dirty` is set.
  scc_members: Vec<Vec<ModuleId>>,
  /// Cached SCC dependency list keyed by cycle root module index.
  ///
  /// Each entry lists the *cycle roots* of SCCs that must be evaluated before this SCC can execute.
  scc_deps: Vec<Vec<ModuleId>>,
  /// Cached SCC reverse-dependency list keyed by cycle root module index.
  ///
  /// Each entry lists the *cycle roots* of SCCs that depend on this SCC.
  scc_parents: Vec<Vec<ModuleId>>,
  scc_dirty: bool,
  global_lexical_env: Option<GcEnv>,
  pending_dynamic_import_evaluations: HashMap<u32, PendingDynamicImportEvaluation>,
  next_pending_dynamic_import_evaluation_id: u32,
  /// `vm.module_graph_ptr()` value to restore once all pending async work that depends on a module
  /// graph pointer has completed.
  ///
  /// This is managed via `module_graph_ptr_refcount` so top-level await evaluation and dynamic
  /// import can overlap without racing to restore the pointer.
  module_graph_ptr_prev: Option<*mut ModuleGraph>,
  /// Number of in-flight async operations that require `vm.module_graph_ptr()` to point at this
  /// graph.
  ///
  /// Currently this counts:
  /// - pending top-level await module evaluation continuations, and
  /// - pending dynamic import evaluation continuations.
  module_graph_ptr_refcount: usize,
  torn_down: bool,
  async_eval_states: Vec<AsyncModuleEvalState>,
  /// SCC roots that are ready to execute (all dependencies completed).
  ///
  /// This queue is drained in spec order (`[[AsyncEvaluationOrder]]`) so that when multiple SCCs
  /// become available at once (or are made available transitively by synchronous execution),
  /// evaluation order follows `InnerModuleEvaluation` discovery order rather than module insertion
  /// order.
  ready_scc_queue: Vec<ModuleId>,
  /// Re-entrancy guard for `ready_scc_queue` processing.
  ///
  /// `execute_scc` can synchronously complete an SCC, which calls `complete_scc` and enqueues more
  /// ready SCCs. We must not recursively drain the queue in that case, otherwise newly-ready SCCs
  /// could execute ahead of already-ready SCCs with a lower `[[AsyncEvaluationOrder]]`.
  processing_ready_scc_queue: bool,

  /// Spec `[[LoadedModules]]` cache for Script Records.
  ///
  /// ECMA-262 requires `FinishLoadingImportedModule` to memoize successful `(referrer, request)`
  /// resolutions for **all** referrer kinds (Script, Module, Realm). Module referrers store their
  /// `[[LoadedModules]]` list in the module record itself (`SourceTextModuleRecord.loaded_modules`),
  /// but scripts do not yet have a concrete record type in the VM.
  ///
  /// This graph-owned cache provides equivalent memoization for Script referrers so repeated
  /// dynamic `import()` from the same script with the same `ModuleRequest` is idempotent.
  script_loaded_modules: HashMap<ScriptId, Vec<LoadedModuleRequest<ModuleId>>>,

  /// Spec `[[LoadedModules]]` cache for Realm Records.
  ///
  /// Like [`ModuleGraph::script_loaded_modules`], this is used solely to enforce the
  /// `FinishLoadingImportedModule` caching invariant for Realm referrers.
  realm_loaded_modules: HashMap<RealmId, Vec<LoadedModuleRequest<ModuleId>>>,
}

impl Default for ModuleGraph {
  fn default() -> Self {
    Self {
      modules: Vec::new(),
      host_resolve: Vec::new(),
      tla_states: Vec::new(),
      scc_eval_states: Vec::new(),
      scc_members: Vec::new(),
      scc_deps: Vec::new(),
      scc_parents: Vec::new(),
      scc_dirty: true,
      global_lexical_env: None,
      pending_dynamic_import_evaluations: HashMap::new(),
      next_pending_dynamic_import_evaluation_id: 0,
      module_graph_ptr_prev: None,
      module_graph_ptr_refcount: 0,
      // A freshly-created graph does not own any persistent roots yet, and can be dropped safely.
      torn_down: true,
      async_eval_states: Vec::new(),
      ready_scc_queue: Vec::new(),
      processing_ready_scc_queue: false,
      script_loaded_modules: HashMap::new(),
      realm_loaded_modules: HashMap::new(),
    }
  }
}

#[derive(Debug)]
struct PendingDynamicImportEvaluation {
  state: DynamicImportState,
  module: ModuleId,
}

impl ModuleGraph {
  pub fn new() -> Self {
    Self::default()
  }

  /// Ensures that `module` has a parsed `parse-js` AST available in its module record.
  ///
  /// This supports execution paths where a module is primarily compiled to HIR, but must retain an
  /// AST for a fallback interpreter path (for example top-level await or async-generator fallback).
  ///
  /// When parsing/storing an AST into a module record that previously had none, this charges a
  /// conservative estimate of the AST's host memory usage against [`HeapLimits`] via
  /// [`Heap::charge_external`].
  fn ensure_module_ast(&mut self, vm: &mut Vm, heap: &mut Heap, module: ModuleId) -> Result<(), VmError> {
    let idx = module_index(module);
    let record = self
      .modules
      .get_mut(idx)
      .ok_or_else(|| VmError::invalid_handle())?;
    if record.ast.is_some() {
      return Ok(());
    }

    debug_assert!(
      record.ast_external_memory.is_none(),
      "module record has an AST external-memory token but no AST"
    );

    let source = record
      .source
      .clone()
      .or_else(|| record.compiled.as_ref().map(|c| c.source.clone()))
      .ok_or(VmError::Unimplemented("module source missing"))?;

    // `parse-js` AST nodes can be significantly larger than the original source. Use the same
    // conservative multiplier as `Vm::ecma_function_ast` so hostile modules can't bypass heap
    // limits by forcing large retained ASTs.
    let estimated_ast_bytes = source.text.len().saturating_mul(4);

    // Charge before parsing so we can fail fast without allocating an untracked AST when the heap
    // is already at its limit. If parsing fails, the token is dropped and the charge is released.
    let token = arc_try_new_vm(heap.charge_external(estimated_ast_bytes)?)?;

    let opts = ParseOptions {
      dialect: Dialect::Ecma,
      source_type: SourceType::Module,
    };
    let top = vm.parse_top_level_with_budget(&source.text, opts)?;

    {
      let mut cancel = || vm.tick();
      crate::early_errors::validate_top_level(
        &top.stx.body,
        crate::early_errors::EarlyErrorOptions::module(),
        Some(source.text.as_ref()),
        &mut cancel,
      )?;
    }

    // Install the AST + token. This should be infallible after successful parsing and charging.
    let top = arc_try_new_vm(top)?;
    // Storing the `SourceText` in the module record ensures AST interpreter fallbacks can access it
    // without needing to plumb the `CompiledScript` through all module graph paths.
    record.source = Some(source);
    record.ast = Some(top);
    record.ast_external_memory = Some(token);
    self.torn_down = false;
    Ok(())
  }

  /// Marks the cached SCC (cycle) structure as dirty so it will be recomputed before the next
  /// module evaluation.
  ///
  /// SCC membership and dependency edges are computed over the resolved `[[LoadedModules]]` graph
  /// (stored in each module record's `loaded_modules` list), not just the static
  /// `[[RequestedModules]]` list.
  ///
  /// Any mutation of `loaded_modules` / `[[LoadedModules]]` edges must call this method (e.g.
  /// during host-driven module loading / `FinishLoadingImportedModule`) so cached SCC structure is
  /// recomputed before module evaluation.
  pub(crate) fn mark_scc_dirty(&mut self) {
    self.scc_dirty = true;
  }

  pub(crate) fn script_loaded_modules_mut(
    &mut self,
    script: ScriptId,
  ) -> Result<&mut Vec<LoadedModuleRequest<ModuleId>>, VmError> {
    if !self.script_loaded_modules.contains_key(&script) {
      // `HashMap::insert` can abort on allocator OOM; reserve fallibly first.
      self
        .script_loaded_modules
        .try_reserve(1)
        .map_err(|_| VmError::OutOfMemory)?;
      self.script_loaded_modules.insert(script, Vec::new());
    }
    // Safe: we inserted the key above if it was missing.
    self
      .script_loaded_modules
      .get_mut(&script)
      .ok_or(VmError::InvariantViolation(
        "script_loaded_modules missing key after insertion",
      ))
  }

  pub(crate) fn realm_loaded_modules_mut(
    &mut self,
    realm: RealmId,
  ) -> Result<&mut Vec<LoadedModuleRequest<ModuleId>>, VmError> {
    if !self.realm_loaded_modules.contains_key(&realm) {
      self
        .realm_loaded_modules
        .try_reserve(1)
        .map_err(|_| VmError::OutOfMemory)?;
      self.realm_loaded_modules.insert(realm, Vec::new());
    }
    self
      .realm_loaded_modules
      .get_mut(&realm)
      .ok_or(VmError::InvariantViolation(
        "realm_loaded_modules missing key after insertion",
      ))
  }
  pub fn set_global_lexical_env(&mut self, env: GcEnv) {
    self.global_lexical_env = Some(env);
  }

  /// Retain this graph as the VM's `module_graph_ptr` until a matching call to
  /// [`ModuleGraph::release_module_graph_ptr`].
  ///
  /// This is used by async features (top-level await module evaluation, dynamic import) that need
  /// to recover the active graph from Promise jobs / microtasks via `vm.module_graph_ptr()`.
  ///
  /// `prev_graph` is the value that should be restored once the refcount drops back to zero.
  fn retain_module_graph_ptr(&mut self, vm: &mut Vm, prev_graph: Option<*mut ModuleGraph>) {
    if self.module_graph_ptr_refcount == 0 {
      self.module_graph_ptr_prev = prev_graph;
    }
    self.module_graph_ptr_refcount = self.module_graph_ptr_refcount.saturating_add(1);
    vm.set_module_graph(self);
  }

  /// Releases a previous [`ModuleGraph::retain_module_graph_ptr`] reference.
  fn release_module_graph_ptr(&mut self, vm: &mut Vm) {
    debug_assert!(
      self.module_graph_ptr_refcount > 0,
      "release_module_graph_ptr underflow"
    );
    if self.module_graph_ptr_refcount == 0 {
      return;
    }
    self.module_graph_ptr_refcount = self.module_graph_ptr_refcount.saturating_sub(1);
    if self.module_graph_ptr_refcount != 0 {
      return;
    }

    let prev = self.module_graph_ptr_prev.take();
    let self_ptr: *mut ModuleGraph = self;
    match prev {
      Some(ptr) if ptr == self_ptr => vm.set_module_graph(self),
      Some(ptr) => unsafe {
        vm.set_module_graph(&mut *ptr);
      },
      None => vm.clear_module_graph(),
    }
  }

  pub(crate) fn insert_pending_dynamic_import_evaluation(
    &mut self,
    vm: &mut Vm,
    state: DynamicImportState,
    module: ModuleId,
  ) -> Result<u32, VmError> {
    // Ensure capacity first: installing the module graph pointer can happen only if insertion is
    // guaranteed not to OOM, otherwise we might leave the VM pointing at this graph even though no
    // continuation was registered.
    self
      .pending_dynamic_import_evaluations
      .try_reserve(1)
      .map_err(|_| VmError::OutOfMemory)?;

    // Promise jobs use `vm.module_graph_ptr()` to recover the active module graph when resolving
    // dynamic `import()` promises. If this is the first pending dynamic import evaluation, retain
    // a pointer to this graph so callbacks can run even when the embedding did not permanently
    // attach a module graph.
    if self.pending_dynamic_import_evaluations.is_empty() {
      let prev = vm.module_graph_ptr();
      self.retain_module_graph_ptr(vm, prev);
    }

    let id = self.next_pending_dynamic_import_evaluation_id;
    self.next_pending_dynamic_import_evaluation_id = self
      .next_pending_dynamic_import_evaluation_id
      .wrapping_add(1);

    self
      .pending_dynamic_import_evaluations
      .insert(id, PendingDynamicImportEvaluation { state, module });
    self.torn_down = false;

    Ok(id)
  }

  pub(crate) fn take_pending_dynamic_import_evaluation(
    &mut self,
    vm: &mut Vm,
    id: u32,
  ) -> Option<(DynamicImportState, ModuleId)> {
    let entry = self.pending_dynamic_import_evaluations.remove(&id)?;

    if self.pending_dynamic_import_evaluations.is_empty() {
      self.release_module_graph_ptr(vm);
    }

    Some((entry.state, entry.module))
  }

  pub fn add_module(&mut self, record: SourceTextModuleRecord) -> Result<ModuleId, VmError> {
    // Pre-reserve capacity on all per-module vectors before mutating lengths so allocator OOM
    // reports `VmError::OutOfMemory` rather than aborting the process.
    self
      .modules
      .try_reserve(1)
      .map_err(|_| VmError::OutOfMemory)?;
    self
      .tla_states
      .try_reserve(1)
      .map_err(|_| VmError::OutOfMemory)?;
    self
      .scc_eval_states
      .try_reserve(1)
      .map_err(|_| VmError::OutOfMemory)?;
    self
      .scc_members
      .try_reserve(1)
      .map_err(|_| VmError::OutOfMemory)?;
    self
      .scc_deps
      .try_reserve(1)
      .map_err(|_| VmError::OutOfMemory)?;
    self
      .scc_parents
      .try_reserve(1)
      .map_err(|_| VmError::OutOfMemory)?;
    self
      .async_eval_states
      .try_reserve(1)
      .map_err(|_| VmError::OutOfMemory)?;

    let id = ModuleId::from_raw(self.modules.len() as u64);
    self.modules.push(record);
    self.tla_states.push(None);
    self.scc_eval_states.push(None);
    self.scc_members.push(Vec::new());
    self.scc_deps.push(Vec::new());
    self.scc_parents.push(Vec::new());
    self.mark_scc_dirty();
    self.async_eval_states.push(AsyncModuleEvalState::default());
    Ok(id)
  }

  /// Adds a module to the graph and registers it under `specifier` for later linking.
  pub fn add_module_with_specifier(
    &mut self,
    specifier: impl AsRef<str>,
    record: SourceTextModuleRecord,
  ) -> Result<ModuleId, VmError> {
    // Ensure the host-resolve mapping can be installed before adding the module record so we don't
    // leave the graph in a partially-mutated state on allocator OOM.
    self
      .host_resolve
      .try_reserve(1)
      .map_err(|_| VmError::OutOfMemory)?;
    let request = module_request_from_specifier(specifier.as_ref())?;

    let id = self.add_module(record)?;
    // `host_resolve` capacity was reserved above, and `request` has already been allocated, so this
    // push is now infallible.
    self.host_resolve.push((request, id));
    Ok(id)
  }

  /// Registers a host resolution mapping used by [`ModuleGraph::link_all_by_specifier`].
  pub fn register_specifier(
    &mut self,
    specifier: impl AsRef<str>,
    module: ModuleId,
  ) -> Result<(), VmError> {
    self
      .host_resolve
      .try_reserve(1)
      .map_err(|_| VmError::OutOfMemory)?;
    let request = module_request_from_specifier(specifier.as_ref())?;
    self.host_resolve.push((request, module));
    Ok(())
  }

  pub fn module(&self, id: ModuleId) -> &SourceTextModuleRecord {
    &self.modules[module_index(id)]
  }

  pub fn module_mut(&mut self, id: ModuleId) -> &mut SourceTextModuleRecord {
    &mut self.modules[module_index(id)]
  }

  /// Fallible accessor for module records.
  ///
  /// Unlike [`ModuleGraph::module`], this returns `None` for invalid `ModuleId`s instead of
  /// panicking.
  pub fn get_module(&self, id: ModuleId) -> Option<&SourceTextModuleRecord> {
    self.modules.get(id.to_raw() as usize)
  }

  /// Fallible mutable accessor for module records.
  ///
  /// Unlike [`ModuleGraph::module_mut`], this returns `None` for invalid `ModuleId`s instead of
  /// panicking.
  pub fn get_module_mut(&mut self, id: ModuleId) -> Option<&mut SourceTextModuleRecord> {
    self.modules.get_mut(id.to_raw() as usize)
  }

  pub fn module_count(&self) -> usize {
    self.modules.len()
  }

  /// Unregisters all persistent roots owned by this module graph.
  ///
  /// `ModuleGraph` caches several VM values using persistent GC roots (module environments, module
  /// namespace objects, cached `import.meta` objects, async module evaluation promise capabilities,
  /// etc). Dropping the graph without explicitly removing those roots is fine if the entire
  /// [`Heap`] is dropped, but is a leak hazard for embeddings that reuse a heap across multiple
  /// graphs.
  ///
  /// This method is **idempotent**.
  pub fn teardown(&mut self, vm: &mut Vm, heap: &mut Heap) {
    if self.torn_down {
      // Even if there are no roots to remove, ensure the VM does not retain a raw pointer to this
      // graph before the embedding drops it.
      if vm.module_graph_ptr() == Some(self as *mut ModuleGraph) {
        vm.clear_module_graph();
      }
      // Also clear any retained module ASTs that were charged as external memory (for example,
      // compiled-module fallback ASTs). Teardown is intended to be safe and idempotent even when the
      // graph is reused, and these tokens are not represented as GC roots.
      for module in &mut self.modules {
        if module.ast_external_memory.is_some() {
          module.clear_ast();
        }
      }
      return;
    }
    self.torn_down = true;

    // Abort any in-progress async module evaluation so its promise capability roots are removed.
    for slot in &mut self.tla_states {
      if let Some(state) = slot.take() {
        state.teardown(vm, heap);
      }
    }

    // Tear down any pending dynamic import evaluation continuations so their promise capability
    // roots do not leak across heap reuse.
    for (_id, entry) in self.pending_dynamic_import_evaluations.drain() {
      entry.state.teardown_roots(heap);
    }

    // Remove per-module persistent roots.
    for module in &mut self.modules {
      if let Some(ns) = module.namespace.take() {
        heap.remove_root(ns.object);
      }
      if let Some(env_root) = module.environment.take() {
        heap.remove_env_root(env_root);
      }
      if let Some(root) = module.import_meta.take() {
        heap.remove_root(root);
      }
      if let Some(root) = module.error.take() {
        heap.remove_root(root);
      }

      // Cyclic module record persistent roots (top-level await state / cached errors).
      module.teardown_top_level_capability(heap);
      module.teardown_evaluation_error(heap);

      // Drop any charged retained ASTs so their external-memory tokens are released when tearing
      // down a graph (important for heap reuse).
      if module.ast_external_memory.is_some() {
        module.clear_ast();
      }
    }

    // Ensure the VM does not retain a raw pointer to this graph after teardown.
    let self_ptr: *mut ModuleGraph = self;
    if vm.module_graph_ptr() == Some(self_ptr) {
      match self.module_graph_ptr_prev.take() {
        Some(ptr) if ptr != self_ptr => unsafe { vm.set_module_graph(&mut *ptr) },
        _ => vm.clear_module_graph(),
      };
    } else {
      // If the module graph pointer already changed, drop any saved pointer to avoid retaining a
      // stale raw pointer unnecessarily.
      self.module_graph_ptr_prev = None;
    }
    self.module_graph_ptr_refcount = 0;
    self.ready_scc_queue.clear();
    self.processing_ready_scc_queue = false;
  }

  /// Alias for [`ModuleGraph::teardown`].
  pub fn remove_roots(&mut self, vm: &mut Vm, heap: &mut Heap) {
    self.teardown(vm, heap);
  }

  /// Returns the module's `[[AsyncEvaluationOrder]]` value if it has been assigned an integer.
  ///
  /// This is a minimal testing/introspection hook used to validate deterministic async module
  /// evaluation ordering.
  pub fn module_async_evaluation_order(&self, module: ModuleId) -> Option<u64> {
    self
      .async_eval_states
      .get(module_index(module))
      .and_then(|s| s.async_evaluation_order.as_integer())
  }

  /// Implements the `InnerModuleEvaluation` bookkeeping needed to assign deterministic
  /// `[[AsyncEvaluationOrder]]` values for top-level await graphs.
  ///
  /// This currently models only the state transitions needed for `AsyncModuleExecutionFulfilled`'s
  /// `execList` sorting (evaluation order + pending dependency counts + async-parent links). It
  /// intentionally does **not** execute module code.
  pub fn inner_module_evaluation(&mut self, vm: &mut Vm, module: ModuleId) -> Result<(), VmError> {
    // Reset the VM's async evaluation counter so repeated invocations on the same VM/graph assign
    // deterministic `[[AsyncEvaluationOrder]]` integers.
    vm.reset_module_async_evaluation_count();

    // Reset per-module async evaluation state so callers can invoke this deterministically on a
    // fresh graph (e.g. unit tests).
    for state in &mut self.async_eval_states {
      state.async_evaluation_order = AsyncEvaluationOrder::Unset;
      state.pending_async_dependencies = None;
      state.async_parent_modules.clear();
    }

    let module_count = self.modules.len();
    let mut visited: Vec<bool> = Vec::new();
    visited
      .try_reserve_exact(module_count)
      .map_err(|_| VmError::OutOfMemory)?;
    visited.resize(module_count, false);
    self.inner_module_evaluation_dfs(vm, module, &mut visited)
  }

  fn inner_module_evaluation_dfs(
    &mut self,
    vm: &mut Vm,
    module: ModuleId,
    visited: &mut [bool],
  ) -> Result<(), VmError> {
    let idx = module_index(module);
    if idx >= self.modules.len() {
      return Err(VmError::invalid_handle());
    }
    if visited[idx] {
      return Ok(());
    }
    visited[idx] = true;

    // Recurse into requested modules first (left-to-right).
    let mut deps: Vec<ModuleId> = Vec::new();
    deps
      .try_reserve_exact(self.modules[idx].requested_modules.len())
      .map_err(|_| VmError::OutOfMemory)?;
    for request in &self.modules[idx].requested_modules {
      let Some(dep) = self.get_imported_module(module, request) else {
        continue;
      };
      deps.push(dep);
    }
    for &dep in &deps {
      self.inner_module_evaluation_dfs(vm, dep, visited)?;
    }

    // Compute PendingAsyncDependencies and build AsyncParentModules edges.
    let mut pending: usize = 0;
    for &dep in &deps {
      let dep_idx = module_index(dep);
      if self.async_eval_states[dep_idx]
        .async_evaluation_order
        .as_integer()
        .is_some()
      {
        pending = pending.saturating_add(1);
        let parent_modules = &mut self.async_eval_states[dep_idx].async_parent_modules;
        parent_modules
          .try_reserve(1)
          .map_err(|_| VmError::OutOfMemory)?;
        parent_modules.push(module);
      }
    }

    if pending > 0 || self.modules[idx].has_tla {
      // `IncrementModuleAsyncEvaluationCount` assigns unique monotonic integers in discovery order.
      let order = vm.increment_module_async_evaluation_count();
      self.async_eval_states[idx].async_evaluation_order = AsyncEvaluationOrder::Order(order);
      self.async_eval_states[idx].pending_async_dependencies = Some(pending);
    } else {
      // Leave the module as `~unset~` / `~empty~` for purely-synchronous modules.
      self.async_eval_states[idx].async_evaluation_order = AsyncEvaluationOrder::Unset;
      self.async_eval_states[idx].pending_async_dependencies = None;
    }

    Ok(())
  }

  fn gather_available_ancestors(
    &mut self,
    module: ModuleId,
    exec_list: &mut Vec<ModuleId>,
  ) -> Result<(), VmError> {
    let idx = module_index(module);
    if idx >= self.modules.len() {
      return Err(VmError::invalid_handle());
    }

    let parents_len = self.async_eval_states[idx].async_parent_modules.len();
    for parent_i in 0..parents_len {
      let parent = self.async_eval_states[idx].async_parent_modules[parent_i];
      if exec_list.contains(&parent) {
        continue;
      }

      let parent_idx = module_index(parent);
      if parent_idx >= self.modules.len() {
        return Err(VmError::invalid_handle());
      }

      let pending = self.async_eval_states[parent_idx]
        .pending_async_dependencies
        .ok_or(VmError::InvariantViolation(
          "GatherAvailableAncestors: async parent missing pending dependency count",
        ))?;
      debug_assert!(pending > 0, "pendingAsyncDependencies underflow");
      let new_pending = pending.saturating_sub(1);
      self.async_eval_states[parent_idx].pending_async_dependencies = Some(new_pending);

      if new_pending == 0 {
        exec_list
          .try_reserve(1)
          .map_err(|_| VmError::OutOfMemory)?;
        exec_list.push(parent);
        if !self.modules[parent_idx].has_tla {
          self.gather_available_ancestors(parent, exec_list)?;
        }
      }
    }

    Ok(())
  }

  /// Implements `AsyncModuleExecutionFulfilled`'s `execList` computation and sorting.
  ///
  /// This updates `[[PendingAsyncDependencies]]` for async parents and returns the `sortedExecList`
  /// (ordered by `[[AsyncEvaluationOrder]]` ascending).
  ///
  /// Note: this is currently a *state-machine helper* for top-level await tests and does not yet
  /// execute module code.
  pub fn async_module_execution_fulfilled(
    &mut self,
    module: ModuleId,
  ) -> Result<Vec<ModuleId>, VmError> {
    let idx = module_index(module);
    if idx >= self.modules.len() {
      return Err(VmError::invalid_handle());
    }

    // Mirror the spec's `~done~` transition.
    debug_assert!(
      matches!(
        self.async_eval_states[idx].async_evaluation_order,
        AsyncEvaluationOrder::Order(_)
      ),
      "AsyncModuleExecutionFulfilled called on a module without an integer AsyncEvaluationOrder"
    );
    self.async_eval_states[idx].async_evaluation_order = AsyncEvaluationOrder::Done;

    let mut exec_list: Vec<ModuleId> = Vec::new();
    self.gather_available_ancestors(module, &mut exec_list)?;

    exec_list.sort_unstable_by(|a, b| {
      let a_order = self
        .async_eval_states
        .get(module_index(*a))
        .and_then(|s| s.async_evaluation_order.as_integer())
        .unwrap_or(u64::MAX);
      let b_order = self
        .async_eval_states
        .get(module_index(*b))
        .and_then(|s| s.async_evaluation_order.as_integer())
        .unwrap_or(u64::MAX);
      a_order.cmp(&b_order)
    });

    // Spec invariant: all elements are sorted by their integer order.
    debug_assert!(exec_list.windows(2).all(|w| match w {
      [a, b] =>
        self
          .module_async_evaluation_order(*a)
          .cmp(&self.module_async_evaluation_order(*b))
          != Ordering::Greater,
      _ => true,
    }));

    Ok(exec_list)
  }

  /// Abort an in-progress async module evaluation created via top-level `await`.
  ///
  /// This is used by embeddings that only support async evaluation when the returned evaluation
  /// promise settles via microtasks (for example, `await Promise.resolve()`), and must fail
  /// deterministically when evaluation remains pending after draining microtasks.
  pub fn abort_tla_evaluation(&mut self, vm: &mut Vm, heap: &mut Heap, module: ModuleId) {
    let idx = module_index(module);
    if idx >= self.modules.len() {
      return;
    }

    // Create a stable abort reason (`TypeError`) when possible.
    let mut scope = heap.scope();
    let reason = match vm.intrinsics() {
      Some(intr) => crate::error_object::new_type_error_object(&mut scope, &intr, TLA_ABORT_REASON)
        .unwrap_or(Value::Undefined),
      None => Value::Undefined,
    };
    // Root the reason across any Promise/job allocations below.
    if scope.push_root(reason).is_err() {
      // If we can't root the reason, aborting is best-effort; still attempt to tear down state and
      // restore the module graph pointer.
    }

    // Reject evaluation promises best-effort. Route any resulting Promise jobs into a local queue
    // and discard them immediately: hosts that call `abort_tla_evaluation` are explicitly *not*
    // going to drive the event loop, but we still need to clean up any persistent roots owned by
    // queued jobs.
    let mut abort_hooks = crate::MicrotaskQueue::new();

    // Collect SCC roots that are currently in-progress so we can mark them errored and reject their
    // evaluation promises.
    let entry_scc_root = self.modules[idx].cycle_root.unwrap_or(module);
    let entry_root_idx = module_index(entry_scc_root);

    // Iterate SCC roots in ascending module id order without allocating a separate root list. This
    // avoids abort-on-OOM behavior in `Vec::push` / stable sort scratch allocations.
    for root_idx in 0..self.scc_eval_states.len() {
      let is_in_progress = self
        .scc_eval_states
        .get(root_idx)
        .and_then(|s| s.as_ref())
        .is_some();
      if !is_in_progress && root_idx != entry_root_idx {
        continue;
      }

      let scc_root = ModuleId::from_raw(root_idx as u64);
      if root_idx >= self.modules.len() {
        continue;
      }

      // Mark all members of the SCC as errored with the abort reason.
      let members_len = self.scc_members.get(root_idx).map(|m| m.len()).unwrap_or(0);
      if members_len == 0 {
        // Fall back to the root module itself if SCC members are unexpectedly missing.
        let midx = module_index(scc_root);
        if midx < self.modules.len() {
          self.modules[midx].status = ModuleStatus::Errored;
          let _ = self.cache_module_error_value(&mut scope, midx, reason);
          if self.modules[midx].ast_external_memory.is_some() {
            self.modules[midx].clear_ast();
          }
        }
      }
      for member_i in 0..members_len {
        let member = self.scc_members[root_idx][member_i];
        let midx = module_index(member);
        if midx >= self.modules.len() {
          continue;
        }
        self.modules[midx].status = ModuleStatus::Errored;
        let _ = self.cache_module_error_value(&mut scope, midx, reason);
        if self.modules[midx].ast_external_memory.is_some() {
          self.modules[midx].clear_ast();
        }
      }

      // Reject the evaluation promise if one exists.
      if let Some(roots) = self.modules[root_idx].top_level_capability.as_ref() {
        if let Some(cap) = roots.capability(scope.heap()) {
          let reject = cap.reject;
          let _ = vm.call_with_host(
            &mut scope,
            &mut abort_hooks,
            reject,
            Value::Undefined,
            &[reason],
          );
        }
      }

      // Stop any in-progress evaluation state machine for this SCC.
      if root_idx < self.scc_eval_states.len() {
        self.scc_eval_states[root_idx] = None;
      }
    }

    // Tear down any pending dynamic import continuations and their roots.
    let had_dynamic_imports = !self.pending_dynamic_import_evaluations.is_empty();
    for (_id, entry) in self.pending_dynamic_import_evaluations.drain() {
      entry.state.teardown_roots(scope.heap_mut());
    }

    // Always tear down any remaining async module evaluation state (continuation frames, awaited
    // promise root, etc) so persistent roots do not leak across heap reuse.
    for slot in &mut self.tla_states {
      if let Some(state) = slot.take() {
        state.teardown(vm, scope.heap_mut());
      }
    }

    // Restore `vm.module_graph_ptr` and clear the refcount. This ensures any queued resume callbacks
    // become no-ops (they early-return because evaluation state was removed, and the VM no longer
    // points at this graph).
    while self.module_graph_ptr_refcount > 0 {
      self.release_module_graph_ptr(vm);
    }
    if had_dynamic_imports {
      // Draining the dynamic import map bypasses `take_pending_dynamic_import_evaluation`, so ensure
      // we do not retain the module graph pointer unnecessarily.
      self.module_graph_ptr_prev = None;
    }

    // Discard any queued SCC executions: the evaluation state machine was torn down above, so these
    // entries (if any) are now stale.
    self.ready_scc_queue.clear();
    self.processing_ready_scc_queue = false;

    struct AbortJobCtx<'a> {
      heap: &'a mut Heap,
    }

    impl crate::VmJobContext for AbortJobCtx<'_> {
      fn call(
        &mut self,
        _host: &mut dyn VmHostHooks,
        _callee: Value,
        _this: Value,
        _args: &[Value],
      ) -> Result<Value, VmError> {
        Err(VmError::Unimplemented("abort_tla_evaluation job call"))
      }

      fn construct(
        &mut self,
        _host: &mut dyn VmHostHooks,
        _callee: Value,
        _args: &[Value],
        _new_target: Value,
      ) -> Result<Value, VmError> {
        Err(VmError::Unimplemented("abort_tla_evaluation job construct"))
      }

      fn add_root(&mut self, value: Value) -> Result<RootId, VmError> {
        self.heap.add_root(value)
      }

      fn remove_root(&mut self, id: RootId) {
        self.heap.remove_root(id);
      }
    }

    {
      let mut ctx = AbortJobCtx {
        heap: scope.heap_mut(),
      };
      abort_hooks.teardown(&mut ctx);
    }
  }

  /// Implements `GetModuleNamespace` (ECMA-262 `#sec-getmodulenamespace`) for a module in this
  /// graph.
  ///
  /// If the module already has a cached namespace object, it is returned. Otherwise this creates
  /// and caches a new namespace object using [`module_namespace_create`].
  ///
  /// Important: this operation must **never throw** due to missing/ambiguous exports; those names
  /// are excluded from the namespace.
  pub fn get_module_namespace(
    &mut self,
    module: ModuleId,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
  ) -> Result<GcObject, VmError> {
    let idx = module_index(module);

    if let Some(cache) = self.modules[idx].namespace.as_ref() {
      let Some(Value::Object(obj)) = scope.heap().get_root(cache.object) else {
        return Err(VmError::invalid_handle());
      };
      return Ok(obj);
    }

    // exportedNames = module.GetExportedNames()
    let mut unambiguous_names = self.modules[idx].get_exported_names_with_vm(vm, self, module)?;

    // unambiguousNames = [ name | name in exportedNames, module.ResolveExport(name) is ResolvedBinding ]
    //
    // This list can be attacker-controlled (large export lists / `export *` graphs). Avoid
    // infallible host allocations (which abort the process on allocator OOM) by filtering in place
    // instead of building a new `Vec` via `push`.
    let mut resolve_err: Option<VmError> = None;
    unambiguous_names.retain(|name| {
      if resolve_err.is_some() {
        return false;
      }
      match self.modules[idx].resolve_export_with_vm(vm, self, module, name) {
        Ok(ResolveExportResult::Resolved(_)) => true,
        Ok(_) => false,
        Err(err) => {
          resolve_err = Some(err);
          false
        }
      }
    });
    if let Some(err) = resolve_err {
      return Err(err);
    }

    // Allocate and cache a placeholder namespace object *before* computing its exports list.
    //
    // `ModuleNamespaceCreate` may need to resolve namespace exports (`export * as ns from ...`),
    // which in turn calls `GetModuleNamespace` for the target module. Self-referential namespaces
    // (or cycles across multiple modules) would otherwise recurse infinitely if we only populated
    // `module.[[Namespace]]` after the exports list was fully constructed.
    let namespace_obj = scope.alloc_module_namespace_object(Vec::new().into_boxed_slice())?;
    let root = scope.heap_mut().add_root(Value::Object(namespace_obj))?;
    self.modules[idx].namespace = Some(ModuleNamespaceCache {
      object: root,
      exports: Vec::new(),
      external_memory: None,
    });
    self.torn_down = false;

    // Populate the namespace's `[[Exports]]` list and %Symbol.toStringTag%.
    let exports_sorted =
      match self.module_namespace_create(vm, scope, module, namespace_obj, unambiguous_names) {
        Ok(exports_sorted) => exports_sorted,
        Err(err) => {
          // Roll back the placeholder cache so subsequent calls don't observe an incomplete namespace.
          scope.heap_mut().remove_root(root);
          self.modules[idx].namespace = None;
          return Err(err);
        }
      };

    // Charge external bytes for the cached `[[Exports]]` list. This can be large for modules with
    // many exports.
    let exports_vec_bytes = exports_sorted
      .capacity()
      .saturating_mul(mem::size_of::<String>());
    let exports_string_bytes = exports_sorted
      .iter()
      .fold(0usize, |acc, s| acc.saturating_add(s.capacity()));
    let exports_total_bytes = exports_vec_bytes.saturating_add(exports_string_bytes);

    // Root the namespace object while charging: `charge_external` can trigger GC.
    let token_res = {
      let mut tmp = scope.reborrow();
      match tmp.push_root(Value::Object(namespace_obj)) {
        Ok(_) => tmp.heap_mut().charge_external(exports_total_bytes),
        Err(err) => Err(err),
      }
    };
    let token = match token_res {
      Ok(token) => token,
      Err(err) => {
        // Avoid leaving the graph in a partially-cached state.
        scope.heap_mut().remove_root(root);
        self.modules[idx].namespace = None;
        return Err(err);
      }
    };

    let token_arc = match arc_try_new_vm(token) {
      Ok(token_arc) => token_arc,
      Err(err) => {
        // Avoid leaving the graph in a partially-cached state.
        scope.heap_mut().remove_root(root);
        self.modules[idx].namespace = None;
        return Err(err);
      }
    };

    // Update the cached export list and keep the external memory charge token alive.
    let Some(cache) = self.modules[idx].namespace.as_mut() else {
      return Err(VmError::InvariantViolation(
        "module namespace placeholder cache was unexpectedly cleared",
      ));
    };
    cache.exports = exports_sorted;
    cache.external_memory = Some(token_arc);

    Ok(namespace_obj)
  }

  pub(crate) fn get_or_create_import_meta_object(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    hooks: &mut dyn VmHostHooks,
    module: ModuleId,
  ) -> Result<GcObject, VmError> {
    let idx = module.to_raw() as usize;
    let Some(record) = self.modules.get(idx) else {
      return Err(VmError::invalid_handle());
    };

    if let Some(root) = record.import_meta {
      let Some(Value::Object(obj)) = scope.heap().get_root(root) else {
        return Err(VmError::invalid_handle());
      };
      return Ok(obj);
    }

    // Bridge `VmHostHooks` into the `VmImportMetaHostHooks` interface used by
    // `create_import_meta_object`.
    struct Adapter<'a>(&'a mut dyn VmHostHooks);

    impl VmImportMetaHostHooks for Adapter<'_> {
      fn host_get_import_meta_properties(
        &mut self,
        vm: &mut Vm,
        scope: &mut Scope<'_>,
        module: ModuleId,
      ) -> Result<Vec<crate::ImportMetaProperty>, VmError> {
        self.0.host_get_import_meta_properties(vm, scope, module)
      }

      fn host_finalize_import_meta(
        &mut self,
        vm: &mut Vm,
        scope: &mut Scope<'_>,
        import_meta: GcObject,
        module: ModuleId,
      ) -> Result<(), VmError> {
        self
          .0
          .host_finalize_import_meta(vm, scope, import_meta, module)
      }
    }

    let mut adapter = Adapter(hooks);
    let import_meta = create_import_meta_object(vm, scope, &mut adapter, module)?;

    // Keep the object alive across GC by storing it as a persistent root.
    scope.push_root(Value::Object(import_meta))?;
    let root = scope.heap_mut().add_root(Value::Object(import_meta))?;

    // `self.modules` is indexed by `ModuleId::to_raw` (see `ModuleGraph::add_module*`).
    let record = self
      .modules
      .get_mut(idx)
      .ok_or_else(|| VmError::invalid_handle())?;
    record.import_meta = Some(root);
    self.torn_down = false;

    Ok(import_meta)
  }

  /// Convenience accessor for the module namespace's cached `[[Exports]]` list.
  pub fn module_namespace_exports(&self, module: ModuleId) -> Option<&[String]> {
    self.module(module).namespace_exports()
  }

  /// Populates each module's `[[LoadedModules]]` mapping using the host resolution map and the
  /// module's `[[RequestedModules]]` list.
  pub fn link_all_by_specifier(&mut self) {
    fn try_clone_js_string(value: &crate::JsString) -> Result<crate::JsString, VmError> {
      // Avoid `JsString::clone`, which allocates infallibly and can abort on allocator OOM.
      crate::JsString::from_code_units(value.as_code_units())
    }

    fn try_clone_import_attribute(
      value: &crate::ImportAttribute,
    ) -> Result<crate::ImportAttribute, VmError> {
      Ok(crate::ImportAttribute::new(
        try_clone_js_string(&value.key)?,
        try_clone_js_string(&value.value)?,
      ))
    }

    fn try_clone_module_request(value: &ModuleRequest) -> Result<ModuleRequest, VmError> {
      let specifier = crate::JsString::from_code_units(value.specifier.as_code_units())?;
      let mut attributes: Vec<crate::ImportAttribute> = Vec::new();
      attributes
        .try_reserve_exact(value.attributes.len())
        .map_err(|_| VmError::OutOfMemory)?;
      for attr in &value.attributes {
        attributes.push(try_clone_import_attribute(attr)?);
      }
      Ok(ModuleRequest::new(specifier, attributes))
    }

    for referrer_idx in 0..self.modules.len() {
      let requested_len = self.modules[referrer_idx].requested_modules.len();
      // Best-effort preallocation: `link_all_by_specifier` is a test convenience API. If we cannot
      // reserve space for all edges, stop linking rather than aborting the process.
      if self.modules[referrer_idx]
        .loaded_modules
        .try_reserve(requested_len)
        .is_err()
      {
        return;
      }

      for i in 0..requested_len {
        let imported = {
          let request = &self.modules[referrer_idx].requested_modules[i];
          self.resolve_host_module(request)
        };
        if let Some(imported) = imported {
          let request =
            match try_clone_module_request(&self.modules[referrer_idx].requested_modules[i]) {
              Ok(r) => r,
              Err(_) => return,
            };
          // `loaded_modules` capacity was reserved above, and the request has been cloned
          // successfully, so this push is now infallible.
          self.modules[referrer_idx]
            .loaded_modules
            .push(LoadedModuleRequest::new(request, imported));
        } else {
          // `ModuleGraph` is a small in-memory helper used primarily by unit tests. Avoid panicking
          // in library code; missing host resolution simply leaves the request unlinked.
          debug_assert!(
            false,
            "ModuleGraph::link_all_by_specifier: no module registered for specifier {:?}",
            self.modules[referrer_idx].requested_modules[i].specifier
          );
        }
      }
    }

    // `link_all_by_specifier` is a convenience helper used by tests that construct module graphs
    // entirely in-memory. Treat linking as "modules have been loaded", and advance `New` modules to
    // `Unlinked` like `LoadRequestedModules` does.
    for module in &mut self.modules {
      if module.status == ModuleStatus::New {
        module.status = ModuleStatus::Unlinked;
      }
    }

    // SCC structure depends on the resolved `[[LoadedModules]]` edges.
    self.mark_scc_dirty();
  }

  /// Implements ECMA-262 `GetImportedModule(referrer, request)`.
  pub fn get_imported_module(
    &self,
    referrer: ModuleId,
    request: &ModuleRequest,
  ) -> Option<ModuleId> {
    self.modules[module_index(referrer)]
      .loaded_modules
      .iter()
      .find(|loaded| loaded.request.spec_equal(request))
      .map(|loaded| loaded.module)
  }

  fn resolve_host_module(&self, request: &ModuleRequest) -> Option<ModuleId> {
    self
      .host_resolve
      .iter()
      .find_map(|(req, id)| (req == request).then_some(*id))
  }

  fn module_namespace_create(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    module: ModuleId,
    obj: GcObject,
    mut exports: Vec<String>,
  ) -> Result<Vec<String>, VmError> {
    // 1. Let exports be a List whose elements are the String values representing the exports of module.
    // 2. Let sortedExports be a List containing the same values as exports in ascending order.
    //
    // Avoid cloning attacker-controlled strings: infallible `String::clone` can abort the process
    // under allocator OOM. Also use `sort_unstable_by` to avoid allocations from the stable sort
    // implementation (sorting does not require stability because export names are unique).
    crate::tick::sort_unstable_by_with_ticks(&mut exports, |a, b| cmp_utf16(a, b), || vm.tick())?;

    let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
      "module namespaces require intrinsics",
    ))?;
    let getter_call = vm.module_namespace_getter_call_id()?;

    // Allocate the export list and capture binding resolution information.
    //
    // Root allocated strings (export names / binding names) while constructing the list: the export
    // list itself lives in a Rust vector and is not GC-traced until it is stored in the module
    // namespace object.
    let mut inner = scope.reborrow();
    let mut export_entries: Vec<ModuleNamespaceExport> = Vec::new();
    export_entries
      .try_reserve_exact(exports.len())
      .map_err(|_| VmError::OutOfMemory)?;

    for export_name in &exports {
      let resolution = match self.modules[module_index(module)].resolve_export_with_vm(
        vm,
        self,
        module,
        export_name,
      )? {
        ResolveExportResult::Resolved(res) => res,
        _ => {
          return Err(VmError::InvariantViolation(
            "module namespace export list contains a missing/ambiguous name",
          ))
        }
      };

      let export_key_s = inner.alloc_string(export_name)?;
      inner.push_root(Value::String(export_key_s))?;

      let (value, getter_slots, getter_env) = match resolution.binding_name {
        crate::module_record::BindingName::Name(local_name) => {
          let env_root = self.modules[module_index(resolution.module)]
            .environment
            .ok_or(VmError::Unimplemented(
              "module namespace requires linked module environments",
            ))?;
          let env = inner
            .heap()
            .get_env_root(env_root)
            .ok_or_else(|| VmError::invalid_handle())?;

          let binding_name_s = inner.alloc_string(&local_name)?;
          inner.push_root(Value::String(binding_name_s))?;

          (
            ModuleNamespaceExportValue::Binding {
              env,
              name: binding_name_s,
            },
            [Value::String(binding_name_s), Value::String(export_key_s)],
            Some(env),
          )
        }
        crate::module_record::BindingName::Namespace => {
          let ns = self.get_module_namespace(resolution.module, vm, &mut inner)?;
          inner.push_root(Value::Object(ns))?;
          (
            ModuleNamespaceExportValue::Namespace { namespace: ns },
            [Value::Object(ns), Value::String(export_key_s)],
            None,
          )
        }
      };

      let getter = inner.alloc_native_function_with_slots_and_env(
        getter_call,
        None,
        export_key_s,
        0,
        &getter_slots,
        getter_env,
      )?;
      inner
        .heap_mut()
        .object_set_prototype(getter, Some(intr.function_prototype()))?;
      inner.push_root(Value::Object(getter))?;

      export_entries.push(ModuleNamespaceExport {
        name: export_key_s,
        getter,
        value,
      });
    }

    let exports_boxed = export_entries.into_boxed_slice();

    // Attach the exports list to the (already-allocated) namespace object.
    //
    // The namespace object may have been cached before this call (to break cycles), so update it in
    // place rather than allocating a new object.
    inner.set_module_namespace_exports(obj, exports_boxed)?;
    inner.push_root(Value::Object(obj))?;

    // Define %Symbol.toStringTag% = "Module" (non-writable, non-enumerable, non-configurable).
    let tag_string = inner.alloc_string("Module")?;
    let desc = PropertyDescriptor {
      enumerable: false,
      configurable: false,
      kind: PropertyKind::Data {
        value: Value::String(tag_string),
        writable: false,
      },
    };
    inner.define_property(
      obj,
      PropertyKey::Symbol(intr.well_known_symbols().to_string_tag),
      desc,
    )?;
    Ok(exports)
  }

  fn cache_module_error_value(
    &mut self,
    scope: &mut Scope<'_>,
    module_index: usize,
    value: Value,
  ) -> Result<(), VmError> {
    if self.modules[module_index].error.is_some() {
      return Ok(());
    }
    let root = scope.heap_mut().add_root(value)?;
    self.modules[module_index].error = Some(root);
    self.torn_down = false;
    Ok(())
  }

  fn cache_module_error_from_err(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    module_index: usize,
    err: &VmError,
  ) -> Result<(), VmError> {
    if let Some(thrown) = err.thrown_value() {
      return self.cache_module_error_value(scope, module_index, thrown);
    }

    // Internal VM errors (OOM, termination, invalid handles, unimplemented paths, etc) should still
    // transition the module into a deterministic errored state. Create and cache a stable `Error`
    // instance when possible.
    let message = non_throw_vm_error_message(err);
    let value = match vm.intrinsics() {
      Some(intr) => crate::new_error(scope, intr.error_prototype(), "Error", message)
        .unwrap_or(Value::Undefined),
      None => Value::Undefined,
    };
    self.cache_module_error_value(scope, module_index, value)
  }

  fn module_errored_value(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    module_index: usize,
  ) -> Result<Value, VmError> {
    if let Some(root) = self.modules[module_index].error {
      return scope
        .heap()
        .get_root(root)
        .ok_or_else(|| VmError::InvariantViolation("module error root missing from heap"));
    }

    // If we don't have a cached error value, this is most likely an internal bug (the module should
    // only enter `Errored` via a throw completion). Best-effort: if intrinsics are available,
    // allocate a generic `Error` instance so callers still observe an ECMAScript-style abrupt
    // completion.
    let Some(intr) = vm.intrinsics() else {
      return Err(VmError::InvariantViolation(
        "errored module has no cached error value",
      ));
    };

    let value = crate::new_error(scope, intr.error_prototype(), "Error", "errored module")?;
    self.cache_module_error_value(scope, module_index, value)?;
    Ok(value)
  }

  /// Links a module using an existing [`Scope`].
  ///
  /// This is a lower-level variant of [`ModuleGraph::link`] for callers that already hold a `Scope`
  /// (e.g. module loading continuations).
  pub fn link_with_scope(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    global_object: GcObject,
    realm_id: RealmId,
    module: ModuleId,
  ) -> Result<(), VmError> {
    // Use a nested scope so any temporary stack roots are popped before returning to the caller.
    let mut link_scope = scope.reborrow();
    self.link_inner(vm, &mut link_scope, global_object, realm_id, module)
  }

  pub fn link(
    &mut self,
    vm: &mut Vm,
    heap: &mut Heap,
    global_object: GcObject,
    realm_id: RealmId,
    module: ModuleId,
  ) -> Result<(), VmError> {
    let mut scope = heap.scope();
    self.link_with_scope(vm, &mut scope, global_object, realm_id, module)
  }

  fn link_inner(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    global_object: GcObject,
    realm_id: RealmId,
    module: ModuleId,
  ) -> Result<(), VmError> {
    let idx = module_index(module);
    let status = self
      .modules
      .get(idx)
      .ok_or_else(|| VmError::invalid_handle())?
      .status;

    match status {
      ModuleStatus::Linked
      | ModuleStatus::Evaluating
      | ModuleStatus::EvaluatingAsync
      | ModuleStatus::Evaluated => return Ok(()),
      ModuleStatus::Linking => return Ok(()),
      ModuleStatus::Errored => {
        let value = self.module_errored_value(vm, scope, idx)?;
        return Err(VmError::Throw(value));
      }
      ModuleStatus::New | ModuleStatus::Unlinked => {}
    }

    // Ensure module linking work observes VM fuel/deadline/interrupt state, even when modules have
    // no executable statements (and therefore do not run through the evaluator's statement-level
    // tick loop during instantiation).
    vm.tick()?;

    // Mark linking in progress (cycle-safe).
    self.modules[idx].status = ModuleStatus::Linking;
    let link_result = (|| -> Result<(), VmError> {
      // Ensure the module has an environment root allocated early so cycles can create import
      // bindings to it.
      if self.modules[idx].environment.is_none() {
        let env = scope.env_create(self.global_lexical_env)?;
        scope.push_env_root(env)?;
        let root = scope.heap_mut().add_env_root(env)?;
        self.modules[idx].environment = Some(root);
        self.torn_down = false;
      }

      // Avoid cloning attacker-controlled strings during linking: infallible `String::clone` can
      // abort the process under allocator OOM (see `oom_regressions` / `oom_harness` moduleLink).
      //
      // In particular:
      // - `requested_modules` can contain huge module specifiers / attribute strings,
      // - `import_entries` can contain huge local binding names,
      // - `indirect_export_entries` can contain huge export names / module specifiers.
      //
      // This function avoids cloning those lists and instead iterates them with scoped borrows,
      // temporarily taking ownership of `import_entries` only when needed to avoid borrow conflicts
      // while mutating the graph (creating namespace objects, linking dependencies, etc).
      let has_default_export = self.modules[idx]
        .local_export_entries
        .iter()
        .any(|e| e.local_name == "*default*");
      let compiled = self.modules[idx].compiled.clone();
      let source = self.modules[idx]
        .source
        .clone()
        .or_else(|| compiled.as_ref().map(|c| c.source.clone()))
        .ok_or(VmError::Unimplemented("module source missing"))?;
      let ast = self.modules[idx].ast.clone();

      // Link dependencies first.
      const LINK_TICK_EVERY: usize = 32;
      let requested_len = self.modules[idx].requested_modules.len();
      for i in 0..requested_len {
        if i % LINK_TICK_EVERY == 0 && i != 0 {
          vm.tick()?;
        }
        let imported = {
          let request = &self.modules[idx].requested_modules[i];
          self
            .get_imported_module(module, request)
            .ok_or(VmError::Unimplemented("unlinked module request"))?
        };
        self.link_inner(vm, scope, global_object, realm_id, imported)?;
      }

      // Validate `[[IndirectExportEntries]]` (re-exports).
      //
      // ECMA-262 requires `ModuleDeclarationInstantiation` to throw a SyntaxError if any indirect
      // export does not resolve to a concrete binding. This ensures broken re-exports fail during
      // linking (test262 `negative.phase: resolution`) even when no other module imports them.
      for (i, entry) in self.modules[idx].indirect_export_entries.iter().enumerate() {
        if i % LINK_TICK_EVERY == 0 && i != 0 {
          vm.tick()?;
        }

        let resolution =
          self.modules[idx].resolve_export_with_vm(vm, self, module, &entry.export_name)?;

        match resolution {
          ResolveExportResult::Resolved(_) => {}
          ResolveExportResult::NotFound => {
            let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
              "module linking requires intrinsics to create SyntaxError objects",
            ))?;
            let (specifier, _) = crate::string::utf16_to_utf8_lossy_bounded_with_tick(
              entry.module_request.specifier.as_code_units(),
              crate::fallible_format::MAX_ERROR_MESSAGE_BYTES,
              || vm.tick(),
            )?;
            let message = crate::fallible_format::try_format_error_message2(
              "Indirect export '",
              &entry.export_name,
              "' could not be resolved (re-export from '",
              &specifier,
              "')",
            )?;
            let err_obj = crate::error_object::new_syntax_error_object(scope, &intr, &message)?;
            return Err(VmError::Throw(err_obj));
          }
          ResolveExportResult::Ambiguous => {
            let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
              "module linking requires intrinsics to create SyntaxError objects",
            ))?;
            let (specifier, _) = crate::string::utf16_to_utf8_lossy_bounded_with_tick(
              entry.module_request.specifier.as_code_units(),
              crate::fallible_format::MAX_ERROR_MESSAGE_BYTES,
              || vm.tick(),
            )?;
            let message = crate::fallible_format::try_format_error_message2(
              "Indirect export '",
              &entry.export_name,
              "' is ambiguous (re-export from '",
              &specifier,
              "')",
            )?;
            let err_obj = crate::error_object::new_syntax_error_object(scope, &intr, &message)?;
            return Err(VmError::Throw(err_obj));
          }
        }
      }

      let env_root = self.modules[idx]
        .environment
        .ok_or(VmError::InvariantViolation(
          "module environment root missing",
        ))?;
      let module_env = scope
        .heap()
        .get_env_root(env_root)
        .ok_or_else(|| VmError::invalid_handle())?;

      // Create import bindings.
      //
      // Take ownership of the import entry list to avoid holding an immutable borrow of
      // `self.modules[idx]` while calling into routines that require `&mut self` (namespace creation,
      // module linking recursion, etc). Restore the list before returning so module records remain
      // self-contained even when linking fails.
      let import_entries = std::mem::take(&mut self.modules[idx].import_entries);
      let import_bindings_result =
        (|| -> Result<(), VmError> {
          for (i, entry) in import_entries.iter().enumerate() {
            if i % LINK_TICK_EVERY == 0 && i != 0 {
              vm.tick()?;
            }
            let imported_module = self
              .get_imported_module(module, &entry.module_request)
              .ok_or(VmError::Unimplemented("unlinked module request"))?;

            match &entry.import_name {
              crate::module_record::ImportName::All => {
                let ns = self.get_module_namespace(imported_module, vm, scope)?;
                let mut init_scope = scope.reborrow();
                init_scope.push_root(Value::Object(ns))?;
                init_scope.env_create_immutable_binding(module_env, &entry.local_name)?;
                init_scope
                  .heap_mut()
                  .env_initialize_binding(module_env, &entry.local_name, Value::Object(ns))?;
              }
              crate::module_record::ImportName::Name(import_name) => {
                let resolution = self.modules[module_index(imported_module)]
                  .resolve_export_with_vm(vm, self, imported_module, import_name)?;
                let resolution = match resolution {
                  ResolveExportResult::Resolved(resolution) => resolution,
                  ResolveExportResult::NotFound => {
                    let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
                      "module linking requires intrinsics to create SyntaxError objects",
                    ))?;
                    let (specifier, _) = crate::string::utf16_to_utf8_lossy_bounded_with_tick(
                      entry.module_request.specifier.as_code_units(),
                      crate::fallible_format::MAX_ERROR_MESSAGE_BYTES,
                      || vm.tick(),
                    )?;
                    let message = crate::fallible_format::try_format_error_message2(
                      "The requested module '",
                      &specifier,
                      "' does not provide an export named '",
                      import_name,
                      "'",
                    )?;
                    let err_obj =
                      crate::error_object::new_syntax_error_object(scope, &intr, &message)?;
                    return Err(VmError::Throw(err_obj));
                  }
                  ResolveExportResult::Ambiguous => {
                    let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
                      "module linking requires intrinsics to create SyntaxError objects",
                    ))?;
                    let (specifier, _) = crate::string::utf16_to_utf8_lossy_bounded_with_tick(
                      entry.module_request.specifier.as_code_units(),
                      crate::fallible_format::MAX_ERROR_MESSAGE_BYTES,
                      || vm.tick(),
                    )?;
                    let message = crate::fallible_format::try_format_error_message2(
                      "The requested module '",
                      &specifier,
                      "' provides an ambiguous export named '",
                      import_name,
                      "'",
                    )?;
                    let err_obj =
                      crate::error_object::new_syntax_error_object(scope, &intr, &message)?;
                    return Err(VmError::Throw(err_obj));
                  }
                };

                match resolution.binding_name {
                  crate::module_record::BindingName::Namespace => {
                    let ns = self.get_module_namespace(resolution.module, vm, scope)?;
                    let mut init_scope = scope.reborrow();
                    init_scope.push_root(Value::Object(ns))?;
                    init_scope.env_create_immutable_binding(module_env, &entry.local_name)?;
                    init_scope
                      .heap_mut()
                      .env_initialize_binding(module_env, &entry.local_name, Value::Object(ns))?;
                  }
                  crate::module_record::BindingName::Name(target_name) => {
                    let target_env_root = self.modules[module_index(resolution.module)]
                      .environment
                      .ok_or(VmError::InvariantViolation(
                        "resolved export module missing environment",
                      ))?;
                    let target_env = scope
                      .heap()
                      .get_env_root(target_env_root)
                      .ok_or_else(|| VmError::invalid_handle())?;
                    scope.env_create_import_binding(
                      module_env,
                      &entry.local_name,
                      target_env,
                      &target_name,
                    )?;
                  }
                }
              }
            }
          }
          Ok(())
        })();
      self.modules[idx].import_entries = import_entries;
      import_bindings_result?;

      // Ensure `*default*` exists for `export default <expr>`.
      if has_default_export {
        if !scope.heap().env_has_binding(module_env, "*default*")? {
          scope.env_create_immutable_binding(module_env, "*default*")?;
        }
      }

      // Instantiate local declarations (creates bindings + hoists function objects).
      if let Some(ast) = ast {
        instantiate_module_decls(
          vm,
          scope,
          global_object,
          module,
          module_env,
          source,
          &ast.stx.body,
        )?;
      } else {
        let has_tla = self.modules[idx].has_tla;
        let compiled = self.modules[idx].compiled.clone();
        match compiled {
          Some(script)
            if !script.requires_ast_fallback && !script.contains_async_generators && !has_tla =>
          {
            instantiate_compiled_module_decls(vm, scope, global_object, module, module_env, script)?;
          }
          _ => {
            // Either:
            // - no compiled module payload exists, or
            // - this module must run through the AST evaluator (top-level await, async/generator
            //   fallback, ...).
            //
            // Modules normally do not retain an AST after parsing. Parse/charge it on demand so the
            // interpreter instantiation path can run.
            self.ensure_module_ast(vm, scope.heap_mut(), module)?;
            let ast = self.modules[idx]
              .ast
              .clone()
              .ok_or(VmError::Unimplemented("module AST missing"))?;
            instantiate_module_decls(
              vm,
              scope,
              global_object,
              module,
              module_env,
              source,
              &ast.stx.body,
            )?;
          }
        }
      }
      Ok(())
    })();

    match link_result {
      Ok(()) => {
        self.modules[idx].status = ModuleStatus::Linked;
        Ok(())
      }
      Err(err) => {
        self.modules[idx].status = ModuleStatus::Errored;
        self.cache_module_error_from_err(vm, scope, idx, &err)?;
        // If we parsed and retained an AST solely for a compiled-module fallback, it is no longer
        // needed once the module has transitioned to an errored state.
        if self.modules[idx].ast_external_memory.is_some() {
          self.modules[idx].clear_ast();
        }
        Err(err)
      }
    }
  }

  /// Evaluates a module using an existing [`Scope`].
  ///
  /// This is a lower-level variant of [`ModuleGraph::evaluate`] that avoids creating a fresh scope
  /// (which is not possible when the caller already holds one).
  pub fn evaluate_with_scope(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    global_object: GcObject,
    realm_id: RealmId,
    module: ModuleId,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
  ) -> Result<Value, VmError> {
    // Ensure dynamic `import()` expressions executed during module evaluation can resolve the active
    // module graph even when the embedding uses the low-level `ModuleGraph::{link,evaluate}` APIs
    // directly (without constructing a `JsRuntime`, which sets this pointer at runtime creation).
    let mut graph_guard = ModuleGraphPtrGuard::install(vm, self);

    // If async module evaluation is already in progress for this module, return the existing
    // (spec-visible) evaluation promise.
    //
    // Spec: `Evaluate()` must be idempotent for in-progress async module evaluation: callers observe
    // the same Promise rather than a new Promise that could settle inconsistently.
    let idx = module_index(module);
    if let Some(record) = self.modules.get(idx) {
      if record.status == ModuleStatus::EvaluatingAsync {
        // Modules can be in the `EvaluatingAsync` state even if they do not directly contain
        // top-level await (e.g. modules in an async SCC where another module suspends).
        //
        // Only modules that *directly* contain top-level await should have a stored TLA evaluation
        // state; other SCC members can safely return the shared SCC evaluation promise without
        // requiring per-module state.
        if record.has_tla && self.tla_states.get(idx).and_then(|s| s.as_ref()).is_none() {
          return Err(VmError::InvariantViolation(
            "module is evaluating-async but has no stored TLA evaluation state",
          ));
        }

        // Async module evaluation is in progress; per spec, `Evaluate()` is idempotent and must
        // return the existing evaluation promise (stored on the SCC root module record via
        // `[[TopLevelCapability]]`).
        //
        // Note: modules can be in `EvaluatingAsync` even without top-level await when they are in
        // the same async SCC as (or have async dependencies on) a module that does.
        let scc_root = record.cycle_root.unwrap_or(module);
        let root_idx = module_index(scc_root);
        let Some(roots) = self
          .modules
          .get(root_idx)
          .and_then(|r| r.top_level_capability.as_ref())
        else {
          return Err(VmError::InvariantViolation(
            "module is evaluating-async but has no stored evaluation promise capability",
          ));
        };
        let promise = roots
          .capability(scope.heap())
          .ok_or_else(VmError::invalid_handle)?
          .promise;

        // Keep the module graph pointer installed until the in-progress evaluation completes.
        // The async resume / dynamic-import callbacks restore the previous pointer once the module
        // graph pointer refcount reaches zero (`release_module_graph_ptr`).
        graph_guard.disarm();
        return Ok(promise);
      }
    }

    let result = (|| -> Result<Value, VmError> {
      let mut eval_scope = scope.reborrow();

      // Ensure module linking runs with a defined Realm so hoisted function objects capture
      // `[[JobRealm]]` metadata. This context is intentionally `script_or_module: None` so nested
      // per-module contexts (pushed during instantiation) can set the active module precisely.
      let link_ctx = ExecutionContext {
        realm: realm_id,
        script_or_module: None,
      };
      let mut vm_ctx = vm.execution_context_guard(link_ctx)?;
      let prev_state = vm_ctx.load_realm_state(eval_scope.heap_mut(), realm_id)?;

      let inner: Result<Value, VmError> = (|| {
        // Cache SCC structure (cycle roots, dependency edges) before starting evaluation.
        self.ensure_scc_info(&mut *vm_ctx)?;

        // Determine the SCC (cycle) root for this module.
        let idx = module_index(module);
        let scc_root = self
          .modules
          .get(idx)
          .ok_or_else(|| VmError::invalid_handle())?
          .cycle_root
          .unwrap_or(module);

        // Ensure an evaluation promise exists for the SCC root and return it to the host.
        let promise =
          self.ensure_scc_promise(&mut *vm_ctx, &mut eval_scope, host, hooks, scc_root)?;

        // Link before evaluating. If linking fails (including the module already being in an errored
        // state), reject the evaluation promise with the thrown/cached value.
        if let Err(err) = self.link_with_scope(
          &mut *vm_ctx,
          &mut eval_scope,
          global_object,
          realm_id,
          module,
        ) {
          let reason = if let Some(thrown) = err.thrown_value() {
            thrown
          } else {
            // Best-effort: ensure we have a cached thrown value for deterministic subsequent
            // operations.
            self.cache_module_error_from_err(&mut *vm_ctx, &mut eval_scope, idx, &err)?;
            self.module_errored_value(&mut *vm_ctx, &mut eval_scope, idx)?
          };
          self.reject_scc_promise(
            &mut *vm_ctx,
            &mut eval_scope,
            host,
            hooks,
            scc_root,
            reason,
            Some(&err),
          )?;
          return Ok(promise);
        }

        // Start (or continue) evaluating the SCC rooted at `scc_root`.
        self.start_scc_evaluation(
          &mut *vm_ctx,
          &mut eval_scope,
          global_object,
          realm_id,
          scc_root,
          host,
          hooks,
        )?;

        Ok(promise)
      })();

      drop(vm_ctx);
      let restore_res = vm.restore_realm_state(eval_scope.heap_mut(), prev_state);
      match (inner, restore_res) {
        (Ok(v), Ok(())) => Ok(v),
        (Err(err), Ok(())) => Err(err),
        (Ok(_), Err(err)) => Err(err),
        (Err(err), Err(_)) => Err(err),
      }
    })();

    // If module evaluation initiated an async continuation whose Promise reactions are still
    // pending (top-level await or dynamic import), keep `vm.module_graph_ptr` installed until those
    // reactions run.
    //
    // Note: `insert_pending_dynamic_import_evaluation` captures the VM's current module graph
    // pointer as the "previous" value to restore. When dynamic import begins during this
    // synchronous evaluation, that value will be `self`. Overwrite it with the true outer previous
    // pointer before disarming the guard so completion restores correctly.
    if result.is_ok() && graph_guard.restore_on_drop && self.module_graph_ptr_refcount > 0 {
      let self_ptr: *mut ModuleGraph = self;
      if self.module_graph_ptr_prev == Some(self_ptr) && graph_guard.prev_graph() != Some(self_ptr)
      {
        self.module_graph_ptr_prev = graph_guard.prev_graph();
      }
      graph_guard.disarm();
    }

    result
  }

  fn ensure_scc_info(&mut self, vm: &mut Vm) -> Result<(), VmError> {
    if !self.scc_dirty {
      return Ok(());
    }

    let module_count = self.modules.len();
    for slot in &mut self.scc_members {
      slot.clear();
    }
    for slot in &mut self.scc_deps {
      slot.clear();
    }
    for slot in &mut self.scc_parents {
      slot.clear();
    }
    fn ensure_outer_len<T>(
      v: &mut Vec<Vec<T>>,
      len: usize,
    ) -> Result<(), VmError> {
      if v.len() < len {
        v.try_reserve_exact(len - v.len())
          .map_err(|_| VmError::OutOfMemory)?;
        while v.len() < len {
          v.push(Vec::new());
        }
      } else if v.len() > len {
        v.truncate(len);
      }
      Ok(())
    }

    ensure_outer_len(&mut self.scc_members, module_count)?;
    ensure_outer_len(&mut self.scc_deps, module_count)?;
    ensure_outer_len(&mut self.scc_parents, module_count)?;

    for module in &mut self.modules {
      module.cycle_root = None;
    }

    // --- Tarjan SCC over the module graph ---
    let graph: &ModuleGraph = &*self;
    let mut index: usize = 0;
    let mut stack: Vec<usize> = Vec::new();
    stack
      .try_reserve_exact(module_count)
      .map_err(|_| VmError::OutOfMemory)?;

    let mut on_stack: Vec<bool> = Vec::new();
    on_stack
      .try_reserve_exact(module_count)
      .map_err(|_| VmError::OutOfMemory)?;
    on_stack.resize(module_count, false);

    let mut indices: Vec<Option<usize>> = Vec::new();
    indices
      .try_reserve_exact(module_count)
      .map_err(|_| VmError::OutOfMemory)?;
    indices.resize(module_count, None);

    let mut lowlink: Vec<usize> = Vec::new();
    lowlink
      .try_reserve_exact(module_count)
      .map_err(|_| VmError::OutOfMemory)?;
    lowlink.resize(module_count, 0);
    let mut sccs: Vec<Vec<usize>> = Vec::new();
    sccs
      .try_reserve_exact(module_count)
      .map_err(|_| VmError::OutOfMemory)?;

    fn strongconnect(
      v: usize,
      graph: &ModuleGraph,
      vm: &mut Vm,
      index: &mut usize,
      stack: &mut Vec<usize>,
      on_stack: &mut [bool],
      indices: &mut [Option<usize>],
      lowlink: &mut [usize],
      sccs: &mut Vec<Vec<usize>>,
    ) -> Result<(), VmError> {
      vm.tick()?;

      indices[v] = Some(*index);
      lowlink[v] = *index;
      *index = index.saturating_add(1);
      stack.push(v);
      on_stack[v] = true;

      let module = ModuleId::from_raw(v as u64);
      let requests = &graph.modules[v].requested_modules;
      const EDGE_TICK_EVERY: usize = 32;
      for (i, request) in requests.iter().enumerate() {
        if i % EDGE_TICK_EVERY == 0 && i != 0 {
          vm.tick()?;
        }
        let Some(imported) = graph.get_imported_module(module, request) else {
          continue;
        };
        let w = module_index(imported);
        if w >= indices.len() {
          return Err(VmError::invalid_handle());
        }
        if indices[w].is_none() {
          strongconnect(w, graph, vm, index, stack, on_stack, indices, lowlink, sccs)?;
          lowlink[v] = lowlink[v].min(lowlink[w]);
        } else if on_stack[w] {
          let w_index = indices[w].unwrap_or(usize::MAX);
          lowlink[v] = lowlink[v].min(w_index);
        }
      }

      let v_index = indices[v].unwrap_or(usize::MAX);
      if lowlink[v] == v_index {
        let mut scc: Vec<usize> = Vec::new();
        loop {
          let Some(w) = stack.pop() else {
            return Err(VmError::InvariantViolation("SCC stack underflow"));
          };
          on_stack[w] = false;
          scc.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
          scc.push(w);
          if w == v {
            break;
          }
        }
        sccs.push(scc);
      }

      Ok(())
    }

    for v in 0..module_count {
      if indices[v].is_none() {
        strongconnect(
          v,
          graph,
          vm,
          &mut index,
          &mut stack,
          &mut on_stack,
          &mut indices,
          &mut lowlink,
          &mut sccs,
        )?;
      }
    }

    // --- Assign canonical cycle roots (minimum module id per SCC) and cache member lists ---
    for scc in sccs {
      let Some(&root_idx) = scc.iter().min() else {
        continue;
      };
      let root = ModuleId::from_raw(root_idx as u64);

      let mut members: Vec<ModuleId> = Vec::new();
      members
        .try_reserve_exact(scc.len())
        .map_err(|_| VmError::OutOfMemory)?;
      for &idx in &scc {
        let id = ModuleId::from_raw(idx as u64);
        members.push(id);
      }
      // Preserve the SCC member order produced by the DFS-based Tarjan walk. This matches the
      // spec-observable execution order for synchronous cyclic module evaluation (dependencies that
      // are discovered deeper in the DFS should execute before their parents). Sorting by module id
      // would be deterministic but can be observably wrong: a module may read an imported lexical
      // binding before the exporter has executed and initialized it, causing an unintended TDZ
      // ReferenceError.

      // Record the SCC membership on the cycle root.
      self.scc_members[root_idx] = members;

      // Assign the canonical cycle root to each member.
      for &idx in &scc {
        if let Some(record) = self.modules.get_mut(idx) {
          record.cycle_root = Some(root);
        }
      }
    }

    // --- Compute SCC dependencies and reverse dependencies ---
    for root_idx in 0..module_count {
      let Some(members) = self.scc_members.get(root_idx) else {
        continue;
      };
      if members.is_empty() {
        continue;
      }
      let root = ModuleId::from_raw(root_idx as u64);

      let mut deps: Vec<ModuleId> = Vec::new();
      for (mi, member) in members.iter().copied().enumerate() {
        if mi % 32 == 0 && mi != 0 {
          vm.tick()?;
        }
        let member_idx = module_index(member);
        let requests_len = self.modules[member_idx].requested_modules.len();
        for (ri, request) in self.modules[member_idx]
          .requested_modules
          .iter()
          .take(requests_len)
          .enumerate()
        {
          if ri % 64 == 0 && ri != 0 {
            vm.tick()?;
          }
          let Some(imported) = self.get_imported_module(member, request) else {
            continue;
          };
          let imported_idx = module_index(imported);
          let dep_root = self.modules[imported_idx].cycle_root.unwrap_or(imported);
          if dep_root == root {
            continue;
          }
          if !deps.contains(&dep_root) {
            deps.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
            deps.push(dep_root);
          }
        }
      }

      for dep_root in deps.iter().copied() {
        let dep_idx = module_index(dep_root);
        let parents = &mut self.scc_parents[dep_idx];
        if !parents.contains(&root) {
          parents.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
          parents.push(root);
        }
      }

      self.scc_deps[root_idx] = deps;
    }

    self.scc_dirty = false;
    Ok(())
  }

  fn ensure_scc_promise(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    scc_root: ModuleId,
  ) -> Result<Value, VmError> {
    let root_idx = module_index(scc_root);
    let record = self
      .modules
      .get_mut(root_idx)
      .ok_or_else(|| VmError::invalid_handle())?;

    if let Some(roots) = record.top_level_capability.as_ref() {
      let cap = roots
        .capability(scope.heap())
        .ok_or_else(VmError::invalid_handle)?;
      return Ok(cap.promise);
    }

    let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
      "module evaluation requires intrinsics",
    ))?;
    let cap = crate::builtins::new_promise_capability_with_host_and_hooks(
      vm,
      scope,
      host,
      hooks,
      Value::Object(intr.promise()),
    )?;

    record.set_top_level_capability(scope, cap)?;
    self.torn_down = false;
    Ok(cap.promise)
  }

  fn resolve_scc_promise(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    scc_root: ModuleId,
  ) -> Result<(), VmError> {
    let root_idx = module_index(scc_root);
    let Some(roots) = self
      .modules
      .get(root_idx)
      .and_then(|r| r.top_level_capability.as_ref())
    else {
      return Ok(());
    };
    let cap = roots
      .capability(scope.heap())
      .ok_or_else(VmError::invalid_handle)?;
    let resolve = cap.resolve;
    let mut call_scope = scope.reborrow();
    call_scope.push_root(resolve)?;
    let _ = vm.call_with_host_and_hooks(
      host,
      &mut call_scope,
      hooks,
      resolve,
      Value::Undefined,
      &[Value::Undefined],
    )?;
    Ok(())
  }

  fn reject_scc_promise(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    scc_root: ModuleId,
    reason: Value,
    err: Option<&VmError>,
  ) -> Result<(), VmError> {
    if let Some(err) = err {
      attach_stack_property_for_promise_rejection(scope, reason, err);
    }

    let root_idx = module_index(scc_root);
    let Some(roots) = self
      .modules
      .get(root_idx)
      .and_then(|r| r.top_level_capability.as_ref())
    else {
      return Ok(());
    };
    let cap = roots
      .capability(scope.heap())
      .ok_or_else(VmError::invalid_handle)?;
    let reject = cap.reject;
    let mut call_scope = scope.reborrow();
    call_scope.push_roots(&[reject, reason])?;
    let _ = vm.call_with_host_and_hooks(
      host,
      &mut call_scope,
      hooks,
      reject,
      Value::Undefined,
      &[reason],
    )?;
    Ok(())
  }

  fn start_scc_evaluation(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    global_object: GcObject,
    realm_id: RealmId,
    scc_root: ModuleId,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
  ) -> Result<(), VmError> {
    let root_idx = module_index(scc_root);

    // If the SCC already completed (success or error), ensure its evaluation promise is settled and
    // return.
    let root_status = self
      .modules
      .get(root_idx)
      .ok_or_else(|| VmError::invalid_handle())?
      .status;
    match root_status {
      ModuleStatus::Evaluated => {
        self.resolve_scc_promise(vm, scope, host, hooks, scc_root)?;
        return Ok(());
      }
      ModuleStatus::Errored => {
        let reason = self.module_errored_value(vm, scope, root_idx)?;
        self.reject_scc_promise(vm, scope, host, hooks, scc_root, reason, None)?;
        return Ok(());
      }
      ModuleStatus::EvaluatingAsync => {
        // In progress.
      }
      _ => {}
    }

    if self
      .scc_eval_states
      .get(root_idx)
      .and_then(|s| s.as_ref())
      .is_some()
    {
      return Ok(());
    }

    // Mark all members in this SCC as evaluating-async so recursive graph walks treat them as "in
    // progress" until the SCC settles.
    let members_len = self.scc_members.get(root_idx).map(|m| m.len()).unwrap_or(0);
    let mut members: Vec<ModuleId> = Vec::new();
    if members_len == 0 {
      members
        .try_reserve_exact(1)
        .map_err(|_| VmError::OutOfMemory)?;
      members.push(scc_root);
    } else {
      members
        .try_reserve_exact(members_len)
        .map_err(|_| VmError::OutOfMemory)?;
      members.extend_from_slice(&self.scc_members[root_idx]);
    }

    let scc_has_tla = members
      .iter()
      .copied()
      .any(|m| self.modules.get(module_index(m)).is_some_and(|r| r.has_tla));

    for (i, member) in members.iter().copied().enumerate() {
      if i % 32 == 0 && i != 0 {
        vm.tick()?;
      }
      let idx = module_index(member);
      if idx >= self.modules.len() {
        return Err(VmError::invalid_handle());
      }
      match self.modules[idx].status {
        ModuleStatus::Linked | ModuleStatus::Evaluating | ModuleStatus::EvaluatingAsync => {
          self.modules[idx].status = ModuleStatus::EvaluatingAsync;
        }
        ModuleStatus::Evaluated | ModuleStatus::Errored => {}
        _ => {}
      }
    }

    // Compute pending SCC dependencies.
    let mut pending_deps: usize = 0;
    let deps_len = self.scc_deps.get(root_idx).map(|d| d.len()).unwrap_or(0);
    for dep_i in 0..deps_len {
      if dep_i % 32 == 0 && dep_i != 0 {
        vm.tick()?;
      }
      let dep_root = self.scc_deps[root_idx][dep_i];

      // Ensure the dependency has an evaluation promise for caching, and start evaluation.
      let _ = self.ensure_scc_promise(vm, scope, host, hooks, dep_root)?;
      self.start_scc_evaluation(vm, scope, global_object, realm_id, dep_root, host, hooks)?;

      let dep_status = self.modules[module_index(dep_root)].status;
      match dep_status {
        ModuleStatus::Evaluated => {}
        ModuleStatus::Errored => {
          let reason = self.module_errored_value(vm, scope, module_index(dep_root))?;
          self.fail_scc(vm, scope, scc_root, host, hooks, reason, None)?;
          return Ok(());
        }
        _ => pending_deps = pending_deps.saturating_add(1),
      }
    }

    self.scc_eval_states[root_idx] = Some(SccEvaluationState {
      members,
      pending_deps,
      next_member_index: 0,
      waiting_on: None,
      global_object,
      realm_id,
    });
    self.torn_down = false;

    // Assign a deterministic async evaluation order for this SCC if it is part of async module
    // evaluation (has TLA or depends on an async SCC).
    //
    // This mirrors the spec's `IncrementModuleAsyncEvaluationCount` / `[[AsyncEvaluationOrder]]`
    // assignment: we call it only after recursing into requested modules (via `start_scc_evaluation`
    // on dependencies), which yields a post-order traversal consistent with `InnerModuleEvaluation`
    // discovery order (requested-modules left-to-right).
    if pending_deps > 0 || scc_has_tla {
      let slot = self
        .async_eval_states
        .get_mut(root_idx)
        .ok_or_else(|| VmError::invalid_handle())?;
      if slot.async_evaluation_order == AsyncEvaluationOrder::Unset {
        let order = vm.increment_module_async_evaluation_count();
        slot.async_evaluation_order = AsyncEvaluationOrder::Order(order);
      }
    }

    if pending_deps == 0 {
      self.execute_scc(vm, scope, scc_root, host, hooks)?;
    }

    Ok(())
  }

  fn execute_scc(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    scc_root: ModuleId,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
  ) -> Result<(), VmError> {
    let root_idx = module_index(scc_root);
    let Some(mut state) = self
      .scc_eval_states
      .get_mut(root_idx)
      .and_then(|s| s.take())
    else {
      return Ok(());
    };

    if state.pending_deps != 0 || state.waiting_on.is_some() {
      self.scc_eval_states[root_idx] = Some(state);
      return Ok(());
    }

    // Execute remaining modules in this SCC sequentially until we either:
    // - suspend at a top-level await, or
    // - complete the SCC.
    while state.next_member_index < state.members.len() {
      // Ensure module-level work observes VM budgets even when module bodies are empty.
      vm.tick()?;

      let module = state.members[state.next_member_index];
      let idx = module_index(module);

      // Execute the module body.
      let (has_tla, env_root, source, ast, compiled) = {
        let record = self
          .modules
          .get(idx)
          .ok_or_else(|| VmError::invalid_handle())?;
        (
          record.has_tla,
          record.environment,
          record.source.clone(),
          record.ast.clone(),
          record.compiled.clone(),
        )
      };

      let env_root = env_root.ok_or(VmError::InvariantViolation("module environment missing"))?;
      let module_env = scope
        .heap()
        .get_env_root(env_root)
        .ok_or_else(|| VmError::invalid_handle())?;
      let source = source
        .or_else(|| compiled.as_ref().map(|c| c.source.clone()))
        .ok_or(VmError::Unimplemented("module source missing"))?;

      if has_tla {
        let mut tla_fallback_ast: Option<Arc<Node<TopLevel>>> = None;
        let mut tla_fallback_ast_memory: Option<ExternalMemoryToken> = None;

        // Top-level await is not supported by the compiled executor. Even if the module has a
        // pre-compiled HIR body, fall back to the async AST evaluator.
        //
        // If the module record does not retain an AST (e.g. compiled modules that discard parse
        // trees after linking), parse it on demand and retain it across async suspension. The async
        // evaluator stores raw pointers into the AST statement list (`AsyncFrame::StmtList`), so the
        // backing `Arc<Node<TopLevel>>` must remain alive until the continuation completes or is
        // aborted.
        let ast = match ast {
          Some(ast) => ast,
          None => {
            let (ast, token) = parse_module_ast_for_tla_fallback(vm, scope.heap_mut(), &source)?;
            tla_fallback_ast = Some(ast.clone());
            tla_fallback_ast_memory = Some(token);
            ast
          }
        };

        match start_module_tla_evaluation(
          vm,
          scope,
          host,
          hooks,
          state.global_object,
          state.realm_id,
          module,
          module_env,
          source,
          &ast.stx.body,
        ) {
          Ok(ModuleTlaStepResult::Completed) => {
            state.next_member_index = state.next_member_index.saturating_add(1);
            continue;
          }
          Ok(ModuleTlaStepResult::Await {
            promise: awaited_promise,
            continuation_id,
          }) => {
            let mut async_continuation_ids = Vec::<u32>::new();
            async_continuation_ids
              .try_reserve(1)
              .map_err(|_| VmError::OutOfMemory)?;
            async_continuation_ids.push(continuation_id);

            self.tla_states[idx] = Some(TlaEvaluationState {
              continuation_id,
              global_object: state.global_object,
              realm_id: state.realm_id,
              ast: None,
              async_continuation_ids,
              tla_fallback_ast,
              tla_fallback_ast_memory,
            });
            self.torn_down = false;

            // Schedule the first resume step.
            if let Err(err) =
              self.schedule_tla_resume(vm, scope, host, hooks, module, awaited_promise)
            {
              // Scheduling failed: reject the SCC promise.
              self.tla_states[idx]
                .take()
                .map(|s| s.teardown(vm, scope.heap_mut()));

              let reason = if let Some(thrown) = err.thrown_value() {
                thrown
              } else {
                self.cache_module_error_from_err(vm, scope, idx, &err)?;
                self.module_errored_value(vm, scope, idx)?
              };
              self.scc_eval_states[root_idx] = None;
              self.fail_scc(vm, scope, scc_root, host, hooks, reason, Some(&err))?;
              return Ok(());
            }

            // Keep the module graph pointer installed for Promise job callbacks.
            self.retain_module_graph_ptr(vm, vm.module_graph_ptr());

            state.waiting_on = Some(module);
            self.scc_eval_states[root_idx] = Some(state);
            return Ok(());
          }
          Err(err) => {
            let reason = if let Some(thrown) = err.thrown_value() {
              thrown
            } else {
              self.cache_module_error_from_err(vm, scope, idx, &err)?;
              self.module_errored_value(vm, scope, idx)?
            };
            self.scc_eval_states[root_idx] = None;
            self.fail_scc(vm, scope, scc_root, host, hooks, reason, Some(&err))?;
            return Ok(());
          }
        }
      } else {
        // Non-TLA modules can execute via either:
        // - the compiled executor (HIR), if present and safe, or
        // - the AST interpreter (fallback).
        if let Some(compiled) = compiled.clone() {
          if !compiled.contains_async_generators && !compiled.requires_ast_fallback {
            match crate::hir_exec::run_compiled_module(
              vm,
              scope,
              host,
              hooks,
              state.global_object,
              state.realm_id,
              module,
              module_env,
              compiled,
            ) {
              Ok(()) => {
                state.next_member_index = state.next_member_index.saturating_add(1);
                continue;
              }
              Err(err) => {
                let reason = if let Some(thrown) = err.thrown_value() {
                  thrown
                } else {
                  self.cache_module_error_from_err(vm, scope, idx, &err)?;
                  self.module_errored_value(vm, scope, idx)?
                };
                self.scc_eval_states[root_idx] = None;
                self.fail_scc(vm, scope, scc_root, host, hooks, reason, Some(&err))?;
                return Ok(());
              }
            }
          }
        }

        let ast = match ast {
          Some(ast) => ast,
          None => {
            // Interpreter fallback: ensure a parsed module AST exists, charging it against heap
            // limits when installing it into the module record.
            self.ensure_module_ast(vm, scope.heap_mut(), module)?;
            self.modules[idx]
              .ast
              .clone()
              .ok_or(VmError::Unimplemented("module AST missing"))?
          }
        };
        match run_module(
          vm,
          scope,
          host,
          hooks,
          state.global_object,
          state.realm_id,
          module,
          module_env,
          source,
          &ast.stx.body,
        ) {
          Ok(()) => {
            state.next_member_index = state.next_member_index.saturating_add(1);
            continue;
          }
          Err(err) => {
            let reason = if let Some(thrown) = err.thrown_value() {
              thrown
            } else {
              self.cache_module_error_from_err(vm, scope, idx, &err)?;
              self.module_errored_value(vm, scope, idx)?
            };
            self.scc_eval_states[root_idx] = None;
            self.fail_scc(vm, scope, scc_root, host, hooks, reason, Some(&err))?;
            return Ok(());
          }
        }
      }
    }

    // SCC completed successfully: mark all members as evaluated and resolve the evaluation promise.
    let members = state.members;
    self.scc_eval_states[root_idx] = None;
    self.complete_scc(vm, scope, scc_root, host, hooks, &members)?;
    Ok(())
  }

  fn complete_scc(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    scc_root: ModuleId,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    members: &[ModuleId],
  ) -> Result<(), VmError> {
    let root_idx = module_index(scc_root);

    for (i, member) in members.iter().copied().enumerate() {
      if i % 32 == 0 && i != 0 {
        vm.tick()?;
      }
      let idx = module_index(member);
      if idx < self.modules.len() {
        self.modules[idx].status = ModuleStatus::Evaluated;
        if self.modules[idx].ast_external_memory.is_some() {
          self.modules[idx].clear_ast();
        }
      }
    }

    self.scc_eval_states[root_idx] = None;

    // Mirror the spec's transition to `~done~` for this SCC's async evaluation order.
    if let Some(state) = self.async_eval_states.get_mut(root_idx) {
      if matches!(state.async_evaluation_order, AsyncEvaluationOrder::Order(_)) {
        state.async_evaluation_order = AsyncEvaluationOrder::Done;
      }
    }

    self.resolve_scc_promise(vm, scope, host, hooks, scc_root)?;

    // Notify async parents that this SCC is now available.
    let parents_len = self.scc_parents.get(root_idx).map(|p| p.len()).unwrap_or(0);
    for parent_i in 0..parents_len {
      if parent_i % 32 == 0 && parent_i != 0 {
        vm.tick()?;
      }
      let parent_root = self.scc_parents[root_idx][parent_i];
      let parent_idx = module_index(parent_root);
      let Some(parent_state) = self
        .scc_eval_states
        .get_mut(parent_idx)
        .and_then(|s| s.as_mut())
      else {
        continue;
      };
      if parent_state.pending_deps == 0 {
        continue;
      }
      parent_state.pending_deps = parent_state.pending_deps.saturating_sub(1);
      if parent_state.pending_deps == 0 {
        self
          .ready_scc_queue
          .try_reserve(1)
          .map_err(|_| VmError::OutOfMemory)?;
        self.ready_scc_queue.push(parent_root);
      }
    }
    self.process_ready_scc_queue(vm, scope, host, hooks)?;

    Ok(())
  }

  /// Drain `ready_scc_queue` in spec order.
  ///
  /// Spec note: this implements the ordering constraints of `AsyncModuleExecutionFulfilled`'s
  /// `sortedExecList` using the SCC-level execution engine. We must ensure that when multiple SCCs
  /// become ready at once (or when an SCC completes synchronously and makes more SCCs ready) we
  /// always start execution in ascending `[[AsyncEvaluationOrder]]`, not module insertion order.
  fn process_ready_scc_queue(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
  ) -> Result<(), VmError> {
    if self.processing_ready_scc_queue {
      return Ok(());
    }
    self.processing_ready_scc_queue = true;

    let result = (|| {
      while !self.ready_scc_queue.is_empty() {
        // Avoid quadratic behavior on large graphs: sort once per "batch" of ready SCCs. Any SCCs
        // that become ready while executing will be appended and picked up by the next loop
        // iteration.
        let orders = &self.async_eval_states;
        self.ready_scc_queue.sort_unstable_by(|a, b| {
          let a_order = orders
            .get(module_index(*a))
            .and_then(|s| s.async_evaluation_order.as_integer())
            .unwrap_or(u64::MAX);
          let b_order = orders
            .get(module_index(*b))
            .and_then(|s| s.async_evaluation_order.as_integer())
            .unwrap_or(u64::MAX);
          a_order.cmp(&b_order).then_with(|| a.to_raw().cmp(&b.to_raw()))
        });
        let next = self.ready_scc_queue.remove(0);
        self.execute_scc(vm, scope, next, host, hooks)?;
      }
      Ok(())
    })();

    self.processing_ready_scc_queue = false;
    result
  }

  fn fail_scc(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    scc_root: ModuleId,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    reason: Value,
    err: Option<&VmError>,
  ) -> Result<(), VmError> {
    // Root the rejection reason across error caching + promise rejection: both can allocate and
    // trigger GC.
    scope.push_root(reason)?;

    let root_idx = module_index(scc_root);

    // Mark members as errored and cache a deterministic error value.
    let members_len = self.scc_members.get(root_idx).map(|m| m.len()).unwrap_or(0);
    if members_len == 0 {
      // Fall back to the root module itself if SCC members are unexpectedly missing.
      let idx = module_index(scc_root);
      if idx < self.modules.len() {
        self.modules[idx].status = ModuleStatus::Errored;
        let _ = self.cache_module_error_value(scope, idx, reason);
        if self.modules[idx].ast_external_memory.is_some() {
          self.modules[idx].clear_ast();
        }
      }
    }
    for member_i in 0..members_len {
      if member_i % 32 == 0 && member_i != 0 {
        vm.tick()?;
      }
      let member = self.scc_members[root_idx][member_i];
      let idx = module_index(member);
      if idx >= self.modules.len() {
        continue;
      }
      self.modules[idx].status = ModuleStatus::Errored;
      // Best-effort: cache the thrown value for deterministic future operations.
      let _ = self.cache_module_error_value(scope, idx, reason);
      // If this module retained a charged AST for a compiled-module fallback, drop it now that the
      // module is irrecoverably errored.
      if self.modules[idx].ast_external_memory.is_some() {
        self.modules[idx].clear_ast();
      }
    }

    self.scc_eval_states[root_idx] = None;

    self.reject_scc_promise(vm, scope, host, hooks, scc_root, reason, err)?;

    // Propagate the rejection to async parents.
    let parents_len = self.scc_parents.get(root_idx).map(|p| p.len()).unwrap_or(0);
    for parent_i in 0..parents_len {
      let parent_root = self.scc_parents[root_idx][parent_i];
      let parent_idx = module_index(parent_root);
      if parent_idx >= self.modules.len() {
        continue;
      }
      if self.modules[parent_idx].status == ModuleStatus::Errored {
        continue;
      }
      // Parents reject with the same reason.
      let _ = self.fail_scc(vm, scope, parent_root, host, hooks, reason, None);
    }

    Ok(())
  }

  /// Evaluates a module synchronously and returns its completion as a direct `Result`.
  ///
  /// This is a host convenience API for embeddings that:
  /// - do not need the spec-visible "evaluation promise", and
  /// - currently do not support top-level await (TLA).
  ///
  /// If the module (or one of its dependencies) throws, the returned [`VmError`] preserves the
  /// captured stack trace (`VmError::ThrowWithStack`), unlike [`ModuleGraph::evaluate_with_scope`],
  /// which settles a Promise with only the thrown value (per ECMA-262).
  pub fn evaluate_sync_with_scope(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    global_object: GcObject,
    realm_id: RealmId,
    module: ModuleId,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
  ) -> Result<(), VmError> {
    // Ensure dynamic `import()` expressions executed during module evaluation can resolve the active
    // module graph even when the embedding uses the low-level `ModuleGraph::{link,evaluate}` APIs
    // directly (without constructing a `JsRuntime`, which sets this pointer at runtime creation).
    let mut graph_guard = ModuleGraphPtrGuard::install(vm, self);

    let result = (|| -> Result<(), VmError> {
      self.link_with_scope(vm, scope, global_object, realm_id, module)?;

      // `evaluate_sync_with_scope` is intended for embeddings that do not support async module
      // evaluation / top-level await. Reject any graph that contains TLA *before* executing any
      // module bodies so callers don't observe partial execution.
      self.ensure_no_tla_in_resolved_graph(vm, module)?;

      self.eval_inner(vm, scope, global_object, realm_id, module, host, hooks)
    })();

    // If module evaluation initiated an async continuation whose Promise reactions are still
    // pending (dynamic import and/or top-level await), keep `vm.module_graph_ptr` installed until
    // those reactions run.
    //
    // Note: `insert_pending_dynamic_import_evaluation` captures the VM's current module graph
    // pointer as the "previous" value to restore. When dynamic import begins during this
    // synchronous evaluation, that value will be `self`. Overwrite it with the true outer previous
    // pointer before disarming the guard so completion restores correctly.
    if graph_guard.restore_on_drop && self.module_graph_ptr_refcount > 0 {
      let self_ptr: *mut ModuleGraph = self;
      if self.module_graph_ptr_prev == Some(self_ptr) && graph_guard.prev_graph() != Some(self_ptr) {
        self.module_graph_ptr_prev = graph_guard.prev_graph();
      }
      graph_guard.disarm();
    }

    // Ensure host-visible failures never leak internal helper errors (TypeError, NotCallable, etc.)
    // when intrinsics are available.
    match result {
      Err(err) if err.is_throw_completion() => Err(crate::vm::coerce_error_to_throw_with_stack(
        &*vm,
        scope,
        err,
      )),
      other => other,
    }
  }

  fn ensure_no_tla_in_resolved_graph(&self, vm: &mut Vm, module: ModuleId) -> Result<(), VmError> {
    let module_count = self.modules.len();
    if module_count == 0 {
      return Ok(());
    }

    let mut stack: Vec<ModuleId> = Vec::new();
    stack
      .try_reserve_exact(module_count)
      .map_err(|_| VmError::OutOfMemory)?;
    stack.push(module);

    let mut visited: Vec<bool> = Vec::new();
    visited
      .try_reserve_exact(module_count)
      .map_err(|_| VmError::OutOfMemory)?;
    visited.resize(module_count, false);

    const MODULE_TICK_EVERY: usize = 32;
    const EDGE_TICK_EVERY: usize = 32;
    let mut visited_modules: usize = 0;

    while let Some(current) = stack.pop() {
      let idx = module_index(current);
      if idx >= module_count {
        return Err(VmError::invalid_handle());
      }
      if visited[idx] {
        continue;
      }
      visited[idx] = true;

      visited_modules = visited_modules.wrapping_add(1);
      if visited_modules % MODULE_TICK_EVERY == 0 || visited_modules == 1 {
        vm.tick()?;
      }

      let record = &self.modules[idx];
      if record.has_tla {
        return Err(VmError::Unimplemented("top-level await"));
      }

      for (i, loaded) in record.loaded_modules.iter().enumerate() {
        if i % EDGE_TICK_EVERY == 0 && i != 0 {
          vm.tick()?;
        }
        let imported = loaded.module;
        let imported_idx = module_index(imported);
        if imported_idx >= module_count {
          return Err(VmError::invalid_handle());
        }
        if !visited[imported_idx] {
          stack.push(imported);
        }
      }
    }

    Ok(())
  }

  /// Convenience wrapper around [`ModuleGraph::evaluate_sync_with_scope`] that creates a new scope.
  pub fn evaluate_sync(
    &mut self,
    vm: &mut Vm,
    heap: &mut Heap,
    global_object: GcObject,
    realm_id: RealmId,
    module: ModuleId,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
  ) -> Result<(), VmError> {
    let mut scope = heap.scope();
    self.evaluate_sync_with_scope(vm, &mut scope, global_object, realm_id, module, host, hooks)
  }

  pub fn evaluate(
    &mut self,
    vm: &mut Vm,
    heap: &mut Heap,
    global_object: GcObject,
    realm_id: RealmId,
    module: ModuleId,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
  ) -> Result<Value, VmError> {
    let mut scope = heap.scope();
    self.evaluate_with_scope(vm, &mut scope, global_object, realm_id, module, host, hooks)
  }

  fn eval_inner(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    global_object: GcObject,
    realm_id: RealmId,
    module: ModuleId,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
  ) -> Result<(), VmError> {
    let idx = module_index(module);
    let status = self.modules[idx].status;
    match status {
      ModuleStatus::Evaluated => return Ok(()),
      ModuleStatus::Evaluating | ModuleStatus::EvaluatingAsync => return Ok(()),
      ModuleStatus::Linked => {}
      ModuleStatus::Errored => {
        let value = self.module_errored_value(vm, scope, idx)?;
        return Err(VmError::Throw(value));
      }
      _ => return Err(VmError::Unimplemented("module is not linked")),
    }

    // Ensure module evaluation observes budgets even when the module body is empty (no statement
    // ticks).
    vm.tick()?;

    self.modules[idx].status = ModuleStatus::Evaluating;

    let eval_result = (|| -> Result<(), VmError> {
      const EVAL_TICK_EVERY: usize = 32;
      // Compute dependency list first to avoid cloning attacker-controlled module requests (which
      // can abort on allocator OOM) and to avoid borrow conflicts while recursively evaluating.
      let requested_len = self.modules[idx].requested_modules.len();
      let mut deps: Vec<ModuleId> = Vec::new();
      deps
        .try_reserve_exact(requested_len)
        .map_err(|_| VmError::OutOfMemory)?;
      for i in 0..requested_len {
        if i % EVAL_TICK_EVERY == 0 && i != 0 {
          vm.tick()?;
        }
        let imported = {
          let request = &self.modules[idx].requested_modules[i];
          self
            .get_imported_module(module, request)
            .ok_or(VmError::Unimplemented("unlinked module request"))?
        };
        deps.push(imported);
      }
      for imported in deps {
        self.eval_inner(vm, scope, global_object, realm_id, imported, host, hooks)?;
      }

      let env_root = self.modules[idx]
        .environment
        .ok_or(VmError::InvariantViolation("module environment missing"))?;
      let module_env = scope
        .heap()
        .get_env_root(env_root)
        .ok_or_else(|| VmError::invalid_handle())?;

      let compiled = self.modules[idx].compiled.clone();
      let has_tla = self.modules[idx].has_tla;
      match compiled {
        Some(script)
          if !script.requires_ast_fallback && !script.contains_async_generators && !has_tla =>
        {
          crate::hir_exec::run_compiled_module(
            vm,
            scope,
            host,
            hooks,
            global_object,
            realm_id,
            module,
            module_env,
            script,
          )?;
        }
        _ => {
          // Ensure we have an AST for interpreter execution.
          self.ensure_module_ast(vm, scope.heap_mut(), module)?;

          let source = self.modules[idx]
            .source
            .clone()
            .or_else(|| self.modules[idx].compiled.as_ref().map(|c| c.source.clone()))
            .ok_or(VmError::Unimplemented("module source missing"))?;
          let ast = self.modules[idx]
            .ast
            .clone()
            .ok_or(VmError::Unimplemented("module AST missing"))?;
          run_module(
            vm,
            scope,
            host,
            hooks,
            global_object,
            realm_id,
            module,
            module_env,
            source,
            &ast.stx.body,
          )?;
        }
      }
      Ok(())
    })();
    match eval_result {
      Ok(()) => {
        self.modules[idx].status = ModuleStatus::Evaluated;
        if self.modules[idx].ast_external_memory.is_some() {
          self.modules[idx].clear_ast();
        }
        Ok(())
      }
      Err(err) => {
        self.modules[idx].status = ModuleStatus::Errored;
        self.cache_module_error_from_err(vm, scope, idx, &err)?;
        if self.modules[idx].ast_external_memory.is_some() {
          self.modules[idx].clear_ast();
        }
        Err(err)
      }
    }
  }

  fn eval_tla_start(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    global_object: GcObject,
    realm_id: RealmId,
    module: ModuleId,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
  ) -> Result<ModuleTlaStepResult, VmError> {
    let idx = module_index(module);
    let status = self.modules[idx].status;
    match status {
      ModuleStatus::Evaluated => return Ok(ModuleTlaStepResult::Completed),
      ModuleStatus::Evaluating | ModuleStatus::EvaluatingAsync => {
        return Ok(ModuleTlaStepResult::Completed)
      }
      ModuleStatus::Linked => {}
      ModuleStatus::Errored => {
        let value = self.module_errored_value(vm, scope, idx)?;
        return Err(VmError::Throw(value));
      }
      _ => return Err(VmError::Unimplemented("module is not linked")),
    }

    // Ensure module evaluation observes budgets even when the module body is empty.
    vm.tick()?;

    self.modules[idx].status = ModuleStatus::EvaluatingAsync;

    let eval_result = (|| -> Result<ModuleTlaStepResult, VmError> {
      // Evaluate dependencies synchronously (top-level await in dependencies remains unsupported).
      const EVAL_TICK_EVERY: usize = 32;
      // Compute dependency list first to avoid cloning attacker-controlled module requests (which
      // can abort on allocator OOM) and to avoid borrow conflicts while recursively evaluating.
      let requested_len = self.modules[idx].requested_modules.len();
      let mut deps: Vec<ModuleId> = Vec::new();
      deps
        .try_reserve_exact(requested_len)
        .map_err(|_| VmError::OutOfMemory)?;
      for i in 0..requested_len {
        if i % EVAL_TICK_EVERY == 0 && i != 0 {
          vm.tick()?;
        }
        let imported = {
          let request = &self.modules[idx].requested_modules[i];
          self
            .get_imported_module(module, request)
            .ok_or(VmError::Unimplemented("unlinked module request"))?
        };
        deps.push(imported);
      }
      for imported in deps {
        self.eval_inner(vm, scope, global_object, realm_id, imported, host, hooks)?;
      }

      let env_root = self.modules[idx]
        .environment
        .ok_or(VmError::InvariantViolation("module environment missing"))?;
      let module_env = scope
        .heap()
        .get_env_root(env_root)
        .ok_or_else(|| VmError::invalid_handle())?;

      // Ensure we have an AST for interpreter-based async module execution.
      self.ensure_module_ast(vm, scope.heap_mut(), module)?;

      let source = self.modules[idx]
        .source
        .clone()
        .ok_or(VmError::Unimplemented("module source missing"))?;
      let ast = self.modules[idx]
        .ast
        .clone()
        .ok_or(VmError::Unimplemented("module AST missing"))?;

      let step = start_module_tla_evaluation(
        vm,
        scope,
        host,
        hooks,
        global_object,
        realm_id,
        module,
        module_env,
        source,
        &ast.stx.body,
      )?;

      match step {
        ModuleTlaStepResult::Completed => {
          self.modules[idx].status = ModuleStatus::Evaluated;
          Ok(ModuleTlaStepResult::Completed)
        }
        ModuleTlaStepResult::Await { .. } => Ok(step),
      }
    })();

    match eval_result {
      Ok(step) => {
        if matches!(step, ModuleTlaStepResult::Completed) {
          if self.modules[idx].ast_external_memory.is_some() {
            self.modules[idx].clear_ast();
          }
        }
        Ok(step)
      }
      Err(err) => {
        self.modules[idx].status = ModuleStatus::Errored;
        self.cache_module_error_from_err(vm, scope, idx, &err)?;
        if self.modules[idx].ast_external_memory.is_some() {
          self.modules[idx].clear_ast();
        }
        Err(err)
      }
    }
  }

  fn schedule_tla_resume(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    module: ModuleId,
    awaited_promise: Value,
  ) -> Result<(), VmError> {
    let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
      "top-level await requires intrinsics (create a Realm first)",
    ))?;

    let idx = module_index(module);
    let state_opt = self.tla_states.get(idx).and_then(|s| s.as_ref());
    let function_realm = state_opt.map(|s| s.global_object);
    let job_realm = vm.current_realm();

    let on_fulfilled_call = vm.module_tla_on_fulfilled_call_id()?;
    let on_rejected_call = vm.module_tla_on_rejected_call_id()?;

    let on_fulfilled_name = scope.alloc_string("moduleTlaOnFulfilled")?;
    scope.push_root(Value::String(on_fulfilled_name))?;
    let on_rejected_name = scope.alloc_string("moduleTlaOnRejected")?;
    scope.push_root(Value::String(on_rejected_name))?;

    let module_slot = Value::Number(module.to_raw() as f64);
    let slots = [module_slot];

    let on_fulfilled = scope.alloc_native_function_with_slots(
      on_fulfilled_call,
      None,
      on_fulfilled_name,
      1,
      &slots,
    )?;
    scope
      .heap_mut()
      .object_set_prototype(on_fulfilled, Some(intr.function_prototype()))?;
    if let Some(global_object) = function_realm {
      scope
        .heap_mut()
        .set_function_realm(on_fulfilled, global_object)?;
    }
    if let Some(realm) = job_realm {
      scope
        .heap_mut()
        .set_function_job_realm(on_fulfilled, realm)?;
    }
    scope.push_root(Value::Object(on_fulfilled))?;

    let on_rejected = scope.alloc_native_function_with_slots(
      on_rejected_call,
      None,
      on_rejected_name,
      1,
      &slots,
    )?;
    scope
      .heap_mut()
      .object_set_prototype(on_rejected, Some(intr.function_prototype()))?;
    if let Some(global_object) = function_realm {
      scope
        .heap_mut()
        .set_function_realm(on_rejected, global_object)?;
    }
    if let Some(realm) = job_realm {
      scope
        .heap_mut()
        .set_function_job_realm(on_rejected, realm)?;
    }
    scope.push_roots(&[Value::Object(on_rejected), awaited_promise])?;
    crate::promise_ops::perform_promise_then_with_result_capability_with_host_and_hooks(
      vm,
      scope,
      host,
      hooks,
      awaited_promise,
      Value::Object(on_fulfilled),
      Value::Object(on_rejected),
      None,
    )?;
    Ok(())
  }
}

impl Drop for ModuleGraph {
  fn drop(&mut self) {
    // Avoid panicking from a destructor while unwinding (that would abort).
    if std::thread::panicking() {
      return;
    }
    debug_assert!(
      self.torn_down,
      "ModuleGraph dropped with leaked persistent roots; call teardown() if the Heap is reused"
    );
  }
}

#[derive(Debug)]
struct TlaEvaluationState {
  continuation_id: u32,
  global_object: GcObject,
  realm_id: RealmId,
  /// Parsed AST kept alive for the duration of async module evaluation.
  ///
  /// The async evaluator stores raw pointers into the AST statement list (`AsyncFrame::StmtList`),
  /// so when a compiled module falls back to AST evaluation for top-level await we must retain the
  /// backing `Arc<Node<TopLevel>>` across async suspension until the continuation completes or is
  /// aborted.
  #[allow(dead_code)]
  ast: Option<Arc<Node<TopLevel>>>,
  /// Async continuation ids created solely for this module's top-level await evaluation.
  ///
  /// When an embedding aborts async module evaluation, these continuations must be torn down so
  /// their persistent roots do not leak.
  async_continuation_ids: Vec<u32>,
  /// Parsed AST retained for compiled-module top-level await fallback.
  ///
  /// Async continuation frames store raw pointers into the `parse-js` AST. Source-text modules
  /// store their AST in `SourceTextModuleRecord`; compiled modules parse on demand and keep the
  /// resulting AST alive here until evaluation completes or is aborted.
  #[allow(dead_code)]
  tla_fallback_ast: Option<Arc<Node<TopLevel>>>,
  /// External heap memory token for [`TlaEvaluationState::tla_fallback_ast`].
  ///
  /// The `parse-js` AST lives outside the GC heap; holding this token ensures it is accounted for
  /// against `HeapLimits` and released when the AST is dropped.
  #[allow(dead_code)]
  tla_fallback_ast_memory: Option<ExternalMemoryToken>,
}

impl TlaEvaluationState {
  fn teardown(mut self, vm: &mut Vm, heap: &mut Heap) {
    for id in self.async_continuation_ids.drain(..) {
      vm.abort_async_continuation(heap, id);
    }
  }
}

impl Drop for TlaEvaluationState {
  fn drop(&mut self) {
    // Avoid panicking from a destructor while unwinding (that would abort).
    if std::thread::panicking() {
      return;
    }
    debug_assert!(
      self.async_continuation_ids.is_empty(),
      "TlaEvaluationState dropped with leaked persistent roots; ensure the module evaluation promise is settled or aborted"
    );
  }
}

fn parse_module_ast_for_tla_fallback(
  vm: &mut Vm,
  heap: &mut Heap,
  source: &Arc<SourceText>,
) -> Result<(Arc<Node<TopLevel>>, ExternalMemoryToken), VmError> {
  // `parse-js` AST nodes can be significantly larger than the original source (each token and
  // syntactic construct becomes one-or-more Rust structs). Use a conservative multiplier so hostile
  // input can't bypass heap limits by forcing large cached ASTs.
  //
  // This mirrors the heuristic used for `Vm::ecma_function_ast` (function snippet AST caching).
  let estimated_ast_bytes = source.text.len().saturating_mul(4);
  let token = heap.charge_external(estimated_ast_bytes)?;

  let opts = ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Module,
  };
  let top = vm.parse_top_level_with_budget(&source.text, opts)?;

  {
    let mut tick = || vm.tick();
    crate::early_errors::validate_top_level(
      &top.stx.body,
      crate::early_errors::EarlyErrorOptions::module(),
      Some(source.text.as_ref()),
      &mut tick,
    )?;
  }
  Ok((arc_try_new_vm(top)?, token))
}

/// Per-SCC (cycle-root) module evaluation progress for async module evaluation graphs.
///
/// This is an engine-internal state machine that drives module execution across a dependency graph,
/// including:
/// - waiting for async dependencies (top-level await in imported modules),
/// - executing the modules within an SCC sequentially, and
/// - resuming execution after awaited promises settle.
#[derive(Debug)]
struct SccEvaluationState {
  members: Vec<ModuleId>,
  pending_deps: usize,
  next_member_index: usize,
  waiting_on: Option<ModuleId>,
  global_object: GcObject,
  realm_id: RealmId,
}

fn module_id_from_native_slot(scope: &Scope<'_>, callee: GcObject) -> Result<ModuleId, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let raw = match slots.get(0).copied().unwrap_or(Value::Undefined) {
    Value::Number(n) if n.is_finite() && n >= 0.0 && n <= u64::MAX as f64 => n as u64,
    _ => {
      return Err(VmError::InvariantViolation(
        "module TLA callback missing module id slot",
      ))
    }
  };
  Ok(ModuleId::from_raw(raw))
}

pub(crate) fn module_tla_on_fulfilled(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let module = module_id_from_native_slot(scope, callee)?;
  let value = args.get(0).copied().unwrap_or(Value::Undefined);
  module_tla_resume_inner(vm, scope, host, hooks, module, Ok(value))?;
  Ok(Value::Undefined)
}

pub(crate) fn module_tla_on_rejected(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let module = module_id_from_native_slot(scope, callee)?;
  let reason = args.get(0).copied().unwrap_or(Value::Undefined);
  module_tla_resume_inner(vm, scope, host, hooks, module, Err(reason))?;
  Ok(Value::Undefined)
}

fn module_tla_resume_inner(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  module: ModuleId,
  resume_value: Result<Value, Value>,
) -> Result<(), VmError> {
  let Some(ptr) = vm.module_graph_ptr() else {
    // If the embedding cleared the module graph pointer, treat this as a no-op.
    return Ok(());
  };
  let graph = unsafe { &mut *ptr };
  let idx = module_index(module);

  let Some(mut state) = graph.tla_states.get_mut(idx).and_then(|s| s.take()) else {
    // State was already cleaned up (module finished or was aborted).
    return Ok(());
  };

  let scc_root = graph
    .modules
    .get(idx)
    .and_then(|m| m.cycle_root)
    .unwrap_or(module);
  let scc_root_idx = module_index(scc_root);

  // `vm.tick` is a potential termination point. Ensure we free persistent roots even if it returns
  // an error (termination, OOM, etc).
  if let Err(err) = vm.tick() {
    if idx < graph.modules.len() {
      graph.modules[idx].status = ModuleStatus::Errored;
      let _ = graph.cache_module_error_from_err(vm, scope, idx, &err);
      if graph.modules[idx].ast_external_memory.is_some() {
        graph.modules[idx].clear_ast();
      }
    }
    state.teardown(vm, scope.heap_mut());
    if graph.module_graph_ptr_refcount > 0 {
      graph.release_module_graph_ptr(vm);
    }
    return Err(err);
  }

  let realm_id = state.realm_id;
  let continuation_id = state.continuation_id;

  let result = resume_module_tla_evaluation(
    vm,
    scope,
    host,
    hooks,
    realm_id,
    module,
    continuation_id,
    resume_value,
  );

  match result {
    Ok(ModuleTlaStepResult::Completed) => {
      // Mark the SCC evaluation state as ready to continue.
      if scc_root_idx < graph.scc_eval_states.len() {
        if let Some(scc_state) = graph.scc_eval_states[scc_root_idx].as_mut() {
          scc_state.waiting_on = None;
          scc_state.next_member_index = scc_state.next_member_index.saturating_add(1);
        }
      }

      let exec_res = graph.execute_scc(vm, scope, scc_root, host, hooks);
      state.teardown(vm, scope.heap_mut());
      if graph.module_graph_ptr_refcount > 0 {
        graph.release_module_graph_ptr(vm);
      }
      exec_res
    }
    Ok(ModuleTlaStepResult::Await {
      promise: awaited_promise,
      continuation_id,
    }) => {
      // Track the updated continuation id (should remain stable, but preserve the value returned by
      // the evaluator).
      state.continuation_id = continuation_id;
      if !state.async_continuation_ids.contains(&continuation_id) {
        state
          .async_continuation_ids
          .try_reserve(1)
          .map_err(|_| VmError::OutOfMemory)?;
        state.async_continuation_ids.push(continuation_id);
      }
      graph.tla_states[idx] = Some(state);

      // Ensure the SCC knows which module we are waiting on.
      if scc_root_idx < graph.scc_eval_states.len() {
        if let Some(scc_state) = graph.scc_eval_states[scc_root_idx].as_mut() {
          scc_state.waiting_on = Some(module);
        }
      }

      if let Err(err) = graph.schedule_tla_resume(vm, scope, host, hooks, module, awaited_promise) {
        // Scheduling failed: treat this as a module evaluation failure and reject the SCC's
        // evaluation promise.
        let Some(state) = graph.tla_states.get_mut(idx).and_then(|s| s.take()) else {
          return Err(VmError::InvariantViolation(
            "missing async module evaluation state after scheduling failure",
          ));
        };

        if idx < graph.modules.len() {
          graph.modules[idx].status = ModuleStatus::Errored;
        }

        let reason = if let Some(thrown) = err.thrown_value() {
          thrown
        } else {
          graph.cache_module_error_from_err(vm, scope, idx, &err)?;
          graph.module_errored_value(vm, scope, idx)?
        };

        let fail_res = graph.fail_scc(vm, scope, scc_root, host, hooks, reason, Some(&err));
        state.teardown(vm, scope.heap_mut());
        if graph.module_graph_ptr_refcount > 0 {
          graph.release_module_graph_ptr(vm);
        }

        fail_res?;
        if matches!(err, VmError::Termination(_)) {
          return Err(err);
        }
      }

      Ok(())
    }
    Err(err) => {
      let reason = if let Some(thrown) = err.thrown_value() {
        thrown
      } else {
        graph.cache_module_error_from_err(vm, scope, idx, &err)?;
        graph.module_errored_value(vm, scope, idx)?
      };

      let fail_res = graph.fail_scc(vm, scope, scc_root, host, hooks, reason, Some(&err));
      state.teardown(vm, scope.heap_mut());
      if graph.module_graph_ptr_refcount > 0 {
        graph.release_module_graph_ptr(vm);
      }

      if matches!(err, VmError::Termination(_)) {
        return Err(err);
      }
      // Do not propagate the original error: module evaluation failures are represented by rejecting
      // the module evaluation promise (per ECMA-262).
      fail_res?;
      Ok(())
    }
  }
}

fn module_index(id: ModuleId) -> usize {
  // `ModuleId` is an opaque token at the VM boundary, but `ModuleGraph` uses it as a stable index
  // into its module vector for tests. Tests construct module ids exclusively via
  // `ModuleGraph::add_module*`, which uses the raw index representation.
  id.to_raw() as usize
}

fn module_request_from_specifier(specifier: &str) -> Result<ModuleRequest, VmError> {
  // Build an owned module specifier string using fallible allocation. ECMAScript module specifiers
  // are UTF-16 code units; Rust `str` inputs cannot contain unpaired surrogates, so this path is
  // inherently lossy for those values (it is intended for host-supplied test helpers).
  let specifier_owned = crate::JsString::from_str(specifier)?;
  Ok(ModuleRequest::new(specifier_owned, Vec::new()))
}

/// Accessor getter for Module Namespace export properties.
///
/// Module namespace objects expose each export as a non-configurable, enumerable accessor property
/// with this native function as the getter (`ModuleNamespaceExoticObject.[[GetOwnProperty]]`,
/// ECMA-262 §9.4.6).
///
/// The function captures:
/// - native slot 0: either a binding-name string (for live bindings) or a namespace object (for
///   namespace exports),
/// - native slot 1: the exported name string (used for error messages),
/// - closure environment: the module environment for live bindings.
pub(crate) fn module_namespace_getter(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn crate::VmHost,
  _hooks: &mut dyn crate::VmHostHooks,
  callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  if slots.len() != 2 {
    return Err(VmError::InvariantViolation(
      "module namespace getter expected two native slots",
    ));
  }

  let export_name = match slots[1] {
    Value::String(s) => {
      let units = scope.heap().get_string(s)?.as_code_units();
      crate::string::utf16_to_utf8_lossy_with_tick(units, || vm.tick())?
    }
    _ => {
      return Err(VmError::InvariantViolation(
        "module namespace getter export name slot must be a string",
      ))
    }
  };

  match slots[0] {
    Value::Object(obj) => Ok(Value::Object(obj)),
    Value::String(binding_name) => {
      let Some(env) = scope.heap().get_function_closure_env(callee)? else {
        return Err(VmError::InvariantViolation(
          "module namespace binding getter missing closure env",
        ));
      };

      let (binding_value, initialized) = {
        let rec = scope.heap().get_env_record(env)?;
        let crate::env::EnvRecord::Declarative(rec) = rec else {
          return Err(VmError::Unimplemented("object env records in modules"));
        };

        let binding_name_units = scope.heap().get_string(binding_name)?.as_code_units();
        let mut found: Option<(crate::env::EnvBindingValue, bool)> = None;
        let mut scanned: usize = 0;
        for binding in rec.bindings.iter() {
          let Some(name) = binding.name else {
            continue;
          };
          scanned = scanned.wrapping_add(1);
          if scanned % 1024 == 0 {
            vm.tick()?;
          }
          let name_units = scope.heap().get_string(name)?.as_code_units();
          if crate::tick::code_units_eq_with_ticks(name_units, binding_name_units, || vm.tick())? {
            found = Some((binding.value, binding.initialized));
            break;
          }
        }
        found.ok_or(VmError::InvariantViolation(
          "module namespace getter binding not found in closure env",
        ))?
      };

      if !initialized {
        let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
          "module namespace getter requires intrinsics for ReferenceError",
        ))?;
        let message = crate::fallible_format::try_format_error_message(
          "Cannot access '",
          &export_name,
          "' before initialization",
        )?;
        let err_obj = crate::new_reference_error(scope, intr, &message)?;
        return Err(VmError::Throw(err_obj));
      }

      // `EnvBindingValue::Indirect` uses a tickless heap-internal lookup; do the lookup here so
      // large module environments and export names still observe budgets/interrupts.
      fn env_get_binding_value_by_gc_string_with_tick(
        heap: &Heap,
        env: GcEnv,
        name: crate::GcString,
        tick: &mut impl FnMut() -> Result<(), VmError>,
      ) -> Result<Value, VmError> {
        let rec = heap.get_env_record(env)?;
        let crate::env::EnvRecord::Declarative(rec) = rec else {
          return Err(VmError::Unimplemented("object environment record"));
        };
        let needle_units = heap.get_string(name)?.as_code_units();
        let mut found: Option<&crate::env::EnvBinding> = None;
        for (i, binding) in rec.bindings.iter().enumerate() {
          if i % 1024 == 0 {
            tick()?;
          }
          let Some(binding_name) = binding.name else {
            continue;
          };
          let units = heap.get_string(binding_name)?.as_code_units();
          if crate::tick::code_units_eq_with_ticks(units, needle_units, || tick())? {
            found = Some(binding);
            break;
          }
        }
        let binding = found.ok_or(VmError::Unimplemented("unbound identifier"))?;
        if !binding.initialized {
          // TDZ sentinel; see `Heap::env_get_binding_value`.
          return Err(VmError::Throw(Value::Null));
        }
        match binding.value {
          crate::env::EnvBindingValue::Direct(v) => Ok(v),
          crate::env::EnvBindingValue::Indirect { env, name } => {
            env_get_binding_value_by_gc_string_with_tick(heap, env, name, tick)
          }
        }
      }

      let mut tick = || vm.tick();
      let value_res = match binding_value {
        crate::env::EnvBindingValue::Direct(v) => Ok(v),
        crate::env::EnvBindingValue::Indirect { env, name } => {
          env_get_binding_value_by_gc_string_with_tick(scope.heap(), env, name, &mut tick)
        }
      };

      match value_res {
        Ok(v) => Ok(v),
        Err(VmError::Throw(Value::Null)) => {
          let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
            "module namespace getter requires intrinsics for ReferenceError",
          ))?;
          let message = crate::fallible_format::try_format_error_message(
            "Cannot access '",
            &export_name,
            "' before initialization",
          )?;
          let err_obj = crate::new_reference_error(scope, intr, &message)?;
          Err(VmError::Throw(err_obj))
        }
        Err(err) => Err(err),
      }
    }
    _ => Err(VmError::InvariantViolation(
      "module namespace getter slot must be a binding name or namespace object",
    )),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::microtasks::MicrotaskQueue;
  use crate::test_alloc::FailAllocsGuard;
  use crate::{HeapLimits, Realm, VmOptions};

  #[test]
  fn tla_fallback_ast_is_charged_against_heap_limits() -> Result<(), VmError> {
    // The module source itself should fit under heap limits, but the retained `parse-js` AST
    // estimate should exceed them. This ensures we fail fast with `VmError::OutOfMemory` rather than
    // attempting to allocate an enormous off-heap AST that bypasses heap accounting.
    let max_bytes = 1024 * 1024; // 1 MiB
    let mut heap = Heap::new(HeapLimits::new(max_bytes, max_bytes));
    let mut vm = Vm::new(VmOptions::default());

    let filler_len = max_bytes / 3;
    let src = format!("await 0;{}", ";".repeat(filler_len));
    // Avoid `Arc::new`, which can abort the process on allocator OOM.
    let source = SourceText::new_charged_arc(&mut heap, "m", src)?;

    let err = parse_module_ast_for_tla_fallback(&mut vm, &mut heap, &source).unwrap_err();
    assert!(matches!(err, VmError::OutOfMemory));
    Ok(())
  }

  #[test]
  fn tla_fallback_ast_charge_is_retained_until_tla_state_dropped() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;

    // Ensure module environments created by the graph have the correct outer.
    let mut graph = ModuleGraph::new();
    graph.set_global_lexical_env(realm.global_lexical_env());

    // Create a module record that is already `Linked` but has no stored AST; this forces the
    // compiled-module TLA fallback path to parse + retain a `parse-js` AST during evaluation.
    let env_root = {
      let mut scope = heap.scope();
      let env = scope.env_create(Some(realm.global_lexical_env()))?;
      scope.push_env_root(env)?;
      scope.heap_mut().add_env_root(env)?
    };

    let filler = ";".repeat(1024);
    let source = SourceText::new_charged_arc(&mut heap, "m", format!("await 0;{filler}"))?;
    let expected_bytes = source.text.len().saturating_mul(4);

    let module = graph.add_module(SourceTextModuleRecord {
      source: Some(source.clone()),
      ast: None,
      status: ModuleStatus::Linked,
      has_tla: true,
      environment: Some(env_root),
      ..Default::default()
    })?;

    let mut host = ();
    let mut hooks = MicrotaskQueue::new();
    let _promise = graph.evaluate(
      &mut vm,
      &mut heap,
      realm.global_object(),
      realm.id(),
      module,
      &mut host,
      &mut hooks,
    )?;

    let idx = module_index(module);
    let state = graph.tla_states[idx]
      .as_ref()
      .expect("expected module evaluation to suspend and store TLA state");

    assert!(state.tla_fallback_ast.is_some());
    let token = state
      .tla_fallback_ast_memory
      .as_ref()
      .expect("expected external memory token for retained fallback AST");
    assert_eq!(token.bytes(), expected_bytes);

    // Dropping the state should release exactly the bytes charged for the retained AST.
    let before = heap.estimated_total_bytes();
    let token_bytes = token.bytes();
    let state = graph.tla_states[idx]
      .take()
      .expect("expected TLA evaluation state to exist");
    state.teardown(&mut vm, &mut heap);
    let after = heap.estimated_total_bytes();
    assert_eq!(before.saturating_sub(after), token_bytes);

    // Clean up graph roots and discard queued Promise jobs (which may reference the aborted
    // continuation).
    graph.teardown(&mut vm, &mut heap);
    struct TeardownCtx<'a> {
      heap: &'a mut Heap,
    }
    impl crate::VmJobContext for TeardownCtx<'_> {
      fn call(
        &mut self,
        _host: &mut dyn VmHostHooks,
        _callee: Value,
        _this: Value,
        _args: &[Value],
      ) -> Result<Value, VmError> {
        Err(VmError::Unimplemented("microtask teardown ctx call"))
      }
      fn construct(
        &mut self,
        _host: &mut dyn VmHostHooks,
        _callee: Value,
        _args: &[Value],
        _new_target: Value,
      ) -> Result<Value, VmError> {
        Err(VmError::Unimplemented("microtask teardown ctx construct"))
      }
      fn add_root(&mut self, value: Value) -> Result<RootId, VmError> {
        self.heap.add_root(value)
      }
      fn remove_root(&mut self, id: RootId) {
        self.heap.remove_root(id)
      }
    }
    let mut ctx = TeardownCtx { heap: &mut heap };
    hooks.teardown(&mut ctx);

    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn tla_resume_callbacks_are_cached_per_vm() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;

    let mut host = ();
    let mut hooks = MicrotaskQueue::new();

    // Create a pending promise to use as the awaited TLA promise.
    let awaited_promise_root = {
      let mut scope = heap.scope();
      let cap = crate::promise_ops::new_promise_capability_with_host_and_hooks(
        &mut vm, &mut scope, &mut host, &mut hooks,
      )?;
      scope.push_root(cap.promise)?;
      scope.heap_mut().add_root(cap.promise)?
    };

    let module = ModuleId::from_raw(0);
    let mut graph = ModuleGraph::new();

    // First scheduling may register the two internal callbacks; subsequent schedules must not.
    let before = vm.native_call_count();
    {
      let promise = heap
        .get_root(awaited_promise_root)
        .ok_or_else(VmError::invalid_handle)?;
      let mut scope = heap.scope();
      graph.schedule_tla_resume(&mut vm, &mut scope, &mut host, &mut hooks, module, promise)?;
    }
    let after_first = vm.native_call_count();
    {
      let promise = heap
        .get_root(awaited_promise_root)
        .ok_or_else(VmError::invalid_handle)?;
      let mut scope = heap.scope();
      graph.schedule_tla_resume(&mut vm, &mut scope, &mut host, &mut hooks, module, promise)?;
    }
    let after_second = vm.native_call_count();

    assert_eq!(
      after_first, after_second,
      "schedule_tla_resume should not register new native calls after first use (native_calls: {before} -> {after_first} -> {after_second})"
    );

    heap.remove_root(awaited_promise_root);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn ensure_scc_info_returns_out_of_memory_on_alloc_failure() -> Result<(), VmError> {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut vm = Vm::new(VmOptions::default());

    let mut graph = ModuleGraph::new();
    let _module = graph.add_module(SourceTextModuleRecord::parse(&mut heap, "export const x = 1;")?)?;

    // Simulate global allocator OOM and ensure SCC computation reports `VmError::OutOfMemory`
    // instead of aborting the process (e.g. via infallible `vec![..; module_count]`).
    let _guard = FailAllocsGuard::new();
    let err = graph.ensure_scc_info(&mut vm).expect_err("expected OOM");
    assert!(matches!(err, VmError::OutOfMemory));
    Ok(())
  }

  #[test]
  fn ensure_no_tla_in_resolved_graph_returns_out_of_memory_on_alloc_failure() -> Result<(), VmError> {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut vm = Vm::new(VmOptions::default());

    let mut graph = ModuleGraph::new();
    let module = graph.add_module(SourceTextModuleRecord::parse(&mut heap, "export const x = 1;")?)?;

    let _guard = FailAllocsGuard::new();
    let err = graph
      .ensure_no_tla_in_resolved_graph(&mut vm, module)
      .expect_err("expected OOM");
    assert!(matches!(err, VmError::OutOfMemory));
    Ok(())
  }

  #[test]
  fn inner_module_evaluation_returns_out_of_memory_on_alloc_failure() -> Result<(), VmError> {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut vm = Vm::new(VmOptions::default());

    let mut graph = ModuleGraph::new();
    let module = graph.add_module(SourceTextModuleRecord::parse(&mut heap, "export const x = 1;")?)?;

    let _guard = FailAllocsGuard::new();
    let err = graph
      .inner_module_evaluation(&mut vm, module)
      .expect_err("expected OOM");
    assert!(matches!(err, VmError::OutOfMemory));
    Ok(())
  }
}
