use crate::debug::runtime::runtime_toggles;
use crate::error::{Error, Result};
use crate::js::console_sink::{fanout_console_sink, formatting_console_sink, stderr_console_sink};
use crate::js::time::update_time_bindings_clock;
use crate::js::vm_error_format;
use crate::js::window_file_reader::install_window_file_reader_bindings;
use crate::js::window_realm::{WindowRealm, WindowRealmConfig};
use crate::js::window_timers::VmJsEventLoopHooks;
use crate::js::web_storage::{
  alloc_session_storage_namespace_id, with_default_hub_mut, SessionNamespaceId, StorageListenerGuard,
};
use crate::js::{
  install_window_animation_frame_bindings, install_window_fetch_bindings_with_guard,
  install_window_timers_bindings, install_window_websocket_bindings_with_guard,
  install_window_xhr_bindings_with_guard,
  import_maps::{
    create_import_map_parse_result_with_limits, register_import_map_with_limits, ImportMapError,
    ImportMapWarningKind,
  },
  CurrentScriptStateHandle, HtmlScriptId, JsExecutionOptions, LocationNavigationRequest, ModuleKey,
  ScriptElementSpec, TaskSource, WindowFetchBindings, WindowFetchEnv, WindowWebSocketBindings,
  WindowWebSocketEnv, WindowXhrBindings, WindowXhrEnv,
};
use crate::resource::{origin_from_url, CorsMode, ReferrerPolicy, ResourceFetcher};
use crate::style::media::{MediaContext, MediaType};
use crate::web::events::{Event, EventTargetId};
use std::collections::HashMap;
use std::ptr::NonNull;
use std::sync::Arc;
use vm_js::{
  GcObject, HostDefined, ModuleGraph, ModuleId, PromiseState, PropertyDescriptor, PropertyKey,
  PropertyKind, RootId, Scope, SourceText, StackFrame, Value, VmError, VmHost,
};
use webidl_vm_js::WebIdlBindingsHost;

use super::BrowserDocumentDom2;
use super::{
  BrowserTabHost, BrowserTabJsExecutor, ConsoleMessageLevel, ModuleScriptExecutionStatus,
  SharedRenderDiagnostics,
};
use super::browser_tab::ModuleScriptEvaluationOutcome;

#[derive(Debug, Clone, Copy)]
struct PendingModuleEvaluation {
  module: ModuleId,
  promise: GcObject,
  promise_root: RootId,
  turns_waited: usize,
}

fn console_stderr_enabled() -> bool {
  // Local debugging hook: allow printing JS `console.*` output to stderr even when render
  // diagnostics collection is disabled (kept opt-in to avoid noisy tests/CI logs).
  let toggles = runtime_toggles();
  let raw = toggles
    .get("FASTR_JS_CONSOLE_STDERR")
    // Backwards-compatible alias used by some tooling/scripts.
    .or_else(|| toggles.get("FASTR_CONSOLE_STDERR"));
  let Some(raw) = raw else {
    return false;
  };
  let raw = raw.trim();
  !raw.eq_ignore_ascii_case("0")
    && !raw.eq_ignore_ascii_case("false")
    && !raw.eq_ignore_ascii_case("no")
    && !raw.eq_ignore_ascii_case("off")
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
  websocket_bindings: Option<WindowWebSocketBindings>,
  js_execution_options: JsExecutionOptions,
  inline_module_id_counter: u64,
  document_url: String,
  document_referrer_policy: ReferrerPolicy,
  session_storage_namespace_id: u64,
  session_storage_guard: Option<StorageListenerGuard>,
  pending_module_evaluations: HashMap<HtmlScriptId, PendingModuleEvaluation>,
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
      websocket_bindings: None,
      js_execution_options: JsExecutionOptions::default(),
      inline_module_id_counter: 0,
      document_url: "about:blank".to_string(),
      document_referrer_policy: ReferrerPolicy::default(),
      session_storage_namespace_id: alloc_session_storage_namespace_id(),
      session_storage_guard: None,
      pending_module_evaluations: HashMap::new(),
      pending_navigation: None,
      diagnostics: None,
      vm_host: None,
      webidl_bindings_host: None,
    }
  }

  fn record_js_exception(
    diag: &SharedRenderDiagnostics,
    realm: &mut WindowRealm,
    err: vm_js::VmError,
  ) {
    let (message, stack) = vm_error_format::vm_error_to_message_and_stack(realm.heap_mut(), err);
    diag.record_js_exception(message, stack);
  }

  fn report_js_exception_as_window_error_event(
    diagnostics: Option<&SharedRenderDiagnostics>,
    webidl_bindings_host: Option<NonNull<dyn WebIdlBindingsHost>>,
    realm: &mut WindowRealm,
    document: &mut BrowserDocumentDom2,
    event_loop: &mut crate::js::EventLoop<BrowserTabHost>,
    err: VmError,
    filename_hint: Option<&str>,
  ) -> Result<()> {
    let mut thrown_value = err.thrown_value();
    // Root the thrown value while we format the exception + construct the ErrorEvent.
    //
    // `VmError::Throw*` stores the thrown JS `Value` by handle only; it is not a GC root. Creating
    // strings/objects below can trigger GC, so keep the value alive until the event has captured it
    // via `ErrorEventInit.error`.
    //
    // Best-effort: if we fail to root (e.g. due to OOM), continue without surfacing `event.error`.
    let thrown_value_root: Option<RootId> = if let Some(value) = thrown_value {
      match realm.heap_mut().add_root(value) {
        Ok(root) => Some(root),
        Err(_) => {
          thrown_value = None;
          None
        }
      }
    } else {
      None
    };
    let first_frame = err
      .thrown_stack()
      .and_then(|stack| stack.first())
      .cloned();
    let (message, stack) = vm_error_format::vm_error_to_message_and_stack(realm.heap_mut(), err);

    let (filename, lineno, colno) =
      resolve_error_event_location(filename_hint, first_frame.as_ref());

    // Best-effort: dispatching the error event should not prevent script execution from continuing.
    let dispatch_result = Self::dispatch_window_error_event(
      diagnostics,
      webidl_bindings_host,
      realm,
      document,
      event_loop,
      &message,
      &filename,
      lineno,
      colno,
      thrown_value,
    );
    let not_canceled = dispatch_result.unwrap_or(true);

    // Preserve existing behavior: record uncaught exceptions to renderer diagnostics. If the error
    // event is canceled (e.g. `window.onerror` returns true), treat it as handled and skip the
    // default diagnostics record (matches browser console behavior).
    if not_canceled {
      if let Some(diag) = diagnostics {
        diag.record_js_exception(message, stack);
      }
    }

    if let Some(root) = thrown_value_root {
      realm.heap_mut().remove_root(root);
    }

    Ok(())
  }

  fn dispatch_window_error_event(
    diagnostics: Option<&SharedRenderDiagnostics>,
    webidl_bindings_host: Option<NonNull<dyn WebIdlBindingsHost>>,
    realm: &mut WindowRealm,
    document: &mut BrowserDocumentDom2,
    event_loop: &mut crate::js::EventLoop<BrowserTabHost>,
    message: &str,
    filename: &str,
    lineno: u32,
    colno: u32,
    error_value: Option<Value>,
  ) -> Result<bool> {
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

    realm.reset_interrupt();
    let global_obj = realm.global_object();
    let budget = realm.vm_budget_now();

    let result: std::result::Result<bool, VmError> = (|| {
      let (vm, heap) = realm.vm_and_heap_mut();
      let mut vm = vm.push_budget(budget);
      vm.tick()?;

      let mut scope = heap.scope();
      scope.push_root(Value::Object(global_obj))?;

      let type_s = scope.alloc_string("error")?;
      scope.push_root(Value::String(type_s))?;

      // Build the init dict.
      let init_obj = scope.alloc_object()?;
      scope.push_root(Value::Object(init_obj))?;

      let cancelable_key = alloc_key(&mut scope, "cancelable")?;
      scope.define_property(init_obj, cancelable_key, data_desc(Value::Bool(true)))?;

      // ErrorEventInit:
      let message_s = scope.alloc_string(message)?;
      scope.push_root(Value::String(message_s))?;
      let message_key = alloc_key(&mut scope, "message")?;
      scope.define_property(init_obj, message_key, data_desc(Value::String(message_s)))?;

      let filename_s = scope.alloc_string(filename)?;
      scope.push_root(Value::String(filename_s))?;
      let filename_key = alloc_key(&mut scope, "filename")?;
      scope.define_property(init_obj, filename_key, data_desc(Value::String(filename_s)))?;

      let lineno_key = alloc_key(&mut scope, "lineno")?;
      scope.define_property(
        init_obj,
        lineno_key,
        data_desc(Value::Number(lineno as f64)),
      )?;

      let colno_key = alloc_key(&mut scope, "colno")?;
      scope.define_property(
        init_obj,
        colno_key,
        data_desc(Value::Number(colno as f64)),
      )?;

      let error_key = alloc_key(&mut scope, "error")?;
      let error_value = error_value.unwrap_or(Value::Null);
      scope.push_root(error_value)?;
      scope.define_property(init_obj, error_key, data_desc(error_value))?;

      let error_event_ctor_key = alloc_key(&mut scope, "ErrorEvent")?;
      let error_event_ctor = vm.get_with_host_and_hooks(
        document,
        &mut scope,
        &mut hooks,
        global_obj,
        error_event_ctor_key,
      )?;
      scope.push_root(error_event_ctor)?;

      let (event_value, needs_payload_define) = if scope
        .heap()
        .is_constructor(error_event_ctor)
        .unwrap_or(false)
      {
        (
          vm.construct_with_host_and_hooks(
            document,
            &mut scope,
            &mut hooks,
            error_event_ctor,
            &[Value::String(type_s), Value::Object(init_obj)],
            error_event_ctor,
          )?,
          false,
        )
      } else {
        let event_ctor_key = alloc_key(&mut scope, "Event")?;
        let event_ctor =
          vm.get_with_host_and_hooks(document, &mut scope, &mut hooks, global_obj, event_ctor_key)?;
        scope.push_root(event_ctor)?;
        (
          vm.construct_with_host_and_hooks(
            document,
            &mut scope,
            &mut hooks,
            event_ctor,
            &[Value::String(type_s), Value::Object(init_obj)],
            event_ctor,
          )?,
          true,
        )
      };

      let Value::Object(event_obj) = event_value else {
        return Err(VmError::Unimplemented(
          "ErrorEvent/Event constructor returned non-object",
        ));
      };
      scope.push_root(Value::Object(event_obj))?;

      if needs_payload_define {
        scope.define_property(event_obj, message_key, read_only_data_desc(Value::String(message_s)))?;
        scope.define_property(
          event_obj,
          filename_key,
          read_only_data_desc(Value::String(filename_s)),
        )?;
        scope.define_property(
          event_obj,
          lineno_key,
          read_only_data_desc(Value::Number(lineno as f64)),
        )?;
        scope.define_property(
          event_obj,
          colno_key,
          read_only_data_desc(Value::Number(colno as f64)),
        )?;
        scope.define_property(event_obj, error_key, read_only_data_desc(error_value))?;
      }

      let dispatch_key = alloc_key(&mut scope, "dispatchEvent")?;
      let dispatch =
        vm.get_with_host_and_hooks(document, &mut scope, &mut hooks, global_obj, dispatch_key)?;
      let dispatch_result = vm.call_with_host_and_hooks(
        document,
        &mut scope,
        &mut hooks,
        dispatch,
        Value::Object(global_obj),
        &[Value::Object(event_obj)],
      )?;

      Ok(matches!(dispatch_result, Value::Bool(true)))
    })();

    let finish_err = hooks.finish(realm.heap_mut());
    if let Some(err) = finish_err {
      return Err(err);
    }

    match result {
      Ok(not_canceled) => Ok(not_canceled),
      Err(err) => {
        // Don't rethrow: we're already reporting an uncaught exception.
        if let Some(diag) = diagnostics {
          diag.record_console_message(
            ConsoleMessageLevel::Error,
            format!("failed to dispatch window error event: {err}"),
          );
        }
        Ok(true)
      }
    }
  }

  fn module_tla_turn_limit(&self) -> usize {
    // Bound how many event-loop turns we are willing to wait for a module top-level await promise
    // to settle.
    //
    // This uses the JS execution options rather than per-call `RunLimits` so step-wise callers
    // (e.g. `BrowserTab::tick_frame`) can drive the event loop incrementally without instantly
    // forcing module TLA to fail.
    self.js_execution_options.event_loop_run_limits.max_tasks
  }

  fn is_event_loop_quiescent(
    &self,
    event_loop: &mut crate::js::EventLoop<BrowserTabHost>,
  ) -> bool {
    event_loop.is_quiescent()
  }

  fn abort_pending_module_evaluation(&mut self) {
    if self.pending_module_evaluations.is_empty() {
      return;
    }
    let Some(realm) = self.realm.as_mut() else {
      self.pending_module_evaluations.clear();
      return;
    };
    let (vm, _realm_ref, heap) = realm.vm_realm_and_heap_mut();
    let pending = std::mem::take(&mut self.pending_module_evaluations);
    if let Some(modules_ptr) = vm.module_graph_ptr() {
      // SAFETY: `module_graph_ptr` points at the boxed module graph stored in realm user data when
      // module loading is enabled.
      let module_graph = unsafe { &mut *(modules_ptr as *mut ModuleGraph) };
      for (_, pending) in pending {
        module_graph.abort_tla_evaluation(vm, heap, pending.module);
        heap.remove_root(pending.promise_root);
      }
      return;
    }
    for (_, pending) in pending {
      heap.remove_root(pending.promise_root);
    }
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
    // Ensure any pending module evaluation does not leak roots/state in `vm-js`.
    self.abort_pending_module_evaluation();
    // Drop the realm first so any remaining JS globals stop referencing the document host.
    self.fetch_bindings = None;
    self.xhr_bindings = None;
    self.websocket_bindings = None;
    self.realm = None;
    // Clear the tab's sessionStorage data (spec: cleared when the tab closes).
    crate::js::web_storage::clear_session(self.session_storage_namespace_id);
    self.session_storage_guard = None;
  }
}
impl BrowserTabJsExecutor for VmJsBrowserTabExecutor {
  fn supports_incremental_dynamic_script_discovery(&self) -> bool {
    true
  }

