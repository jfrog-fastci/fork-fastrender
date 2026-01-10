use crate::execution_context::ModuleId;
use crate::exec::{instantiate_module_decls, run_module};
use crate::module_record::ModuleNamespaceCache;
use crate::module_record::ModuleStatus;
use crate::module_record::ResolveExportResult;
use crate::module_record::SourceTextModuleRecord;
use crate::property::{PropertyDescriptor, PropertyKey, PropertyKind};
use crate::{
  cmp_utf16, GcObject, LoadedModuleRequest, ModuleRequest, RealmId, Scope, Value, Vm, VmError,
};
use crate::{Heap, VmHost, VmHostHooks};

/// Minimal in-memory module graph used to exercise ECMA-262 module record algorithms.
///
/// This intentionally does **not** implement a full module loader. Tests are responsible for
/// constructing module records and linking their `[[RequestedModules]]` entries to concrete
/// [`ModuleId`]s.
#[derive(Debug, Default)]
pub struct ModuleGraph {
  modules: Vec<SourceTextModuleRecord>,
  host_resolve: Vec<(ModuleRequest, ModuleId)>,
}

impl ModuleGraph {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn add_module(&mut self, record: SourceTextModuleRecord) -> ModuleId {
    let id = ModuleId::from_raw(self.modules.len() as u64);
    self.modules.push(record);
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
        return Err(VmError::InvalidHandle);
      };
      return Ok(obj);
    }

    // exportedNames = module.GetExportedNames()
    let exported_names = self.modules[idx].get_exported_names(self, module);

    // unambiguousNames = [ name | name in exportedNames, module.ResolveExport(name) is ResolvedBinding ]
    let mut unambiguous_names = Vec::<String>::new();
    for name in exported_names {
      if matches!(
        self.modules[idx].resolve_export(self, module, &name),
        ResolveExportResult::Resolved(_)
      ) {
        unambiguous_names.push(name);
      }
    }

    // namespace = ModuleNamespaceCreate(module, unambiguousNames)
    let (namespace_obj, exports_sorted) =
      self.module_namespace_create(vm, scope, module, &unambiguous_names)?;

    // Cache the namespace object via a persistent root so it remains live across GC.
    let root = scope.heap_mut().add_root(Value::Object(namespace_obj))?;

    self.modules[idx].namespace = Some(ModuleNamespaceCache {
      object: root,
      exports: exports_sorted,
    });

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

    let getter_call = vm.register_native_call(module_namespace_getter)?;

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
      let resolution = match self.modules[module_index(module)].resolve_export(self, module, export_name) {
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
            .ok_or(VmError::InvalidHandle)?;

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
      .ok_or(VmError::InvalidHandle)?
      .status;

    match status {
      ModuleStatus::Linked | ModuleStatus::Evaluating | ModuleStatus::Evaluated => return Ok(()),
      ModuleStatus::Linking => return Ok(()),
      ModuleStatus::Errored => return Err(VmError::Unimplemented("module is in an errored state")),
      ModuleStatus::New | ModuleStatus::Unlinked => {}
    }

    // Mark linking in progress (cycle-safe).
    self.modules[idx].status = ModuleStatus::Linking;

    // Ensure the module has an environment root allocated early so cycles can create import bindings
    // to it.
    if self.modules[idx].environment.is_none() {
      let env = scope.env_create(None)?;
      scope.push_env_root(env)?;
      let root = scope.heap_mut().add_env_root(env)?;
      self.modules[idx].environment = Some(root);
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
    for request in requested_modules {
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
      .ok_or(VmError::InvalidHandle)?;

    // Create import bindings.
    for entry in import_entries {
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
          let resolution = self.modules[module_index(imported_module)].resolve_export(
            self,
            imported_module,
            &import_name,
          );
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
                .ok_or(VmError::InvalidHandle)?;
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

    let result = (|| -> Result<Value, VmError> {
      self.link_with_scope(vm, scope, global_object, module)?;

      let mut eval_scope = scope.reborrow();
      let intr = vm
        .intrinsics()
        .ok_or(VmError::Unimplemented("module evaluation requires intrinsics"))?;
      let cap = crate::builtins::new_promise_capability(
        vm,
        &mut eval_scope,
        hooks,
        Value::Object(intr.promise()),
      )?;

      let promise = cap.promise;
      let roots = [cap.promise, cap.resolve, cap.reject];
      eval_scope.push_roots(&roots)?;

      let result = if self.modules[module_index(module)].has_tla {
        Err(VmError::Unimplemented("top-level await"))
      } else {
        self.eval_inner(
          vm,
          &mut eval_scope,
          global_object,
          realm_id,
          module,
          host,
          hooks,
        )
      };

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
            let message = err.to_string();
            crate::new_error(&mut eval_scope, intr.error_prototype(), "Error", &message)
              .unwrap_or(Value::Undefined)
          };
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

      Ok(promise)
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
      ModuleStatus::Evaluating => return Ok(()),
      ModuleStatus::Linked => {}
      ModuleStatus::Errored => return Err(VmError::Unimplemented("module is in an errored state")),
      _ => return Err(VmError::Unimplemented("module is not linked")),
    }

    self.modules[idx].status = ModuleStatus::Evaluating;

    let requested_modules = self.modules[idx].requested_modules.clone();
    for request in requested_modules {
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
      .ok_or(VmError::InvalidHandle)?;

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
fn module_namespace_getter(
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
        let message = format!("Cannot access '{}' before initialization", export_name);
        let err_obj = crate::new_reference_error(scope, intr, &message)?;
        return Err(VmError::Throw(err_obj));
      }

      match binding_value.get(scope.heap()) {
        Ok(v) => Ok(v),
        Err(VmError::Throw(Value::Null)) => {
          let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
            "module namespace getter requires intrinsics for ReferenceError",
          ))?;
          let message = format!("Cannot access '{}' before initialization", export_name);
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
