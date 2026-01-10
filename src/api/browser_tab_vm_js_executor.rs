use crate::error::{Error, Result};
use crate::js::runtime::with_event_loop;
use crate::js::script_encoding::decode_classic_script_bytes;
use crate::js::time::update_time_bindings_clock;
use crate::js::vm_error_format;
use crate::js::window_realm::{WindowRealm, WindowRealmConfig};
use crate::js::window_timers::VmJsEventLoopHooks;
use crate::js::{
  install_window_animation_frame_bindings, install_window_fetch_bindings_with_guard,
  install_window_timers_bindings,
  import_maps::{
    create_import_map_parse_result, register_import_map, resolve_module_specifier, ImportMapError,
    ImportMapState, ImportMapWarningKind,
  },
  CurrentScriptStateHandle, JsExecutionOptions, LocationNavigationRequest, ScriptElementSpec,
  WindowFetchBindings, WindowFetchEnv,
};
use crate::resource::{
  ensure_cors_allows_origin, ensure_http_success, ensure_script_mime_sane, CorsMode, DocumentOrigin,
  FetchDestination, FetchRequest, ResourceFetcher,
};
use crate::style::media::{MediaContext, MediaType};
use crate::web::events::{Event, EventTargetId};
use encoding_rs::UTF_8;
use std::collections::HashMap;
use std::ptr::NonNull;
use std::sync::Arc;
use vm_js::{
  HostDefined, Job, JobCallback, ModuleGraph, ModuleId, ModuleLoadPayload, ModuleReferrer,
  ModuleRequest, PromiseHandle, PromiseRejectionOperation, PromiseState, PropertyKey, RealmId, Scope,
  SourceText, SourceTextModuleRecord, Value, Vm, VmError, VmHost, VmHostHooks, VmJobContext,
};

use super::BrowserDocumentDom2;
use super::{BrowserTabHost, BrowserTabJsExecutor, ConsoleMessageLevel, SharedRenderDiagnostics};

/// `vm-js`-backed [`BrowserTabJsExecutor`] that provides a minimal `window`/`document` environment.
///
/// Navigation creates a fresh JS realm for each document (matching browser semantics). The realm
/// receives a `dom_source_id` that resolves to a stable `NonNull<dom2::Document>` pointer for the
/// lifetime of the currently committed document.
pub struct VmJsBrowserTabExecutor {
  realm: Option<WindowRealm>,
  fetch_bindings: Option<WindowFetchBindings>,
  module_graph: Option<ModuleGraph>,
  module_map: HashMap<String, ModuleId>,
  import_map_state: ImportMapState,
  document_origin: Option<DocumentOrigin>,
  js_execution_options: JsExecutionOptions,
  inline_module_id_counter: u64,
  pending_navigation: Option<LocationNavigationRequest>,
  diagnostics: Option<SharedRenderDiagnostics>,
  /// Cached `vm-js` host context for Rust-driven event dispatch.
  ///
  /// `BrowserTabHost` owns the `BrowserDocumentDom2` for the lifetime of this executor, so we can
  /// store a stable pointer during navigation reset and reuse it when invoking JS event listeners
  /// from Rust (`BrowserTab::dispatch_click_event`, script load/error events, etc).
  vm_host: Option<NonNull<dyn VmHost>>,
}

impl VmJsBrowserTabExecutor {
  pub fn new() -> Self {
    Self {
      realm: None,
      fetch_bindings: None,
      module_graph: None,
      module_map: HashMap::new(),
      import_map_state: ImportMapState::new_empty(),
      document_origin: None,
      js_execution_options: JsExecutionOptions::default(),
      inline_module_id_counter: 0,
      pending_navigation: None,
      diagnostics: None,
      vm_host: None,
    }
  }

  fn record_js_exception(diag: &SharedRenderDiagnostics, realm: &mut WindowRealm, err: vm_js::VmError) {
    let (message, stack) = vm_error_format::vm_error_to_message_and_stack(realm.heap_mut(), err);
    diag.record_js_exception(message, stack);
  }

