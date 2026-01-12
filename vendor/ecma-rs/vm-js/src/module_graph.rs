use crate::execution_context::ModuleId;
use crate::exec::{instantiate_module_decls, run_module, run_module_until_await, ModuleTlaStepResult};
use crate::module_record::ModuleNamespaceCache;
use crate::module_record::ModuleStatus;
use crate::module_record::PromiseCapabilityRoots;
use crate::module_record::ResolveExportResult;
use crate::module_record::SourceTextModuleRecord;
use crate::property::{PropertyDescriptor, PropertyKey, PropertyKind};
use crate::{
  cmp_utf16, GcObject, LoadedModuleRequest, ModuleRequest, RealmId, RootId, Scope, StackFrame, Value, Vm,
  VmError,
};
use crate::{Heap, VmHost, VmHostHooks};
use core::mem;
use std::sync::Arc;

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

fn attach_stack_property_for_promise_rejection(scope: &mut Scope<'_>, reason: Value, err: &VmError) {
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

/// Minimal in-memory module graph used to exercise ECMA-262 module record algorithms.
///
/// This intentionally does **not** implement a full module loader. Tests are responsible for
/// constructing module records and linking their `[[RequestedModules]]` entries to concrete
/// [`ModuleId`]s.
#[derive(Debug)]
pub struct ModuleGraph {
  modules: Vec<SourceTextModuleRecord>,
  host_resolve: Vec<(ModuleRequest, ModuleId)>,
  tla_states: Vec<Option<TlaEvaluationState>>,
  torn_down: bool,
}

impl Default for ModuleGraph {
  fn default() -> Self {
    Self {
      modules: Vec::new(),
      host_resolve: Vec::new(),
      tla_states: Vec::new(),
      // A freshly-created graph does not own any persistent roots yet, and can be dropped safely.
      torn_down: true,
    }
  }
}

impl ModuleGraph {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn add_module(&mut self, record: SourceTextModuleRecord) -> ModuleId {
    let id = ModuleId::from_raw(self.modules.len() as u64);
    self.modules.push(record);
    self.tla_states.push(None);
    id
  }

  /// Adds a module to the graph and registers it under `specifier` for later linking.
  pub fn add_module_with_specifier(
    &mut self,
    specifier: impl AsRef<str>,
    record: SourceTextModuleRecord,
  ) -> ModuleId {
    let id = self.add_module(record);
    self.register_specifier(specifier, id);
    id
  }

  /// Registers a host resolution mapping used by [`ModuleGraph::link_all_by_specifier`].
  pub fn register_specifier(&mut self, specifier: impl AsRef<str>, module: ModuleId) {
    self
      .host_resolve
      .push((module_request_from_specifier(specifier.as_ref()), module));
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
      return;
    }
    self.torn_down = true;

    // Abort any in-progress async module evaluation so its promise capability roots are removed.
    for slot in &mut self.tla_states {
      if let Some(state) = slot.take() {
        state.teardown(vm, heap);
      }
    }

    // Remove per-module persistent roots.
    for (idx, module) in self.modules.iter_mut().enumerate() {
      if let Some(ns) = module.namespace.take() {
        heap.remove_root(ns.object);
      }
      if let Some(env_root) = module.environment.take() {
        heap.remove_env_root(env_root);
      }
      if let Some(root) = module.import_meta.take() {
        heap.remove_root(root);
      }

      // Cyclic module record persistent roots (top-level await state / cached errors).
      module.teardown_top_level_capability(heap);
      module.teardown_evaluation_error(heap);

      // `import.meta` is currently cached on the `Vm` (not on the module record).
      vm.remove_import_meta_cache_entry(heap, ModuleId::from_raw(idx as u64));
    }

    // Ensure the VM does not retain a raw pointer to this graph after teardown.
    if vm.module_graph_ptr() == Some(self as *mut ModuleGraph) {
      vm.clear_module_graph();
    }
  }

  /// Alias for [`ModuleGraph::teardown`].
  pub fn remove_roots(&mut self, vm: &mut Vm, heap: &mut Heap) {
    self.teardown(vm, heap);
  }

  /// Abort an in-progress async module evaluation created via top-level `await`.
  ///
  /// This is used by embeddings that only support async evaluation when the returned evaluation
  /// promise settles via microtasks (for example, `await Promise.resolve()`), and must fail
  /// deterministically when evaluation remains pending after draining microtasks.
  pub fn abort_tla_evaluation(&mut self, vm: &mut Vm, heap: &mut Heap, module: ModuleId) {
    let idx = module_index(module);
    let Some(slot) = self.tla_states.get_mut(idx) else {
      return;
    };
    let Some(mut state) = slot.take() else {
      return;
    };

    // Restore the previous module graph pointer early: once we abort, any queued resume callbacks
    // should no-op (they will early-return because their evaluation state is removed).
    state.restore_module_graph(vm);

    // Best-effort: abort any pending async continuations that belong to the in-progress module
    // evaluation. When the host calls `abort_tla_evaluation`, it is explicitly not going to drive
    // the event loop further, so we must ensure no rooted async state remains.
    for id in state.async_continuation_ids.drain(..) {
      vm.abort_async_continuation(heap, id);
    }

    if let Some(roots) = state.promise_roots.take() {
      let mut scope = heap.scope();

      let reason = match vm.intrinsics() {
        Some(intr) => crate::new_error(&mut scope, intr.error_prototype(), "Error", TLA_ABORT_REASON)
          .unwrap_or(Value::Undefined),
        None => Value::Undefined,
      };

      if let Some(reject) = scope.heap().get_root(roots.reject_root()) {
        // Best-effort: ensure the promise settles deterministically so embeddings that inspect the
        // evaluation promise see a rejection instead of a forever-pending promise.
        //
        // Route any resulting Promise jobs into a local queue and discard them immediately: hosts
        // that call `abort_tla_evaluation` are explicitly *not* going to drive the event loop, but
        // we still need to clean up any persistent roots owned by queued jobs.
        let mut abort_hooks = crate::MicrotaskQueue::new();
        let _ = vm.call_with_host(&mut scope, &mut abort_hooks, reject, Value::Undefined, &[reason]);

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
          let mut ctx = AbortJobCtx { heap: scope.heap_mut() };
          abort_hooks.teardown(&mut ctx);
        }
      }

      roots.teardown(scope.heap_mut());
    }

    // Mark the module as evaluated-with-error so further evaluation attempts fail deterministically
    // (mirrors ECMA-262's "evaluated with error" pattern).
    if let Some(record) = self.modules.get_mut(idx) {
      record.status = ModuleStatus::Evaluated;
      record.evaluation_error_unimplemented = Some(TLA_ABORT_REASON);
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
    let exported_names = self.modules[idx].get_exported_names_with_vm(vm, self, module)?;

    // unambiguousNames = [ name | name in exportedNames, module.ResolveExport(name) is ResolvedBinding ]
    let mut unambiguous_names = Vec::<String>::new();
    for name in exported_names {
      if matches!(
        self.modules[idx].resolve_export_with_vm(vm, self, module, &name)?,
        ResolveExportResult::Resolved(_)
      ) {
        unambiguous_names.push(name);
      }
    }

    // namespace = ModuleNamespaceCreate(module, unambiguousNames)
    let (namespace_obj, exports_sorted) =
      self.module_namespace_create(vm, scope, module, &unambiguous_names)?;

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
    let token = {
      let mut tmp = scope.reborrow();
      tmp.push_root(Value::Object(namespace_obj))?;
      tmp.heap_mut().charge_external(exports_total_bytes)?
    };

    // Cache the namespace object via a persistent root so it remains live across GC.
    let root = scope.heap_mut().add_root(Value::Object(namespace_obj))?;

    self.modules[idx].namespace = Some(ModuleNamespaceCache {
      object: root,
      exports: exports_sorted,
      external_memory: Some(Arc::new(token)),
    });
    self.torn_down = false;

    Ok(namespace_obj)
  }

  /// Convenience accessor for the module namespace's cached `[[Exports]]` list.
  pub fn module_namespace_exports(&self, module: ModuleId) -> Option<&[String]> {
    self.module(module).namespace_exports()
  }

  /// Populates each module's `[[LoadedModules]]` mapping using the host resolution map and the
  /// module's `[[RequestedModules]]` list.
  pub fn link_all_by_specifier(&mut self) {
    for referrer_idx in 0..self.modules.len() {
      let requests = self.modules[referrer_idx].requested_modules.clone();
      for request in requests {
        if let Some(imported) = self.resolve_host_module(&request) {
          self.modules[referrer_idx]
            .loaded_modules
            .push(LoadedModuleRequest::new(request, imported));
        } else {
          // `ModuleGraph` is a small in-memory helper used primarily by unit tests. Avoid panicking
          // in library code; missing host resolution simply leaves the request unlinked.
          debug_assert!(
            false,
            "ModuleGraph::link_all_by_specifier: no module registered for specifier {:?}",
            request.specifier
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
  }

  /// Implements ECMA-262 `GetImportedModule(referrer, request)`.
  pub fn get_imported_module(&self, referrer: ModuleId, request: &ModuleRequest) -> Option<ModuleId> {
    self
      .modules[module_index(referrer)]
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
    exports: &[String],
  ) -> Result<(GcObject, Vec<String>), VmError> {
    // 1. Let exports be a List whose elements are the String values representing the exports of module.
    // 2. Let sortedExports be a List containing the same values as exports in ascending order.
    let mut sorted_exports = exports.to_vec();
    sorted_exports.sort_by(|a, b| cmp_utf16(a, b));

    let intr = vm
      .intrinsics()
      .ok_or(VmError::Unimplemented("module namespaces require intrinsics"))?;

    let getter_call = vm.module_namespace_getter_call_id()?;

    // Allocate the namespace object.
    //
    // Root it before any further allocations (e.g. the `toStringTag` value string) in case those
    // allocations trigger a GC.
    let mut inner = scope.reborrow();
    let obj = inner.alloc_object_with_prototype(None)?;
    inner.push_root(Value::Object(obj))?;

    // prototype must be `null`.
    inner.heap_mut().object_set_prototype(obj, None)?;

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

    // Define accessor properties for each exported binding.
    //
    // A real module namespace is an exotic object with special internal methods; for now we model
    // the export properties using ordinary accessor properties whose getters read from module
    // environments (live bindings).
    for export_name in &sorted_exports {
      let resolution = match self.modules[module_index(module)].resolve_export_with_vm(vm, self, module, export_name)? {
        ResolveExportResult::Resolved(res) => res,
        // `GetModuleNamespace` filters out missing/ambiguous names; treat any mismatch as an
        // internal invariant violation.
        _ => {
          return Err(VmError::InvariantViolation(
            "module namespace export list contains a missing/ambiguous name",
          ))
        }
      };

      let getter = match resolution.binding_name {
        crate::module_record::BindingName::Name(local_name) => {
          let env_root = self.modules[module_index(resolution.module)]
            .environment
            .ok_or(VmError::Unimplemented("module namespace requires linked module environments"))?;
          let env = inner
            .heap()
            .get_env_root(env_root)
            .ok_or_else(|| VmError::invalid_handle())?;

          let mut fn_scope = inner.reborrow();
          let binding_name = fn_scope.alloc_string(&local_name)?;
          fn_scope.push_root(Value::String(binding_name))?;
          let name_s = fn_scope.alloc_string(export_name)?;
          fn_scope.push_root(Value::String(name_s))?;

          fn_scope.alloc_native_function_with_slots_and_env(
            getter_call,
            None,
            name_s,
            0,
            &[Value::String(binding_name), Value::String(name_s)],
            Some(env),
          )?
        }
        crate::module_record::BindingName::Namespace => {
          let ns = self.get_module_namespace(resolution.module, vm, &mut inner)?;
          let mut fn_scope = inner.reborrow();
          fn_scope.push_root(Value::Object(ns))?;
          let name_s = fn_scope.alloc_string(export_name)?;
          fn_scope.push_root(Value::String(name_s))?;

          fn_scope.alloc_native_function_with_slots_and_env(
            getter_call,
            None,
            name_s,
            0,
            &[Value::Object(ns), Value::String(name_s)],
            None,
          )?
        }
      };

      // Root the getter while allocating the property key and descriptor.
      let mut prop_scope = inner.reborrow();
      prop_scope.push_root(Value::Object(getter))?;

      let key = prop_scope.alloc_string(export_name)?;
      let desc = PropertyDescriptor {
        enumerable: true,
        configurable: false,
        kind: PropertyKind::Accessor {
          get: Value::Object(getter),
          set: Value::Undefined,
        },
      };
      prop_scope.define_property(obj, PropertyKey::String(key), desc)?;
    }

    inner.heap_mut().object_set_extensible(obj, false)?;

    Ok((obj, sorted_exports))
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
    module: ModuleId,
  ) -> Result<(), VmError> {
    // Use a nested scope so any temporary stack roots are popped before returning to the caller.
    let mut link_scope = scope.reborrow();
    self.link_inner(vm, &mut link_scope, global_object, module)
  }

  pub fn link(
    &mut self,
    vm: &mut Vm,
    heap: &mut Heap,
    global_object: GcObject,
    module: ModuleId,
  ) -> Result<(), VmError> {
    let mut scope = heap.scope();
    self.link_with_scope(vm, &mut scope, global_object, module)
  }

  fn link_inner(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    global_object: GcObject,
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
      ModuleStatus::Errored => return Err(VmError::Unimplemented("module is in an errored state")),
      ModuleStatus::New | ModuleStatus::Unlinked => {}
    }

    // Ensure module linking work observes VM fuel/deadline/interrupt state, even when modules have
    // no executable statements (and therefore do not run through the evaluator's statement-level
    // tick loop during instantiation).
    vm.tick()?;

    // Mark linking in progress (cycle-safe).
    self.modules[idx].status = ModuleStatus::Linking;

    // Ensure the module has an environment root allocated early so cycles can create import bindings
    // to it.
    if self.modules[idx].environment.is_none() {
      let env = scope.env_create(None)?;
      scope.push_env_root(env)?;
      let root = scope.heap_mut().add_env_root(env)?;
      self.modules[idx].environment = Some(root);
      self.torn_down = false;
    }

    let requested_modules = self.modules[idx].requested_modules.clone();
    let import_entries = self.modules[idx].import_entries.clone();
    let local_exports = self.modules[idx].local_export_entries.clone();
    let source = self.modules[idx]
      .source
      .clone()
      .ok_or(VmError::Unimplemented("module source missing"))?;
    let ast = self.modules[idx]
      .ast
      .clone()
      .ok_or(VmError::Unimplemented("module AST missing"))?;

    // Link dependencies first.
    const LINK_TICK_EVERY: usize = 32;
    for (i, request) in requested_modules.into_iter().enumerate() {
      if i % LINK_TICK_EVERY == 0 && i != 0 {
        vm.tick()?;
      }
      let imported = self
        .get_imported_module(module, &request)
        .ok_or(VmError::Unimplemented("unlinked module request"))?;
      self.link_inner(vm, scope, global_object, imported)?;
    }

    let env_root = self.modules[idx]
      .environment
      .ok_or(VmError::InvariantViolation("module environment root missing"))?;
    let module_env = scope
      .heap()
      .get_env_root(env_root)
      .ok_or_else(|| VmError::invalid_handle())?;

    // Create import bindings.
    for (i, entry) in import_entries.into_iter().enumerate() {
      if i % LINK_TICK_EVERY == 0 && i != 0 {
        vm.tick()?;
      }
      let imported_module = self
        .get_imported_module(module, &entry.module_request)
        .ok_or(VmError::Unimplemented("unlinked module request"))?;

      match entry.import_name {
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
            .resolve_export_with_vm(vm, self, imported_module, &import_name)?;
          let ResolveExportResult::Resolved(resolution) = resolution else {
            return Err(VmError::Unimplemented("imported binding resolution failure"));
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

    // Ensure `*default*` exists for `export default <expr>`.
    if local_exports.iter().any(|e| e.local_name == "*default*") {
      if !scope.heap().env_has_binding(module_env, "*default*")? {
        scope.env_create_immutable_binding(module_env, "*default*")?;
      }
    }

    // Instantiate local declarations (creates bindings + hoists function objects).
    instantiate_module_decls(vm, scope, global_object, module_env, source, &ast.stx.body)?;

    self.modules[idx].status = ModuleStatus::Linked;
    Ok(())
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
    let prev_graph = vm.module_graph_ptr();
    vm.set_module_graph(self);

    // For async module evaluation (top-level await), `vm.module_graph_ptr` must remain set until
    // the evaluation promise is settled (promise reactions run as microtasks after this function
    // returns). Track whether we should restore immediately on return, or defer restoration to the
    // async continuation/abort path.
    let mut restore_graph_on_return = true;

    let result = (|| -> Result<Value, VmError> {
      self.link_with_scope(vm, scope, global_object, module)?;

      let mut eval_scope = scope.reborrow();
      let intr = vm
        .intrinsics()
        .ok_or(VmError::Unimplemented("module evaluation requires intrinsics"))?;
      let cap = crate::builtins::new_promise_capability_with_host_and_hooks(
        vm,
        &mut eval_scope,
        host,
        hooks,
        Value::Object(intr.promise()),
      )?;

      let promise = cap.promise;
      let roots = [cap.promise, cap.resolve, cap.reject];
      eval_scope.push_roots(&roots)?;

      let idx = module_index(module);

      if !self.modules[idx].has_tla {
        let result = self.eval_inner(
          vm,
          &mut eval_scope,
          global_object,
          realm_id,
          module,
          host,
          hooks,
        );

        match result {
          Ok(()) => {
            eval_scope.push_root(cap.resolve)?;
            let _ = vm.call_with_host_and_hooks(
              host,
              &mut eval_scope,
              hooks,
              cap.resolve,
              Value::Undefined,
              &[Value::Undefined],
            )?;
          }
          Err(err) => {
            // For JS throw completions, reject with the thrown value. For internal VM errors (OOM,
            // InvalidHandle, unimplemented paths), prefer rejecting with an `Error` object so callers
            // have some debugging signal instead of a bare `undefined`.
            let reason = if let Some(thrown) = err.thrown_value() {
              thrown
            } else {
              let message = non_throw_vm_error_message(&err);
              crate::new_error(&mut eval_scope, intr.error_prototype(), "Error", message)
                .unwrap_or(Value::Undefined)
            };
            attach_stack_property_for_promise_rejection(&mut eval_scope, reason, &err);
            eval_scope.push_root(cap.reject)?;
            eval_scope.push_root(reason)?;
            let _ = vm.call_with_host_and_hooks(
              host,
              &mut eval_scope,
              hooks,
              cap.reject,
              Value::Undefined,
              &[reason],
            )?;
          }
        }

        return Ok(promise);
      }

      // Minimal top-level await support: execute the module body until a supported `await` statement
      // is encountered, then resume via Promise jobs.
      let step = self.eval_tla_start(
        vm,
        &mut eval_scope,
        global_object,
        realm_id,
        module,
        host,
        hooks,
      );

      match step {
        Ok(ModuleTlaStepResult::Completed) => {
          eval_scope.push_root(cap.resolve)?;
          let _ = vm.call_with_host_and_hooks(
            host,
            &mut eval_scope,
            hooks,
            cap.resolve,
            Value::Undefined,
            &[Value::Undefined],
          )?;
        }
        Ok(ModuleTlaStepResult::Await { promise: awaited, resume_index }) => {
          // Keep the module graph pointer installed until async evaluation completes.
          restore_graph_on_return = false;

          // Root the capability values in the heap so they survive across microtasks.
          let roots = PromiseCapabilityRoots::new(&mut eval_scope, cap)?;
          self.torn_down = false;

          // Store async evaluation state for resume/reject callbacks.
          if self.tla_states.len() <= idx {
            self
              .tla_states
              .resize_with(idx.saturating_add(1), || None);
          }
          self.tla_states[idx] = Some(TlaEvaluationState {
            resume_index,
            promise_roots: Some(roots),
            global_object,
            realm_id,
            prev_graph,
            async_continuation_ids: Vec::new(),
          });

          // Schedule the first resume step.
          self.schedule_tla_resume(vm, &mut eval_scope, host, hooks, module, awaited)?;
        }
        Err(err) => {
          let reason = if let Some(thrown) = err.thrown_value() {
            thrown
          } else {
            let message = non_throw_vm_error_message(&err);
            crate::new_error(&mut eval_scope, intr.error_prototype(), "Error", message)
              .unwrap_or(Value::Undefined)
          };
          attach_stack_property_for_promise_rejection(&mut eval_scope, reason, &err);
          eval_scope.push_root(cap.reject)?;
          eval_scope.push_root(reason)?;
          let _ = vm.call_with_host_and_hooks(
            host,
            &mut eval_scope,
            hooks,
            cap.reject,
            Value::Undefined,
            &[reason],
          )?;
        }
      };

      Ok(promise)
    })();

    // Restore any previous module graph pointer.
    if restore_graph_on_return {
      match prev_graph {
        Some(ptr) => unsafe {
          vm.set_module_graph(&mut *ptr);
        },
        None => vm.clear_module_graph(),
      }
    }

    result
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
    let prev_graph = vm.module_graph_ptr();
    vm.set_module_graph(self);

    let result = (|| -> Result<(), VmError> {
      self.link_with_scope(vm, scope, global_object, module)?;

      if self.modules[module_index(module)].has_tla {
        return Err(VmError::Unimplemented("top-level await"));
      }

      self.eval_inner(vm, scope, global_object, realm_id, module, host, hooks)
    })();

    // Restore any previous module graph pointer.
    match prev_graph {
      Some(ptr) => unsafe {
        vm.set_module_graph(&mut *ptr);
      },
      None => vm.clear_module_graph(),
    }

    result
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
    let evaluation_error = self.modules[idx].evaluation_error_unimplemented;
    match status {
      ModuleStatus::Evaluated => match evaluation_error {
        Some(msg) => return Err(VmError::Unimplemented(msg)),
        None => return Ok(()),
      },
      ModuleStatus::Evaluating | ModuleStatus::EvaluatingAsync => return Ok(()),
      ModuleStatus::Linked => {}
      ModuleStatus::Errored => return Err(VmError::Unimplemented("module is in an errored state")),
      _ => return Err(VmError::Unimplemented("module is not linked")),
    }

    // Ensure module evaluation observes budgets even when the module body is empty (no statement
    // ticks).
    vm.tick()?;

    self.modules[idx].status = ModuleStatus::Evaluating;

    let requested_modules = self.modules[idx].requested_modules.clone();
    const EVAL_TICK_EVERY: usize = 32;
    for (i, request) in requested_modules.into_iter().enumerate() {
      if i % EVAL_TICK_EVERY == 0 && i != 0 {
        vm.tick()?;
      }
      let imported = self
        .get_imported_module(module, &request)
        .ok_or(VmError::Unimplemented("unlinked module request"))?;
      self.eval_inner(vm, scope, global_object, realm_id, imported, host, hooks)?;
    }

    let env_root = self.modules[idx]
      .environment
      .ok_or(VmError::InvariantViolation("module environment missing"))?;
    let module_env = scope
      .heap()
      .get_env_root(env_root)
      .ok_or_else(|| VmError::invalid_handle())?;

    let source = self.modules[idx]
      .source
      .clone()
      .ok_or(VmError::Unimplemented("module source missing"))?;
    let ast = self.modules[idx]
      .ast
      .clone()
      .ok_or(VmError::Unimplemented("module AST missing"))?;

    let run_result = run_module(
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
    );

    match run_result {
      Ok(()) => {
        self.modules[idx].status = ModuleStatus::Evaluated;
        Ok(())
      }
      Err(err) => {
        self.modules[idx].status = ModuleStatus::Errored;
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
    let evaluation_error = self.modules[idx].evaluation_error_unimplemented;
    match status {
      ModuleStatus::Evaluated => match evaluation_error {
        Some(msg) => return Err(VmError::Unimplemented(msg)),
        None => return Ok(ModuleTlaStepResult::Completed),
      },
      ModuleStatus::Evaluating | ModuleStatus::EvaluatingAsync => return Ok(ModuleTlaStepResult::Completed),
      ModuleStatus::Linked => {}
      ModuleStatus::Errored => return Err(VmError::Unimplemented("module is in an errored state")),
      _ => return Err(VmError::Unimplemented("module is not linked")),
    }

    // Ensure module evaluation observes budgets even when the module body is empty.
    vm.tick()?;

    self.modules[idx].status = ModuleStatus::Evaluating;

    // Evaluate dependencies synchronously (top-level await in dependencies remains unsupported).
    let requested_modules = self.modules[idx].requested_modules.clone();
    const EVAL_TICK_EVERY: usize = 32;
    for (i, request) in requested_modules.into_iter().enumerate() {
      if i % EVAL_TICK_EVERY == 0 && i != 0 {
        vm.tick()?;
      }
      let imported = self
        .get_imported_module(module, &request)
        .ok_or(VmError::Unimplemented("unlinked module request"))?;
      self.eval_inner(vm, scope, global_object, realm_id, imported, host, hooks)?;
    }

    self.eval_tla_body_from_index(vm, scope, global_object, realm_id, module, host, hooks, 0)
  }

  fn eval_tla_body_from_index(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    global_object: GcObject,
    realm_id: RealmId,
    module: ModuleId,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    start_index: usize,
  ) -> Result<ModuleTlaStepResult, VmError> {
    let idx = module_index(module);
    let env_root = self.modules[idx]
      .environment
      .ok_or(VmError::InvariantViolation("module environment missing"))?;
    let module_env = scope
      .heap()
      .get_env_root(env_root)
      .ok_or_else(|| VmError::invalid_handle())?;
    let source = self.modules[idx]
      .source
      .clone()
      .ok_or(VmError::Unimplemented("module source missing"))?;
    let ast = self.modules[idx]
      .ast
      .clone()
      .ok_or(VmError::Unimplemented("module AST missing"))?;

    let step = run_module_until_await(
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
      start_index,
    )?;

    match step {
      ModuleTlaStepResult::Completed => {
        self.modules[idx].status = ModuleStatus::Evaluated;
        Ok(ModuleTlaStepResult::Completed)
      }
      ModuleTlaStepResult::Await { promise, resume_index } => Ok(ModuleTlaStepResult::Await {
        promise,
        resume_index,
      }),
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

    let on_fulfilled_call = vm.module_tla_on_fulfilled_call_id()?;
    let on_rejected_call = vm.module_tla_on_rejected_call_id()?;

    let on_fulfilled_name = scope.alloc_string("moduleTlaOnFulfilled")?;
    scope.push_root(Value::String(on_fulfilled_name))?;
    let on_rejected_name = scope.alloc_string("moduleTlaOnRejected")?;
    scope.push_root(Value::String(on_rejected_name))?;

    let module_slot = Value::Number(module.to_raw() as f64);
    let slots = [module_slot];

    let on_fulfilled =
      scope.alloc_native_function_with_slots(on_fulfilled_call, None, on_fulfilled_name, 1, &slots)?;
    scope
      .heap_mut()
      .object_set_prototype(on_fulfilled, Some(intr.function_prototype()))?;
    scope.push_root(Value::Object(on_fulfilled))?;

    let on_rejected =
      scope.alloc_native_function_with_slots(on_rejected_call, None, on_rejected_name, 1, &slots)?;
    scope
      .heap_mut()
      .object_set_prototype(on_rejected, Some(intr.function_prototype()))?;
    scope.push_root(Value::Object(on_rejected))?;

    scope.push_root(awaited_promise)?;
    crate::promise_ops::perform_promise_then_no_capability_with_host_and_hooks(
      vm,
      scope,
      host,
      hooks,
      awaited_promise,
      Value::Object(on_fulfilled),
      Value::Object(on_rejected),
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
  resume_index: usize,
  promise_roots: Option<PromiseCapabilityRoots>,
  global_object: GcObject,
  realm_id: RealmId,
  prev_graph: Option<*mut ModuleGraph>,
  /// Async continuation ids created solely for this module's top-level await evaluation.
  ///
  /// When an embedding aborts async module evaluation, these continuations must be torn down so
  /// their persistent roots do not leak.
  async_continuation_ids: Vec<u32>,
}

impl TlaEvaluationState {
  fn restore_module_graph(self: &Self, vm: &mut Vm) {
    match self.prev_graph {
      Some(ptr) => unsafe {
        vm.set_module_graph(&mut *ptr);
      },
      None => vm.clear_module_graph(),
    }
  }

  fn teardown(mut self, vm: &mut Vm, heap: &mut Heap) {
    // Restore the previous graph pointer so any queued resume callbacks cannot access a graph that
    // is being torn down.
    self.restore_module_graph(vm);

    for id in self.async_continuation_ids.drain(..) {
      vm.abort_async_continuation(heap, id);
    }

    if let Some(roots) = self.promise_roots.take() {
      roots.teardown(heap);
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
      self.promise_roots.is_none(),
      "TlaEvaluationState dropped with leaked persistent roots; ensure the module evaluation promise is settled or aborted"
    );
  }
}

fn module_id_from_native_slot(scope: &Scope<'_>, callee: GcObject) -> Result<ModuleId, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let raw = match slots.get(0).copied().unwrap_or(Value::Undefined) {
    Value::Number(n) if n.is_finite() && n >= 0.0 && n <= u64::MAX as f64 => n as u64,
    _ => return Err(VmError::InvariantViolation("module TLA callback missing module id slot")),
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
  _args: &[Value],
) -> Result<Value, VmError> {
  let module = module_id_from_native_slot(scope, callee)?;
  let Some(ptr) = vm.module_graph_ptr() else {
    // If the embedding cleared the module graph pointer, treat this as a no-op.
    return Ok(Value::Undefined);
  };
  let graph = unsafe { &mut *ptr };
  let idx = module_index(module);
  let (resume_index, global_object, realm_id) = {
    let Some(state) = graph.tla_states.get(idx).and_then(|s| s.as_ref()) else {
      // State was already cleaned up (module finished or was aborted).
      return Ok(Value::Undefined);
    };
    (state.resume_index, state.global_object, state.realm_id)
  };

  vm.tick()?;

  let step = graph.eval_tla_body_from_index(
    vm,
    scope,
    global_object,
    realm_id,
    module,
    host,
    hooks,
    resume_index,
  );

  match step {
    Ok(ModuleTlaStepResult::Completed) => {
      // Take and teardown state before resolving.
      let mut state = graph
        .tla_states
        .get_mut(idx)
        .and_then(|s| s.take())
        .ok_or(VmError::InvariantViolation(
          "missing async module evaluation state on completion",
        ))?;
      state.restore_module_graph(vm);

      let roots = state
        .promise_roots
        .take()
        .ok_or(VmError::InvariantViolation(
          "missing async module evaluation promise roots on completion",
        ))?;
      let cap = roots
        .capability(scope.heap())
        .ok_or_else(VmError::invalid_handle)?;
      let resolve = cap.resolve;
      scope.push_root(resolve)?;
      let _ = vm.call_with_host_and_hooks(
        host,
        scope,
        hooks,
        resolve,
        Value::Undefined,
        &[Value::Undefined],
      )?;

      // Remove persistent roots after resolving.
      roots.teardown(scope.heap_mut());
    }
    Ok(ModuleTlaStepResult::Await { promise, resume_index }) => {
      if let Some(state) = graph.tla_states.get_mut(idx).and_then(|s| s.as_mut()) {
        state.resume_index = resume_index;
      }
      graph.schedule_tla_resume(vm, scope, host, hooks, module, promise)?;
    }
    Err(err) => {
      // Take and teardown state before rejecting.
      let mut state = graph
        .tla_states
        .get_mut(idx)
        .and_then(|s| s.take())
        .ok_or(VmError::InvariantViolation(
          "missing async module evaluation state on error",
        ))?;
      state.restore_module_graph(vm);

      graph.modules[idx].status = ModuleStatus::Errored;

      let roots = state
        .promise_roots
        .take()
        .ok_or(VmError::InvariantViolation(
          "missing async module evaluation promise roots on error",
        ))?;
      let cap = roots
        .capability(scope.heap())
        .ok_or_else(VmError::invalid_handle)?;
      let reject = cap.reject;

      let reason = if let Some(thrown) = err.thrown_value() {
        thrown
      } else {
        let intr = vm
          .intrinsics()
          .ok_or(VmError::Unimplemented("module evaluation requires intrinsics"))?;
        let message = non_throw_vm_error_message(&err);
        crate::new_error(scope, intr.error_prototype(), "Error", message).unwrap_or(Value::Undefined)
      };

      scope.push_root(reject)?;
      scope.push_root(reason)?;
      let _ = vm.call_with_host_and_hooks(host, scope, hooks, reject, Value::Undefined, &[reason])?;

      roots.teardown(scope.heap_mut());
    }
  }

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
  let Some(ptr) = vm.module_graph_ptr() else {
    return Ok(Value::Undefined);
  };
  let graph = unsafe { &mut *ptr };
  let idx = module_index(module);
  let Some(mut state) = graph.tla_states.get_mut(idx).and_then(|s| s.take()) else {
    return Ok(Value::Undefined);
  };

  state.restore_module_graph(vm);
  graph.modules[idx].status = ModuleStatus::Errored;

  let roots = state
    .promise_roots
    .take()
    .ok_or(VmError::InvariantViolation(
      "missing async module evaluation promise roots on rejection",
    ))?;
  let cap = roots
    .capability(scope.heap())
    .ok_or_else(VmError::invalid_handle)?;
  let reject = cap.reject;
  let reason = args.get(0).copied().unwrap_or(Value::Undefined);
  scope.push_root(reject)?;
  scope.push_root(reason)?;
  let _ = vm.call_with_host_and_hooks(host, scope, hooks, reject, Value::Undefined, &[reason])?;

  roots.teardown(scope.heap_mut());
  Ok(Value::Undefined)
}

fn module_index(id: ModuleId) -> usize {
  // `ModuleId` is an opaque token at the VM boundary, but `ModuleGraph` uses it as a stable index
  // into its module vector for tests. Tests construct module ids exclusively via
  // `ModuleGraph::add_module*`, which uses the raw index representation.
  id.to_raw() as usize
}

fn module_request_from_specifier(specifier: &str) -> ModuleRequest {
  ModuleRequest::new(specifier, Vec::new())
}

/// Implements `ModuleNamespaceCreate` (ECMA-262 `#sec-modulenamespacecreate`) – MVP version.
///
/// This creates an ordinary object with the correct `[[Prototype]]` and `%Symbol.toStringTag%`
/// property. A real module namespace is an *exotic object* with virtual string-keyed export
/// properties backed by live bindings; that behaviour will be added once module environments exist.
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
    Value::String(s) => scope.heap().get_string(s)?.to_utf8_lossy(),
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
        for binding in rec.bindings.iter() {
          let Some(name) = binding.name else {
            continue;
          };
          if scope.heap().get_string(name)?.as_code_units() == binding_name_units {
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

      match binding_value.get(scope.heap()) {
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
  use crate::{HeapLimits, Realm, VmOptions};

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
      let cap =
        crate::promise_ops::new_promise_capability_with_host_and_hooks(&mut vm, &mut scope, &mut host, &mut hooks)?;
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
}
