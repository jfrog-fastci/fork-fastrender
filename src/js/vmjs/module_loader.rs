use crate::error::{Error, Result};
use crate::js::import_maps::{
  resolve_module_specifier as resolve_module_specifier_with_import_maps, ImportMapError, ImportMapState,
};
use crate::js::runtime::with_event_loop;
use crate::js::url_resolve::{resolve_url, UrlResolveError};
use crate::js::vm_error_format;
use crate::js::window_realm::WindowRealmHost;
use crate::js::window_timers::VmJsEventLoopHooks;
use crate::js::{EventLoop, JsExecutionOptions};
use crate::resource::{
  cors_enforcement_enabled, ensure_cors_allows_origin, ensure_http_success, ensure_script_mime_sane,
  origin_from_url, CorsMode, DocumentOrigin, FetchDestination, FetchRequest, ResourceFetcher,
};
use std::collections::HashMap;
use std::sync::Arc;
use url::Url;
use vm_js::{
  HostDefined, ImportMetaProperty, ModuleGraph, ModuleId, ModuleLoadPayload, ModuleReferrer,
  ModuleRequest, PromiseState, PropertyKey, Scope, Value, Vm, VmError, VmHostHooks,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImportMapThrowKind {
  TypeError,
  SyntaxError,
}

fn import_map_error_to_throw_kind_and_message(err: ImportMapError) -> (ImportMapThrowKind, String) {
  match err {
    ImportMapError::TypeError(msg) => (ImportMapThrowKind::TypeError, msg),
    ImportMapError::LimitExceeded(msg) => {
      // `ImportMapError::LimitExceeded` already implies the "limit exceeded" context; keep the
      // message itself consistent with other TypeError messages and avoid duplicating the prefix.
      let msg = msg
        .strip_prefix("import map limit exceeded:")
        .map(|s| s.trim_start())
        .unwrap_or(msg.as_str());
      (ImportMapThrowKind::TypeError, format!("import map limit exceeded: {msg}"))
    }
    ImportMapError::Json(err) => (ImportMapThrowKind::SyntaxError, err.to_string()),
  }
}

/// Per-document module loader and cache for `vm-js` modules.
///
/// This is used by tooling entry points like `fetch_and_render --js` to execute `<script type="module">`
/// via real ECMAScript module linking + evaluation.
pub struct VmJsModuleLoader {
  fetcher: Arc<dyn ResourceFetcher>,
  document_url: String,
  document_origin: Option<DocumentOrigin>,
  module_graph: ModuleGraph,
  module_id_by_url: HashMap<String, ModuleId>,
  module_url_by_id: HashMap<ModuleId, String>,
  module_base_url_by_id: HashMap<ModuleId, String>,
}

impl VmJsModuleLoader {
  pub fn new(fetcher: Arc<dyn ResourceFetcher>, document_url: impl Into<String>) -> Self {
    let document_url = document_url.into();
    let document_origin = origin_from_url(&document_url);
    Self {
      fetcher,
      document_url,
      document_origin,
      module_graph: ModuleGraph::new(),
      module_id_by_url: HashMap::new(),
      module_url_by_id: HashMap::new(),
      module_base_url_by_id: HashMap::new(),
    }
  }

  pub fn module_graph(&self) -> &ModuleGraph {
    &self.module_graph
  }

  pub fn module_graph_mut(&mut self) -> &mut ModuleGraph {
    &mut self.module_graph
  }

  /// Evaluate an external (URL-backed) module script, fetching it if needed.
  ///
  /// The caller is responsible for resetting interrupt state if desired (VM budgets are applied
  /// internally using the realm's configured [`crate::js::JsExecutionOptions`]).
  pub fn evaluate_module_url<Host: WindowRealmHost + 'static>(
    &mut self,
    host: &mut Host,
    event_loop: &mut EventLoop<Host>,
    entry_url: &str,
  ) -> Result<Value> {
    self.evaluate_module_entry(host, event_loop, EntryModule::ExternalUrl(entry_url), None)
  }

  /// Evaluate an inline module script using a synthetic URL and explicit base URL for resolving imports.
  ///
  /// The caller is responsible for resetting interrupt state if desired (VM budgets are applied
  /// internally using the realm's configured [`crate::js::JsExecutionOptions`]).
  pub fn evaluate_inline_module<Host: WindowRealmHost + 'static>(
    &mut self,
    host: &mut Host,
    event_loop: &mut EventLoop<Host>,
    synthetic_url: &str,
    base_url: &str,
    source_text: &str,
  ) -> Result<Value> {
    self.evaluate_module_entry(
      host,
      event_loop,
      EntryModule::Inline {
        url: synthetic_url,
        base_url,
        source_text,
      },
      None,
    )
  }

  /// Evaluate an external (URL-backed) module script using WHATWG HTML import maps.
  pub fn evaluate_module_url_with_import_maps<Host: WindowRealmHost + 'static>(
    &mut self,
    host: &mut Host,
    event_loop: &mut EventLoop<Host>,
    import_map_state: &mut ImportMapState,
    entry_url: &str,
  ) -> Result<Value> {
    self.evaluate_module_entry(
      host,
      event_loop,
      EntryModule::ExternalUrl(entry_url),
      Some(import_map_state),
    )
  }

  /// Evaluate an inline module script using WHATWG HTML import maps.
  pub fn evaluate_inline_module_with_import_maps<Host: WindowRealmHost + 'static>(
    &mut self,
    host: &mut Host,
    event_loop: &mut EventLoop<Host>,
    import_map_state: &mut ImportMapState,
    synthetic_url: &str,
    base_url: &str,
    source_text: &str,
  ) -> Result<Value> {
    self.evaluate_module_entry(
      host,
      event_loop,
      EntryModule::Inline {
        url: synthetic_url,
        base_url,
        source_text,
      },
      Some(import_map_state),
    )
  }

  fn evaluate_module_entry<Host: WindowRealmHost + 'static>(
    &mut self,
    host: &mut Host,
    event_loop: &mut EventLoop<Host>,
    entry: EntryModule<'_>,
    import_map_state: Option<&mut ImportMapState>,
  ) -> Result<Value> {
    let VmJsModuleLoader {
      fetcher,
      document_url,
      document_origin,
      module_graph,
      module_id_by_url,
      module_url_by_id,
      module_base_url_by_id,
    } = self;
    let fetcher = Arc::clone(fetcher);
    let document_url = document_url.clone();
    let document_origin = document_origin.clone();

    with_event_loop(event_loop, move || {
      let options = {
        let (_, window_realm) = host.vm_host_and_window_realm();
        window_realm.js_execution_options()
      };

      let mut hooks = VmJsModuleHooks::<Host> {
        inner: VmJsEventLoopHooks::<Host>::new_with_host(host),
        fetcher,
        document_url: document_url.as_str(),
        document_origin,
        options,
        loaded_modules: 0,
        loaded_bytes: 0,
        module_depths: HashMap::new(),
        module_id_by_url,
        module_url_by_id,
        module_base_url_by_id,
        import_map_state,
      };

      // Borrow-split: the VM needs `&mut ModuleGraph`, while module loading uses the hooks' maps.
      // Module evaluation also needs both the embedder `VmHost` and the `WindowRealm`.
      // Pass the real embedder `VmHost` context into module evaluation (same as `WindowHost`'s
      // classic-script path). This avoids the old `VmJsHostContext` shim and keeps downcasting
      // reliable.
      let (vm_host, window_realm) = host.vm_host_and_window_realm();
      let budget = window_realm.vm_budget_now();
      let (vm, realm, heap) = window_realm.vm_realm_and_heap_mut();
      let mut vm = vm.push_budget(budget);
      // Attach the loader's module graph to the VM while we load + evaluate modules. `vm-js` uses
      // this pointer to support dynamic `import()` expressions from nested function calls.
      //
      // Note: this guard scopes the pointer to this evaluation call. If the embedding needs dynamic
      // `import()` from callbacks executed *after* this function returns (e.g. Promise reactions run
      // at a later microtask checkpoint), the embedding must ensure the module graph remains
      // attached for that dynamic extent as well.
      let _module_graph_guard = VmModuleGraphGuard::new(&mut vm, module_graph);
      let global_object = realm.global_object();
      let realm_id = realm.id();

      let tick_result: std::result::Result<(), VmError> = vm.tick();
      let outcome: Result<Value> = match tick_result {
        Ok(()) => {
          // First: fetch/parse the entry module and load its static dependency graph.
          let mut entry_module: Option<ModuleId> = None;
          let mut outcome: Result<Value> = Ok(Value::Undefined);

          {
            let mut scope = heap.scope();
            let entry_id_result: std::result::Result<ModuleId, VmError> = match entry {
              EntryModule::ExternalUrl(url) => hooks.get_or_fetch_module(
                &mut vm,
                &mut scope,
                module_graph,
                url,
                Some(hooks.document_url),
              ),
              EntryModule::Inline {
                url,
                base_url,
                source_text,
              } => hooks.get_or_parse_inline_module(&mut vm, &mut scope, module_graph, url, base_url, source_text),
            };

            let entry_id = match entry_id_result {
              Ok(id) => Some(id),
              Err(err) => {
                outcome = Err(vm_error_to_error_in_scope(&mut scope, err));
                None
              }
            };

            if let (Ok(_), Some(entry_id)) = (&outcome, entry_id) {
              hooks.module_depths.insert(entry_id, 0);
              let load_promise = match vm_js::load_requested_modules(
                &mut vm,
                &mut scope,
                module_graph,
                &mut hooks,
                entry_id,
                HostDefined::default(),
              ) {
                Ok(p) => p,
                Err(err) => {
                  outcome = Err(vm_error_to_error_in_scope(&mut scope, err));
                  Value::Undefined
                }
              };

              if outcome.is_ok() {
                if let Err(err) = ensure_promise_fulfilled(&mut scope, load_promise) {
                  outcome = Err(vm_error_to_error_in_scope(&mut scope, err));
                } else {
                  entry_module = Some(entry_id);
                }
              }
            }
          }

          // Second: link + evaluate.
          if let (Ok(_), Some(entry_id)) = (&outcome, entry_module) {
            match module_graph.evaluate(&mut vm, heap, global_object, realm_id, entry_id, vm_host, &mut hooks) {
              Ok(promise) => {
                let mut scope = heap.scope();
                if let Err(err) = ensure_promise_fulfilled(&mut scope, promise) {
                  outcome = Err(vm_error_to_error_in_scope(&mut scope, err));
                } else {
                  outcome = Ok(promise);
                }
              }
              Err(err) => {
                // Convert via a fresh scope so thrown values (if any) are rooted while formatting.
                outcome = Err(vm_error_to_error_with_fresh_scope(heap, err));
              }
            }
          }

          outcome
        }
        Err(err) => Err(vm_error_to_error_with_fresh_scope(heap, err)),
      };

      if let Some(err) = hooks.finish(heap) {
        return Err(err);
      }

      outcome
    })
  }
}