  fn next_inline_module_id(&mut self, spec: &ScriptElementSpec) -> String {
    if let Some(node_id) = spec.node_id {
      return format!("inline-module-{}", node_id.index());
    }
    let id = self.inline_module_id_counter;
    self.inline_module_id_counter = self.inline_module_id_counter.saturating_add(1);
    format!("inline-module-{id}")
  }
}

impl Default for VmJsBrowserTabExecutor {
  fn default() -> Self {
    Self::new()
  }
}

impl Drop for VmJsBrowserTabExecutor {
  fn drop(&mut self) {
    // Drop the realm first so any remaining JS globals stop referencing the DOM source id.
    self.fetch_bindings = None;
    self.realm = None;
  }
}
impl BrowserTabJsExecutor for VmJsBrowserTabExecutor {
  fn event_listener_invoker(
    &self,
  ) -> Option<Box<dyn crate::web::events::EventListenerInvoker>> {
    // SAFETY: The returned invoker is stored alongside this executor in `BrowserTabHost`, so the
    // pointer remains valid for the lifetime of the host. All access occurs on the host thread.
    let realm_ptr = (&self.realm as *const Option<WindowRealm>) as *mut Option<WindowRealm>;
    let vm_host_ptr = (&self.vm_host as *const Option<NonNull<dyn VmHost>>) as *mut _;
    Some(Box::new(
      crate::js::window_realm::WindowRealmDomEventListenerInvoker::<BrowserTabHost>::new(
        realm_ptr, vm_host_ptr,
      ),
    ))
  }

  fn on_document_base_url_updated(&mut self, base_url: Option<&str>) {
    let Some(realm) = self.realm.as_mut() else {
      return;
    };
    realm.set_base_url(base_url.map(|s| s.to_string()));
  }

  fn reset_for_navigation(
    &mut self,
    document_url: Option<&str>,
    document: &mut BrowserDocumentDom2,
    current_script: &CurrentScriptStateHandle,
    js_execution_options: JsExecutionOptions,
  ) -> Result<()> {
    self.pending_navigation = None;
    self.diagnostics = document.shared_diagnostics();
    self.vm_host = Some(NonNull::from(document as &mut dyn VmHost));

    // Tear down the previous realm so we don't leak rooted callbacks or global state across
    // navigations.
    self.fetch_bindings = None;
    self.realm = None;
    self.module_graph = None;
    self.module_map.clear();
    self.import_map_state = ImportMapState::new_empty();
    self.document_origin = document_url.and_then(crate::resource::origin_from_url);
    self.js_execution_options = js_execution_options;
    self.inline_module_id_counter = 0;

    let dom_source_id = document.ensure_dom_source_registered();

    let url = document_url.unwrap_or("about:blank");
    let options = document.options();
    let (viewport_w, viewport_h) = options.viewport.unwrap_or((1024, 768));
    let width = viewport_w as f32;
    let height = viewport_h as f32;
    let mut media = match options.media_type {
      MediaType::Print => MediaContext::print(width, height),
      _ => MediaContext::screen(width, height),
    };
    media.media_type = options.media_type;
    if let Some(dpr) = options.device_pixel_ratio {
      media = media.with_device_pixel_ratio(dpr);
    }

    let mut config = WindowRealmConfig::new(url)
      .with_media_context(media)
      .with_dom_source_id(dom_source_id)
      .with_current_script_state(current_script.clone());

    if let Some(diag) = self.diagnostics.clone() {
      let sink: crate::js::ConsoleSink = Arc::new(move |level, heap, args| {
        let message = vm_error_format::format_console_arguments_limited(heap, args);
        diag.record_console_message(level, message);
      });
      config.console_sink = Some(sink);
    }

    let fetcher = document.fetcher();
    let mut realm = WindowRealm::new_with_js_execution_options(config, js_execution_options)
      .map_err(|err| Error::Other(err.to_string()))?;
    realm.set_cookie_fetcher(Arc::clone(&fetcher));

    // Install EventLoop-backed Web APIs (`setTimeout`, `queueMicrotask`, `requestAnimationFrame`, `fetch`).
    let fetch_bindings = {
      let (vm, realm_ref, heap) = realm.vm_realm_and_heap_mut();
      install_window_timers_bindings::<BrowserTabHost>(vm, realm_ref, heap)
        .map_err(|err| Error::Other(err.to_string()))?;
      install_window_animation_frame_bindings::<BrowserTabHost>(vm, realm_ref, heap)
        .map_err(|err| Error::Other(err.to_string()))?;
      install_window_fetch_bindings_with_guard::<BrowserTabHost>(
        vm,
        realm_ref,
        heap,
        WindowFetchEnv::for_document(fetcher, Some(url.to_string())),
      )
      .map_err(|err| Error::Other(err.to_string()))?
    };

    self.fetch_bindings = Some(fetch_bindings);
    self.realm = Some(realm);
    self.module_graph = Some(ModuleGraph::new());
    Ok(())
  }

