use crate::error::{Error, Result};
use crate::js::time::update_time_bindings_clock;
use crate::js::vm_error_format;
use crate::js::window_realm::{WindowRealm, WindowRealmConfig};
use crate::js::window_timers::VmJsEventLoopHooks;
use crate::js::{
  install_window_animation_frame_bindings, install_window_fetch_bindings_with_guard,
  install_window_timers_bindings, install_window_xhr_bindings_with_guard,
  import_maps::{
    create_import_map_parse_result_with_limits, register_import_map_with_limits, ImportMapError,
    ImportMapWarningKind,
  },
  CurrentScriptStateHandle, JsExecutionOptions, LocationNavigationRequest, ModuleKey, ScriptElementSpec,
  WindowFetchBindings, WindowFetchEnv, WindowXhrBindings, WindowXhrEnv,
};
use crate::resource::{origin_from_url, CorsMode, ResourceFetcher};
use crate::style::media::{MediaContext, MediaType};
use crate::web::events::{Event, EventTargetId};
use std::ptr::NonNull;
use std::sync::Arc;
use vm_js::{
  GcObject, HostDefined, ModuleGraph, ModuleId, PromiseState, SourceText, Value, VmError, VmHost,
};
use webidl_vm_js::WebIdlBindingsHost;

use super::BrowserDocumentDom2;
use super::{BrowserTabHost, BrowserTabJsExecutor, ConsoleMessageLevel, SharedRenderDiagnostics};

#[derive(Debug, Clone, Copy)]
struct PendingModuleEvaluation {
  module: ModuleId,
  promise: GcObject,
}

/// `vm-js`-backed [`BrowserTabJsExecutor`] that provides a minimal `window`/`document` environment.
///
/// Navigation creates a fresh JS realm for each document (matching browser semantics). The realm
/// executes JS with the real `BrowserDocumentDom2` as the active `vm-js` host context, so DOM shims
/// can access the live `dom2::Document` by downcasting `&mut dyn vm_js::VmHost`.
pub struct VmJsBrowserTabExecutor {
  realm: Option<WindowRealm>,
  fetch_bindings: Option<WindowFetchBindings>,
  xhr_bindings: Option<WindowXhrBindings>,
  js_execution_options: JsExecutionOptions,
  inline_module_id_counter: u64,
  document_url: String,
  pending_module_evaluation: Option<PendingModuleEvaluation>,
  pending_navigation: Option<LocationNavigationRequest>,
  diagnostics: Option<SharedRenderDiagnostics>,
  /// Cached `vm-js` host context for Rust-driven event dispatch.
  ///
  /// `BrowserTabHost` owns the `BrowserDocumentDom2` for the lifetime of this executor, so we can
  /// store a stable pointer during navigation reset and reuse it when invoking JS event listeners
  /// from Rust (`BrowserTab::dispatch_click_event`, script load/error events, etc).
  vm_host: Option<NonNull<dyn VmHost>>,
  webidl_bindings_host: Option<NonNull<dyn WebIdlBindingsHost>>,
}

impl VmJsBrowserTabExecutor {
  pub fn new() -> Self {
    Self {
      realm: None,
      fetch_bindings: None,
      xhr_bindings: None,
      js_execution_options: JsExecutionOptions::default(),
      inline_module_id_counter: 0,
      document_url: "about:blank".to_string(),
      pending_module_evaluation: None,
      pending_navigation: None,
      diagnostics: None,
      vm_host: None,
      webidl_bindings_host: None,
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
    // Drop the realm first so any remaining JS globals stop referencing the document host.
    self.fetch_bindings = None;
    self.xhr_bindings = None;
    self.realm = None;
  }
}
impl BrowserTabJsExecutor for VmJsBrowserTabExecutor {
  fn set_webidl_bindings_host(&mut self, host: &mut dyn WebIdlBindingsHost) {
    self.webidl_bindings_host = Some(NonNull::from(host));
  }