enum EntryModule<'a> {
  ExternalUrl(&'a str),
  Inline {
    url: &'a str,
    base_url: &'a str,
    source_text: &'a str,
  },
}

fn ensure_promise_fulfilled(
  scope: &mut Scope<'_>,
  promise: Value,
) -> std::result::Result<(), VmError> {
  let Value::Object(promise_obj) = promise else {
    return Err(VmError::InvariantViolation("expected a Promise object"));
  };

  // Root the Promise while we inspect and potentially stringify/rethrow its rejection reason.
  scope.push_root(Value::Object(promise_obj))?;

  let heap = scope.heap();
  match heap.promise_state(promise_obj)? {
    PromiseState::Pending => Err(VmError::Unimplemented(
      "asynchronous module loading/evaluation is not supported",
    )),
    PromiseState::Fulfilled => Ok(()),
    PromiseState::Rejected => {
      let reason = heap.promise_result(promise_obj)?.unwrap_or(Value::Undefined);
      scope.push_root(reason)?;
      Err(VmError::Throw(reason))
    }
  }
}

fn vm_error_to_error_in_scope(scope: &mut Scope<'_>, err: VmError) -> Error {
  if let Some(thrown) = err.thrown_value() {
    let _ = scope.push_root(thrown);
  }
  vm_error_format::vm_error_to_error(scope.heap_mut(), err)
}

fn vm_error_to_error_with_fresh_scope(heap: &mut vm_js::Heap, err: VmError) -> Error {
  let mut scope = heap.scope();
  vm_error_to_error_in_scope(&mut scope, err)
}

struct VmModuleGraphGuard {
  vm: *mut Vm,
  prev_graph: Option<*mut ModuleGraph>,
}

impl VmModuleGraphGuard {
  fn new(vm: &mut Vm, graph: &mut ModuleGraph) -> Self {
    let prev_graph = vm.module_graph_ptr();
    vm.set_module_graph(graph);
    Self {
      vm: vm as *mut Vm,
      prev_graph,
    }
  }
}

impl Drop for VmModuleGraphGuard {
  fn drop(&mut self) {
    // Safety: `VmModuleGraphGuard::new` captures a stable pointer to the VM borrowed by the caller.
    // The guard is only used within the dynamic extent of module loading/evaluation, so the VM is
    // still live when `drop` runs.
    unsafe {
      let vm = &mut *self.vm;
      match self.prev_graph {
        Some(ptr) => vm.set_module_graph(&mut *ptr),
        None => vm.clear_module_graph(),
      }
    }
  }
}

struct VmJsModuleHooks<'a, Host: WindowRealmHost + 'static> {
  inner: VmJsEventLoopHooks<Host>,
  fetcher: Arc<dyn ResourceFetcher>,
  document_url: &'a str,
  document_origin: Option<DocumentOrigin>,
  options: JsExecutionOptions,
  loaded_modules: usize,
  loaded_bytes: usize,
  module_depths: HashMap<ModuleId, usize>,
  module_id_by_url: &'a mut HashMap<String, ModuleId>,
  module_url_by_id: &'a mut HashMap<ModuleId, String>,
  module_base_url_by_id: &'a mut HashMap<ModuleId, String>,
  import_map_state: Option<&'a mut ImportMapState>,
}