  fn execute_classic_script(
    &mut self,
    script_text: &str,
    spec: &ScriptElementSpec,
    current_script: Option<crate::dom2::NodeId>,
    document: &mut BrowserDocumentDom2,
    event_loop: &mut crate::js::EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    let Some(realm) = self.realm.as_mut() else {
      return Err(Error::Other(
        "VmJsBrowserTabExecutor has no active WindowRealm; did reset_for_navigation run?".to_string(),
      ));
    };
    let diagnostics = self.diagnostics.clone();
    let clock = event_loop.clock();
    let name: Arc<str> = if let Some(url) = spec.src.as_deref() {
      Arc::from(url)
    } else if let Some(node_id) = current_script {
      Arc::from(format!("<inline script node_id={}>", node_id.index()))
    } else {
      Arc::from("<inline>")
    };
    let source = Arc::new(SourceText::new(name, Arc::from(script_text)));

    let exec_result: Result<()> = with_event_loop(event_loop, || {
      update_time_bindings_clock(realm.heap(), clock.clone()).map_err(|err| Error::Other(err.to_string()))?;
      realm.set_base_url(spec.base_url.clone());
      realm.reset_interrupt();

      let mut hooks = VmJsEventLoopHooks::<BrowserTabHost>::new(document);
      let result = realm.exec_script_source_with_host_and_hooks(document, &mut hooks, source);

      if let Some(err) = hooks.finish(realm.heap_mut()) {
        return Err(err);
      }

      match result {
        Ok(_) => Ok(()),
        Err(err) => {
          if vm_error_format::vm_error_is_js_exception(&err) {
            if let Some(diag) = diagnostics.as_ref() {
              Self::record_js_exception(diag, realm, err);
            }
            Ok(())
          } else {
            Err(vm_error_format::vm_error_to_error(realm.heap_mut(), err))
          }
        }
      }
    });

    if let Some(req) = realm.take_pending_navigation_request() {
      // Clear the interrupt flag so the realm can be reused if the embedding chooses to keep
      // executing (e.g. navigation fails and scripts continue running).
      realm.reset_interrupt();
      self.pending_navigation = Some(req);
      return Ok(());
    }

    exec_result
  }