  fn event_listener_invoker(
    &self,
  ) -> Option<Box<dyn crate::web::events::EventListenerInvoker>> {
    // SAFETY: The returned invoker is stored alongside this executor in `BrowserTabHost`, so the
    // pointer remains valid for the lifetime of the host. All access occurs on the host thread.
    let realm_ptr = (&self.realm as *const Option<WindowRealm>) as *mut Option<WindowRealm>;
    let vm_host_ptr = (&self.vm_host as *const Option<NonNull<dyn VmHost>>) as *mut _;
    let webidl_bindings_host_ptr =
      (&self.webidl_bindings_host as *const Option<NonNull<dyn WebIdlBindingsHost>>) as *mut _;
    Some(Box::new(
      crate::js::window_realm::WindowRealmDomEventListenerInvoker::<BrowserTabHost>::new(
        realm_ptr, vm_host_ptr, webidl_bindings_host_ptr,
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
    self.pending_module_evaluation = None;
    self.diagnostics = document.shared_diagnostics();
    // `document.currentScript` is read from the embedder `VmHost` (the document) by vm-js native
    // handlers. Share the stable per-tab `CurrentScriptStateHandle` so JS observes the same
    // bookkeeping mutated by `BrowserTabHost`'s orchestrator.
    document.set_current_script_handle(current_script.clone());
    self.vm_host = Some(NonNull::from(document as &mut dyn VmHost));
    // Tear down the previous realm so we don't leak rooted callbacks or global state across
    // navigations.
    self.fetch_bindings = None;
    self.xhr_bindings = None;
    self.realm = None;
    self.js_execution_options = js_execution_options;
    self.inline_module_id_counter = 0;

    let url = document_url.unwrap_or("about:blank");
    self.document_url = url.to_string();
    let options = document.options();
    let (viewport_w, viewport_h) = options.viewport.unwrap_or((1024, 768));
    let width = viewport_w as f32;
    let height = viewport_h as f32;
    let mut media = match options.media_type {
      MediaType::Print => MediaContext::print(width, height),
      _ => super::headless_chrome_screen_media_context(width, height),
    };
    media.media_type = options.media_type;
    if let Some(dpr) = options.device_pixel_ratio {
      media = media.with_device_pixel_ratio(dpr);
    }

    let mut config = WindowRealmConfig::new(url)
      .with_media_context(media)
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
    if js_execution_options.supports_module_scripts {
      let document_origin = origin_from_url(url);
      realm
        .enable_module_loader(Arc::clone(&fetcher), document_origin)
        .map_err(|err| Error::Other(err.to_string()))?;
    }

    // Install EventLoop-backed Web APIs (`setTimeout`, `queueMicrotask`, `requestAnimationFrame`, `fetch`).
    let (fetch_bindings, xhr_bindings) = {
      let (vm, realm_ref, heap) = realm.vm_realm_and_heap_mut();
      install_window_timers_bindings::<BrowserTabHost>(vm, realm_ref, heap)
        .map_err(|err| Error::Other(err.to_string()))?;
      install_window_animation_frame_bindings::<BrowserTabHost>(vm, realm_ref, heap)
        .map_err(|err| Error::Other(err.to_string()))?;
      let fetch_bindings = install_window_fetch_bindings_with_guard::<BrowserTabHost>(
        vm,
        realm_ref,
        heap,
        WindowFetchEnv::for_document(Arc::clone(&fetcher), Some(url.to_string())),
      )
      .map_err(|err| Error::Other(err.to_string()))?;

      let xhr_bindings = install_window_xhr_bindings_with_guard::<BrowserTabHost>(
        vm,
        realm_ref,
        heap,
        WindowXhrEnv::for_document(Arc::clone(&fetcher), Some(url.to_string())),
      )
      .map_err(|err| Error::Other(err.to_string()))?;

      (fetch_bindings, xhr_bindings)
    };

    self.fetch_bindings = Some(fetch_bindings);
    self.xhr_bindings = Some(xhr_bindings);
    self.realm = Some(realm);
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
    let webidl_bindings_host = self.webidl_bindings_host;
    let name: Arc<str> = if let Some(url) = spec.src.as_deref() {
      Arc::from(url)
    } else if let Some(node_id) = current_script {
      Arc::from(format!("<inline script node_id={}>", node_id.index()))
    } else {
      Arc::from("<inline>")
    };
    let source = Arc::new(SourceText::new(name, Arc::from(script_text)));
    let js_execution_options = self.js_execution_options;
    let module_loader = realm.module_loader_handle();

    update_time_bindings_clock(realm.heap(), clock.clone())
      .map_err(|err| Error::Other(err.to_string()))?;
    realm.set_base_url(spec.base_url.clone());
    realm.reset_interrupt();

    // Classic scripts can evaluate dynamic `import()` expressions. If module loading is enabled for
    // this realm, ensure the per-realm loader uses classic-script defaults.
    if realm.vm().module_graph_ptr().is_some() {
      let mut loader = module_loader.borrow_mut();
      loader.set_fetcher(document.fetcher());
      loader.set_cors_mode(CorsMode::Anonymous);
      loader.set_js_execution_options(js_execution_options);
    }
    let webidl_bindings_host = match webidl_bindings_host {
      Some(mut host_ptr) => Some(unsafe { host_ptr.as_mut() }),
      None => None,
    };
    let mut hooks = VmJsEventLoopHooks::<BrowserTabHost>::new_with_vm_host_and_window_realm(
      document,
      realm,
      webidl_bindings_host,
    );
    hooks.set_event_loop(event_loop);
    let result = realm.exec_script_source_with_host_and_hooks(document, &mut hooks, source);

    if let Some(err) = hooks.finish(realm.heap_mut()) {
      return Err(err);
    }

    let exec_result: Result<()> = match result {
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
      // Clear the interrupt flag so the realm can be reused if the embedding chooses to keep
      // executing (e.g. navigation fails and scripts continue running).
      realm.reset_interrupt();
      self.pending_navigation = Some(req);
      return Ok(());
    }

    exec_result
  }

  fn supports_module_graph_fetch(&self) -> bool {
    true
  }

  fn fetch_module_graph(
    &mut self,
    spec: &ScriptElementSpec,
    fetcher: Arc<dyn ResourceFetcher>,
    document: &mut BrowserDocumentDom2,
    event_loop: &mut crate::js::EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    let entry_specifier = if spec.src_attr_present {
      let Some(entry_url) = spec.src.as_deref().filter(|s| !s.is_empty()) else {
        // HTML: modules with `src` present but empty/invalid do not execute.
        return Ok(());
      };
      entry_url.to_string()
    } else {
      let base_url = spec.base_url.as_deref().unwrap_or("about:blank");
      let inline_id = self.next_inline_module_id(spec);
      synthesize_inline_module_url(base_url, &inline_id)
    };

    let Some(realm) = self.realm.as_mut() else {
      return Err(Error::Other(
        "VmJsBrowserTabExecutor has no active WindowRealm; did reset_for_navigation run?".to_string(),
      ));
    };

    // HTML: module scripts are fetched in CORS mode by default. When the `crossorigin` attribute is
    // missing, the default state is "anonymous" (same-origin credentials for same-origin requests).
    let cors_mode = spec.crossorigin.unwrap_or(CorsMode::Anonymous);

    let diagnostics = self.diagnostics.clone();
    let clock = event_loop.clock();
    let js_execution_options = self.js_execution_options;
    let webidl_bindings_host = self.webidl_bindings_host;
    let entry_key = ModuleKey {
      url: entry_specifier.clone(),
      attributes: Vec::new(),
    };
    let module_loader = realm.module_loader_handle();

    update_time_bindings_clock(realm.heap(), clock.clone()).map_err(|err| Error::Other(err.to_string()))?;
    realm.set_base_url(spec.base_url.clone());
    realm.reset_interrupt();

    let exec_result: Result<()> = (|| {

      {
        let mut loader = module_loader.borrow_mut();
        loader.set_fetcher(Arc::clone(&fetcher));
        loader.set_cors_mode(cors_mode);
        loader.set_js_execution_options(js_execution_options);
      }

      // Route Promise jobs (including module-loading promise reactions) through FastRender's
      // microtask queue.
      let webidl_bindings_host = match webidl_bindings_host {
        Some(mut host_ptr) => Some(unsafe { host_ptr.as_mut() }),
        None => None,
      };
      let mut hooks = VmJsEventLoopHooks::<BrowserTabHost>::new_with_vm_host_and_window_realm(
        document,
        realm,
        webidl_bindings_host,
      );
      hooks.set_event_loop(event_loop);

      // Apply a fresh per-run VM budget so module graph loading is interruptible.
      let budget = realm.vm_budget_now();
      let (vm, _realm_ref, heap) = realm.vm_realm_and_heap_mut();
      let mut vm = vm.push_budget(budget);
      vm.tick()
        .map_err(|err| vm_error_format::vm_error_to_error(heap, err))?;

      let Some(modules_ptr) = vm.module_graph_ptr() else {
        return Err(Error::Other(
          "module scripts requested but module loading is not enabled for this realm".to_string(),
        ));
      };
      let module_graph = unsafe { &mut *(modules_ptr as *mut ModuleGraph) };

      let mut scope = heap.scope();

      let vm_error_to_host_error = |scope: &mut vm_js::Scope<'_>, err: VmError| -> Error {
        if vm_error_format::vm_error_is_js_exception(&err) {
          let (message, stack) =
            vm_error_format::vm_error_to_message_and_stack(scope.heap_mut(), err);
          if let Some(diag) = diagnostics.as_ref() {
            diag.record_js_exception(message.clone(), stack.clone());
          }
          if let Some(stack) = stack {
            Error::Other(format!("{message}\n{stack}"))
          } else {
            Error::Other(message)
          }
        } else {
          vm_error_format::vm_error_to_error(scope.heap_mut(), err)
        }
      };

      let entry_module: std::result::Result<ModuleId, VmError> = {
        let mut loader = module_loader.borrow_mut();
        if spec.src_attr_present {
          loader.get_or_fetch_module(module_graph, entry_key.clone())
        } else {
          loader.get_or_parse_inline_module(module_graph, entry_key.clone(), spec.inline_text.as_str())
        }
      };

      let entry_module = match entry_module {
        Ok(id) => id,
        Err(err) => return Err(vm_error_to_host_error(&mut scope, err)),
      };

      let load_promise = match vm_js::load_requested_modules(
        &mut vm,
        &mut scope,
        module_graph,
        &mut hooks,
        entry_module,
        HostDefined::default(),
      ) {
        Ok(p) => p,
        Err(err) => return Err(vm_error_to_host_error(&mut scope, err)),
      };
      if let Err(err) = ensure_promise_fulfilled(scope.heap(), load_promise) {
        return Err(vm_error_to_host_error(&mut scope, err));
      }

      if let Some(err) = hooks.finish(scope.heap_mut()) {
        return Err(err);
      }

      Ok(())
    })();

    if let Some(req) = realm.take_pending_navigation_request() {
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

    let diagnostics = self.diagnostics.clone();
    let clock = event_loop.clock();
    let webidl_bindings_host = self.webidl_bindings_host;
    let js_execution_options = self.js_execution_options;
    let entry_key = ModuleKey {
      url: entry_specifier.clone(),
      attributes: Vec::new(),
    };

    let Some(realm) = self.realm.as_mut() else {
      return Err(Error::Other(
        "VmJsBrowserTabExecutor has no active WindowRealm; did reset_for_navigation run?".to_string(),
      ));
    };
    let module_loader = realm.module_loader_handle();

    update_time_bindings_clock(realm.heap(), clock.clone()).map_err(|err| Error::Other(err.to_string()))?;
    realm.set_base_url(spec.base_url.clone());
    {
      let mut loader = module_loader.borrow_mut();
      loader.set_fetcher(document.fetcher());
      loader.set_cors_mode(cors_mode);
      loader.set_js_execution_options(js_execution_options);
    }
    realm.reset_interrupt();

    // Route Promise jobs (including module-loading promise reactions) through FastRender's
    // microtask queue.
    let webidl_bindings_host = match webidl_bindings_host {
      Some(mut host_ptr) => Some(unsafe { host_ptr.as_mut() }),
      None => None,
    };
    let inner_hooks = VmJsEventLoopHooks::<BrowserTabHost>::new_with_vm_host_and_window_realm(
      document,
      realm,
      webidl_bindings_host,
    );

    let exec_result: Result<Option<PendingModuleEvaluation>> = (|| {
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

      let Some(modules_ptr) = vm.module_graph_ptr() else {
        return Err(Error::Other(
          "module scripts requested but module loading is not enabled for this realm".to_string(),
        ));
      };
      let module_graph = unsafe { &mut *(modules_ptr as *mut ModuleGraph) };

      let entry_module = {
        let mut loader = module_loader.borrow_mut();
        match loader.get_or_parse_inline_module(module_graph, entry_key.clone(), script_text) {
          Ok(id) => id,
          Err(err) => {
            if vm_error_format::vm_error_is_js_exception(&err) {
              if let Some(diag) = diagnostics.as_ref() {
                let (message, stack) = vm_error_format::vm_error_to_message_and_stack(heap, err);
                diag.record_js_exception(message, stack);
              }
              return Ok(None);
            }
            return Err(vm_error_format::vm_error_to_error(heap, err));
          }
        }
      };

      let mut hooks = inner_hooks;
      hooks.set_event_loop(event_loop);

      let mut scope = heap.scope();

      let module_result: std::result::Result<Option<PendingModuleEvaluation>, VmError> = (|| {
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
        //
        // `vm-js` module evaluation returns a Promise to model top-level await semantics. We allow
        // that promise to be transiently pending as long as it settles once the host drains
        // microtasks (see `after_microtask_checkpoint`).
        let eval_promise = module_graph.evaluate_with_scope(
          &mut vm,
          &mut scope,
          realm_ref.global_object(),
          realm_ref.id(),
          entry_module,
          document,
          &mut hooks,
        )?;
        scope.push_root(eval_promise)?;
        let Value::Object(promise_obj) = eval_promise else {
          return Err(VmError::InvariantViolation("expected a Promise object"));
        };
        match scope.heap().promise_state(promise_obj)? {
          PromiseState::Pending => Ok(Some(PendingModuleEvaluation {
            module: entry_module,
            promise: promise_obj,
          })),
          _ => {
            ensure_promise_fulfilled(scope.heap(), eval_promise)?;
            Ok(None)
          }
        }
      })();

      if let Some(err) = hooks.finish(scope.heap_mut()) {
        return Err(err);
      }

      match module_result {
        Ok(pending) => Ok(pending),
        Err(err) => {
          if vm_error_format::vm_error_is_js_exception(&err) {
            if let Some(diag) = diagnostics.as_ref() {
              let (message, stack) =
                vm_error_format::vm_error_to_message_and_stack(scope.heap_mut(), err);
              diag.record_js_exception(message, stack);
            }
            Ok(None)
          } else {
            Err(vm_error_format::vm_error_to_error(scope.heap_mut(), err))
          }
        }
      }
    })();

    if let Some(realm) = self.realm.as_mut() {
      if let Some(req) = realm.take_pending_navigation_request() {
        realm.reset_interrupt();
        self.pending_navigation = Some(req);
        return Ok(());
      }
    }

    match exec_result {
      Ok(pending) => {
        self.pending_module_evaluation = pending;
        Ok(())
      }
      Err(err) => Err(err),
    }
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

    let limits = &self.js_execution_options.import_map_limits;
    let result = create_import_map_parse_result_with_limits(script_text, &base_url, limits);

    if let Some(diag) = self.diagnostics.as_ref() {
      for warning in &result.warnings {
        diag.record_console_message(
          ConsoleMessageLevel::Warn,
          format_import_map_warning(&warning.kind),
        );
      }
    }

    let Some(realm) = self.realm.as_mut() else {
      return Ok(());
    };
    // Import maps are only meaningful when module scripts are enabled for the realm.
    if realm.vm().module_graph_ptr().is_none() {
      return Ok(());
    }

    let module_loader = realm.module_loader_handle();
    let mut module_loader = module_loader.borrow_mut();
    let import_map_state = module_loader.import_map_state_mut();

    match register_import_map_with_limits(import_map_state, result, limits) {
      Ok(()) => Ok(()),
      Err(err) => {
        if let Some(diag) = self.diagnostics.as_ref() {
          diag.record_js_exception(format_import_map_error(&err), None);
        }
        Ok(())
      }
    }
  }

  fn after_microtask_checkpoint(
    &mut self,
    _document: &mut BrowserDocumentDom2,
    _event_loop: &mut crate::js::EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    let Some(pending) = self.pending_module_evaluation.take() else {
      return Ok(());
    };
    let Some(realm) = self.realm.as_mut() else {
      return Ok(());
    };

    let diagnostics = self.diagnostics.clone();
    let (vm, _realm_ref, heap) = realm.vm_realm_and_heap_mut();

    let Some(modules_ptr) = vm.module_graph_ptr() else {
      // Module loading disabled; nothing to do.
      return Ok(());
    };
    let module_graph = unsafe { &mut *(modules_ptr as *mut ModuleGraph) };

    let state = match heap.promise_state(pending.promise) {
      Ok(state) => state,
      Err(err) => return Err(vm_error_format::vm_error_to_error(heap, err)),
    };

    match state {
      PromiseState::Fulfilled => Ok(()),
      PromiseState::Rejected => {
        let reason = match heap.promise_result(pending.promise) {
          Ok(reason) => reason.unwrap_or(Value::Undefined),
          Err(err) => return Err(vm_error_format::vm_error_to_error(heap, err)),
        };
        if let Some(diag) = diagnostics.as_ref() {
          let (message, stack) =
            vm_error_format::vm_error_to_message_and_stack(heap, VmError::Throw(reason));
          diag.record_js_exception(message, stack);
        }
        Ok(())
      }
      PromiseState::Pending => {
        // The evaluation promise did not settle after draining microtasks, meaning it requires real
        // async work (timers/tasks/network). Abort the in-progress TLA state so we do not leak
        // persistent roots in `vm-js`.
        module_graph.abort_tla_evaluation(vm, heap, pending.module);
        if let Some(diag) = diagnostics.as_ref() {
          diag.record_js_exception(
            "asynchronous module loading/evaluation is not supported".to_string(),
            None,
          );
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
    event_loop: &mut crate::js::EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    let Some(realm) = self.realm.as_mut() else {
      return Ok(());
    };

    let diagnostics = self.diagnostics.clone();
    let webidl_bindings_host = self.webidl_bindings_host;

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

    let clock = event_loop.clock();

    update_time_bindings_clock(realm.heap(), clock).map_err(|err| Error::Other(err.to_string()))?;
    realm.reset_interrupt();
    let webidl_bindings_host = match webidl_bindings_host {
      Some(mut host_ptr) => Some(unsafe { host_ptr.as_mut() }),
      None => None,
    };
    let mut hooks = VmJsEventLoopHooks::<BrowserTabHost>::new_with_vm_host_and_window_realm(
      document,
      realm,
      webidl_bindings_host,
    );
    hooks.set_event_loop(event_loop);
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
    ImportMapError::LimitExceeded(message) => format!("TypeError: import map limit exceeded: {message}"),
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
#[cfg(test)]
mod tests {
  use super::*;
  use crate::api::{BrowserTab, FastRender, RenderOptions};
  use crate::js::ImportMapLimits;
  use crate::resource::{FetchRequest, FetchedResource};
  use crate::text::font_db::FontConfig;
  use std::collections::HashMap;
  use std::sync::Mutex;
  use vm_js::PropertyKey;

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

    #[allow(dead_code)]
    fn calls(&self) -> Vec<String> {
      self
        .calls
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
    }
  }

  impl ResourceFetcher for MapFetcher {
    fn fetch(&self, url: &str) -> Result<FetchedResource> {
      self.fetch_with_request(FetchRequest::new(url, crate::resource::FetchDestination::Other))
    }

    fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
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

  fn get_global_prop(realm: &mut WindowRealm, name: &str) -> Value {
    let (_vm, realm_ref, heap) = realm.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let global = realm_ref.global_object();
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

  fn get_global_prop_utf8(realm: &mut WindowRealm, name: &str) -> Option<String> {
    let value = get_global_prop(realm, name);
    match value {
      Value::String(s) => Some(
        realm
          .heap()
          .get_string(s)
          .expect("get string")
          .to_utf8_lossy(),
      ),
      _ => None,
    }
  }

  fn import_map_spec(base_url: &str) -> ScriptElementSpec {
    ScriptElementSpec {
      base_url: Some(base_url.to_string()),
      src: None,
      src_attr_present: false,
      inline_text: String::new(),
      async_attr: false,
      force_async: false,
      defer_attr: false,
      nomodule_attr: false,
      crossorigin: None,
      integrity_attr_present: false,
      integrity: None,
      referrer_policy: None,
      fetch_priority: None,
      parser_inserted: true,
      node_id: None,
      script_type: crate::js::ScriptType::ImportMap,
    }
  }

  #[test]
  fn vm_js_browser_tab_executor_sets_import_meta_url() -> Result<()> {
    let mut document =
      BrowserDocumentDom2::from_html("<!doctype html><html></html>", RenderOptions::default())?;
    let current_script = CurrentScriptStateHandle::default();
    let mut executor = VmJsBrowserTabExecutor::new();
    let mut event_loop = crate::js::EventLoop::<BrowserTabHost>::new();

    let mut options = JsExecutionOptions::default();
    options.supports_module_scripts = true;
    executor.reset_for_navigation(
      Some("https://example.com/doc.html"),
      &mut document,
      &current_script,
      options,
    )?;

    let script_text = "globalThis.__metaUrl = import.meta.url;";
    let spec = ScriptElementSpec {
      base_url: Some("https://example.com/doc.html".to_string()),
      src: None,
      src_attr_present: false,
      inline_text: script_text.to_string(),
      async_attr: false,
      force_async: false,
      defer_attr: false,
      nomodule_attr: false,
      crossorigin: None,
      integrity_attr_present: false,
      integrity: None,
      referrer_policy: None,
      fetch_priority: None,
      parser_inserted: true,
      node_id: None,
      script_type: crate::js::ScriptType::Module,
    };
    executor.execute_module_script(script_text, &spec, None, &mut document, &mut event_loop)?;

    let realm = executor.realm.as_mut().expect("realm initialized");
    assert_eq!(
      get_global_prop_utf8(realm, "__metaUrl").as_deref(),
      Some("https://example.com/doc.html#inline-module-0")
    );
    Ok(())
  }

  #[test]
  fn vm_js_browser_tab_executor_resolves_bare_specifiers_via_import_maps() -> Result<()> {
    let dep_url = "https://example.com/dep.js";

    let mut map = HashMap::<String, FetchedResource>::new();
    map.insert(
      dep_url.to_string(),
      FetchedResource::new(
        "export const value = 7;".as_bytes().to_vec(),
        Some("application/javascript".to_string()),
      ),
    );
    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(MapFetcher::new(map));

    let renderer = FastRender::builder()
      .dom_scripting_enabled(true)
      .font_sources(FontConfig::bundled_only())
      .fetcher(fetcher)
      .build()?;
    let mut document =
      BrowserDocumentDom2::new(renderer, "<!doctype html><html></html>", RenderOptions::default())?;

    let current_script = CurrentScriptStateHandle::default();
    let mut executor = VmJsBrowserTabExecutor::new();
    let mut event_loop = crate::js::EventLoop::<BrowserTabHost>::new();

    let mut options = JsExecutionOptions::default();
    options.supports_module_scripts = true;
    executor.reset_for_navigation(
      Some("https://example.com/doc.html"),
      &mut document,
      &current_script,
      options,
    )?;

    let import_map_text = r#"{ "imports": { "dep": "/dep.js" } }"#;
    let import_map_spec = ScriptElementSpec {
      base_url: Some("https://example.com/doc.html".to_string()),
      src: None,
      src_attr_present: false,
      inline_text: import_map_text.to_string(),
      async_attr: false,
      force_async: false,
      defer_attr: false,
      nomodule_attr: false,
      crossorigin: None,
      integrity_attr_present: false,
      integrity: None,
      referrer_policy: None,
      fetch_priority: None,
      parser_inserted: true,
      node_id: None,
      script_type: crate::js::ScriptType::ImportMap,
    };
    executor.execute_import_map_script(
      import_map_text,
      &import_map_spec,
      None,
      &mut document,
      &mut event_loop,
    )?;

    let module_text = "import { value } from 'dep'; globalThis.result = value;";
    let module_spec = ScriptElementSpec {
      base_url: Some("https://example.com/doc.html".to_string()),
      src: None,
      src_attr_present: false,
      inline_text: module_text.to_string(),
      async_attr: false,
      force_async: false,
      defer_attr: false,
      nomodule_attr: false,
      crossorigin: None,
      integrity_attr_present: false,
      integrity: None,
      referrer_policy: None,
      fetch_priority: None,
      parser_inserted: true,
      node_id: None,
      script_type: crate::js::ScriptType::Module,
    };
    executor.execute_module_script(
      module_text,
      &module_spec,
      None,
      &mut document,
      &mut event_loop,
    )?;

    let realm = executor.realm.as_mut().expect("realm initialized");
    assert_eq!(get_global_prop(realm, "result"), Value::Number(7.0));
    Ok(())
  }

  #[test]
  fn vm_js_browser_tab_executor_fetch_module_graph_counts_entry_module_bytes_for_total_budget() -> Result<()> {
    let entry_source = "import './dep.js';";
    let dep_source = "export const value = 1;";
    let dep_url = "https://example.com/dep.js";
    let document_url = "https://example.com/doc.html";

    let total_limit = entry_source
      .as_bytes()
      .len()
      .saturating_add(dep_source.as_bytes().len())
      .saturating_sub(1);

    let mut map = HashMap::<String, FetchedResource>::new();
    map.insert(
      dep_url.to_string(),
      FetchedResource::new(
        dep_source.as_bytes().to_vec(),
        Some("application/javascript".to_string()),
      ),
    );
    let fetcher = Arc::new(MapFetcher::new(map));
    let fetcher_trait: Arc<dyn ResourceFetcher> = fetcher.clone();

    let renderer = FastRender::builder()
      .dom_scripting_enabled(true)
      .font_sources(FontConfig::bundled_only())
      .fetcher(fetcher_trait.clone())
      .build()?;
    let mut document =
      BrowserDocumentDom2::new(renderer, "<!doctype html><html></html>", RenderOptions::default())?;

    let current_script = CurrentScriptStateHandle::default();
    let mut executor = VmJsBrowserTabExecutor::new();
    let mut event_loop = crate::js::EventLoop::<BrowserTabHost>::new();

    let mut options = JsExecutionOptions::default();
    options.supports_module_scripts = true;
    options.max_module_graph_total_bytes = total_limit;
    executor.reset_for_navigation(Some(document_url), &mut document, &current_script, options)?;

    let spec = ScriptElementSpec {
      base_url: Some(document_url.to_string()),
      src: None,
      src_attr_present: false,
      inline_text: entry_source.to_string(),
      async_attr: false,
      force_async: false,
      defer_attr: false,
      nomodule_attr: false,
      crossorigin: None,
      integrity_attr_present: false,
      integrity: None,
      referrer_policy: None,
      fetch_priority: None,
      parser_inserted: true,
      node_id: None,
      script_type: crate::js::ScriptType::Module,
    };

    let err = executor
      .fetch_module_graph(&spec, fetcher_trait, &mut document, &mut event_loop)
      .expect_err("expected module graph total bytes budget error");
    assert!(
      err.to_string().contains("max_module_graph_total_bytes"),
      "unexpected error: {err}"
    );
    Ok(())
  }

  #[test]
  fn vm_js_browser_tab_executor_supports_dynamic_import_in_classic_scripts() -> Result<()> {
    let dep_url = "https://example.com/dep.js";

    let mut map = HashMap::<String, FetchedResource>::new();
    map.insert(
      dep_url.to_string(),
      FetchedResource::new(
        "export const value = 7;".as_bytes().to_vec(),
        Some("application/javascript".to_string()),
      ),
    );
    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(MapFetcher::new(map));

    let renderer = FastRender::builder()
      .dom_scripting_enabled(true)
      .font_sources(FontConfig::bundled_only())
      .fetcher(fetcher)
      .build()?;
    let mut document =
      BrowserDocumentDom2::new(renderer, "<!doctype html><html></html>", RenderOptions::default())?;

    let current_script = CurrentScriptStateHandle::default();
    let mut executor = VmJsBrowserTabExecutor::new();
    let mut event_loop = crate::js::EventLoop::<BrowserTabHost>::new();

    let mut options = JsExecutionOptions::default();
    options.supports_module_scripts = true;
    executor.reset_for_navigation(
      Some("https://example.com/doc.html"),
      &mut document,
      &current_script,
      options,
    )?;

    let script_text = "globalThis.__dynImportPromise = import('./dep.js');";
    let spec = ScriptElementSpec {
      base_url: Some("https://example.com/doc.html".to_string()),
      src: None,
      src_attr_present: false,
      inline_text: script_text.to_string(),
      async_attr: false,
      force_async: false,
      defer_attr: false,
      nomodule_attr: false,
      crossorigin: None,
      integrity_attr_present: false,
      integrity: None,
      referrer_policy: None,
      fetch_priority: None,
      parser_inserted: true,
      node_id: None,
      script_type: crate::js::ScriptType::Classic,
    };
    executor.execute_classic_script(script_text, &spec, None, &mut document, &mut event_loop)?;

    let realm = executor.realm.as_mut().expect("realm initialized");
    let promise_value = get_global_prop(realm, "__dynImportPromise");
    let Value::Object(promise_obj) = promise_value else {
      panic!("expected dynamic import to return a Promise object");
    };
    assert_eq!(
      realm
        .heap()
        .promise_state(promise_obj)
        .map_err(|err| Error::Other(err.to_string()))?,
      PromiseState::Fulfilled,
      "expected dynamic import promise to be fulfilled"
    );

    let ns_value = realm
      .heap()
      .promise_result(promise_obj)
      .map_err(|err| Error::Other(err.to_string()))?
      .unwrap_or(Value::Undefined);
    let Value::Object(ns_obj) = ns_value else {
      panic!("expected dynamic import promise to fulfill with a module namespace object");
    };

    let (_vm, _realm_ref, heap) = realm.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    scope
      .push_root(Value::Object(ns_obj))
      .map_err(|err| Error::Other(err.to_string()))?;
    let key_s = scope
      .alloc_string("value")
      .map_err(|err| Error::Other(err.to_string()))?;
    scope
      .push_root(Value::String(key_s))
      .map_err(|err| Error::Other(err.to_string()))?;
    let key = PropertyKey::from_string(key_s);
    assert!(
      scope
        .heap()
        .object_get_own_property(ns_obj, &key)
        .map_err(|err| Error::Other(err.to_string()))?
        .is_some(),
      "expected module namespace to expose exported binding"
    );

    Ok(())
  }

  #[test]
  fn importmap_script_respects_max_bytes_limit_from_js_execution_options() -> Result<()> {
    let mut options = JsExecutionOptions::default();
    options.supports_module_scripts = true;
    options.import_map_limits = ImportMapLimits {
      max_bytes: 1,
      ..ImportMapLimits::default()
    };
    let mut executor = VmJsBrowserTabExecutor::new();
    let mut document = BrowserDocumentDom2::from_html("<!doctype html>", RenderOptions::default())?;
    let mut event_loop = crate::js::EventLoop::<BrowserTabHost>::new();
    let current_script = CurrentScriptStateHandle::default();
    executor.reset_for_navigation(
      Some("https://example.com/"),
      &mut document,
      &current_script,
      options,
    )?;

    executor.execute_import_map_script(
      r#"{"imports":{"a":"/a.js"}}"#,
      &import_map_spec("https://example.com/"),
      None,
      &mut document,
      &mut event_loop,
    )?;

    let realm = executor.realm.as_mut().expect("realm initialized");
    let module_loader = realm.module_loader_handle();
    assert!(
      module_loader
        .borrow_mut()
        .import_map_state_mut()
        .import_map
        .imports
        .is_empty(),
      "expected import map registration to be blocked by max_bytes"
    );
    Ok(())
  }

  #[test]
  fn importmap_registration_respects_max_total_entries_limit_from_js_execution_options() -> Result<()> {
    let mut options = JsExecutionOptions::default();
    options.supports_module_scripts = true;
    options.import_map_limits = ImportMapLimits {
      max_total_entries: 1,
      ..ImportMapLimits::default()
    };
    let mut executor = VmJsBrowserTabExecutor::new();
    let mut document = BrowserDocumentDom2::from_html("<!doctype html>", RenderOptions::default())?;
    let mut event_loop = crate::js::EventLoop::<BrowserTabHost>::new();
    let current_script = CurrentScriptStateHandle::default();
    executor.reset_for_navigation(
      Some("https://example.com/"),
      &mut document,
      &current_script,
      options,
    )?;
    let spec = import_map_spec("https://example.com/");

    executor.execute_import_map_script(
      r#"{"imports":{"a":"/a.js"}}"#,
      &spec,
      None,
      &mut document,
      &mut event_loop,
    )?;
    let realm = executor.realm.as_mut().expect("realm initialized");
    let module_loader = realm.module_loader_handle();
    assert!(
      module_loader
        .borrow_mut()
        .import_map_state_mut()
        .import_map
        .imports
        .contains_key("a"),
      "expected first import map entry to be registered"
    );

    // Second import map would push total entries over max_total_entries, so it must not be merged.
    executor.execute_import_map_script(
      r#"{"imports":{"b":"/b.js"}}"#,
      &spec,
      None,
      &mut document,
      &mut event_loop,
    )?;
    assert!(
      !module_loader
        .borrow_mut()
        .import_map_state_mut()
        .import_map
        .imports
        .contains_key("b"),
      "expected import map merge to be blocked by max_total_entries"
    );
    Ok(())
  }

  #[test]
  fn vm_js_executor_drains_promise_jobs_after_script_execution() -> Result<()> {
    let html = "<!doctype html><html><head><script>\
      document.documentElement.className = 'y';\
      Promise.resolve().then(function () { document.documentElement.className = 'x'; });\
      </script></head><body></body></html>";

    let tab = BrowserTab::from_html(html, RenderOptions::default(), VmJsBrowserTabExecutor::new())?;

    let dom = tab.dom();
    let document_element = dom.document_element().expect("document element");
    let class = dom
      .get_attribute(document_element, "class")
      .expect("get class attribute");
    assert_eq!(class, Some("x"));
    Ok(())
  }
}