impl<'a, Host: WindowRealmHost + 'static> VmJsModuleHooks<'a, Host> {
  fn finish(self, heap: &mut vm_js::Heap) -> Option<Error> {
    self.inner.finish(heap)
  }

  fn get_or_parse_inline_module(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    modules: &mut ModuleGraph,
    url: &str,
    base_url: &str,
    source_text: &str,
  ) -> std::result::Result<ModuleId, VmError> {
    if let Err(err) = self.options.check_module_specifier(url) {
      return Err(self.throw_type_error(vm, scope, &err.to_string()));
    }

    if let Some(existing) = self.module_id_by_url.get(url).copied() {
      return Ok(existing);
    }

    let next_modules = self
      .loaded_modules
      .checked_add(1)
      .ok_or_else(|| VmError::OutOfMemory)?;
    if let Err(err) = self.options.check_module_graph_modules(next_modules, url) {
      return Err(self.throw_type_error(vm, scope, &err.to_string()));
    }

    let module_bytes = source_text.as_bytes().len();
    let context = format!("source=module specifier={url}");
    if let Err(err) = self.options.check_script_source_bytes(module_bytes, &context) {
      return Err(self.throw_type_error(vm, scope, &err.to_string()));
    }

    let next_bytes = match self
      .options
      .check_module_graph_total_bytes(self.loaded_bytes, module_bytes, url)
    {
      Ok(next) => next,
      Err(err) => return Err(self.throw_type_error(vm, scope, &err.to_string())),
    };
    self.loaded_modules = next_modules;
    self.loaded_bytes = next_bytes;

    let source = Arc::new(vm_js::SourceText::new(url.to_string(), source_text.to_string()));
    let record = vm_js::SourceTextModuleRecord::parse_source_with_vm(vm, source)?;
    let id = modules.add_module(record);

    self.module_id_by_url.insert(url.to_string(), id);
    self.module_url_by_id.insert(id, url.to_string());
    self
      .module_base_url_by_id
      .insert(id, base_url.to_string());

    Ok(id)
  }

  fn get_or_fetch_module(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    modules: &mut ModuleGraph,
    url: &str,
    referrer_url: Option<&str>,
  ) -> std::result::Result<ModuleId, VmError> {
    if let Err(err) = self.options.check_module_specifier(url) {
      return Err(self.throw_type_error(vm, scope, &err.to_string()));
    }

    if let Some(existing) = self.module_id_by_url.get(url).copied() {
      return Ok(existing);
    }

    let next_modules = self
      .loaded_modules
      .checked_add(1)
      .ok_or_else(|| VmError::OutOfMemory)?;

    let remaining_total = self
      .options
      .max_module_graph_total_bytes
      .saturating_sub(self.loaded_bytes);
    let max_fetch = self
      .options
      .max_script_bytes
      .min(remaining_total)
      .saturating_add(1);

    // Fetch module scripts in CORS mode (`<script type="module">` / module imports).
    let mut req = FetchRequest::new(url, FetchDestination::ScriptCors);
    if let Some(referrer_url) = referrer_url {
      req = req.with_referrer_url(referrer_url);
    }
    if let Some(origin) = self.document_origin.as_ref() {
      req = req.with_client_origin(origin);
    }
    let res = self
      .fetcher
      .fetch_partial_with_request(req, max_fetch)
      .map_err(|err| self.throw_type_error(vm, scope, &format!("failed to fetch module {url}: {err}")))?;

    // If the fetcher followed redirects, prefer the final URL for:
    // - the module's `import.meta.url`, and
    // - the base URL used to resolve further module imports.
    //
    // This matches browser behavior where a module script's URL is the response's URL, not
    // necessarily the initially requested URL.
    let effective_url = res.final_url.as_deref().unwrap_or(url);
    if effective_url != url {
      if let Err(err) = self.options.check_module_specifier(effective_url) {
        return Err(self.throw_type_error(vm, scope, &err.to_string()));
      }
      if let Some(existing) = self.module_id_by_url.get(effective_url).copied() {
        self.module_id_by_url.insert(url.to_string(), existing);
        return Ok(existing);
      }
    }

    if let Err(err) = self
      .options
      .check_module_graph_modules(next_modules, effective_url)
    {
      return Err(self.throw_type_error(vm, scope, &err.to_string()));
    }

    ensure_http_success(&res, url)
      .and_then(|_| ensure_script_mime_sane(&res, url))
      .and_then(|_| {
        if cors_enforcement_enabled() {
          ensure_cors_allows_origin(self.document_origin.as_ref(), &res, url, CorsMode::Anonymous)
        } else {
          Ok(())
        }
      })
      .map_err(|err| self.throw_type_error(vm, scope, &format!("{err}")))?;

    // WHATWG HTML import maps: module scripts can be associated with Subresource Integrity metadata
    // via the import map `"integrity"` table. Enforce integrity metadata when present.
    if let Some(import_map_state) = self.import_map_state.as_ref() {
      let integrity_metadata = Url::parse(url)
        .ok()
        .map(|url| import_map_state.resolve_module_integrity_metadata(&url).to_string())
        .unwrap_or_default();
      if !integrity_metadata.is_empty() {
        if let Err(message) = crate::js::sri::verify_integrity(&res.bytes, &integrity_metadata) {
          return Err(self.throw_type_error(vm, scope, &format!(
            "SRI blocked module {url}: {message}"
          )));
        }
      }
    }

    let module_bytes = res.bytes.len();
    let context = format!("source=module specifier={effective_url}");
    if let Err(err) = self.options.check_script_source_bytes(module_bytes, &context) {
      return Err(self.throw_type_error(vm, scope, &err.to_string()));
    }

    let next_bytes = match self
      .options
      .check_module_graph_total_bytes(self.loaded_bytes, module_bytes, effective_url)
    {
      Ok(next) => next,
      Err(err) => return Err(self.throw_type_error(vm, scope, &err.to_string())),
    };
    self.loaded_modules = next_modules;
    self.loaded_bytes = next_bytes;

    let source_text = String::from_utf8(res.bytes).map_err(|err| {
      self.throw_type_error(vm, scope, &format!("module {url} response was not valid UTF-8: {err}"))
    })?;

    let effective_url_owned = effective_url.to_string();
    let source = Arc::new(vm_js::SourceText::new(effective_url_owned.clone(), source_text));
    let record = vm_js::SourceTextModuleRecord::parse_source_with_vm(vm, source)?;
    let id = modules.add_module(record);

    self.module_id_by_url.insert(url.to_string(), id);
    self.module_id_by_url.insert(effective_url_owned.clone(), id);
    self.module_url_by_id.insert(id, effective_url_owned.clone());
    self
      .module_base_url_by_id
      .insert(id, effective_url_owned);

    Ok(id)
  }

  fn resolve_module_specifier(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    specifier: &str,
    base_url: &str,
  ) -> std::result::Result<String, VmError> {
    if self.import_map_state.is_some() {
      let base_url_parsed = match Url::parse(base_url) {
        Ok(url) => url,
        Err(err) => {
          return Err(self.throw_type_error(
            vm,
            scope,
            &format!("invalid module base URL {base_url:?}: {err}"),
          ));
        }
      };

      let resolved = {
        let import_map_state = self
          .import_map_state
          .as_deref_mut()
          .expect("checked is_some above");
        resolve_module_specifier_with_import_maps(import_map_state, specifier, &base_url_parsed)
      };
      return match resolved {
        Ok(url) => Ok(url.to_string()),
        Err(err) => {
          let (kind, msg) = import_map_error_to_throw_kind_and_message(err);
          let thrown = match kind {
            ImportMapThrowKind::TypeError => self.throw_type_error(vm, scope, msg.as_str()),
            ImportMapThrowKind::SyntaxError => self.throw_syntax_error(vm, scope, msg.as_str()),
          };
          Err(thrown)
        }
      };
    }

    let allowed_relative =
      specifier.starts_with('/') || specifier.starts_with("./") || specifier.starts_with("../");
    if allowed_relative {
      return resolve_url(specifier, Some(base_url)).map_err(|err| {
        self.throw_type_error(vm, scope, &format!("failed to resolve module specifier {specifier:?}: {err}"))
      });
    }

    match resolve_url(specifier, None) {
      Ok(abs) => Ok(abs),
      Err(UrlResolveError::RelativeUrlWithoutBase) => Err(self.throw_type_error(
        vm,
        scope,
        &format!("unsupported bare module specifier {specifier:?} (no import map provided)"),
      )),
      Err(err) => Err(self.throw_type_error(
        vm,
        scope,
        &format!("failed to resolve module specifier {specifier:?}: {err}"),
      )),
    }
  }

  fn throw_type_error(&mut self, vm: &mut Vm, scope: &mut Scope<'_>, message: &str) -> VmError {
    let Some(intr) = vm.intrinsics() else {
      return VmError::Unimplemented(
        "module loading requires intrinsics (create a Realm first before evaluating modules)",
      );
    };
    match vm_js::new_type_error_object(scope, &intr, message) {
      Ok(value) => VmError::Throw(value),
      Err(err) => err,
    }
  }

  fn throw_syntax_error(&mut self, vm: &mut Vm, scope: &mut Scope<'_>, message: &str) -> VmError {
    let Some(intr) = vm.intrinsics() else {
      return VmError::Unimplemented(
        "module loading requires intrinsics (create a Realm first before evaluating modules)",
      );
    };
    match vm_js::new_syntax_error_object(scope, &intr, message) {
      Ok(value) => VmError::Throw(value),
      Err(err) => err,
    }
  }
}