  fn execute_module_script(
    &mut self,
    script_text: &str,
    spec: &ScriptElementSpec,
    _current_script: Option<crate::dom2::NodeId>,
    document: &mut BrowserDocumentDom2,
    event_loop: &mut crate::js::EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    // HTML: module scripts are fetched in CORS mode by default. When the `crossorigin` attribute is
    // missing, the default state is "anonymous" (same-origin credentials for same-origin requests).
    let cors_mode = spec.crossorigin.unwrap_or(CorsMode::Anonymous);

    let entry_specifier = if spec.src_attr_present {
      // External module script: use the resolved `src` URL as the module's specifier.
      let Some(entry_url) = spec.src.as_deref().filter(|s| !s.is_empty()) else {
        // HTML: modules with `src` present but empty/invalid do not execute.
        return Ok(());
      };
      entry_url.to_string()
    } else {
      // Inline module script: synthesize an opaque URL using the document base URL at discovery so
      // relative imports resolve correctly.
      let base_url = spec.base_url.as_deref().unwrap_or("about:blank");
      let inline_id = self.next_inline_module_id(spec);
      synthesize_inline_module_url(base_url, &inline_id)
    };

    let Some(realm) = self.realm.as_mut() else {
      return Err(Error::Other(
        "VmJsBrowserTabExecutor has no active WindowRealm; did reset_for_navigation run?".to_string(),
      ));
    };
    let Some(module_graph) = self.module_graph.as_mut() else {
      return Err(Error::Other(
        "VmJsBrowserTabExecutor has no active ModuleGraph; did reset_for_navigation run?".to_string(),
      ));
    };

    let diagnostics = self.diagnostics.clone();
    let clock = event_loop.clock();
    let max_script_bytes = self.js_execution_options.max_script_bytes;
    let document_origin = self.document_origin.clone();
    let module_map = &mut self.module_map;
    let import_map_state = &mut self.import_map_state;

    let exec_result: Result<()> = with_event_loop(event_loop, || {
      update_time_bindings_clock(realm.heap(), clock.clone()).map_err(|err| Error::Other(err.to_string()))?;
      realm.set_base_url(spec.base_url.clone());
      realm.reset_interrupt();
      // Apply a fresh per-run VM budget (fuel + deadline) for module parsing/loading/evaluation.
      //
      // Module scripts are executed from event loop tasks (like classic scripts) and must be
      // interruptible. In particular, the VM's construction-time default deadline is relative to
      // realm creation, so we must reset the budget so deadlines are relative to "now".
      let budget = realm.vm_budget_now();
      let (vm, realm_ref, heap) = realm.vm_realm_and_heap_mut();
      let mut vm = vm.push_budget(budget);
      // Ensure immediate termination when no budget remains (deadline exceeded, interrupted, etc).
      vm.tick()
        .map_err(|err| vm_error_format::vm_error_to_error(heap, err))?;

      let entry_module = if let Some(id) = module_map.get(&entry_specifier).copied() {
        id
      } else {
        let source = Arc::new(SourceText::new(entry_specifier.clone(), script_text));
        let record = match SourceTextModuleRecord::parse_source_with_vm(&mut vm, source) {
          Ok(record) => record,
          Err(err) => {
            if vm_error_format::vm_error_is_js_exception(&err) {
              if let Some(diag) = diagnostics.as_ref() {
                let (message, stack) =
                  vm_error_format::vm_error_to_message_and_stack(heap, err);
                diag.record_js_exception(message, stack);
              }
              return Ok(());
            }
            return Err(vm_error_format::vm_error_to_error(heap, err));
          }
        };
        let id = module_graph.add_module(record);
        module_map.insert(entry_specifier.clone(), id);
        id
      };

      // Route Promise jobs (including module-loading promise reactions) through FastRender's
      // microtask queue.
      let mut hooks = ModuleLoaderHooks {
        inner: VmJsEventLoopHooks::<BrowserTabHost>::new(document),
        fetcher: document.fetcher(),
        max_script_bytes,
        module_map,
        import_map_state,
        document_origin,
        cors_mode,
      };

      let mut scope = heap.scope();

      let module_result: std::result::Result<(), VmError> = (|| {
        // Load all modules in the static import graph.
        let load_promise = vm_js::load_requested_modules(
          &mut vm,
          &mut scope,
          module_graph,
          &mut hooks,
          entry_module,
          HostDefined::default(),
        )?;
        scope.push_root(load_promise)?;
        ensure_promise_fulfilled(scope.heap(), load_promise)?;

        // Link + evaluate the entry module.
        let eval_promise = module_graph.evaluate(
          &mut vm,
          scope.heap_mut(),
          realm_ref.global_object(),
          realm_ref.id(),
          entry_module,
          document,
          &mut hooks,
        )?;
        scope.push_root(eval_promise)?;
        ensure_promise_fulfilled(scope.heap(), eval_promise)?;

        Ok(())
      })();

      if let Some(err) = hooks.finish(scope.heap_mut()) {
        return Err(err);
      }

      match module_result {
        Ok(()) => Ok(()),
        Err(err) => {
          if vm_error_format::vm_error_is_js_exception(&err) {
            if let Some(diag) = diagnostics.as_ref() {
              let (message, stack) =
                vm_error_format::vm_error_to_message_and_stack(scope.heap_mut(), err);
              diag.record_js_exception(message, stack);
            }
            Ok(())
          } else {
            Err(vm_error_format::vm_error_to_error(scope.heap_mut(), err))
          }
        }
      }
    });

    if let Some(req) = realm.take_pending_navigation_request() {
      realm.reset_interrupt();
      self.pending_navigation = Some(req);
      return Ok(());
    }

    exec_result
  }

