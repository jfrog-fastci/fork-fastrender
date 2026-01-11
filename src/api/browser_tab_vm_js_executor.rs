use crate::error::{Error, Result};
use crate::js::runtime::with_event_loop;
use crate::js::script_encoding::decode_classic_script_bytes;
use crate::js::time::update_time_bindings_clock;
use crate::js::vm_error_format;
use crate::js::window_realm::{WindowRealm, WindowRealmConfig, WindowRealmUserData};
use crate::js::window_timers::VmJsEventLoopHooks;
use crate::js::{
  install_window_animation_frame_bindings, install_window_fetch_bindings_with_guard,
  install_window_timers_bindings,
  import_maps::{
    create_import_map_parse_result_with_limits, register_import_map_with_limits, ImportMapError,
    ImportMapWarningKind,
  },
  CurrentScriptStateHandle, JsExecutionOptions, LocationNavigationRequest, ScriptElementSpec,
  WindowFetchBindings, WindowFetchEnv,
};
use crate::resource::{
  cors_enforcement_enabled, ensure_cors_allows_origin, ensure_http_success, ensure_script_mime_sane,
  origin_from_url, CorsMode, FetchDestination, FetchRequest, ResourceFetcher,
};
use crate::style::media::{MediaContext, MediaType};
use crate::web::events::{Event, EventTargetId};
use encoding_rs::UTF_8;
use std::ptr::NonNull;
use std::sync::Arc;
use vm_js::{
  HostDefined, ModuleGraph, ModuleId, PromiseState, SourceText, SourceTextModuleRecord, Value, VmError,
  VmHost,
};
use webidl_vm_js::WebIdlBindingsHost;

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
  js_execution_options: JsExecutionOptions,
  inline_module_id_counter: u64,
  document_url: String,
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
      js_execution_options: JsExecutionOptions::default(),
      inline_module_id_counter: 0,
      document_url: "about:blank".to_string(),
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
    // Drop the realm first so any remaining JS globals stop referencing the DOM source id.
    self.fetch_bindings = None;
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
    self.diagnostics = document.shared_diagnostics();
    // `document.currentScript` is read from the embedder `VmHost` (the document) by vm-js native
    // handlers. Share the stable per-tab `CurrentScriptStateHandle` so JS observes the same
    // bookkeeping mutated by `BrowserTabHost`'s orchestrator.
    document.set_current_script_handle(current_script.clone());
    self.vm_host = Some(NonNull::from(document as &mut dyn VmHost));
    // Tear down the previous realm so we don't leak rooted callbacks or global state across
    // navigations.
    self.fetch_bindings = None;
    self.realm = None;
    self.js_execution_options = js_execution_options;
    self.inline_module_id_counter = 0;

    let dom_source_id = document.ensure_dom_source_registered();

    let url = document_url.unwrap_or("about:blank");
    self.document_url = url.to_string();
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
      .with_dom_source_id(dom_source_id);

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
        .enable_module_loader(Arc::clone(&fetcher), js_execution_options.max_script_bytes, document_origin)
        .map_err(|err| Error::Other(err.to_string()))?;
    }

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
    let exec_result: Result<()> = with_event_loop(event_loop, || {
      update_time_bindings_clock(realm.heap(), clock.clone())
        .map_err(|err| Error::Other(err.to_string()))?;
      realm.set_base_url(spec.base_url.clone());
      realm.reset_interrupt();

      // Classic scripts can evaluate dynamic `import()` expressions. If module loading is enabled
      // for this realm, ensure the per-realm loader uses classic-script defaults.
      if let Some(data) = realm.vm_mut().user_data_mut::<WindowRealmUserData>() {
        if let Some(loader) = data.module_loader.as_mut() {
          loader.fetcher = document.fetcher();
          loader.cors_mode = CorsMode::Anonymous;
        }
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

  fn fetch_module_graph(
    &mut self,
    spec: &ScriptElementSpec,
    fetcher: Arc<dyn ResourceFetcher>,
    document: &mut BrowserDocumentDom2,
    event_loop: &mut crate::js::EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    // HTML: module scripts are fetched in CORS mode by default. When the `crossorigin` attribute is
    // missing, the default state is "anonymous" (same-origin credentials for same-origin requests).
    let cors_mode = spec.crossorigin.unwrap_or(CorsMode::Anonymous);

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

    let diagnostics = self.diagnostics.clone();
    let document_url = self.document_url.clone();
    let clock = event_loop.clock();
    let webidl_bindings_host = self.webidl_bindings_host;

    let exec_result: Result<()> = with_event_loop(event_loop, || {
      update_time_bindings_clock(realm.heap(), clock.clone()).map_err(|err| Error::Other(err.to_string()))?;
      realm.set_base_url(spec.base_url.clone());
      realm.reset_interrupt();

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

      let (
        max_script_bytes,
        document_origin,
        module_map_ptr,
        import_map_state_ptr,
      ) = {
        let Some(data) = vm.user_data_mut::<WindowRealmUserData>() else {
          return Err(Error::Other("window realm missing user data".to_string()));
        };
        let Some(loader) = data.module_loader.as_mut() else {
          return Err(Error::Other(
            "module scripts requested but module loading is not enabled for this realm".to_string(),
          ));
        };
        loader.fetcher = Arc::clone(&fetcher);
        loader.cors_mode = cors_mode;
        (
          loader.max_script_bytes,
          loader.document_origin.clone(),
          &mut loader.module_map as *mut std::collections::HashMap<String, ModuleId>,
          &mut loader.import_map_state as *mut crate::js::import_maps::ImportMapState,
        )
      };

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

      let record_nonfatal_error = |message: String| -> Error {
        if let Some(diag) = diagnostics.as_ref() {
          diag.record_js_exception(message.clone(), None);
        }
        Error::Other(message)
      };

      let entry_module: ModuleId = if let Some(id) =
        unsafe { (&*module_map_ptr).get(&entry_specifier).copied() }
      {
        id
      } else if spec.src_attr_present {
        let max_fetch = max_script_bytes.saturating_add(1);
        let mut req = FetchRequest::new(&entry_specifier, FetchDestination::ScriptCors)
          .with_referrer_url(&document_url)
          .with_credentials_mode(cors_mode.credentials_mode());
        if let Some(origin) = document_origin.as_ref() {
          req = req.with_client_origin(origin);
        }

        let fetched = fetcher.fetch_partial_with_request(req, max_fetch).map_err(|err| {
          record_nonfatal_error(format!("failed to fetch module {entry_specifier}: {err}"))
        })?;

        ensure_http_success(&fetched, &entry_specifier)
          .map_err(|err| record_nonfatal_error(err.to_string()))?;
        ensure_script_mime_sane(&fetched, &entry_specifier)
          .map_err(|err| record_nonfatal_error(err.to_string()))?;
        if cors_enforcement_enabled() {
          ensure_cors_allows_origin(document_origin.as_ref(), &fetched, &entry_specifier, cors_mode)
            .map_err(|err| record_nonfatal_error(err.to_string()))?;
        }

        // HTML import maps: enforce Subresource Integrity metadata (when present).
        let integrity_metadata = url::Url::parse(&entry_specifier)
          .ok()
          .map(|url| unsafe { &*import_map_state_ptr }.resolve_module_integrity_metadata(&url))
          .unwrap_or("");
        if !integrity_metadata.is_empty() {
          if let Err(message) = crate::js::sri::verify_integrity(&fetched.bytes, integrity_metadata)
          {
            return Err(record_nonfatal_error(format!(
              "SRI blocked module {entry_specifier}: {message}"
            )));
          }
        }

        if fetched.bytes.len() > max_script_bytes {
          return Err(record_nonfatal_error(format!(
            "module {entry_specifier} is too large ({} bytes > max {})",
            fetched.bytes.len(),
            max_script_bytes
          )));
        }

        let source_text = decode_classic_script_bytes(
          &fetched.bytes,
          fetched.content_type.as_deref(),
          UTF_8,
        );
        let source = Arc::new(SourceText::new(entry_specifier.clone(), source_text));
        let record = match SourceTextModuleRecord::parse_source_with_vm(&mut vm, source) {
          Ok(record) => record,
          Err(err) => return Err(vm_error_to_host_error(&mut scope, err)),
        };
        let id = module_graph.add_module(record);
        unsafe {
          (&mut *module_map_ptr).insert(entry_specifier.clone(), id);
        }
        id
      } else {
        if max_script_bytes != usize::MAX && spec.inline_text.as_bytes().len() > max_script_bytes {
          return Err(record_nonfatal_error(format!(
            "inline module {entry_specifier} is too large ({} bytes > max {})",
            spec.inline_text.as_bytes().len(),
            max_script_bytes
          )));
        }

        let source = Arc::new(SourceText::new(
          entry_specifier.clone(),
          spec.inline_text.as_str(),
        ));
        let record = match SourceTextModuleRecord::parse_source_with_vm(&mut vm, source) {
          Ok(record) => record,
          Err(err) => return Err(vm_error_to_host_error(&mut scope, err)),
        };
        let id = module_graph.add_module(record);
        unsafe {
          (&mut *module_map_ptr).insert(entry_specifier.clone(), id);
        }
        id
      };

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
        Ok(())
      })();

      if let Some(err) = hooks.finish(scope.heap_mut()) {
        return Err(err);
      }

      match module_result {
        Ok(()) => Ok(()),
        Err(err) => Err(vm_error_to_host_error(&mut scope, err)),
      }
    });

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

    let Some(realm) = self.realm.as_mut() else {
      return Err(Error::Other(
        "VmJsBrowserTabExecutor has no active WindowRealm; did reset_for_navigation run?".to_string(),
      ));
    };

    let diagnostics = self.diagnostics.clone();
    let clock = event_loop.clock();
    let webidl_bindings_host = self.webidl_bindings_host;

    let exec_result: Result<()> = with_event_loop(event_loop, || {
      update_time_bindings_clock(realm.heap(), clock.clone()).map_err(|err| Error::Other(err.to_string()))?;
      realm.set_base_url(spec.base_url.clone());
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

      let module_map_ptr: *mut std::collections::HashMap<String, ModuleId> = {
        let Some(data) = vm.user_data_mut::<WindowRealmUserData>() else {
          return Err(Error::Other("window realm missing user data".to_string()));
        };
        let Some(loader) = data.module_loader.as_mut() else {
          return Err(Error::Other(
            "module scripts requested but module loading is not enabled for this realm".to_string(),
          ));
        };
        loader.fetcher = document.fetcher();
        loader.cors_mode = cors_mode;
        &mut loader.module_map as *mut std::collections::HashMap<String, ModuleId>
      };

      let entry_module = {
        let module_map = unsafe { &mut *module_map_ptr };
        if let Some(id) = module_map.get(&entry_specifier).copied() {
          id
        } else {
          let source = Arc::new(SourceText::new(entry_specifier.clone(), script_text));
          let record = match SourceTextModuleRecord::parse_source_with_vm(&mut vm, source) {
            Ok(record) => record,
            Err(err) => {
              if vm_error_format::vm_error_is_js_exception(&err) {
                if let Some(diag) = diagnostics.as_ref() {
                  let (message, stack) = vm_error_format::vm_error_to_message_and_stack(heap, err);
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
        }
      };

      let mut hooks = inner_hooks;

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
    let Some(import_map_state) = realm
      .vm_mut()
      .user_data_mut::<WindowRealmUserData>()
      .and_then(|data| data.module_loader.as_mut().map(|loader| &mut loader.import_map_state))
    else {
      return Ok(());
    };

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
    let webidl_bindings_host = match webidl_bindings_host {
      Some(mut host_ptr) => Some(unsafe { host_ptr.as_mut() }),
      None => None,
    };
    let mut hooks = VmJsEventLoopHooks::<BrowserTabHost>::new_with_vm_host_and_window_realm(
      document,
      realm,
      webidl_bindings_host,
    );
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

    assert!(
      executor
        .realm
        .as_mut()
        .and_then(|realm| realm.vm_mut().user_data_mut::<WindowRealmUserData>())
        .and_then(|data| data.module_loader.as_ref().map(|loader| &loader.import_map_state))
        .is_some_and(|state| state.import_map.imports.is_empty()),
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
    assert!(
      executor
        .realm
        .as_mut()
        .and_then(|realm| realm.vm_mut().user_data_mut::<WindowRealmUserData>())
        .and_then(|data| data.module_loader.as_ref().map(|loader| &loader.import_map_state))
        .is_some_and(|state| state.import_map.imports.contains_key("a")),
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
      executor
        .realm
        .as_mut()
        .and_then(|realm| realm.vm_mut().user_data_mut::<WindowRealmUserData>())
        .and_then(|data| data.module_loader.as_ref().map(|loader| &loader.import_map_state))
        .is_some_and(|state| !state.import_map.imports.contains_key("b")),
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