  fn on_document_referrer_policy_updated(&mut self, policy: ReferrerPolicy) {
    self.document_referrer_policy = policy;
  }

  fn set_webidl_bindings_host(&mut self, host: &mut dyn WebIdlBindingsHost) {
    self.webidl_bindings_host = Some(NonNull::from(host));
  }

  fn event_listener_invoker(&self) -> Option<Box<dyn crate::web::events::EventListenerInvoker>> {
    // SAFETY: The returned invoker is stored alongside this executor in `BrowserTabHost`, so the
    // pointer remains valid for the lifetime of the host. All access occurs on the host thread.
    let realm_ptr = (&self.realm as *const Option<WindowRealm>) as *mut Option<WindowRealm>;
    let vm_host_ptr = (&self.vm_host as *const Option<NonNull<dyn VmHost>>) as *mut _;
    let webidl_bindings_host_ptr =
      (&self.webidl_bindings_host as *const Option<NonNull<dyn WebIdlBindingsHost>>) as *mut _;
    Some(Box::new(
      crate::js::window_realm::WindowRealmDomEventListenerInvoker::<BrowserTabHost>::new(
        realm_ptr,
        vm_host_ptr,
        webidl_bindings_host_ptr,
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
    // If a module evaluation is in-progress (top-level await), abort it before tearing down the
    // existing realm so any internal `vm-js` resources are released and our persistent roots are
    // removed.
    self.abort_pending_module_evaluation();
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
    self.websocket_bindings = None;
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

    // Keep the session storage namespace registered for the lifetime of this tab executor.
    //
    // Registering before `WindowRealm` creation ensures we don't leak session storage areas if realm
    // initialization fails midway (the realm constructor allocates Web Storage areas early).
    if self.session_storage_guard.is_none() {
      self.session_storage_guard = Some(with_default_hub_mut(|hub| {
        hub.register_window(SessionNamespaceId(self.session_storage_namespace_id))
      }));
    }

    let mut config = WindowRealmConfig::new(url)
      .with_media_context(media)
      .with_current_script_state(current_script.clone());
    config.session_storage_namespace_id = Some(self.session_storage_namespace_id);

    let stderr_console = console_stderr_enabled();
    let mut console_sink: Option<crate::js::ConsoleSink> = self.diagnostics.clone().map(|diag| {
      formatting_console_sink(move |level, message| {
        diag.record_console_message(level, message);
      })
    });

    if stderr_console {
      let stderr_sink = stderr_console_sink();
      console_sink = Some(match console_sink {
        Some(existing) => fanout_console_sink(existing, stderr_sink),
        None => stderr_sink,
      });
    }

    config.console_sink = console_sink;

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
    let (fetch_bindings, xhr_bindings, websocket_bindings) = {
      let (vm, realm_ref, heap) = realm.vm_realm_and_heap_mut();
      install_window_timers_bindings::<BrowserTabHost>(vm, realm_ref, heap)
        .map_err(|err| Error::Other(err.to_string()))?;
      install_window_animation_frame_bindings::<BrowserTabHost>(vm, realm_ref, heap)
        .map_err(|err| Error::Other(err.to_string()))?;
      install_window_file_reader_bindings::<BrowserTabHost>(vm, realm_ref, heap)
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

      #[cfg(feature = "direct_websocket")]
      let ws_env = WindowWebSocketEnv::for_document(Arc::clone(&fetcher), Some(url.to_string()));
      #[cfg(not(feature = "direct_websocket"))]
      let ws_env = WindowWebSocketEnv::for_document(Some(url.to_string()));

      let websocket_bindings = install_window_websocket_bindings_with_guard::<BrowserTabHost>(
        vm,
        realm_ref,
        heap,
        ws_env,
      )
      .map_err(|err| Error::Other(err.to_string()))?;

      crate::js::window_streams::install_window_streams_bindings(vm, realm_ref, heap)
        .map_err(|err| Error::Other(err.to_string()))?;

      (fetch_bindings, xhr_bindings, websocket_bindings)
    };

    self.fetch_bindings = Some(fetch_bindings);
    self.xhr_bindings = Some(xhr_bindings);
    self.websocket_bindings = Some(websocket_bindings);
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
        "VmJsBrowserTabExecutor has no active WindowRealm; did reset_for_navigation run?"
          .to_string(),
      ));
    };
    let diagnostics = self.diagnostics.clone();
    if let Some(diag) = diagnostics.as_ref() {
      diag.record_js_script_executed();
    }
    let clock = event_loop.clock();
    let webidl_bindings_host_ptr = self.webidl_bindings_host;
    let name: Arc<str> = if let Some(url) = spec.src.as_deref() {
      Arc::from(url)
    } else if let Some(node_id) = current_script {
      Arc::from(format!("<inline script node_id={}>", node_id.index()))
    } else {
      Arc::from("<inline>")
    };
    let js_execution_options = self.js_execution_options;
    let module_loader = realm.module_loader_handle();
    let effective_referrer_policy = spec.referrer_policy.unwrap_or(self.document_referrer_policy);

    update_time_bindings_clock(realm.heap(), clock.clone())
      .map_err(|err| Error::Other(err.to_string()))?;
    realm.set_base_url(spec.base_url.clone());
    realm.reset_interrupt();

    let source = match SourceText::new_charged(realm.heap_mut(), name, script_text) {
      Ok(source) => Arc::new(source),
      Err(err) => {
        if let Some(diag) = diagnostics.as_ref() {
          diag.record_js_vm_error(&err);
        }
        return Err(vm_error_format::vm_error_to_error(realm.heap_mut(), err));
      }
    };

    // Classic scripts can evaluate dynamic `import()` expressions. If module loading is enabled for
    // this realm, ensure the per-realm loader uses classic-script defaults.
    if realm.vm().module_graph_ptr().is_some() {
      let mut loader = module_loader.borrow_mut();
      loader.set_fetcher(document.fetcher());
      loader.set_cors_mode(CorsMode::Anonymous);
      loader.set_referrer_policy(effective_referrer_policy);
      loader.set_entry_module_integrity_override(None);
      loader.set_js_execution_options(js_execution_options);
    }
    let webidl_bindings_host = match webidl_bindings_host_ptr {
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
          Self::report_js_exception_as_window_error_event(
            diagnostics.as_ref(),
            webidl_bindings_host_ptr,
            realm,
            document,
            event_loop,
            err,
            spec
              .src
              .as_deref()
              .or_else(|| Some(self.document_url.as_str())),
          )?;
          Ok(())
        } else {
          if let Some(diag) = diagnostics.as_ref() {
            diag.record_js_vm_error(&err);
          }
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
        "VmJsBrowserTabExecutor has no active WindowRealm; did reset_for_navigation run?"
          .to_string(),
      ));
    };

    // HTML: module scripts are fetched in CORS mode by default. When the `crossorigin` attribute is
    // missing, the default state is "anonymous" (same-origin credentials for same-origin requests).
    let cors_mode = spec.crossorigin.unwrap_or(CorsMode::Anonymous);
    let effective_referrer_policy = spec.referrer_policy.unwrap_or(self.document_referrer_policy);
    let entry_integrity_override = if spec.src_attr_present && spec.integrity_attr_present {
      let Some(integrity) = spec.integrity.clone() else {
        return Err(Error::Other(format!(
          "SRI blocked module script {entry_specifier}: integrity attribute exceeded max length of {} bytes",
          crate::js::sri::MAX_INTEGRITY_ATTRIBUTE_BYTES
        )));
      };
      Some(integrity)
    } else {
      None
    };

    let diagnostics = self.diagnostics.clone();
    let clock = event_loop.clock();
    let js_execution_options = self.js_execution_options;
    let webidl_bindings_host = self.webidl_bindings_host;
    let entry_key = ModuleKey {
      url: entry_specifier.clone(),
      attributes: Vec::new(),
    };
    let module_loader = realm.module_loader_handle();

    update_time_bindings_clock(realm.heap(), clock.clone())
      .map_err(|err| Error::Other(err.to_string()))?;
    realm.set_base_url(spec.base_url.clone());
    realm.reset_interrupt();

    let exec_result: Result<()> = (|| {
      {
        let mut loader = module_loader.borrow_mut();
        loader.set_fetcher(Arc::clone(&fetcher));
        loader.set_cors_mode(cors_mode);
        loader.set_referrer_policy(effective_referrer_policy);
        loader.set_js_execution_options(js_execution_options);
        // Only the entry module fetch is eligible for the `<script>` integrity attribute override.
        // Clear any previous value so inline modules do not leak an override into subsequent loads.
        loader.set_entry_module_integrity_override(None);
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
        .map_err(|err| {
          if let Some(diag) = diagnostics.as_ref() {
            diag.record_js_vm_error(&err);
          }
          vm_error_format::vm_error_to_error(heap, err)
        })?;

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
          if let Some(diag) = diagnostics.as_ref() {
            diag.record_js_vm_error(&err);
          }
          vm_error_format::vm_error_to_error(scope.heap_mut(), err)
        }
      };

        let entry_module: std::result::Result<ModuleId, VmError> = {
          let mut loader = module_loader.borrow_mut();
          if spec.src_attr_present {
            loader.set_entry_module_integrity_override(entry_integrity_override.clone());
            let result = loader.get_or_fetch_module(scope.heap_mut(), module_graph, entry_key.clone());
            loader.set_entry_module_integrity_override(None);
            result
          } else {
            loader.set_entry_module_integrity_override(None);
            loader.get_or_parse_inline_module(
              scope.heap_mut(),
              module_graph,
              entry_key.clone(),
              spec.inline_text.as_str(),
            )
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
    script_id: HtmlScriptId,
    script_text: &str,
    spec: &ScriptElementSpec,
    _current_script: Option<crate::dom2::NodeId>,
    document: &mut BrowserDocumentDom2,
    event_loop: &mut crate::js::EventLoop<BrowserTabHost>,
  ) -> Result<ModuleScriptExecutionStatus> {
    // HTML: module scripts are fetched in CORS mode by default. When the `crossorigin` attribute is
    // missing, the default state is "anonymous" (same-origin credentials for same-origin requests).
    let cors_mode = spec.crossorigin.unwrap_or(CorsMode::Anonymous);

    let entry_specifier = if spec.src_attr_present {
      // External module script: use the resolved `src` URL as the module's specifier.
      let Some(entry_url) = spec.src.as_deref().filter(|s| !s.is_empty()) else {
        // HTML: modules with `src` present but empty/invalid do not execute.
        return Ok(ModuleScriptExecutionStatus::Completed);
      };
      entry_url.to_string()
    } else {
      // Inline module script: synthesize an opaque URL using the document base URL at discovery so
      // relative imports resolve correctly.
      let base_url = spec.base_url.as_deref().unwrap_or("about:blank");
      let inline_id = self.next_inline_module_id(spec);
      synthesize_inline_module_url(base_url, &inline_id)
    };

    let clock = event_loop.clock();
    let diagnostics = self.diagnostics.as_ref();
    let webidl_bindings_host_ptr = self.webidl_bindings_host;
    let js_execution_options = self.js_execution_options;
    let effective_referrer_policy = spec.referrer_policy.unwrap_or(self.document_referrer_policy);
    let entry_key = ModuleKey {
      url: entry_specifier.clone(),
      attributes: Vec::new(),
    };

    let Some(realm) = self.realm.as_mut() else {
      return Err(Error::Other(
        "VmJsBrowserTabExecutor has no active WindowRealm; did reset_for_navigation run?"
          .to_string(),
      ));
    };
    if let Some(diag) = diagnostics.as_ref() {
      diag.record_js_script_executed();
    }
    let module_loader = realm.module_loader_handle();

    update_time_bindings_clock(realm.heap(), clock.clone())
      .map_err(|err| Error::Other(err.to_string()))?;
    realm.set_base_url(spec.base_url.clone());
    {
      let mut loader = module_loader.borrow_mut();
      loader.set_fetcher(document.fetcher());
      loader.set_cors_mode(cors_mode);
      loader.set_referrer_policy(effective_referrer_policy);
      // Entry module source is provided inline (either actual inline text or host-fetched bytes),
      // so `<script integrity>` does not apply here.
      loader.set_entry_module_integrity_override(None);
      loader.set_js_execution_options(js_execution_options);
    }
    realm.reset_interrupt();

    // Route Promise jobs (including module-loading promise reactions) through FastRender's
    // microtask queue.
    let webidl_bindings_host = match webidl_bindings_host_ptr {
      Some(mut host_ptr) => Some(unsafe { host_ptr.as_mut() }),
      None => None,
    };
    let inner_hooks = VmJsEventLoopHooks::<BrowserTabHost>::new_with_vm_host_and_window_realm(
      document,
      realm,
      webidl_bindings_host,
    );

    enum ModuleScriptOutcome {
      Success(Option<PendingModuleEvaluation>),
      JsException(VmError),
    }

    let exec_result: Result<ModuleScriptOutcome> = (|| {
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
        .map_err(|err| {
          if let Some(diag) = diagnostics.as_ref() {
            diag.record_js_vm_error(&err);
          }
          vm_error_format::vm_error_to_error(heap, err)
        })?;

      let Some(modules_ptr) = vm.module_graph_ptr() else {
        return Err(Error::Other(
          "module scripts requested but module loading is not enabled for this realm".to_string(),
        ));
      };
      let module_graph = unsafe { &mut *(modules_ptr as *mut ModuleGraph) };

      let entry_module = {
        let mut loader = module_loader.borrow_mut();
        loader.get_or_parse_inline_module(heap, module_graph, entry_key.clone(), script_text)
      };
      let entry_module = match entry_module {
        Ok(id) => id,
        Err(err) => {
          if vm_error_format::vm_error_is_js_exception(&err) {
            return Ok(ModuleScriptOutcome::JsException(err));
          }
          if let Some(diag) = diagnostics.as_ref() {
            diag.record_js_vm_error(&err);
          }
          return Err(vm_error_format::vm_error_to_error(heap, err));
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
          PromiseState::Pending => {
            let promise_root = scope.heap_mut().add_root(Value::Object(promise_obj))?;
            Ok(Some(PendingModuleEvaluation {
              module: entry_module,
              promise: promise_obj,
              promise_root,
              turns_waited: 0,
            }))
          }
          _ => {
            ensure_promise_fulfilled(scope.heap(), eval_promise)?;
            Ok(None)
          }
        }
      })();

      if let Some(err) = hooks.finish(scope.heap_mut()) {
        if let Ok(Some(pending)) = &module_result {
          module_graph.abort_tla_evaluation(&mut vm, scope.heap_mut(), pending.module);
          scope.heap_mut().remove_root(pending.promise_root);
        }
        return Err(err);
      }

      match module_result {
        Ok(pending) => Ok(ModuleScriptOutcome::Success(pending)),
        Err(err) => {
          if vm_error_format::vm_error_is_js_exception(&err) {
            Ok(ModuleScriptOutcome::JsException(err))
          } else {
            if let Some(diag) = diagnostics.as_ref() {
              diag.record_js_vm_error(&err);
            }
            Err(vm_error_format::vm_error_to_error(scope.heap_mut(), err))
          }
        }
      }
    })();

    if let Some(req) = realm.take_pending_navigation_request() {
      realm.reset_interrupt();
      self.pending_navigation = Some(req);
      return Ok(ModuleScriptExecutionStatus::Completed);
    }

    match exec_result {
      Ok(ModuleScriptOutcome::Success(pending)) => match pending {
        Some(pending) => {
          self.pending_module_evaluations.insert(script_id, pending);
          Ok(ModuleScriptExecutionStatus::Pending)
        }
        None => Ok(ModuleScriptExecutionStatus::Completed),
      },
      Ok(ModuleScriptOutcome::JsException(err)) => {
        // HTML: module errors are reported as exceptions for the global object (observable via
        // `window.addEventListener("error")` / `window.onerror`).
        Self::report_js_exception_as_window_error_event(
          diagnostics,
          webidl_bindings_host_ptr,
          realm,
          document,
          event_loop,
          err,
          Some(&entry_specifier),
        )?;
        // Unlike classic scripts, module evaluation failures should surface as `<script>` element
        // errors (and should not fire a `load` event for external module scripts). Bubble a normal
        // host error so `BrowserTabHost` dispatches the script element `error` event.
        Err(Error::Other("module script evaluation failed".to_string()))
      }
      Err(err) => Err(err),
    }
  }

  fn execute_import_map_script(
    &mut self,
    script_text: &str,
    spec: &ScriptElementSpec,
    _current_script: Option<crate::dom2::NodeId>,
    document: &mut BrowserDocumentDom2,
    event_loop: &mut crate::js::EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    let base_url = spec.base_url.as_deref().unwrap_or("about:blank");
    let base_url = url::Url::parse(base_url)
      .or_else(|_| url::Url::parse("about:blank"))
      .map_err(|err| Error::Other(format!("failed to parse import map base URL: {err}")))?;

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

    // Scope the module loader borrow so we can run JS (to dispatch a window `error` event) in the
    // failure path. `WindowRealm::exec_script_source_with_host_and_hooks` registers per-script URL
    // metadata with the module loader and will fail if the loader is still borrowed.
    let registration_result = {
      let module_loader = realm.module_loader_handle();
      let mut module_loader = module_loader.borrow_mut();
      let import_map_state = module_loader.import_map_state_mut();
      register_import_map_with_limits(import_map_state, result, limits)
    };

    match registration_result {
      Ok(()) => Ok(()),
      Err(err) => {
        let formatted = format_import_map_error(&err);
        if let Some(diag) = self.diagnostics.as_ref() {
          // Keep existing behavior: import map failures show up as "uncaught JS exceptions" in the
          // diagnostics snapshot.
          diag.record_js_exception(formatted.clone(), None);
          // Additionally surface failures as console errors so tooling that only inspects console
          // output (e.g. Playwright/Puppeteer console listeners) can observe import map failures.
          diag.record_console_message(ConsoleMessageLevel::Error, format!("importmap: {formatted}"));
        }

        // WHATWG HTML: "register an import map" reports the error as an exception for the global
        // object. This manifests as a window `error` event (observable via
        // `window.addEventListener('error', ...)` and `window.onerror`).
        //
        // Note: browsers do not fire a `<script>` element "error" event for import map parse
        // failures; see docs/import_maps.md.
        let dispatch_message = formatted.clone();
        let filename = self.document_url.clone();
        // Best-effort: dispatching the error event must not crash parsing.
        if let (Ok(message_lit), Ok(filename_lit)) = (
          serde_json::to_string(&dispatch_message),
          serde_json::to_string(&filename),
        ) {
          let (error_expr, error_message_lit) = match &err {
            ImportMapError::Json(e) => (
              "SyntaxError",
              serde_json::to_string(&e.to_string()).unwrap_or_else(|_| "\"\"".to_string()),
            ),
            ImportMapError::TypeError(message) => (
              "TypeError",
              serde_json::to_string(message).unwrap_or_else(|_| "\"\"".to_string()),
            ),
            ImportMapError::LimitExceeded(message) => (
              "TypeError",
              serde_json::to_string(&format!("import map limit exceeded: {message}"))
                .unwrap_or_else(|_| "\"\"".to_string()),
            ),
          };
          let source = format!(
             "(function(){{\
               const ev = new Event('error', {{ cancelable: true }});\
               try {{ ev.message = {message_lit}; }} catch (_) {{}}\
               try {{ ev.filename = {filename_lit}; }} catch (_) {{}}\
               try {{ ev.lineno = 0; }} catch (_) {{}}\
               try {{ ev.colno = 0; }} catch (_) {{}}\
               try {{ ev.error = new {error_expr}({error_message_lit}); }} catch (_) {{}}\
               try {{ window.dispatchEvent(ev); }} catch (_) {{}}\
             }})();"
          );

          let clock = event_loop.clock();
          let _ = update_time_bindings_clock(realm.heap(), clock);
          realm.reset_interrupt();
          let webidl_bindings_host = match self.webidl_bindings_host {
            Some(mut host_ptr) => Some(unsafe { host_ptr.as_mut() }),
            None => None,
          };
           let mut hooks = VmJsEventLoopHooks::<BrowserTabHost>::new_with_vm_host_and_window_realm(
             document,
             realm,
             webidl_bindings_host,
           );
           hooks.set_event_loop(event_loop);
           let source_text = match SourceText::new_charged(
             realm.heap_mut(),
             "<importmap error>",
             source,
           ) {
             Ok(source_text) => Arc::new(source_text),
             Err(err) => {
               if let Some(diag) = self.diagnostics.as_ref() {
                 diag.record_js_vm_error(&err);
               }
               return Ok(());
             }
           };
           let result = realm.exec_script_source_with_host_and_hooks(document, &mut hooks, source_text);
           if let Some(err) = hooks.finish(realm.heap_mut()) {
             if let Some(diag) = self.diagnostics.as_ref() {
               diag.record_console_message(ConsoleMessageLevel::Error, format!("importmap: error event dispatch failed: {err}"));
             }
          } else if let Err(vm_err) = result {
            if vm_error_format::vm_error_is_js_exception(&vm_err) {
              if let Some(diag) = self.diagnostics.as_ref() {
                Self::record_js_exception(diag, realm, vm_err);
              }
            }
          }
        }
        Ok(())
      }
    }
  }

  fn after_microtask_checkpoint(
    &mut self,
    document: &mut BrowserDocumentDom2,
    event_loop: &mut crate::js::EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    let turn_limit = self.module_tla_turn_limit();
    let is_quiescent = self.is_event_loop_quiescent(event_loop);
    // Avoid double-counting when `after_microtask_checkpoint` is invoked from within an active task
    // (e.g. `BrowserTabHost::perform_microtask_checkpoint_and_notify_executor`).
    //
    // Note: `EventLoop` runs microtask checkpoint hooks while `currently_running_task` may still be
    // set to the last drained microtask (implementation detail). Treat microtask checkpoints as
    // "between tasks" for the purpose of the module-TLA task budget.
    let increment_turns = match event_loop.currently_running_task() {
      None => true,
      Some(task) => task.is_microtask,
    };

    if self.pending_module_evaluations.is_empty() {
      return Ok(());
    }
    let Some(realm) = self.realm.as_mut() else {
      return Ok(());
    };
    let webidl_bindings_host_ptr = self.webidl_bindings_host;

    let diagnostics = self.diagnostics.clone();
    // Collect settled module evaluation promises without holding an active JS borrow while we
    // dispatch error events (dispatch can allocate/GC).
    let mut completed: Vec<(HtmlScriptId, RootId, ModuleScriptEvaluationOutcome, Option<Value>)> =
      Vec::new();
    let mut aborted_due_to_quiescent = false;
    {
      let (vm, _realm_ref, heap) = realm.vm_realm_and_heap_mut();
      for (script_id, pending) in &mut self.pending_module_evaluations {
        let state = match heap.promise_state(pending.promise) {
          Ok(state) => state,
          Err(err) => {
            if let Some(diag) = diagnostics.as_ref() {
              diag.record_js_vm_error(&err);
            }
            return Err(vm_error_format::vm_error_to_error(heap, err));
          }
        };

        match state {
          PromiseState::Fulfilled => {
            completed.push((
              *script_id,
              pending.promise_root,
              ModuleScriptEvaluationOutcome::Fulfilled,
              None,
            ));
          }
          PromiseState::Rejected => {
            let reason = match heap.promise_result(pending.promise) {
              Ok(reason) => reason.unwrap_or(Value::Undefined),
              Err(err) => {
                if let Some(diag) = diagnostics.as_ref() {
                  diag.record_js_vm_error(&err);
                }
                return Err(vm_error_format::vm_error_to_error(heap, err));
              }
            };
            completed.push((
              *script_id,
              pending.promise_root,
              ModuleScriptEvaluationOutcome::Rejected,
              Some(reason),
            ));
          }
          PromiseState::Pending => {
            if increment_turns {
              pending.turns_waited = pending.turns_waited.saturating_add(1);
            }

            if pending.turns_waited > turn_limit {
              // No more budget: abort the async module evaluation state so vm-js can release roots.
              if let Some(modules_ptr) = vm.module_graph_ptr() {
                // SAFETY: `module_graph_ptr` points at the boxed module graph stored in realm user
                // data when module loading is enabled.
                let module_graph = unsafe { &mut *(modules_ptr as *mut ModuleGraph) };
                module_graph.abort_tla_evaluation(vm, heap, pending.module);
              }
              if let Some(diag) = diagnostics.as_ref() {
                diag.record_js_exception(
                  format!(
                    "module top-level await did not settle within the configured task budget (turns_waited={}, limit={turn_limit})",
                    pending.turns_waited
                  ),
                  None,
                );
              }
              completed.push((
                *script_id,
                pending.promise_root,
                ModuleScriptEvaluationOutcome::Rejected,
                None,
              ));
              continue;
            }

            if is_quiescent {
              // No tasks/timers/microtasks remain that could settle this promise; abort so we do not
              // hang indefinitely.
              if let Some(modules_ptr) = vm.module_graph_ptr() {
                // SAFETY: `module_graph_ptr` points at the boxed module graph stored in realm user
                // data when module loading is enabled.
                let module_graph = unsafe { &mut *(modules_ptr as *mut ModuleGraph) };
                module_graph.abort_tla_evaluation(vm, heap, pending.module);
              }
              aborted_due_to_quiescent = true;
              completed.push((
                *script_id,
                pending.promise_root,
                ModuleScriptEvaluationOutcome::Rejected,
                None,
              ));
            }
          }
        }
      }
    }

    if aborted_due_to_quiescent {
      if let Some(diag) = diagnostics.as_ref() {
        diag.record_js_exception(
          "module top-level await did not settle before the event loop became idle".to_string(),
          None,
        );
      }
    }

    for (script_id, root_id, outcome, rejection_reason) in completed {
      if let Some(reason) = rejection_reason {
        // Propagate top-level await rejections as global `error` events (per HTML module script
        // evaluation error reporting) and record them into diagnostics when not handled.
        Self::report_js_exception_as_window_error_event(
          diagnostics.as_ref(),
          webidl_bindings_host_ptr,
          realm,
          document,
          event_loop,
          VmError::Throw(reason),
          Some(&self.document_url),
        )?;
      }

      event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
        host.on_module_script_evaluation_complete(script_id, outcome, event_loop)
      })?;
      realm.heap_mut().remove_root(root_id);
      self.pending_module_evaluations.remove(&script_id);
    }

    Ok(())
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

  fn dispatch_beforeunload_event(
    &mut self,
    document: &mut BrowserDocumentDom2,
    event_loop: &mut crate::js::EventLoop<BrowserTabHost>,
  ) -> Result<bool> {
    let Some(realm) = self.realm.as_mut() else {
      return Ok(true);
    };

    let diagnostics = self.diagnostics.clone();
    let webidl_bindings_host = self.webidl_bindings_host;
    let clock = event_loop.clock();

    // Run `beforeunload` synchronously and return whether navigation should proceed.
    //
    // We treat `event.preventDefault()` and non-empty `event.returnValue` as cancellation signals.
    // Additionally, `window.onbeforeunload = () => "..."` is supported via `EventTarget.dispatchEvent`
    // EventHandler invocation.
    let source = r#"(function(){
      let e;
      try {
        e = new BeforeUnloadEvent("beforeunload", { cancelable: true });
      } catch (_) {
        e = new Event("beforeunload", { cancelable: true });
        try { e.returnValue = ""; } catch (_) {}
      }
      dispatchEvent(e);
      const rv = e.returnValue;
      return e.defaultPrevented || (typeof rv === "string" && rv.length > 0);
    })();"#;

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
    let source_text = match SourceText::new_charged(realm.heap_mut(), "<beforeunload>", source)
    {
      Ok(source_text) => Arc::new(source_text),
      Err(err) => return Err(vm_error_format::vm_error_to_error(realm.heap_mut(), err)),
    };
    let result = realm.exec_script_source_with_host_and_hooks(document, &mut hooks, source_text);
    if let Some(err) = hooks.finish(realm.heap_mut()) {
      return Err(err);
    }

    // Discard any nested `window.location` navigation request produced by the handler: we're
    // already in the middle of deciding whether the current navigation should proceed.
    if realm.take_pending_navigation_request().is_some() {
      realm.reset_interrupt();
    }

    let canceled = match result {
      Ok(Value::Bool(canceled)) => canceled,
      Ok(_) => false,
      Err(err) => {
        if vm_error_format::vm_error_is_js_exception(&err) {
          if let Some(diag) = diagnostics.as_ref() {
            Self::record_js_exception(diag, realm, err);
          }
          false
        } else {
          return Err(vm_error_format::vm_error_to_error(realm.heap_mut(), err));
        }
      }
    };

    if canceled {
      // `window.location` updates the internal href slot before the navigation commits. Restore the
      // current document URL so `location.href` remains consistent when navigation is canceled.
      realm
        .restore_location_url_to_document_url()
        .map_err(|err| Error::Other(err.to_string()))?;
    }

    Ok(!canceled)
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
    if let Some(diag) = diagnostics.as_ref() {
      diag.record_js_script_executed();
    }
    let webidl_bindings_host = self.webidl_bindings_host;

    let dispatch_expr = match target {
      EventTargetId::Document => "document.dispatchEvent(e);",
      EventTargetId::Window => "dispatchEvent(e);",
      EventTargetId::Node(_) | EventTargetId::Opaque(_) => return Ok(()),
    };

    let type_lit = serde_json::to_string(&event.type_).unwrap_or_else(|_| "\"\"".to_string());
    let (ctor_name, init_lit, post_init) = match event.type_.as_str() {
      "pagehide" | "pageshow" => (
        "PageTransitionEvent",
        serde_json::json!({
          "bubbles": event.bubbles,
          "cancelable": event.cancelable,
          "composed": event.composed,
          "persisted": false,
        })
        .to_string(),
        "try { e.persisted = false; } catch (_) {};",
      ),
      "beforeunload" => (
        "BeforeUnloadEvent",
        serde_json::json!({
          "bubbles": event.bubbles,
          "cancelable": event.cancelable,
          "composed": event.composed,
        })
        .to_string(),
        "try { e.returnValue = \"\"; } catch (_) {};",
      ),
      _ => (
        "Event",
        serde_json::json!({
          "bubbles": event.bubbles,
          "cancelable": event.cancelable,
          "composed": event.composed,
        })
        .to_string(),
        "",
      ),
    };

    let source = if ctor_name == "Event" {
      format!("(function(){{const e=new Event({type_lit},{init_lit});{post_init}{dispatch_expr}}})();")
    } else {
      format!(
        "(function(){{let e;try{{e=new {ctor_name}({type_lit},{init_lit});}}catch(_){{e=new Event({type_lit},{init_lit});}};{post_init}{dispatch_expr}}})();",
      )
    };

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
    let source_text = match SourceText::new_charged(realm.heap_mut(), "<lifecycle>", source)
    {
      Ok(source_text) => Arc::new(source_text),
      Err(err) => return Err(vm_error_format::vm_error_to_error(realm.heap_mut(), err)),
    };
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
          if let Some(diag) = diagnostics.as_ref() {
            diag.record_js_vm_error(&err);
          }
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