  fn execute_import_map_script(
    &mut self,
    script_text: &str,
    spec: &ScriptElementSpec,
    _current_script: Option<crate::dom2::NodeId>,
    _document: &mut BrowserDocumentDom2,
    _event_loop: &mut crate::js::EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    let base_url = spec.base_url.as_deref().unwrap_or("about:blank");
    let base_url =
      url::Url::parse(base_url).unwrap_or_else(|_| url::Url::parse("about:blank").unwrap());

    let result = create_import_map_parse_result(script_text, &base_url);

    if let Some(diag) = self.diagnostics.as_ref() {
      for warning in &result.warnings {
        diag.record_console_message(
          ConsoleMessageLevel::Warn,
          format_import_map_warning(&warning.kind),
        );
      }
    }

    match register_import_map(&mut self.import_map_state, result) {
      Ok(()) => Ok(()),
      Err(err) => {
        if let Some(diag) = self.diagnostics.as_ref() {
          diag.record_js_exception(format_import_map_error(&err), None);
        }
        Ok(())
      }
    }
  }

  fn take_navigation_request(&mut self) -> Option<LocationNavigationRequest> {
    if let Some(req) = self.pending_navigation.take() {
      return Some(req);
    }
    self
      .realm
      .as_mut()
      .and_then(WindowRealm::take_pending_navigation_request)
  }