impl<Host: WindowRealmHost + 'static> VmHostHooks for VmJsModuleHooks<'_, Host> {
  fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
    self.inner.as_any_mut()
  }

  fn host_enqueue_promise_job(&mut self, job: vm_js::Job, realm: Option<vm_js::RealmId>) {
    self.inner.host_enqueue_promise_job(job, realm);
  }

  fn host_exotic_get(
    &mut self,
    scope: &mut Scope<'_>,
    obj: vm_js::GcObject,
    key: PropertyKey,
    receiver: Value,
  ) -> std::result::Result<Option<Value>, VmError> {
    self.inner.host_exotic_get(scope, obj, key, receiver)
  }

  fn host_exotic_set(
    &mut self,
    scope: &mut Scope<'_>,
    obj: vm_js::GcObject,
    key: PropertyKey,
    value: Value,
    receiver: Value,
  ) -> std::result::Result<Option<bool>, VmError> {
    self.inner.host_exotic_set(scope, obj, key, value, receiver)
  }

  fn host_exotic_delete(
    &mut self,
    scope: &mut Scope<'_>,
    obj: vm_js::GcObject,
    key: PropertyKey,
  ) -> std::result::Result<Option<bool>, VmError> {
    self.inner.host_exotic_delete(scope, obj, key)
  }

  fn host_call_job_callback(
    &mut self,
    ctx: &mut dyn vm_js::VmJobContext,
    callback: &vm_js::JobCallback,
    this_argument: Value,
    arguments: &[Value],
  ) -> std::result::Result<Value, VmError> {
    self
      .inner
      .host_call_job_callback(ctx, callback, this_argument, arguments)
  }

  fn host_promise_rejection_tracker(
    &mut self,
    promise: vm_js::PromiseHandle,
    operation: vm_js::PromiseRejectionOperation,
  ) {
    self.inner.host_promise_rejection_tracker(promise, operation);
  }

  fn host_get_import_meta_properties(
    &mut self,
    _vm: &mut Vm,
    scope: &mut Scope<'_>,
    module: ModuleId,
  ) -> std::result::Result<Vec<ImportMetaProperty>, VmError> {
    let Some(url) = self.module_url_by_id.get(&module) else {
      return Ok(Vec::new());
    };

    let key_s = scope.alloc_string("url")?;
    scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);

    let url_s = scope.alloc_string(url.as_str())?;
    scope.push_root(Value::String(url_s))?;

    Ok(vec![ImportMetaProperty {
      key,
      value: Value::String(url_s),
    }])
  }

  fn host_load_imported_module(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    modules: &mut ModuleGraph,
    referrer: ModuleReferrer,
    module_request: ModuleRequest,
    host_defined: HostDefined,
    payload: ModuleLoadPayload,
  ) -> std::result::Result<(), VmError> {
    let _ = host_defined;

    if let Err(err) = self.options.check_module_specifier(&module_request.specifier) {
      let thrown = self.throw_type_error(vm, scope, &err.to_string());
      vm.finish_loading_imported_module(
        scope,
        modules,
        self,
        referrer,
        module_request,
        payload,
        Err(thrown),
      )?;
      return Ok(());
    }

    let base_url = match referrer {
      ModuleReferrer::Module(module) => self
        .module_base_url_by_id
        .get(&module)
        .cloned()
        .unwrap_or_else(|| self.document_url.to_string()),
      ModuleReferrer::Script(_) | ModuleReferrer::Realm(_) => self.document_url.to_string(),
    };

    let resolved_url = match self.resolve_module_specifier(vm, scope, &module_request.specifier, base_url.as_str()) {
      Ok(url) => url,
      Err(err) => {
        vm.finish_loading_imported_module(
          scope,
          modules,
          self,
          referrer,
          module_request,
          payload,
          Err(err),
        )?;
        return Ok(());
      }
    };

    let depth = match referrer {
      ModuleReferrer::Module(id) => {
        let parent_depth = self.module_depths.get(&id).copied().unwrap_or(0);
        match parent_depth.checked_add(1) {
          Some(next) => next,
          None => {
            let thrown = self.throw_type_error(vm, scope, "module graph depth overflowed usize");
            vm.finish_loading_imported_module(
              scope,
              modules,
              self,
              referrer,
              module_request,
              payload,
              Err(thrown),
            )?;
            return Ok(());
          }
        }
      }
      _ => 0,
    };
    if let Err(err) = self.options.check_module_graph_depth(depth, &resolved_url) {
      let thrown = self.throw_type_error(vm, scope, &err.to_string());
      vm.finish_loading_imported_module(
        scope,
        modules,
        self,
        referrer,
        module_request,
        payload,
        Err(thrown),
      )?;
      return Ok(());
    }

    let module_id = match self.get_or_fetch_module(vm, scope, modules, &resolved_url, Some(base_url.as_str())) {
      Ok(id) => id,
      Err(err) => {
        vm.finish_loading_imported_module(
          scope,
          modules,
          self,
          referrer,
          module_request,
          payload,
          Err(err),
        )?;
        return Ok(());
      }
    };

    self
      .module_depths
      .entry(module_id)
      .and_modify(|d| *d = (*d).min(depth))
      .or_insert(depth);

    vm.finish_loading_imported_module(
      scope,
      modules,
      self,
      referrer,
      module_request,
      payload,
      Ok(module_id),
    )?;
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::debug::runtime::{with_thread_runtime_toggles, RuntimeToggles};
  use crate::dom2;
  use crate::js::window_realm::{WindowRealm, WindowRealmConfig};
  use crate::js::import_maps::{
    create_import_map_parse_result, create_import_map_parse_result_with_limits, register_import_map, ImportMapLimits,
  };
  use crate::resource::FetchedResource;
  use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
  use base64::Engine;
  use selectors::context::QuirksMode;
  use sha2::{Digest, Sha256};
  use std::sync::Mutex;
  use std::sync::Arc;
  use vm_js::{
    Budget, PropertyDescriptor, PropertyKey, PropertyKind, Scope, Value, Vm, VmError, VmHost, VmHostHooks,
  };
  use webidl_vm_js::{host_from_hooks, WebIdlBindingsHost};

  #[derive(Clone, Debug)]
  struct RecordedRequest {
    url: String,
    destination: FetchDestination,
    referrer_url: Option<String>,
  }

  #[derive(Default)]
  struct MapFetcher {
    map: HashMap<String, FetchedResource>,
    calls: Mutex<Vec<RecordedRequest>>,
  }

  impl MapFetcher {
    fn new(map: HashMap<String, FetchedResource>) -> Self {
      Self {
        map,
        calls: Mutex::new(Vec::new()),
      }
    }

    fn calls(&self) -> Vec<String> {
      self.calls
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .iter()
        .map(|call| call.url.clone())
        .collect()
    }

    fn calls_detailed(&self) -> Vec<RecordedRequest> {
      self
        .calls
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
    }
  }

  impl ResourceFetcher for MapFetcher {
    fn fetch(&self, url: &str) -> crate::Result<FetchedResource> {
      self.fetch_with_request(FetchRequest::new(url, FetchDestination::Other))
    }

    fn fetch_with_request(&self, req: FetchRequest<'_>) -> crate::Result<FetchedResource> {
      self
        .calls
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(RecordedRequest {
          url: req.url.to_string(),
          destination: req.destination,
          referrer_url: req.referrer_url.map(|s| s.to_string()),
        });
      self
        .map
        .get(req.url)
        .cloned()
        .ok_or_else(|| Error::Other(format!("no fixture for url {url}", url = req.url)))
    }
  }

  fn get_global_prop(host: &mut crate::js::WindowHostState, name: &str) -> Value {
    let window = host.window_mut();
    let (_vm, realm, heap) = window.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let global = realm.global_object();
    scope.push_root(Value::Object(global)).expect("root global");
    let key_s = scope.alloc_string(name).expect("alloc name");
    scope.push_root(Value::String(key_s)).expect("root name");
    let key = PropertyKey::from_string(key_s);
    scope
      .heap()
      .object_get_own_data_property_value(global, &key)
      .expect("get prop")
      .unwrap_or(Value::Undefined)
  }

  fn get_global_prop_utf8(host: &mut crate::js::WindowHostState, name: &str) -> Option<String> {
    let value = get_global_prop(host, name);
    let window = host.window_mut();
    match value {
      Value::String(s) => Some(
        window
          .heap()
          .get_string(s)
          .expect("get string")
          .to_utf8_lossy(),
      ),
      _ => None,
    }
  }

  #[test]
  fn module_static_import_executes_and_import_meta_url_is_correct() -> Result<()> {
    let entry_url = "https://example.com/entry.js";
    let dep_url = "https://example.com/dep.js";

    let mut map = HashMap::<String, FetchedResource>::new();
    map.insert(
      entry_url.to_string(),
      FetchedResource::new(
        format!(
          "import {{ value }} from './dep.js';\n\
           globalThis.result = value;\n\
           globalThis.entryUrl = import.meta.url;\n"
        )
        .into_bytes(),
        Some("application/javascript".to_string()),
      ),
    );
    map.insert(
      dep_url.to_string(),
      FetchedResource::new(
        "export const value = 42;\n\
         globalThis.depUrl = import.meta.url;\n"
          .as_bytes()
          .to_vec(),
        Some("application/javascript".to_string()),
      ),
    );

    let fetcher = Arc::new(MapFetcher::new(map));
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = crate::js::WindowHostState::new_with_fetcher(dom, "https://example.com/index.html", fetcher.clone())?;
    let mut event_loop = EventLoop::<crate::js::WindowHostState>::new();

    host
      .window_mut()
      .vm_mut()
      .set_budget(Budget::unlimited(100));

    let mut loader = VmJsModuleLoader::new(fetcher.clone(), "https://example.com/index.html");
    loader.evaluate_module_url(&mut host, &mut event_loop, entry_url)?;

    assert_eq!(get_global_prop(&mut host, "result"), Value::Number(42.0));
    assert_eq!(
      get_global_prop_utf8(&mut host, "entryUrl").as_deref(),
      Some(entry_url)
    );
    assert_eq!(get_global_prop_utf8(&mut host, "depUrl").as_deref(), Some(dep_url));
    Ok(())
  }

  #[test]
  fn module_default_export_anonymous_class_with_semicolon_executes() -> Result<()> {
    let entry_url = "https://example.com/entry.js";
    let dep_url = "https://example.com/dep.js";

    let mut map = HashMap::<String, FetchedResource>::new();
    map.insert(
      entry_url.to_string(),
      FetchedResource::new(
        "import Cls from './dep.js';\n\
         globalThis.ok = typeof Cls === 'function';\n"
          .as_bytes()
          .to_vec(),
        Some("application/javascript".to_string()),
      ),
    );
    map.insert(
      dep_url.to_string(),
      FetchedResource::new(
        // Note the trailing semicolon. This is valid ESM syntax and should evaluate successfully.
        "export default class {};".as_bytes().to_vec(),
        Some("application/javascript".to_string()),
      ),
    );

    let fetcher = Arc::new(MapFetcher::new(map));
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host =
      crate::js::WindowHostState::new_with_fetcher(dom, "https://example.com/index.html", fetcher.clone())?;
    let mut event_loop = EventLoop::<crate::js::WindowHostState>::new();

    host.window_mut().vm_mut().set_budget(Budget::unlimited(100));

    let mut loader = VmJsModuleLoader::new(fetcher.clone(), "https://example.com/index.html");
    loader.evaluate_module_url(&mut host, &mut event_loop, entry_url)?;

    assert_eq!(get_global_prop(&mut host, "ok"), Value::Bool(true));
    Ok(())
  }

  #[test]
  fn module_loader_caches_modules_by_url() -> Result<()> {
    let entry_a = "https://example.com/a.js";
    let entry_b = "https://example.com/b.js";
    let dep_url = "https://example.com/dep.js";

    let mut map = HashMap::<String, FetchedResource>::new();
    map.insert(
      entry_a.to_string(),
      FetchedResource::new(
        "import { value } from './dep.js'; globalThis.a = value;".as_bytes().to_vec(),
        Some("application/javascript".to_string()),
      ),
    );
    map.insert(
      entry_b.to_string(),
      FetchedResource::new(
        "import { value } from './dep.js'; globalThis.b = value;".as_bytes().to_vec(),
        Some("application/javascript".to_string()),
      ),
    );
    map.insert(
      dep_url.to_string(),
      FetchedResource::new(
        "export const value = 1;".as_bytes().to_vec(),
        Some("application/javascript".to_string()),
      ),
    );

    let fetcher = Arc::new(MapFetcher::new(map));
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = crate::js::WindowHostState::new_with_fetcher(dom, "https://example.com/index.html", fetcher.clone())?;
    let mut event_loop = EventLoop::<crate::js::WindowHostState>::new();

    host
      .window_mut()
      .vm_mut()
      .set_budget(Budget::unlimited(100));

    let mut loader = VmJsModuleLoader::new(fetcher.clone(), "https://example.com/index.html");
    loader.evaluate_module_url(&mut host, &mut event_loop, entry_a)?;
    loader.evaluate_module_url(&mut host, &mut event_loop, entry_b)?;

    let calls = fetcher.calls();
    let dep_fetches = calls.iter().filter(|u| u.as_str() == dep_url).count();
    assert_eq!(
      dep_fetches, 1,
      "expected dep module to be fetched once, got calls: {calls:?}"
    );
    Ok(())
  }

  #[test]
  fn module_loader_enforces_module_graph_module_count() -> Result<()> {
    let document_url = "https://example.com/index.html";
    let entry_url = "https://example.com/entry.js";
    let dep_url = "https://example.com/dep.js";

    let mut map = HashMap::<String, FetchedResource>::new();
    map.insert(
      entry_url.to_string(),
      FetchedResource::new(
        "import './dep.js';".as_bytes().to_vec(),
        Some("application/javascript".to_string()),
      ),
    );
    map.insert(
      dep_url.to_string(),
      FetchedResource::new(
        "export const value = 1;".as_bytes().to_vec(),
        Some("application/javascript".to_string()),
      ),
    );

    let fetcher = Arc::new(MapFetcher::new(map));
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut js_options = crate::js::JsExecutionOptions::default();
    js_options.max_module_graph_modules = 1;
    let mut host =
      crate::js::WindowHostState::new_with_fetcher_and_options(dom, document_url, fetcher.clone(), js_options)?;
    let mut event_loop = EventLoop::<crate::js::WindowHostState>::new();
    host.window_mut().vm_mut().set_budget(Budget::unlimited(100));

    let mut loader = VmJsModuleLoader::new(fetcher, document_url);
    let err = loader
      .evaluate_module_url(&mut host, &mut event_loop, entry_url)
      .expect_err("expected module count budget to reject module graph");
    assert!(
      err.to_string().contains("max_module_graph_modules"),
      "unexpected error: {err}"
    );
    Ok(())
  }

  #[test]
  fn module_loader_enforces_module_graph_total_bytes() -> Result<()> {
    let document_url = "https://example.com/index.html";
    let entry_url = "https://example.com/entry.js";
    let dep_url = "https://example.com/dep.js";
    let entry_source = "import './dep.js';";
    let dep_source = "export const value = 1;";

    let total_limit = entry_source
      .as_bytes()
      .len()
      .saturating_add(dep_source.as_bytes().len())
      .saturating_sub(1);

    let mut map = HashMap::<String, FetchedResource>::new();
    map.insert(
      entry_url.to_string(),
      FetchedResource::new(entry_source.as_bytes().to_vec(), Some("application/javascript".to_string())),
    );
    map.insert(
      dep_url.to_string(),
      FetchedResource::new(dep_source.as_bytes().to_vec(), Some("application/javascript".to_string())),
    );

    let fetcher = Arc::new(MapFetcher::new(map));
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut js_options = crate::js::JsExecutionOptions::default();
    js_options.max_module_graph_total_bytes = total_limit;
    let mut host =
      crate::js::WindowHostState::new_with_fetcher_and_options(dom, document_url, fetcher.clone(), js_options)?;
    let mut event_loop = EventLoop::<crate::js::WindowHostState>::new();
    host.window_mut().vm_mut().set_budget(Budget::unlimited(100));

    let mut loader = VmJsModuleLoader::new(fetcher, document_url);
    let err = loader
      .evaluate_module_url(&mut host, &mut event_loop, entry_url)
      .expect_err("expected total bytes budget to reject module graph");
    assert!(
      err.to_string().contains("max_module_graph_total_bytes"),
      "unexpected error: {err}"
    );
    Ok(())
  }

  #[test]
  fn module_loader_enforces_module_graph_depth() -> Result<()> {
    let document_url = "https://example.com/index.html";
    let entry_url = "https://example.com/entry.js";
    let a_url = "https://example.com/a.js";
    let b_url = "https://example.com/b.js";

    let mut map = HashMap::<String, FetchedResource>::new();
    map.insert(
      entry_url.to_string(),
      FetchedResource::new("import './a.js';".as_bytes().to_vec(), Some("application/javascript".to_string())),
    );
    map.insert(
      a_url.to_string(),
      FetchedResource::new(
        "import './b.js'; export const x = 1;"
          .as_bytes()
          .to_vec(),
        Some("application/javascript".to_string()),
      ),
    );
    map.insert(
      b_url.to_string(),
      FetchedResource::new("export const y = 1;".as_bytes().to_vec(), Some("application/javascript".to_string())),
    );

    let fetcher = Arc::new(MapFetcher::new(map));
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut js_options = crate::js::JsExecutionOptions::default();
    js_options.max_module_graph_depth = 1;
    let mut host =
      crate::js::WindowHostState::new_with_fetcher_and_options(dom, document_url, fetcher.clone(), js_options)?;
    let mut event_loop = EventLoop::<crate::js::WindowHostState>::new();
    host.window_mut().vm_mut().set_budget(Budget::unlimited(100));

    let mut loader = VmJsModuleLoader::new(fetcher, document_url);
    let err = loader
      .evaluate_module_url(&mut host, &mut event_loop, entry_url)
      .expect_err("expected depth budget to reject module graph");
    assert!(
      err.to_string().contains("max_module_graph_depth"),
      "unexpected error: {err}"
    );
    Ok(())
  }

  #[test]
  fn module_loader_enforces_module_specifier_length() -> Result<()> {
    let document_url = "https://example.com/index.html";
    let entry_url = "https://example.com/entry.js";
    let long_specifier = format!("./{}.js", "a".repeat(40));
    let entry_source = format!("import '{long_specifier}';");

    let mut map = HashMap::<String, FetchedResource>::new();
    map.insert(
      entry_url.to_string(),
      FetchedResource::new(entry_source.into_bytes(), Some("application/javascript".to_string())),
    );

    let fetcher = Arc::new(MapFetcher::new(map));
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut js_options = crate::js::JsExecutionOptions::default();
    js_options.max_module_specifier_length = 32;
    let mut host =
      crate::js::WindowHostState::new_with_fetcher_and_options(dom, document_url, fetcher.clone(), js_options)?;
    let mut event_loop = EventLoop::<crate::js::WindowHostState>::new();
    host.window_mut().vm_mut().set_budget(Budget::unlimited(100));

    let mut loader = VmJsModuleLoader::new(fetcher, document_url);
    let err = loader
      .evaluate_module_url(&mut host, &mut event_loop, entry_url)
      .expect_err("expected module specifier length budget to reject module graph");
    assert!(
      err.to_string().contains("max_module_specifier_length"),
      "unexpected error: {err}"
    );
    Ok(())
  }

  #[test]
  fn module_loader_sets_referrer_url_for_module_imports() -> Result<()> {
    let document_url = "https://example.com/index.html";
    let entry_url = "https://example.com/entry.js";
    let dep_url = "https://example.com/dep.js";

    let mut map = HashMap::<String, FetchedResource>::new();
    map.insert(
      entry_url.to_string(),
      FetchedResource::new(
        "import x from './dep.js'; globalThis.result = x;"
          .as_bytes()
          .to_vec(),
        Some("application/javascript".to_string()),
      ),
    );
    map.insert(
      dep_url.to_string(),
      FetchedResource::new(
        "export default 1;".as_bytes().to_vec(),
        Some("application/javascript".to_string()),
      ),
    );

    let fetcher = Arc::new(MapFetcher::new(map));
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host =
      crate::js::WindowHostState::new_with_fetcher(dom, document_url, fetcher.clone())?;
    let mut event_loop = EventLoop::<crate::js::WindowHostState>::new();

    host.window_mut().vm_mut().set_budget(Budget::unlimited(100));

    let mut loader = VmJsModuleLoader::new(fetcher.clone(), document_url);
    loader.evaluate_module_url(&mut host, &mut event_loop, entry_url)?;

    let calls = fetcher.calls_detailed();
    let entry_call = calls.iter().find(|call| call.url == entry_url).expect("entry module fetched");
    assert_eq!(entry_call.destination, FetchDestination::ScriptCors);
    assert_eq!(entry_call.referrer_url.as_deref(), Some(document_url));

    let dep_call = calls.iter().find(|call| call.url == dep_url).expect("dep module fetched");
    assert_eq!(dep_call.destination, FetchDestination::ScriptCors);
    assert_eq!(dep_call.referrer_url.as_deref(), Some(entry_url));
    Ok(())
  }

  #[test]
  fn module_loader_uses_final_url_as_base_url_after_redirects() -> Result<()> {
    let document_url = "https://example.com/index.html";
    let entry_url = "https://example.com/entry.js";
    let redirected_url = "https://example.com/a/mod.js";
    let final_url = "https://example.com/b/mod.js";
    let dep_url = "https://example.com/b/dep.js";

    let mut map = HashMap::<String, FetchedResource>::new();
    map.insert(
      entry_url.to_string(),
      FetchedResource::new(
        "import x from './a/mod.js'; globalThis.result = x;"
          .as_bytes()
          .to_vec(),
        Some("application/javascript".to_string()),
      ),
    );

    let mut redirected_resource = FetchedResource::new(
      "import x from './dep.js'; export default x;"
        .as_bytes()
        .to_vec(),
      Some("application/javascript".to_string()),
    );
    redirected_resource.final_url = Some(final_url.to_string());
    map.insert(redirected_url.to_string(), redirected_resource);

    map.insert(
      dep_url.to_string(),
      FetchedResource::new(
        "export default 5;".as_bytes().to_vec(),
        Some("application/javascript".to_string()),
      ),
    );

    let fetcher = Arc::new(MapFetcher::new(map));
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = crate::js::WindowHostState::new_with_fetcher(dom, document_url, fetcher.clone())?;
    let mut event_loop = EventLoop::<crate::js::WindowHostState>::new();

    host.window_mut().vm_mut().set_budget(Budget::unlimited(100));

    let mut loader = VmJsModuleLoader::new(fetcher.clone(), document_url);
    loader.evaluate_module_url(&mut host, &mut event_loop, entry_url)?;

    assert_eq!(get_global_prop(&mut host, "result"), Value::Number(5.0));

    let calls = fetcher.calls();
    assert!(
      calls.iter().any(|u| u == dep_url),
      "expected {dep_url} fetch (resolved from final URL), got calls: {calls:?}"
    );
    assert!(
      calls.iter().all(|u| u != "https://example.com/a/dep.js"),
      "expected redirects to affect base URL resolution (no a/dep.js fetch), got calls: {calls:?}"
    );

    let calls_detailed = fetcher.calls_detailed();
    let dep_call = calls_detailed
      .iter()
      .find(|call| call.url == dep_url)
      .expect("dep module fetched");
    assert_eq!(dep_call.referrer_url.as_deref(), Some(final_url));

    let redirected_id = *loader
      .module_id_by_url
      .get(redirected_url)
      .expect("redirected module id");
    assert_eq!(
      loader.module_url_by_id.get(&redirected_id).map(String::as_str),
      Some(final_url)
    );
    assert_eq!(
      loader
        .module_base_url_by_id
        .get(&redirected_id)
        .map(String::as_str),
      Some(final_url)
    );
    Ok(())
  }

  #[test]
  fn module_loader_resolves_bare_specifiers_via_import_maps() -> Result<()> {
    let entry_url = "https://example.com/entry.js";
    let dep_url = "https://example.com/dep.js";

    let mut map = HashMap::<String, FetchedResource>::new();
    map.insert(
      entry_url.to_string(),
      FetchedResource::new(
        "import { value } from 'dep'; globalThis.result = value;"
          .as_bytes()
          .to_vec(),
        Some("application/javascript".to_string()),
      ),
    );
    map.insert(
      dep_url.to_string(),
      FetchedResource::new(
        "export const value = 7;".as_bytes().to_vec(),
        Some("application/javascript".to_string()),
      ),
    );

    let fetcher = Arc::new(MapFetcher::new(map));
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host =
      crate::js::WindowHostState::new_with_fetcher(dom, "https://example.com/index.html", fetcher.clone())?;
    let mut event_loop = EventLoop::<crate::js::WindowHostState>::new();

    host.window_mut().vm_mut().set_budget(Budget::unlimited(100));

    let mut import_map_state = ImportMapState::default();
    let base_url = Url::parse("https://example.com/index.html")
      .map_err(|err| Error::Other(format!("invalid test base URL: {err}")))?;
    let parse_result = create_import_map_parse_result(r#"{"imports":{"dep":"./dep.js"}}"#, &base_url);
    register_import_map(&mut import_map_state, parse_result)
      .map_err(|err| Error::Other(err.to_string()))?;

    let mut loader = VmJsModuleLoader::new(fetcher.clone(), "https://example.com/index.html");
    loader.evaluate_module_url_with_import_maps(
      &mut host,
      &mut event_loop,
      &mut import_map_state,
      entry_url,
    )?;

    assert_eq!(get_global_prop(&mut host, "result"), Value::Number(7.0));
    Ok(())
  }

  #[test]
  fn import_map_limit_exceeded_is_type_error_with_single_prefix() -> Result<()> {
    let base_url = Url::parse("https://example.com/index.html")
      .map_err(|err| Error::Other(format!("invalid test base URL: {err}")))?;

    // Force a deterministic limit exceeded error by allowing zero `"imports"` entries.
    let limits = ImportMapLimits {
      max_imports_entries: 0,
      ..ImportMapLimits::default()
    };
    let parse_result = create_import_map_parse_result_with_limits(
      r#"{"imports":{"dep":"./dep.js"}}"#,
      &base_url,
      &limits,
    );
    let err = parse_result
      .error_to_rethrow
      .expect("expected LimitExceeded error_to_rethrow");

    let (kind, msg) = import_map_error_to_throw_kind_and_message(err);
    assert_eq!(kind, ImportMapThrowKind::TypeError);
    assert!(
      msg.starts_with("import map limit exceeded:"),
      "expected prefix in message, got: {msg:?}"
    );
    assert!(
      !msg.contains("TypeError:"),
      "expected bare message (TypeError name should be on the thrown object), got: {msg:?}"
    );
    assert_eq!(
      msg.match_indices("import map limit exceeded:").count(),
      1,
      "expected single prefix, got: {msg:?}"
    );

    Ok(())
  }

  #[test]
  fn module_imports_enforce_import_map_integrity_metadata() -> Result<()> {
    let entry_url = "https://example.com/entry.js";
    let dep_url = "https://example.com/dep.js";
    let base_url = Url::parse("https://example.com/index.html")
      .map_err(|err| Error::Other(format!("invalid test base URL: {err}")))?;

    let dep_source = "export default 123;";
    let integrity = {
      let digest = Sha256::digest(dep_source.as_bytes());
      format!("sha256-{}", BASE64_STANDARD.encode(digest))
    };

    let importmap = serde_json::json!({
      "imports": { "dep": dep_url },
      "integrity": { dep_url: integrity },
    })
    .to_string();

    let mut map = HashMap::<String, FetchedResource>::new();
    map.insert(
      entry_url.to_string(),
      FetchedResource::new(
        "import x from 'dep'; globalThis.result = x;"
          .as_bytes()
          .to_vec(),
        Some("application/javascript".to_string()),
      ),
    );
    map.insert(
      dep_url.to_string(),
      FetchedResource::new(
        dep_source.as_bytes().to_vec(),
        Some("application/javascript".to_string()),
      ),
    );

    let fetcher = Arc::new(MapFetcher::new(map));
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = crate::js::WindowHostState::new_with_fetcher(dom, base_url.as_str(), fetcher.clone())?;
    let mut event_loop = EventLoop::<crate::js::WindowHostState>::new();
    host.window_mut().vm_mut().set_budget(Budget::unlimited(100));

    let parse_result = create_import_map_parse_result(importmap.as_str(), &base_url);
    let mut import_map_state = ImportMapState::default();
    register_import_map(&mut import_map_state, parse_result).map_err(|err| Error::Other(err.to_string()))?;

    let mut loader = VmJsModuleLoader::new(fetcher.clone(), base_url.as_str());
    loader.evaluate_module_url_with_import_maps(
      &mut host,
      &mut event_loop,
      &mut import_map_state,
      entry_url,
    )?;

    assert_eq!(get_global_prop(&mut host, "result"), Value::Number(123.0));
    Ok(())
  }

  #[derive(Default)]
  struct DispatchBindingsHost {
    calls: usize,
  }

  impl WebIdlBindingsHost for DispatchBindingsHost {
    fn call_operation(
      &mut self,
      _vm: &mut Vm,
      _scope: &mut Scope<'_>,
      _receiver: Option<Value>,
      _interface: &'static str,
      _operation: &'static str,
      _overload: usize,
      _args: &[Value],
    ) -> std::result::Result<Value, VmError> {
      self.calls += 1;
      Ok(Value::Undefined)
    }

    fn call_constructor(
      &mut self,
      _vm: &mut Vm,
      _scope: &mut Scope<'_>,
      _interface: &'static str,
      _overload: usize,
      _args: &[Value],
      _new_target: Value,
    ) -> std::result::Result<Value, VmError> {
      Err(VmError::Unimplemented(
        "constructor dispatch not implemented in DispatchBindingsHost",
      ))
    }
  }

  #[derive(Default)]
  struct DispatchHostCtx;

  struct DispatchHost {
    vm_host: DispatchHostCtx,
    bindings_host: DispatchBindingsHost,
    window: WindowRealm,
  }

  impl DispatchHost {
    fn new() -> Self {
      let window =
        WindowRealm::new(WindowRealmConfig::new("https://example.com/index.html")).unwrap();
      Self {
        vm_host: DispatchHostCtx,
        bindings_host: DispatchBindingsHost::default(),
        window,
      }
    }
  }

  impl WindowRealmHost for DispatchHost {
    fn vm_host_and_window_realm(&mut self) -> (&mut dyn VmHost, &mut WindowRealm) {
      (&mut self.vm_host, &mut self.window)
    }

    fn webidl_bindings_host(&mut self) -> Option<&mut dyn WebIdlBindingsHost> {
      Some(&mut self.bindings_host)
    }
  }

  fn native_webidl_dispatch(
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    _callee: vm_js::GcObject,
    _this: Value,
    _args: &[Value],
  ) -> std::result::Result<Value, VmError> {
    let host = host_from_hooks(hooks)?;
    let _ = host.call_operation(vm, scope, None, "TestInterface", "testOp", 0, &[])?;
    Ok(Value::Undefined)
  }

  fn install_dispatch_binding(
    vm: &mut Vm,
    heap: &mut vm_js::Heap,
    realm: &vm_js::Realm,
  ) -> std::result::Result<(), VmError> {
    let call_id = vm.register_native_call(native_webidl_dispatch)?;
    let mut scope = heap.scope();
    let global = realm.global_object();
    scope.push_root(Value::Object(global))?;

    let name_s = scope.alloc_string("__webidl_dispatch")?;
    scope.push_root(Value::String(name_s))?;
    let func = scope.alloc_native_function(call_id, None, name_s, 0)?;
    scope.push_root(Value::Object(func))?;

    let key_s = scope.alloc_string("__webidl_dispatch")?;
    scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);
    scope.define_property(
      global,
      key,
      PropertyDescriptor {
        enumerable: true,
        configurable: true,
        kind: PropertyKind::Data {
          value: Value::Object(func),
          writable: true,
        },
      },
    )?;

    Ok(())
  }

  #[test]
  fn module_imports_reject_mismatched_import_map_integrity_metadata() -> Result<()> {
    let entry_url = "https://example.com/entry.js";
    let dep_url = "https://example.com/dep.js";
    let base_url = Url::parse("https://example.com/index.html")
      .map_err(|err| Error::Other(format!("invalid test base URL: {err}")))?;

    let dep_source = "export default 123;";
    let integrity = {
      let digest = Sha256::digest(b"other");
      format!("sha256-{}", BASE64_STANDARD.encode(digest))
    };

    let importmap = serde_json::json!({
      "imports": { "dep": dep_url },
      "integrity": { dep_url: integrity },
    })
    .to_string();

    let mut map = HashMap::<String, FetchedResource>::new();
    map.insert(
      entry_url.to_string(),
      FetchedResource::new(
        "import x from 'dep'; globalThis.result = x;"
          .as_bytes()
          .to_vec(),
        Some("application/javascript".to_string()),
      ),
    );
    map.insert(
      dep_url.to_string(),
      FetchedResource::new(
        dep_source.as_bytes().to_vec(),
        Some("application/javascript".to_string()),
      ),
    );

    let fetcher = Arc::new(MapFetcher::new(map));
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = crate::js::WindowHostState::new_with_fetcher(dom, base_url.as_str(), fetcher.clone())?;
    let mut event_loop = EventLoop::<crate::js::WindowHostState>::new();
    host.window_mut().vm_mut().set_budget(Budget::unlimited(100));

    let parse_result = create_import_map_parse_result(importmap.as_str(), &base_url);
    let mut import_map_state = ImportMapState::default();
    register_import_map(&mut import_map_state, parse_result).map_err(|err| Error::Other(err.to_string()))?;

    let mut loader = VmJsModuleLoader::new(fetcher.clone(), base_url.as_str());
    let err = loader
      .evaluate_module_url_with_import_maps(
        &mut host,
        &mut event_loop,
        &mut import_map_state,
        entry_url,
      )
      .expect_err("expected mismatched import map integrity metadata to reject module");
    assert!(
      err.to_string().contains("SRI blocked module"),
      "unexpected error: {err}"
    );
    assert_eq!(
      get_global_prop(&mut host, "result"),
      Value::Undefined,
      "entry module should not have executed after SRI failure"
    );
    Ok(())
  }

  #[test]
  fn module_loader_blocks_cross_origin_modules_without_cors_headers() -> Result<()> {
    let toggles = Arc::new(RuntimeToggles::from_map(HashMap::from([(
      "FASTR_FETCH_ENFORCE_CORS".to_string(),
      "1".to_string(),
    )])));
    with_thread_runtime_toggles(toggles, || -> Result<()> {
      let entry_url = "https://example.com/entry.js";
      let dep_url = "https://cdn.example.net/dep.js";
      let document_url = "https://example.com/index.html";

      let mut map = HashMap::<String, FetchedResource>::new();
      map.insert(
        entry_url.to_string(),
        FetchedResource::new(
          format!("import x from '{dep_url}'; globalThis.result = x;").into_bytes(),
          Some("application/javascript".to_string()),
        ),
      );
      // Cross-origin module with no Access-Control-Allow-Origin should be blocked.
      map.insert(
        dep_url.to_string(),
        FetchedResource::new(
          "export default 1;".as_bytes().to_vec(),
          Some("application/javascript".to_string()),
        ),
      );

      let fetcher = Arc::new(MapFetcher::new(map));
      let dom = dom2::Document::new(QuirksMode::NoQuirks);
      let mut host = crate::js::WindowHostState::new_with_fetcher(dom, document_url, fetcher.clone())?;
      let mut event_loop = EventLoop::<crate::js::WindowHostState>::new();
      host.window_mut().vm_mut().set_budget(Budget::unlimited(100));

      let mut loader = VmJsModuleLoader::new(fetcher, document_url);
      let err = loader
        .evaluate_module_url(&mut host, &mut event_loop, entry_url)
        .expect_err("expected CORS failure");
      assert!(
        err.to_string().contains("CORS"),
        "expected CORS error, got {err}"
      );
      assert_eq!(
        get_global_prop(&mut host, "result"),
        Value::Undefined,
        "entry module should not execute when an import is blocked by CORS"
      );
      Ok(())
    })
  }

  #[test]
  fn module_loader_allows_cross_origin_modules_with_cors_headers() -> Result<()> {
    let toggles = Arc::new(RuntimeToggles::from_map(HashMap::from([(
      "FASTR_FETCH_ENFORCE_CORS".to_string(),
      "1".to_string(),
    )])));
    with_thread_runtime_toggles(toggles, || -> Result<()> {
      let entry_url = "https://example.com/entry.js";
      let dep_url = "https://cdn.example.net/dep.js";
      let document_url = "https://example.com/index.html";

      let mut map = HashMap::<String, FetchedResource>::new();
      map.insert(
        entry_url.to_string(),
        FetchedResource::new(
          format!("import x from '{dep_url}'; globalThis.result = x;").into_bytes(),
          Some("application/javascript".to_string()),
        ),
      );
      let mut dep = FetchedResource::new(
        "export default 1;".as_bytes().to_vec(),
        Some("application/javascript".to_string()),
      );
      dep.access_control_allow_origin = Some("*".to_string());
      map.insert(dep_url.to_string(), dep);

      let fetcher = Arc::new(MapFetcher::new(map));
      let dom = dom2::Document::new(QuirksMode::NoQuirks);
      let mut host = crate::js::WindowHostState::new_with_fetcher(dom, document_url, fetcher.clone())?;
      let mut event_loop = EventLoop::<crate::js::WindowHostState>::new();
      host.window_mut().vm_mut().set_budget(Budget::unlimited(100));

      let mut loader = VmJsModuleLoader::new(fetcher, document_url);
      loader.evaluate_module_url(&mut host, &mut event_loop, entry_url)?;

      assert_eq!(
        get_global_prop(&mut host, "result"),
        Value::Number(1.0),
        "expected cross-origin module with ACAO=* to load"
      );
      Ok(())
    })
  }

  #[test]
  fn module_evaluation_exposes_webidl_host_slot() -> Result<()> {
    let fetcher = Arc::new(MapFetcher::new(HashMap::new()));
    let mut host = DispatchHost::new();
    let mut event_loop = EventLoop::<DispatchHost>::new();

    {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_dispatch_binding(vm, heap, realm).unwrap();
    }

    let mut loader = VmJsModuleLoader::new(fetcher.clone(), "https://example.com/index.html");
    loader.evaluate_inline_module(
      &mut host,
      &mut event_loop,
      "https://example.com/entry.js",
      "https://example.com/index.html",
      "globalThis.__webidl_dispatch(); export const x = 1;",
    )?;

    assert_eq!(host.bindings_host.calls, 1);
    Ok(())
  }

  #[test]
  fn import_map_error_to_throw_kind_and_message_dedupes_limit_exceeded_prefix() {
    let (kind, msg) = import_map_error_to_throw_kind_and_message(ImportMapError::LimitExceeded(
      "import map limit exceeded: \"imports\" has too many entries (3 > max 2)".to_string(),
    ));
    assert_eq!(kind, ImportMapThrowKind::TypeError);
    assert_eq!(msg.match_indices("import map limit exceeded:").count(), 1);
    assert_eq!(msg, "import map limit exceeded: \"imports\" has too many entries (3 > max 2)");
  }
}
