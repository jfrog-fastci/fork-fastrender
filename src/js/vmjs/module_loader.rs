use crate::error::{Error, Result};
use crate::js::import_maps::{
  resolve_module_specifier as resolve_module_specifier_with_import_maps, ImportMapError, ImportMapState,
};
use crate::js::runtime::with_event_loop;
use crate::js::url_resolve::{resolve_url, UrlResolveError};
use crate::js::vm_error_format;
use crate::js::window_realm::WindowRealmHost;
use crate::js::window_timers::VmJsEventLoopHooks;
use crate::js::EventLoop;
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

/// Per-document module loader and cache for `vm-js` modules.
///
/// This is used by tooling entry points like `fetch_and_render --js` to execute `<script type="module">`
/// via real ECMAScript module linking + evaluation.
pub struct VmJsModuleLoader {
  fetcher: Arc<dyn ResourceFetcher>,
  document_url: String,
  document_origin: Option<DocumentOrigin>,
  max_module_bytes: usize,
  module_graph: ModuleGraph,
  module_id_by_url: HashMap<String, ModuleId>,
  module_url_by_id: HashMap<ModuleId, String>,
  module_base_url_by_id: HashMap<ModuleId, String>,
}

impl VmJsModuleLoader {
  pub fn new(fetcher: Arc<dyn ResourceFetcher>, document_url: impl Into<String>, max_module_bytes: usize) -> Self {
    let document_url = document_url.into();
    let document_origin = origin_from_url(&document_url);
    Self {
      fetcher,
      document_url,
      document_origin,
      max_module_bytes,
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
      max_module_bytes,
      module_graph,
      module_id_by_url,
      module_url_by_id,
      module_base_url_by_id,
    } = self;
    let fetcher = Arc::clone(fetcher);
    let document_url = document_url.clone();
    let document_origin = document_origin.clone();
    let max_module_bytes = *max_module_bytes;

    with_event_loop(event_loop, move || {
      let mut hooks = VmJsModuleHooks::<Host> {
        inner: VmJsEventLoopHooks::<Host>::new_with_host(host),
        fetcher,
        document_url: document_url.as_str(),
        document_origin,
        max_module_bytes,
        module_id_by_url,
        module_url_by_id,
        module_base_url_by_id,
        import_map_state,
      };

      // Borrow-split: the VM needs `&mut ModuleGraph`, while module loading uses the hooks' maps.
      let (host_ctx, window_realm) = host.vm_host_and_window_realm();
      let budget = window_realm.vm_budget_now();
      let (vm, realm, heap) = window_realm.vm_realm_and_heap_mut();
      let mut vm = vm.push_budget(budget);
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
              EntryModule::ExternalUrl(url) => hooks.get_or_fetch_module(&mut vm, &mut scope, module_graph, url, url),
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
            match module_graph.evaluate(&mut vm, heap, global_object, realm_id, entry_id, host_ctx, &mut hooks) {
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

struct VmJsModuleHooks<'a, Host: WindowRealmHost + 'static> {
  inner: VmJsEventLoopHooks<Host>,
  fetcher: Arc<dyn ResourceFetcher>,
  document_url: &'a str,
  document_origin: Option<DocumentOrigin>,
  max_module_bytes: usize,
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
    if let Some(existing) = self.module_id_by_url.get(url).copied() {
      return Ok(existing);
    }

    if self.max_module_bytes != usize::MAX && source_text.as_bytes().len() > self.max_module_bytes {
      return Err(self.throw_type_error(vm, scope, &format!(
        "inline module {url} is too large ({} bytes > max {})",
        source_text.as_bytes().len(),
        self.max_module_bytes
      )));
    }

    let source = Arc::new(vm_js::SourceText::new(url.to_string(), source_text.to_string()));
    let record = vm_js::SourceTextModuleRecord::parse_source(source)?;
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
    base_url: &str,
  ) -> std::result::Result<ModuleId, VmError> {
    if let Some(existing) = self.module_id_by_url.get(url).copied() {
      return Ok(existing);
    }

    // Fetch module scripts in CORS mode (`<script type="module">` / module imports).
    let mut req = FetchRequest::new(url, FetchDestination::ScriptCors).with_referrer_url(self.document_url);
    if let Some(origin) = self.document_origin.as_ref() {
      req = req.with_client_origin(origin);
    }
    let res = if self.max_module_bytes == usize::MAX {
      self.fetcher.fetch_with_request(req)
    } else {
      self
        .fetcher
        .fetch_partial_with_request(req, self.max_module_bytes.saturating_add(1))
    }
    .map_err(|err| self.throw_type_error(vm, scope, &format!("failed to fetch module {url}: {err}")))?;

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

    if self.max_module_bytes != usize::MAX && res.bytes.len() > self.max_module_bytes {
      return Err(self.throw_type_error(vm, scope, &format!(
        "module {url} is too large ({} bytes > max {})",
        res.bytes.len(),
        self.max_module_bytes
      )));
    }

    let source_text = String::from_utf8(res.bytes).map_err(|err| {
      self.throw_type_error(vm, scope, &format!("module {url} response was not valid UTF-8: {err}"))
    })?;

    let source = Arc::new(vm_js::SourceText::new(url.to_string(), source_text));
    let record = vm_js::SourceTextModuleRecord::parse_source(source)?;
    let id = modules.add_module(record);

    self.module_id_by_url.insert(url.to_string(), id);
    self.module_url_by_id.insert(id, url.to_string());
    self
      .module_base_url_by_id
      .insert(id, base_url.to_string());

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
        Err(err) => match err {
          ImportMapError::TypeError(msg) => Err(self.throw_type_error(vm, scope, msg.as_str())),
          ImportMapError::Json(err) => {
            let msg = err.to_string();
            Err(self.throw_syntax_error(vm, scope, msg.as_str()))
          }
          ImportMapError::LimitExceeded(detail) => {
            let msg = format!("import map limit exceeded: {detail}");
            Err(self.throw_type_error(vm, scope, msg.as_str()))
          }
        },
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

    let module_id = match self.get_or_fetch_module(vm, scope, modules, &resolved_url, &resolved_url) {
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
  use crate::js::import_maps::{create_import_map_parse_result, register_import_map};
  use crate::resource::FetchedResource;
  use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
  use base64::Engine;
  use selectors::context::QuirksMode;
  use sha2::{Digest, Sha256};
  use std::sync::Mutex;
  use std::sync::Arc;
  use vm_js::{Budget, PropertyKey};

  #[derive(Default)]
  struct MapFetcher {
    map: HashMap<String, FetchedResource>,
    calls: Mutex<Vec<String>>,
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
        .push(req.url.to_string());
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

    let mut loader = VmJsModuleLoader::new(fetcher.clone(), "https://example.com/index.html", 128 * 1024);
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

    let mut loader =
      VmJsModuleLoader::new(fetcher.clone(), "https://example.com/index.html", 128 * 1024);
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

    let mut loader = VmJsModuleLoader::new(fetcher.clone(), "https://example.com/index.html", 128 * 1024);
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

    let mut loader = VmJsModuleLoader::new(fetcher.clone(), "https://example.com/index.html", 128 * 1024);
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

    let mut loader = VmJsModuleLoader::new(fetcher.clone(), base_url.as_str(), 128 * 1024);
    loader.evaluate_module_url_with_import_maps(
      &mut host,
      &mut event_loop,
      &mut import_map_state,
      entry_url,
    )?;

    assert_eq!(get_global_prop(&mut host, "result"), Value::Number(123.0));
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

    let mut loader = VmJsModuleLoader::new(fetcher.clone(), base_url.as_str(), 128 * 1024);
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

      let mut loader = VmJsModuleLoader::new(fetcher, document_url, 128 * 1024);
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

      let mut loader = VmJsModuleLoader::new(fetcher, document_url, 128 * 1024);
      loader.evaluate_module_url(&mut host, &mut event_loop, entry_url)?;

      assert_eq!(
        get_global_prop(&mut host, "result"),
        Value::Number(1.0),
        "expected cross-origin module with ACAO=* to load"
      );
      Ok(())
    })
  }
}