  fn dispatch_lifecycle_event(
    &mut self,
    target: EventTargetId,
    event: &Event,
    document: &mut BrowserDocumentDom2,
  ) -> Result<()> {
    let Some(realm) = self.realm.as_mut() else {
      return Ok(());
    };

    let diagnostics = self.diagnostics.clone();

    let dispatch_expr = match target {
      EventTargetId::Document => "document.dispatchEvent(e);",
      EventTargetId::Window => "dispatchEvent(e);",
      EventTargetId::Node(_) | EventTargetId::Opaque(_) => return Ok(()),
    };

    let type_lit = serde_json::to_string(&event.type_).unwrap_or_else(|_| "\"\"".to_string());
    let init_lit = serde_json::json!({
      "bubbles": event.bubbles,
      "cancelable": event.cancelable,
      "composed": event.composed,
    })
    .to_string();
    let source = format!(
      "(function(){{const e=new Event({type_lit},{init_lit});{dispatch_expr}}})();",
    );

    let clock = {
      let Some(event_loop) = crate::js::runtime::current_event_loop_mut::<BrowserTabHost>() else {
        return Err(Error::Other(
          "dispatch_lifecycle_event called without an active EventLoop".to_string(),
        ));
      };
      event_loop.clock()
    };
 
    update_time_bindings_clock(realm.heap(), clock).map_err(|err| Error::Other(err.to_string()))?;
    realm.reset_interrupt();
    let mut hooks = VmJsEventLoopHooks::<BrowserTabHost>::new(document);
    let source_text = Arc::new(SourceText::new("<lifecycle>", Arc::from(source)));
    let result = realm.exec_script_source_with_host_and_hooks(document, &mut hooks, source_text);
    if let Some(err) = hooks.finish(realm.heap_mut()) {
      return Err(err);
    }

    let exec_result = match result {
      Ok(_) => Ok(()),
      Err(err) => {
        if vm_error_format::vm_error_is_js_exception(&err) {
          if let Some(diag) = diagnostics.as_ref() {
            Self::record_js_exception(diag, realm, err);
          }
          Ok(())
        } else {
          Err(vm_error_format::vm_error_to_error(realm.heap_mut(), err))
        }
      }
    };

    if let Some(req) = realm.take_pending_navigation_request() {
      realm.reset_interrupt();
      self.pending_navigation = Some(req);
      return Ok(());
    }

    exec_result
  }

  fn window_realm_mut(&mut self) -> Option<&mut WindowRealm> {
    self.realm.as_mut()
  }
}

fn synthesize_inline_module_url(base_url: &str, inline_id: &str) -> String {
  match url::Url::parse(base_url) {
    Ok(mut url) => {
      url.set_fragment(Some(inline_id));
      url.to_string()
    }
    Err(_) => format!("about:blank#{inline_id}"),
  }
}

fn format_import_map_warning(kind: &ImportMapWarningKind) -> String {
  let message = match kind {
    ImportMapWarningKind::UnknownTopLevelKey { key } => format!("unknown top-level key {key:?}"),
    ImportMapWarningKind::EmptySpecifierKey => "empty specifier key".to_string(),
    ImportMapWarningKind::AddressNotString { specifier_key } => {
      format!("address for specifier key {specifier_key:?} was not a string")
    }
    ImportMapWarningKind::AddressInvalid { specifier_key, address } => {
      format!("invalid address {address:?} for specifier key {specifier_key:?}")
    }
    ImportMapWarningKind::TrailingSlashMismatch { specifier_key, address } => {
      format!("trailing-slash mismatch for {specifier_key:?} -> {address:?}")
    }
    ImportMapWarningKind::ScopePrefixNotParseable { prefix } => {
      format!("scope prefix {prefix:?} was not parseable")
    }
    ImportMapWarningKind::IntegrityKeyFailedToResolve { key } => {
      format!("integrity key {key:?} failed to resolve to a URL-like specifier")
    }
    ImportMapWarningKind::IntegrityValueNotString { key } => {
      format!("integrity value for {key:?} was not a string")
    }
  };

  format!("importmap: {message}")
}

fn format_import_map_error(err: &ImportMapError) -> String {
  match err {
    ImportMapError::Json(err) => format!("SyntaxError: {err}"),
    ImportMapError::TypeError(message) => format!("TypeError: {message}"),
    ImportMapError::LimitExceeded(message) => format!("TypeError: {message}"),
  }
}

fn ensure_promise_fulfilled(heap: &vm_js::Heap, promise: Value) -> std::result::Result<(), VmError> {
  let Value::Object(promise_obj) = promise else {
    return Err(VmError::InvariantViolation("expected a Promise object"));
  };
  match heap.promise_state(promise_obj)? {
    PromiseState::Pending => Err(VmError::Unimplemented(
      "asynchronous module loading/evaluation is not supported",
    )),
    PromiseState::Fulfilled => Ok(()),
    PromiseState::Rejected => {
      let reason = heap.promise_result(promise_obj)?.unwrap_or(Value::Undefined);
      Err(VmError::Throw(reason))
    }
  }
}