fn alloc_key(scope: &mut Scope<'_>, name: &str) -> std::result::Result<PropertyKey, VmError> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
}

fn data_desc(value: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: true,
    kind: PropertyKind::Data {
      value,
      writable: true,
    },
  }
}

fn read_only_data_desc(value: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: true,
    kind: PropertyKind::Data {
      value,
      writable: false,
    },
  }
}

fn resolve_error_event_location(
  filename_hint: Option<&str>,
  first_frame: Option<&StackFrame>,
) -> (String, u32, u32) {
  if let Some(frame) = first_frame {
    let from_stack = frame.source.as_ref();
    // vm-js uses synthetic `<inline>` names for unnamed scripts; prefer a real document/script URL
    // when available so `window.onerror` gets a useful filename.
    let filename = if from_stack.starts_with('<') {
      filename_hint.unwrap_or(from_stack)
    } else {
      from_stack
    };
    (filename.to_string(), frame.line, frame.col)
  } else {
    (filename_hint.unwrap_or("").to_string(), 0, 0)
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
    ImportMapWarningKind::AddressInvalid {
      specifier_key,
      address,
    } => {
      format!("invalid address {address:?} for specifier key {specifier_key:?}")
    }
    ImportMapWarningKind::TrailingSlashMismatch {
      specifier_key,
      address,
    } => {
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
    ImportMapError::LimitExceeded(message) => {
      format!("TypeError: import map limit exceeded: {message}")
    }
  }
}

fn ensure_promise_fulfilled(
  heap: &vm_js::Heap,
  promise: Value,
) -> std::result::Result<(), VmError> {
  let Value::Object(promise_obj) = promise else {
    return Err(VmError::InvariantViolation("expected a Promise object"));
  };
  match heap.promise_state(promise_obj)? {
    PromiseState::Pending => Err(VmError::Unimplemented(
      "asynchronous module loading/evaluation is not supported",
    )),
    PromiseState::Fulfilled => Ok(()),
    PromiseState::Rejected => {
      let reason = heap
        .promise_result(promise_obj)?
        .unwrap_or(Value::Undefined);
      Err(VmError::Throw(reason))
    }
  }
}
#[cfg(test)]
mod tests {
  use super::*;
  use crate::api::{BrowserTab, DiagnosticsLevel, FastRender, RenderDiagnostics, RenderOptions};
  use crate::debug::runtime::{with_runtime_toggles, RuntimeToggles};
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
      self.fetch_with_request(FetchRequest::new(
        url,
        crate::resource::FetchDestination::Other,
      ))
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

  fn reset_web_storage_for_test() -> WebStorageResetGuard {
    crate::js::web_storage::clear_default_web_storage_hub();
    WebStorageResetGuard
  }

  struct WebStorageResetGuard;

  impl Drop for WebStorageResetGuard {
    fn drop(&mut self) {
      crate::js::web_storage::clear_default_web_storage_hub();
    }
  }
  #[test]
  fn vm_js_browser_tab_executor_emits_console_to_stderr_when_env_flag_set() -> Result<()> {
    with_runtime_toggles(
      Arc::new(RuntimeToggles::from_map(HashMap::from([
        ("FASTR_JS_CONSOLE_STDERR".to_string(), "1".to_string()),
        ("FASTR_CONSOLE_STDERR".to_string(), "0".to_string()),
      ]))),
      || {
        // Diagnostics default to off; with `FASTR_JS_CONSOLE_STDERR=1` we should still install a
        // console sink so output is visible (and the call must not crash).
        let mut document = BrowserDocumentDom2::from_html(
          "<!doctype html><html></html>",
          RenderOptions::default(),
        )?;
        let current_script = CurrentScriptStateHandle::default();
        let mut executor = VmJsBrowserTabExecutor::new();
        executor.reset_for_navigation(
          Some("https://example.com/doc.html"),
          &mut document,
          &current_script,
          JsExecutionOptions::default(),
        )?;

        let realm = executor.realm.as_mut().expect("realm initialized");
        let has_sink = realm
          .exec_script("typeof console.__fastrender_console_sink_id === 'number'")
          .map_err(|err| Error::Other(err.to_string()))?;
        assert_eq!(
          has_sink,
          Value::Bool(true),
          "expected env flag to install a console sink even when diagnostics are disabled"
        );

        realm
          .exec_script("console.log('x')")
          .map_err(|err| Error::Other(err.to_string()))?;

        Ok(())
      },
    )
  }

  #[test]
  fn vm_js_browser_tab_executor_records_formatted_console_messages_when_diagnostics_enabled(
  ) -> Result<()> {
    with_runtime_toggles(
      Arc::new(RuntimeToggles::from_map(HashMap::from([
        ("FASTR_JS_CONSOLE_STDERR".to_string(), "0".to_string()),
        ("FASTR_CONSOLE_STDERR".to_string(), "1".to_string()),
      ]))),
      || {
        let mut document = BrowserDocumentDom2::from_html(
          "<!doctype html><html></html>",
          RenderOptions::default(),
        )?;
        let diag = Arc::new(Mutex::new(RenderDiagnostics::default()));
        document
          .renderer_mut()
          .set_diagnostics_sink(Some(Arc::clone(&diag)));

        let current_script = CurrentScriptStateHandle::default();
        let mut executor = VmJsBrowserTabExecutor::new();
        let mut event_loop = crate::js::EventLoop::<BrowserTabHost>::new();
        executor.reset_for_navigation(
          Some("https://example.com/doc.html"),
          &mut document,
          &current_script,
          JsExecutionOptions::default(),
        )?;

        assert!(
          diag.lock().unwrap().console_messages.is_empty(),
          "expected console messages to start empty"
        );

        let script_text = "console.log('[%s %d %% %cX]', 'hi', 3, 'color:red');";
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

        let messages = diag.lock().unwrap().console_messages.clone();
        assert!(
          !messages.is_empty(),
          "expected console message to be recorded when diagnostics are enabled"
        );
        assert_eq!(messages[0].level, ConsoleMessageLevel::Log);
        assert_eq!(messages[0].message, "[hi 3 % X]");
        Ok(())
      },
    )
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
    let status = executor.execute_module_script(
      HtmlScriptId::from_u64(1),
      script_text,
      &spec,
      None,
      &mut document,
      &mut event_loop,
    )?;
    assert_eq!(status, ModuleScriptExecutionStatus::Completed);

    let realm = executor.realm.as_mut().expect("realm initialized");
    assert_eq!(
      get_global_prop_utf8(realm, "__metaUrl").as_deref(),
      Some("https://example.com/doc.html#inline-module-0")
    );
    Ok(())
  }

  #[test]
  fn vm_js_browser_tab_executor_dispatches_error_event_for_uncaught_exception() -> Result<()> {
    let mut document =
      BrowserDocumentDom2::from_html("<!doctype html><html></html>", RenderOptions::default())?;
    let current_script = CurrentScriptStateHandle::default();
    let mut executor = VmJsBrowserTabExecutor::new();
    let mut event_loop = crate::js::EventLoop::<BrowserTabHost>::new();
    executor.reset_for_navigation(
      Some("https://example.com/doc.html"),
      &mut document,
      &current_script,
      JsExecutionOptions::default(),
    )?;

    let script_text = "globalThis.__called = false;\n\
      globalThis.__message = '';\n\
      globalThis.__lineno = 0;\n\
      globalThis.__colno = 0;\n\
      globalThis.__error_message = null;\n\
      globalThis.__is_error_event = false;\n\
      addEventListener('error', function (e) {\n\
        globalThis.__called = true;\n\
        globalThis.__message = String(e && e.message);\n\
        globalThis.__lineno = e && e.lineno;\n\
        globalThis.__colno = e && e.colno;\n\
        globalThis.__error_message = e && e.error && e.error.message;\n\
        globalThis.__is_error_event = (e instanceof ErrorEvent);\n\
      });\n\
      throw new Error('boom');\n";

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
    assert!(matches!(get_global_prop(realm, "__called"), Value::Bool(true)));
    assert!(
      get_global_prop_utf8(realm, "__message")
        .unwrap_or_default()
        .contains("boom"),
      "expected ErrorEvent.message to mention the thrown error"
    );
    assert_eq!(get_global_prop_utf8(realm, "__error_message").as_deref(), Some("boom"));
    assert!(matches!(
      get_global_prop(realm, "__is_error_event"),
      Value::Bool(true)
    ));
    assert!(
      matches!(get_global_prop(realm, "__lineno"), Value::Number(_)),
      "expected ErrorEvent.lineno to be a number"
    );
    assert!(
      matches!(get_global_prop(realm, "__colno"), Value::Number(_)),
      "expected ErrorEvent.colno to be a number"
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
    let mut document = BrowserDocumentDom2::new(
      renderer,
      "<!doctype html><html></html>",
      RenderOptions::default(),
    )?;

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
    let status = executor.execute_module_script(
      HtmlScriptId::from_u64(2),
      module_text,
      &module_spec,
      None,
      &mut document,
      &mut event_loop,
    )?;
    assert_eq!(status, ModuleScriptExecutionStatus::Completed);

    let realm = executor.realm.as_mut().expect("realm initialized");
    assert_eq!(get_global_prop(realm, "result"), Value::Number(7.0));
    Ok(())
  }

  #[test]
  fn importmap_invalid_json_emits_console_error_and_allows_following_scripts() -> Result<()> {
    let html = "<!doctype html><html><head>\
      <script type=\"importmap\">{</script>\
      <script>document.documentElement.setAttribute('data-after','ok');</script>\
      </head><body></body></html>";

    let mut render_options = RenderOptions::default();
    render_options.diagnostics_level = DiagnosticsLevel::Basic;
    let mut js_options = JsExecutionOptions::default();
    js_options.supports_module_scripts = true;

    let tab = BrowserTab::from_html_with_js_execution_options(
      html,
      render_options,
      VmJsBrowserTabExecutor::new(),
      js_options,
    )?;

    // Import map errors must not prevent later classic scripts from running.
    let dom = tab.dom();
    let document_element = dom.document_element().expect("document element");
    assert_eq!(
      dom
        .get_attribute(document_element, "data-after")
        .expect("get data-after"),
      Some("ok")
    );

    let diagnostics = tab.diagnostics_snapshot().expect("diagnostics enabled");
    assert!(
      diagnostics.console_messages.iter().any(|msg| {
        msg.level == ConsoleMessageLevel::Error
          && msg.message.contains("importmap:")
          && msg.message.contains("SyntaxError")
      }),
      "expected an importmap SyntaxError console message, got: {:?}",
      diagnostics.console_messages
    );
    Ok(())
  }

  #[test]
  fn importmap_integrity_type_error_emits_console_error_and_does_not_modify_state() -> Result<()> {
    let base_url = "https://example.com/doc.html";

    let renderer = FastRender::builder()
      .dom_scripting_enabled(true)
      .font_sources(FontConfig::bundled_only())
      .build()?;

    let mut render_options = RenderOptions::default();
    render_options.diagnostics_level = DiagnosticsLevel::Basic;
    let mut document =
      BrowserDocumentDom2::new(renderer, "<!doctype html><html></html>", render_options)?;

    let diagnostics = SharedRenderDiagnostics::new();
    document
      .renderer_mut()
      .set_diagnostics_sink(Some(Arc::clone(&diagnostics.inner)));

    let current_script = CurrentScriptStateHandle::default();
    let mut executor = VmJsBrowserTabExecutor::new();
    let mut event_loop = crate::js::EventLoop::<BrowserTabHost>::new();

    let mut options = JsExecutionOptions::default();
    options.supports_module_scripts = true;
    executor.reset_for_navigation(Some(base_url), &mut document, &current_script, options)?;

    let import_map_spec = import_map_spec(base_url);

    // Seed a valid import map entry.
    executor.execute_import_map_script(
      r#"{ "imports": { "dep": "/a.js" } }"#,
      &import_map_spec,
      None,
      &mut document,
      &mut event_loop,
    )?;
    {
      let realm = executor.realm.as_mut().expect("realm initialized");
      let module_loader = realm.module_loader_handle();
      let mut loader = module_loader.borrow_mut();
      let state = loader.import_map_state_mut();
      let dep = state
        .import_map
        .imports
        .get("dep")
        .and_then(|u| u.as_ref())
        .map(|u| u.as_str().to_string());
      assert_eq!(dep.as_deref(), Some("https://example.com/a.js"));
    }

    // Second import map has an invalid `"integrity"` top-level value. This should:
    // - surface an error,
    // - and leave the previously registered map unchanged.
    executor.execute_import_map_script(
      r#"{ "imports": { "dep": "/b.js" }, "integrity": [] }"#,
      &import_map_spec,
      None,
      &mut document,
      &mut event_loop,
    )?;
    {
      let realm = executor.realm.as_mut().expect("realm initialized");
      let module_loader = realm.module_loader_handle();
      let mut loader = module_loader.borrow_mut();
      let state = loader.import_map_state_mut();
      let dep = state
        .import_map
        .imports
        .get("dep")
        .and_then(|u| u.as_ref())
        .map(|u| u.as_str().to_string());
      assert_eq!(
        dep.as_deref(),
        Some("https://example.com/a.js"),
        "expected invalid import map registration to leave state unchanged"
      );
    }

    let snapshot = diagnostics.into_inner();
    assert!(
      snapshot.console_messages.iter().any(|msg| {
        msg.level == ConsoleMessageLevel::Error
          && msg.message.contains("importmap:")
          && msg.message.contains("integrity")
      }),
      "expected an importmap integrity console error, got: {:?}",
      snapshot.console_messages
    );
    Ok(())
  }

  #[test]
  fn importmap_warning_is_reported_and_imports_affect_module_resolution() -> Result<()> {
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

    let mut render_options = RenderOptions::default();
    render_options.diagnostics_level = DiagnosticsLevel::Basic;
    let mut document =
      BrowserDocumentDom2::new(renderer, "<!doctype html><html></html>", render_options)?;

    let diagnostics = SharedRenderDiagnostics::new();
    document
      .renderer_mut()
      .set_diagnostics_sink(Some(Arc::clone(&diagnostics.inner)));

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

    // Register a valid import map that also produces a warning (unknown top-level key).
    let import_map_text = r#"{ "imports": { "dep": "/dep.js" }, "unknown": 1 }"#;
    executor.execute_import_map_script(
      import_map_text,
      &import_map_spec("https://example.com/doc.html"),
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
    let status = executor.execute_module_script(
      HtmlScriptId::from_u64(3),
      module_text,
      &module_spec,
      None,
      &mut document,
      &mut event_loop,
    )?;
    assert_eq!(status, ModuleScriptExecutionStatus::Completed);

    let realm = executor.realm.as_mut().expect("realm initialized");
    assert_eq!(get_global_prop(realm, "result"), Value::Number(7.0));

    let snapshot = diagnostics.into_inner();
    assert!(
      snapshot.console_messages.iter().any(|msg| {
        msg.level == ConsoleMessageLevel::Warn
          && msg.message == "importmap: unknown top-level key \"unknown\""
      }),
      "expected an importmap warning console message, got: {:?}",
      snapshot.console_messages
    );
    Ok(())
  }

  #[test]
  fn vm_js_browser_tab_executor_fetch_module_graph_counts_entry_module_bytes_for_total_budget(
  ) -> Result<()> {
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
    let mut document = BrowserDocumentDom2::new(
      renderer,
      "<!doctype html><html></html>",
      RenderOptions::default(),
    )?;

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
    let document_url = "https://example.com/doc.html";

    let mut map = HashMap::<String, FetchedResource>::new();
    map.insert(
      dep_url.to_string(),
      FetchedResource::new(
        "export const value = 7;".as_bytes().to_vec(),
        Some("application/javascript".to_string()),
      ),
    );
    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(MapFetcher::new(map));

    let mut js_options = JsExecutionOptions::default();
    js_options.supports_module_scripts = true;

    let html = r#"<!doctype html><html><body>
      <script>
        import('./dep.js')
          .then((ns) => {
            document.body.setAttribute('data-value', String(ns.value));
          })
          .catch((err) => {
            document.body.setAttribute('data-error', String(err));
          });
      </script>
    </body></html>"#;

    let mut tab = BrowserTab::from_html_with_vmjs_and_document_url_and_fetcher_and_js_execution_options(
      html,
      document_url,
      RenderOptions::default(),
      fetcher,
      js_options,
    )?;
    tab.run_event_loop_until_idle(crate::js::RunLimits::unbounded())?;

    let dom = tab.dom();
    let body = dom.body().expect("body should exist");
    assert_eq!(
      dom
        .get_attribute(body, "data-error")
        .expect("get_attribute should succeed"),
      None,
      "expected dynamic import to succeed"
    );
    assert_eq!(
      dom
        .get_attribute(body, "data-value")
        .expect("get_attribute should succeed"),
      Some("7"),
      "expected imported module namespace value to be observable"
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
  fn importmap_registration_respects_max_total_entries_limit_from_js_execution_options(
  ) -> Result<()> {
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

    let tab = BrowserTab::from_html(
      html,
      RenderOptions::default(),
      VmJsBrowserTabExecutor::new(),
    )?;

    let dom = tab.dom();
    let document_element = dom.document_element().expect("document element");
    let class = dom
      .get_attribute(document_element, "class")
      .expect("get class attribute");
    assert_eq!(class, Some("x"));
    Ok(())
  }

  #[test]
  fn vm_js_browser_tab_executor_records_unimplemented_failure_telemetry() -> Result<()> {
    let diag = SharedRenderDiagnostics::new();
    let mut document =
      BrowserDocumentDom2::from_html("<!doctype html><html></html>", RenderOptions::default())?;
    document
      .renderer_mut()
      .set_diagnostics_sink(Some(Arc::clone(&diag.inner)));

    let current_script = CurrentScriptStateHandle::default();
    let mut executor = VmJsBrowserTabExecutor::new();
    let mut event_loop = crate::js::EventLoop::<BrowserTabHost>::new();
    executor.reset_for_navigation(
      Some("https://example.com/doc.html"),
      &mut document,
      &current_script,
      JsExecutionOptions::default(),
    )?;

    // Use the test-only host hook installed by the vm-js WindowRealm embedding so this test does
    // not depend on any particular JavaScript feature being unimplemented (which will change as the
    // engine becomes more complete).
    let script_text = "__fastrender_test_unimplemented();";
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

    let _err = executor
      .execute_classic_script(script_text, &spec, None, &mut document, &mut event_loop)
      .expect_err("expected injected VmError::Unimplemented");

    let snapshot = diag.into_inner();
    assert!(
      snapshot.stats.is_none(),
      "expected diagnostics.stats to remain None without diagnostics stats recorder"
    );
    let js = snapshot.js_failure;
    assert!(
      js.scripts_executed > 0,
      "expected scripts_executed > 0, got {js:?}"
    );
    assert!(
      js.top_unimplemented
        .iter()
        .any(|entry| entry.message == "telemetry test"),
      "expected injected unimplemented reason in telemetry, got {js:?}"
    );
    Ok(())
  }

  #[test]
  fn vm_js_browser_tab_executor_records_js_exception_telemetry() -> Result<()> {
    let diag = SharedRenderDiagnostics::new();
    let mut document =
      BrowserDocumentDom2::from_html("<!doctype html><html></html>", RenderOptions::default())?;
    document
      .renderer_mut()
      .set_diagnostics_sink(Some(Arc::clone(&diag.inner)));

    let current_script = CurrentScriptStateHandle::default();
    let mut executor = VmJsBrowserTabExecutor::new();
    let mut event_loop = crate::js::EventLoop::<BrowserTabHost>::new();
    executor.reset_for_navigation(
      Some("https://example.com/doc.html"),
      &mut document,
      &current_script,
      JsExecutionOptions::default(),
    )?;

    let script_text = r#"throw new TypeError("x");"#;
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

    // Uncaught JS exceptions are intentionally treated as non-fatal for the render pipeline; the
    // embedder captures them for diagnostics and continues running.
    executor.execute_classic_script(script_text, &spec, None, &mut document, &mut event_loop)?;

    let snapshot = diag.into_inner();
    assert!(
      snapshot.stats.is_none(),
      "expected diagnostics.stats to remain None without diagnostics stats recorder"
    );
    let js = snapshot.js_failure;
    assert!(
      js.scripts_executed > 0,
      "expected scripts_executed > 0, got {js:?}"
    );
    assert!(
      js.top_exceptions
        .iter()
        .any(|entry| entry.type_ == "TypeError" && entry.message == "x"),
      "expected thrown TypeError in telemetry, got {js:?}"
    );
    Ok(())
  }

  #[test]
  fn session_storage_persists_across_executor_navigations_for_same_origin() -> Result<()> {
    let _storage_guard = reset_web_storage_for_test();

    let url_a = "https://example.com/doc-a.html";
    let url_b = "https://example.com/doc-b.html";

    let current_script = CurrentScriptStateHandle::default();
    let mut executor = VmJsBrowserTabExecutor::new();

    let mut document_a =
      BrowserDocumentDom2::from_html("<!doctype html><html></html>", RenderOptions::default())?;
    executor.reset_for_navigation(
      Some(url_a),
      &mut document_a,
      &current_script,
      JsExecutionOptions::default(),
    )?;
    let realm = executor.realm.as_mut().expect("realm initialized");
    realm
      .exec_script("sessionStorage.setItem('k', 'v');")
      .map_err(|err| Error::Other(err.to_string()))?;

    let mut document_b =
      BrowserDocumentDom2::from_html("<!doctype html><html></html>", RenderOptions::default())?;
    executor.reset_for_navigation(
      Some(url_b),
      &mut document_b,
      &current_script,
      JsExecutionOptions::default(),
    )?;
    let realm = executor.realm.as_mut().expect("realm initialized");
    let v = realm
      .exec_script("sessionStorage.getItem('k')")
      .map_err(|err| Error::Other(err.to_string()))?;
    let Value::String(v) = v else {
      return Err(Error::Other(
        "expected sessionStorage.getItem('k') to return a string".to_string(),
      ));
    };
    assert_eq!(
      realm
        .heap()
        .get_string(v)
        .map_err(|err| Error::Other(err.to_string()))?
        .to_utf8_lossy(),
      "v"
    );
    Ok(())
  }

  #[test]
  fn session_storage_is_isolated_across_executors() -> Result<()> {
    let _storage_guard = reset_web_storage_for_test();

    let url = "https://example.com/doc.html";
    let current_script = CurrentScriptStateHandle::default();

    let mut executor_a = VmJsBrowserTabExecutor::new();
    let mut document_a =
      BrowserDocumentDom2::from_html("<!doctype html><html></html>", RenderOptions::default())?;
    executor_a.reset_for_navigation(
      Some(url),
      &mut document_a,
      &current_script,
      JsExecutionOptions::default(),
    )?;
    let realm_a = executor_a.realm.as_mut().expect("realm initialized");
    realm_a
      .exec_script("sessionStorage.setItem('k', 'v');")
      .map_err(|err| Error::Other(err.to_string()))?;

    let mut executor_b = VmJsBrowserTabExecutor::new();
    let mut document_b =
      BrowserDocumentDom2::from_html("<!doctype html><html></html>", RenderOptions::default())?;
    executor_b.reset_for_navigation(
      Some(url),
      &mut document_b,
      &current_script,
      JsExecutionOptions::default(),
    )?;
    let realm_b = executor_b.realm.as_mut().expect("realm initialized");
    assert_eq!(
      realm_b
        .exec_script("sessionStorage.getItem('k')")
        .map_err(|err| Error::Other(err.to_string()))?,
      Value::Null
    );
    Ok(())
  }

  #[test]
  fn local_storage_is_shared_across_executors_for_same_origin() -> Result<()> {
    let _storage_guard = reset_web_storage_for_test();

    let url = "https://example.com/doc.html";
    let current_script = CurrentScriptStateHandle::default();

    let mut executor_a = VmJsBrowserTabExecutor::new();
    let mut document_a =
      BrowserDocumentDom2::from_html("<!doctype html><html></html>", RenderOptions::default())?;
    executor_a.reset_for_navigation(
      Some(url),
      &mut document_a,
      &current_script,
      JsExecutionOptions::default(),
    )?;
    let realm_a = executor_a.realm.as_mut().expect("realm initialized");
    realm_a
      .exec_script("localStorage.setItem('lk', 'lv');")
      .map_err(|err| Error::Other(err.to_string()))?;

    let mut executor_b = VmJsBrowserTabExecutor::new();
    let mut document_b =
      BrowserDocumentDom2::from_html("<!doctype html><html></html>", RenderOptions::default())?;
    executor_b.reset_for_navigation(
      Some(url),
      &mut document_b,
      &current_script,
      JsExecutionOptions::default(),
    )?;
    let realm_b = executor_b.realm.as_mut().expect("realm initialized");
    let v = realm_b
      .exec_script("localStorage.getItem('lk')")
      .map_err(|err| Error::Other(err.to_string()))?;
    let Value::String(v) = v else {
      return Err(Error::Other(
        "expected localStorage.getItem('lk') to return a string".to_string(),
      ));
    };
    assert_eq!(
      realm_b
        .heap()
        .get_string(v)
        .map_err(|err| Error::Other(err.to_string()))?
        .to_utf8_lossy(),
      "lv"
    );
    Ok(())
  }
}