struct ModuleLoaderHooks<'a> {
  inner: VmJsEventLoopHooks<BrowserTabHost>,
  fetcher: Arc<dyn ResourceFetcher>,
  max_script_bytes: usize,
  module_map: &'a mut HashMap<String, ModuleId>,
  import_map_state: &'a mut ImportMapState,
  document_origin: Option<DocumentOrigin>,
  cors_mode: CorsMode,
}

impl ModuleLoaderHooks<'_> {
  fn finish(self, heap: &mut vm_js::Heap) -> Option<crate::error::Error> {
    self.inner.finish(heap)
  }

  fn referrer_url_for_resolution<'a>(
    modules: &'a ModuleGraph,
    referrer: ModuleReferrer,
  ) -> Option<&'a str> {
    match referrer {
      ModuleReferrer::Module(module_id) => modules
        .get_module(module_id)
        .and_then(|m| m.source.as_ref())
        .map(|s| s.name.as_ref()),
      _ => None,
    }
  }

  fn throw_type_error(vm: &Vm, scope: &mut Scope<'_>, message: &str) -> std::result::Result<Value, VmError> {
    let intr = vm
      .intrinsics()
      .ok_or(VmError::Unimplemented("module loading requires intrinsics"))?;
    vm_js::new_type_error_object(scope, &intr, message)
  }

  fn throw_syntax_error(vm: &Vm, scope: &mut Scope<'_>, message: &str) -> std::result::Result<Value, VmError> {
    let intr = vm
      .intrinsics()
      .ok_or(VmError::Unimplemented("module loading requires intrinsics"))?;
    vm_js::new_syntax_error_object(scope, &intr, message)
  }

  fn load_module_by_url(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    modules: &mut ModuleGraph,
    url: &str,
    referrer_url: Option<&str>,
  ) -> std::result::Result<ModuleId, VmError> {
    if let Some(id) = self.module_map.get(url).copied() {
      return Ok(id);
    }

    let max_fetch = self.max_script_bytes.saturating_add(1);
    let mut req = FetchRequest::new(url, FetchDestination::ScriptCors);
    if let Some(referrer_url) = referrer_url {
      req = req.with_referrer_url(referrer_url);
    }
    if let Some(origin) = self.document_origin.as_ref() {
      req = req.with_client_origin(origin);
    }
    req = req.with_credentials_mode(self.cors_mode.credentials_mode());
    let fetched =
      match self.fetcher.fetch_partial_with_request(req, max_fetch) {
        Ok(fetched) => fetched,
        Err(err) => {
          let message = format!("failed to fetch module {url}: {err}");
          let value = Self::throw_type_error(vm, scope, &message)?;
          return Err(VmError::Throw(value));
        }
      };
    if let Err(err) = ensure_http_success(&fetched, url) {
      let message = err.to_string();
      let value = Self::throw_type_error(vm, scope, &message)?;
      return Err(VmError::Throw(value));
    }
    if let Err(err) = ensure_script_mime_sane(&fetched, url) {
      let message = err.to_string();
      let value = Self::throw_type_error(vm, scope, &message)?;
      return Err(VmError::Throw(value));
    }
    if crate::resource::cors_enforcement_enabled() {
      if let Err(err) = ensure_cors_allows_origin(self.document_origin.as_ref(), &fetched, url, self.cors_mode) {
        let message = err.to_string();
        let value = Self::throw_type_error(vm, scope, &message)?;
        return Err(VmError::Throw(value));
      }
    }

    // HTML: module scripts can be associated with Subresource Integrity metadata via import maps
    // (`"integrity"` top-level key). Enforce the integrity metadata when present.
    //
    // Spec: "resolve a module integrity metadata" (WHATWG HTML import maps).
    let integrity_metadata = url::Url::parse(url)
      .ok()
      .map(|url| self.import_map_state.resolve_module_integrity_metadata(&url))
      .unwrap_or("");
    if !integrity_metadata.is_empty() {
      if let Err(message) = crate::js::sri::verify_integrity(&fetched.bytes, integrity_metadata) {
        let err_value =
          Self::throw_type_error(vm, scope, &format!("SRI blocked module {url}: {message}"))?;
        return Err(VmError::Throw(err_value));
      }
    }

    if fetched.bytes.len() > self.max_script_bytes {
      let message = format!(
        "module {url} is too large ({} bytes > max {})",
        fetched.bytes.len(),
        self.max_script_bytes
      );
      let value = Self::throw_type_error(vm, scope, &message)?;
      return Err(VmError::Throw(value));
    }

    let source_text = decode_classic_script_bytes(&fetched.bytes, fetched.content_type.as_deref(), UTF_8);
    let source = Arc::new(SourceText::new(url, source_text));
    let record = match SourceTextModuleRecord::parse_source_with_vm(vm, source) {
      Ok(record) => record,
      Err(VmError::Syntax(diags)) => {
        let message = vm_error_format::vm_error_to_string(scope.heap_mut(), VmError::Syntax(diags));
        let value = Self::throw_syntax_error(vm, scope, &message)?;
        return Err(VmError::Throw(value));
      }
      Err(other) => return Err(other),
    };

    let id = modules.add_module(record);
    self.module_map.insert(url.to_string(), id);
    Ok(id)
  }
}

impl VmHostHooks for ModuleLoaderHooks<'_> {
  fn host_enqueue_promise_job(&mut self, job: Job, realm: Option<RealmId>) {
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

  fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
    vm_js::VmHostHooks::as_any_mut(&mut self.inner)
  }

  fn host_make_job_callback(&mut self, callback: vm_js::GcObject) -> JobCallback {
    self.inner.host_make_job_callback(callback)
  }

  fn host_call_job_callback(
    &mut self,
    ctx: &mut dyn VmJobContext,
    callback: &JobCallback,
    this_argument: Value,
    arguments: &[Value],
  ) -> std::result::Result<Value, VmError> {
    self
      .inner
      .host_call_job_callback(ctx, callback, this_argument, arguments)
  }

  fn host_promise_rejection_tracker(&mut self, promise: PromiseHandle, operation: PromiseRejectionOperation) {
    self.inner.host_promise_rejection_tracker(promise, operation);
  }

  fn host_get_supported_import_attributes(&self) -> &'static [&'static str] {
    self.inner.host_get_supported_import_attributes()
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

    let base_url = Self::referrer_url_for_resolution(modules, referrer).unwrap_or("about:blank");
    let base_url =
      url::Url::parse(base_url).unwrap_or_else(|_| url::Url::parse("about:blank").unwrap());
    let resolved_url = match resolve_module_specifier(self.import_map_state, &module_request.specifier, &base_url) {
      Ok(url) => url.to_string(),
      Err(err) => {
        let message = match err {
          ImportMapError::TypeError(message) => message,
          ImportMapError::Json(err) => err.to_string(),
          ImportMapError::LimitExceeded(message) => message,
        };
        let err_value = Self::throw_type_error(vm, scope, &message)?;
        vm.finish_loading_imported_module(
          scope,
          modules,
          self,
          referrer,
          module_request,
          payload,
          Err(VmError::Throw(err_value)),
        )?;
        return Ok(());
      }
    };

    let referrer_url = Self::referrer_url_for_resolution(modules, referrer)
      .map(|s| s.to_string());
    let completion = match self.load_module_by_url(
      vm,
      scope,
      modules,
      &resolved_url,
      referrer_url.as_deref(),
    ) {
      Ok(id) => Ok(id),
      Err(err) => Err(err),
    };

    vm.finish_loading_imported_module(scope, modules, self, referrer, module_request, payload, completion)?;
    Ok(())
  }
}
