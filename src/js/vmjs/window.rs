use crate::dom2;
use crate::error::{Error, Result};
use crate::js::host_document::DocumentHostState;
use crate::js::import_maps::{
  ImportMapError, ImportMapState, ImportMapWarning, ModuleResolutionError,
};
use crate::js::orchestrator::CurrentScriptHost;
use crate::js::vm_error_format;
use crate::js::webidl::VmJsWebIdlBindingsHostDispatch;
use crate::js::window_realm::{ConsoleSink, WindowRealm, WindowRealmConfig, WindowRealmHost};
use crate::js::{
  install_window_animation_frame_bindings, install_window_fetch_bindings_with_guard,
  install_window_timers_bindings, install_window_xhr_bindings_with_guard, DomHost, EventLoop,
  JsExecutionOptions, RunLimits, RunUntilIdleOutcome, TaskSource, WindowFetchBindings,
  WindowFetchEnv, WindowXhrBindings, WindowXhrEnv,
};
use crate::js::{Clock, RealClock};
use crate::resource::{origin_from_url, HttpFetcher, ResourceFetcher};
use std::sync::Arc;

/// Host-owned "window" state for executing scripts against a single DOM document.
///
/// This is a convenience composition type that bundles:
/// - a mutable `dom2::Document` (via [`DocumentHostState`]),
/// - a `vm-js` realm with Window-like globals (`window`/`self`/`document`/`location`) via [`WindowRealm`],
/// - and an HTML-like event loop (`setTimeout`/microtasks) via [`EventLoop`].
///
/// `document.currentScript` is observable during script execution via the embedder `VmHost` context
/// passed to the `vm-js` runtime (for `WindowHost`, this is the [`DocumentHostState`]).
pub struct WindowHost {
  host: WindowHostState,
  event_loop: EventLoop<WindowHostState>,
}

impl WindowHost {
  pub fn new(dom: dom2::Document, document_url: impl Into<String>) -> Result<Self> {
    Self::new_with_fetcher_and_options(
      dom,
      document_url,
      Arc::new(HttpFetcher::new()),
      JsExecutionOptions::default(),
    )
  }

  pub fn new_with_js_execution_options(
    dom: dom2::Document,
    document_url: impl Into<String>,
    js_execution_options: JsExecutionOptions,
  ) -> Result<Self> {
    Self::new_with_fetcher_and_options(
      dom,
      document_url,
      Arc::new(HttpFetcher::new()),
      js_execution_options,
    )
  }

  pub fn new_with_fetcher(
    dom: dom2::Document,
    document_url: impl Into<String>,
    fetcher: Arc<dyn ResourceFetcher>,
  ) -> Result<Self> {
    let event_loop = EventLoop::<WindowHostState>::new();
    Self::new_with_fetcher_and_event_loop_and_options(
      dom,
      document_url,
      fetcher,
      event_loop,
      JsExecutionOptions::default(),
    )
  }

  pub fn new_with_event_loop(
    dom: dom2::Document,
    document_url: impl Into<String>,
    event_loop: EventLoop<WindowHostState>,
  ) -> Result<Self> {
    Self::new_with_fetcher_and_event_loop_and_options(
      dom,
      document_url,
      Arc::new(HttpFetcher::new()),
      event_loop,
      JsExecutionOptions::default(),
    )
  }

  pub fn new_with_fetcher_and_event_loop(
    dom: dom2::Document,
    document_url: impl Into<String>,
    fetcher: Arc<dyn ResourceFetcher>,
    event_loop: EventLoop<WindowHostState>,
  ) -> Result<Self> {
    Self::new_with_fetcher_and_event_loop_and_options(
      dom,
      document_url,
      fetcher,
      event_loop,
      JsExecutionOptions::default(),
    )
  }

  pub fn new_with_options(
    dom: dom2::Document,
    document_url: impl Into<String>,
    options: JsExecutionOptions,
  ) -> Result<Self> {
    let event_loop = EventLoop::<WindowHostState>::new();
    Self::new_with_fetcher_and_event_loop_and_options(
      dom,
      document_url,
      Arc::new(HttpFetcher::new()),
      event_loop,
      options,
    )
  }

  pub fn new_with_fetcher_and_options(
    dom: dom2::Document,
    document_url: impl Into<String>,
    fetcher: Arc<dyn ResourceFetcher>,
    options: JsExecutionOptions,
  ) -> Result<Self> {
    let event_loop = EventLoop::<WindowHostState>::new();
    Self::new_with_fetcher_and_event_loop_and_options(
      dom,
      document_url,
      fetcher,
      event_loop,
      options,
    )
  }

  pub fn new_with_event_loop_and_options(
    dom: dom2::Document,
    document_url: impl Into<String>,
    event_loop: EventLoop<WindowHostState>,
    options: JsExecutionOptions,
  ) -> Result<Self> {
    Self::new_with_fetcher_and_event_loop_and_options(
      dom,
      document_url,
      Arc::new(HttpFetcher::new()),
      event_loop,
      options,
    )
  }

  pub fn new_with_fetcher_and_event_loop_and_options(
    dom: dom2::Document,
    document_url: impl Into<String>,
    fetcher: Arc<dyn ResourceFetcher>,
    mut event_loop: EventLoop<WindowHostState>,
    options: JsExecutionOptions,
  ) -> Result<Self> {
    event_loop.set_queue_limits(options.event_loop_queue_limits);
    let clock = event_loop.clock();
    let host = WindowHostState::new_with_fetcher_and_clock_and_options(
      dom,
      document_url,
      fetcher,
      clock,
      options,
    )?;
    Ok(Self { host, event_loop })
  }

  pub fn from_renderer_dom(
    root: &crate::dom::DomNode,
    document_url: impl Into<String>,
  ) -> Result<Self> {
    Self::new(dom2::Document::from_renderer_dom(root), document_url)
  }

  pub fn from_renderer_dom_with_fetcher(
    root: &crate::dom::DomNode,
    document_url: impl Into<String>,
    fetcher: Arc<dyn ResourceFetcher>,
  ) -> Result<Self> {
    Self::new_with_fetcher(
      dom2::Document::from_renderer_dom(root),
      document_url,
      fetcher,
    )
  }

  pub fn host(&self) -> &WindowHostState {
    &self.host
  }

  pub fn host_mut(&mut self) -> &mut WindowHostState {
    &mut self.host
  }

  pub fn event_loop(&self) -> &EventLoop<WindowHostState> {
    &self.event_loop
  }

  pub fn event_loop_mut(&mut self) -> &mut EventLoop<WindowHostState> {
    &mut self.event_loop
  }

  pub fn set_console_sink(&mut self, sink: Option<ConsoleSink>) -> Result<()> {
    self.host.set_console_sink(sink)
  }

  pub fn queue_task<F>(&mut self, source: TaskSource, runnable: F) -> Result<()>
  where
    F: FnOnce(&mut WindowHostState, &mut EventLoop<WindowHostState>) -> Result<()> + 'static,
  {
    self.event_loop.queue_task(source, runnable)
  }

  pub fn perform_microtask_checkpoint(&mut self) -> Result<()> {
    self.event_loop.perform_microtask_checkpoint(&mut self.host)
  }

  pub fn run_until_idle(&mut self, limits: RunLimits) -> Result<RunUntilIdleOutcome> {
    self.event_loop.run_until_idle(&mut self.host, limits)
  }

  /// Execute a classic script in this window's JS realm.
  ///
  /// This installs the accompanying [`EventLoop`] into the vm-js hook payload so Web APIs like
  /// `queueMicrotask`, `setTimeout`, and `requestAnimationFrame` can schedule work.
  ///
  /// Note: this does **not** automatically run a microtask checkpoint. Call
  /// [`WindowHost::perform_microtask_checkpoint`] or drive the event loop as needed.
  pub fn exec_script(&mut self, source: &str) -> Result<vm_js::Value> {
    use crate::js::window_timers::VmJsEventLoopHooks;

    let (host, event_loop) = (&mut self.host, &mut self.event_loop);
    let mut hooks = VmJsEventLoopHooks::<WindowHostState>::new_with_host(host);
    hooks.set_event_loop(event_loop);
    let (vm_host, window) = host.vm_host_and_window_realm();
    let result = window.exec_script_with_host_and_hooks(vm_host, &mut hooks, source);

    if let Some(err) = hooks.finish(window.heap_mut()) {
      return Err(err);
    }

    match result {
      Ok(value) => Ok(value),
      Err(err) => Err(vm_error_format::vm_error_to_error(window.heap_mut(), err)),
    }
  }
}

/// Host state used by [`WindowHost`]'s event loop.
pub struct WindowHostState {
  pub document_url: String,
  /// Current document base URL used for resolving relative URLs.
  ///
  /// This is a host-level concept (HTML `Document.baseURI`) and is not stored in `dom2`.
  ///
  /// Prefer [`WindowHostState::set_document_base_url`] when mutating this value so the underlying
  /// [`WindowRealm`] stays in sync (JS reads base URL state from the realm).
  pub base_url: Option<String>,
  import_map_state: ImportMapState,
  import_map_warnings: Vec<ImportMapWarning>,
  import_map_errors: Vec<ImportMapError>,
  /// Host-owned document state used as the `vm-js` [`vm_js::VmHost`] context.
  document: DocumentHostState,
  window: WindowRealm,
  fetcher: Arc<dyn ResourceFetcher>,
  _fetch_bindings: WindowFetchBindings,
  _xhr_bindings: WindowXhrBindings,
  webidl_bindings_host: VmJsWebIdlBindingsHostDispatch<WindowHostState>,
  js_execution_options: JsExecutionOptions,
}

impl WindowHostState {
  pub fn new(dom: dom2::Document, document_url: impl Into<String>) -> Result<Self> {
    Self::new_with_fetcher_and_options(
      dom,
      document_url,
      Arc::new(HttpFetcher::new()),
      JsExecutionOptions::default(),
    )
  }

  pub fn new_with_fetcher(
    dom: dom2::Document,
    document_url: impl Into<String>,
    fetcher: Arc<dyn ResourceFetcher>,
  ) -> Result<Self> {
    let clock: Arc<dyn Clock> = Arc::new(RealClock::default());
    Self::new_with_fetcher_and_clock_and_options(
      dom,
      document_url,
      fetcher,
      clock,
      JsExecutionOptions::default(),
    )
  }

  pub fn new_with_fetcher_and_clock(
    dom: dom2::Document,
    document_url: impl Into<String>,
    fetcher: Arc<dyn ResourceFetcher>,
    clock: Arc<dyn Clock>,
  ) -> Result<Self> {
    Self::new_with_fetcher_and_clock_and_options(
      dom,
      document_url,
      fetcher,
      clock,
      JsExecutionOptions::default(),
    )
  }

  pub fn new_with_fetcher_and_options(
    dom: dom2::Document,
    document_url: impl Into<String>,
    fetcher: Arc<dyn ResourceFetcher>,
    js_execution_options: JsExecutionOptions,
  ) -> Result<Self> {
    let clock: Arc<dyn Clock> = Arc::new(RealClock::default());
    Self::new_with_fetcher_and_clock_and_options(
      dom,
      document_url,
      fetcher,
      clock,
      js_execution_options,
    )
  }

  pub fn new_with_fetcher_and_clock_and_options(
    dom: dom2::Document,
    document_url: impl Into<String>,
    fetcher: Arc<dyn ResourceFetcher>,
    clock: Arc<dyn Clock>,
    js_execution_options: JsExecutionOptions,
  ) -> Result<Self> {
    let document_url = document_url.into();
    let host_fetcher = fetcher.clone();
    let document = DocumentHostState::new(dom);
    let mut window = WindowRealm::new_with_js_execution_options(
      WindowRealmConfig::new(document_url.clone())
        .with_current_script_state(document.current_script_state().clone())
        .with_clock(clock),
      js_execution_options,
    )
    .map_err(|err| Error::Other(err.to_string()))?;
    window.set_cookie_fetcher(fetcher.clone());
    if js_execution_options.supports_module_scripts {
      let document_origin = origin_from_url(&document_url);
      if let Err(err) = window.enable_module_loader(fetcher.clone(), document_origin) {
        return Err(Error::Other(err.to_string()));
      }
    }

    // Install timer bindings (`setTimeout`, `setInterval`, `queueMicrotask`) so scripts executed in
    // this host can schedule work onto the accompanying `EventLoop`.
    let (fetch_bindings, xhr_bindings) = {
      let (vm, realm, heap) = window.vm_realm_and_heap_mut();
      if let Err(err) = install_window_timers_bindings::<WindowHostState>(vm, realm, heap) {
        return Err(Error::Other(err.to_string()));
      }
      if let Err(err) = install_window_animation_frame_bindings::<WindowHostState>(vm, realm, heap)
      {
        return Err(Error::Other(err.to_string()));
      }
      let fetch_bindings = match install_window_fetch_bindings_with_guard::<WindowHostState>(
        vm,
        realm,
        heap,
        WindowFetchEnv::for_document(Arc::clone(&host_fetcher), Some(document_url.clone())),
      ) {
        Ok(bindings) => bindings,
        Err(err) => {
          return Err(Error::Other(err.to_string()));
        }
      };

      let xhr_bindings = match install_window_xhr_bindings_with_guard::<WindowHostState>(
        vm,
        realm,
        heap,
        WindowXhrEnv::for_document(Arc::clone(&host_fetcher), Some(document_url.clone())),
      ) {
        Ok(bindings) => bindings,
        Err(err) => {
          return Err(Error::Other(err.to_string()));
        }
      };

      (fetch_bindings, xhr_bindings)
    };

    let webidl_bindings_host =
      VmJsWebIdlBindingsHostDispatch::<WindowHostState>::new(window.global_object());

    Ok(Self {
      base_url: Some(document_url.clone()),
      document_url,
      import_map_state: ImportMapState::new_empty(),
      import_map_warnings: Vec::new(),
      import_map_errors: Vec::new(),
      document,
      window,
      fetcher: host_fetcher,
      _fetch_bindings: fetch_bindings,
      _xhr_bindings: xhr_bindings,
      webidl_bindings_host,
      js_execution_options,
    })
  }

  pub fn from_renderer_dom(
    root: &crate::dom::DomNode,
    document_url: impl Into<String>,
  ) -> Result<Self> {
    Self::new(dom2::Document::from_renderer_dom(root), document_url)
  }

  pub fn from_renderer_dom_with_fetcher(
    root: &crate::dom::DomNode,
    document_url: impl Into<String>,
    fetcher: Arc<dyn ResourceFetcher>,
  ) -> Result<Self> {
    Self::new_with_fetcher(
      dom2::Document::from_renderer_dom(root),
      document_url,
      fetcher,
    )
  }

  pub fn dom(&self) -> &dom2::Document {
    self.document.dom()
  }

  pub fn dom_mut(&mut self) -> &mut dom2::Document {
    self.document.dom_mut()
  }

  pub fn document_host(&self) -> &DocumentHostState {
    &self.document
  }

  pub fn document_host_mut(&mut self) -> &mut DocumentHostState {
    &mut self.document
  }

  pub fn window(&self) -> &WindowRealm {
    &self.window
  }

  pub fn window_mut(&mut self) -> &mut WindowRealm {
    &mut self.window
  }

  pub(crate) fn fetcher(&self) -> &Arc<dyn ResourceFetcher> {
    &self.fetcher
  }

  pub fn js_execution_options(&self) -> JsExecutionOptions {
    self.js_execution_options
  }

  /// Update the document base URL (`Document.baseURI`) used for resolving relative URLs.
  ///
  /// This updates both the host-level `base_url` field and the underlying [`WindowRealm`] base URL
  /// state so JS-visible URL resolution (`document.baseURI`, `fetch("rel")`, module specifiers,
  /// etc.) remains coherent.
  pub fn set_document_base_url(&mut self, base_url: Option<String>) {
    self.base_url = base_url;
    // Keep the JS realm state in sync: `document.baseURI` and relative URL resolution in `fetch`
    // read from `WindowRealmUserData.base_url`.
    self.window.set_base_url(self.base_url.clone());
  }

  pub fn set_console_sink(&mut self, sink: Option<ConsoleSink>) -> Result<()> {
    self
      .window
      .set_console_sink(sink)
      .map_err(|err| Error::Other(err.to_string()))
  }

  pub fn import_map_state(&self) -> &ImportMapState {
    &self.import_map_state
  }

  pub fn import_map_state_mut(&mut self) -> &mut ImportMapState {
    &mut self.import_map_state
  }

  fn sync_import_map_state_to_module_loader(&mut self) {
    // `ModuleLoader` lives behind a `RefCell`, so we cannot return references into it. Keep the host
    // import map state as the canonical value, and copy it into the per-realm loader when module
    // loading is enabled.
    if self.window.vm().module_graph_ptr().is_none() {
      return;
    }
    let module_loader = self.window.module_loader_handle();
    let mut module_loader = module_loader.borrow_mut();
    *module_loader.import_map_state_mut() = self.import_map_state.clone();
  }

  pub fn import_maps(&self) -> &ImportMapState {
    self.import_map_state()
  }

  pub fn import_maps_mut(&mut self) -> &mut ImportMapState {
    self.import_map_state_mut()
  }

  pub fn take_import_map_warnings(&mut self) -> Vec<ImportMapWarning> {
    std::mem::take(&mut self.import_map_warnings)
  }

  pub fn take_import_map_errors(&mut self) -> Vec<ImportMapError> {
    std::mem::take(&mut self.import_map_errors)
  }

  pub fn register_import_map_string(
    &mut self,
    json: &str,
    base_url: &::url::Url,
  ) -> std::result::Result<
    Vec<crate::js::import_maps::ImportMapWarning>,
    crate::js::import_maps::ImportMapError,
  > {
    let limits = self.js_execution_options.import_map_limits;
    self.register_import_map_string_with_limits(json, base_url, &limits)
  }

  pub fn register_import_map_string_with_limits(
    &mut self,
    json: &str,
    base_url: &::url::Url,
    limits: &crate::js::import_maps::ImportMapLimits,
  ) -> std::result::Result<
    Vec<crate::js::import_maps::ImportMapWarning>,
    crate::js::import_maps::ImportMapError,
  > {
    let mut parse_result =
      crate::js::import_maps::create_import_map_parse_result_with_limits(json, base_url, limits);
    let warnings = std::mem::take(&mut parse_result.warnings);
    crate::js::import_maps::register_import_map_with_limits(
      self.import_map_state_mut(),
      parse_result,
      limits,
    )?;
    self.sync_import_map_state_to_module_loader();
    Ok(warnings)
  }

  pub fn register_import_map_from_script_text(
    &mut self,
    input: &str,
    base_url: &::url::Url,
  ) -> Result<()> {
    let limits = self.js_execution_options.import_map_limits;
    let mut result =
      crate::js::import_maps::create_import_map_parse_result_with_limits(input, base_url, &limits);
    self.import_map_warnings.append(&mut result.warnings);

    if let Err(err) = crate::js::import_maps::register_import_map_with_limits(
      self.import_map_state_mut(),
      result,
      &limits,
    ) {
      // For now, keep the host API stable and let higher-level HTML plumbing decide how to surface
      // import map errors (console, `window.onerror`, etc.).
      self.import_map_errors.push(err);
    } else {
      self.sync_import_map_state_to_module_loader();
    }

    Ok(())
  }

  pub fn register_import_map_using_document_base(&mut self, input: &str) -> Result<()> {
    let base_str = self.base_url.as_deref().unwrap_or(&self.document_url);
    let base_url = ::url::Url::parse(base_str).map_err(|err| {
      Error::Other(format!(
        "invalid document base URL {base_str:?} while registering import map: {err}"
      ))
    })?;
    self.register_import_map_from_script_text(input, &base_url)
  }

  pub fn resolve_module_specifier_with_import_maps(
    &mut self,
    specifier: &str,
    base_url: &::url::Url,
  ) -> std::result::Result<::url::Url, crate::js::import_maps::ImportMapError> {
    crate::js::import_maps::resolve_module_specifier(
      self.import_map_state_mut(),
      specifier,
      base_url,
    )
  }

  pub fn resolve_module_specifier(
    &mut self,
    specifier: &str,
    referrer_base: &::url::Url,
  ) -> std::result::Result<::url::Url, ModuleResolutionError> {
    crate::js::import_maps::resolve_module_specifier(
      self.import_map_state_mut(),
      specifier,
      referrer_base,
    )
  }

  pub fn resolve_module_specifier_using_document_base(
    &mut self,
    specifier: &str,
  ) -> std::result::Result<::url::Url, ModuleResolutionError> {
    let base_str = self.base_url.as_deref().unwrap_or(&self.document_url);
    let base_url = ::url::Url::parse(base_str).map_err(|err| {
      ModuleResolutionError::TypeError(format!(
        "invalid document base URL {base_str:?} while resolving module specifier: {err}"
      ))
    })?;
    self.resolve_module_specifier(specifier, &base_url)
  }

  pub fn resolve_module_integrity_metadata(&self, url: &::url::Url) -> &str {
    crate::js::import_maps::resolve_module_integrity_metadata(self.import_map_state(), url)
  }

  /// Execute a classic script while integrating Promise jobs into the provided [`EventLoop`]'s
  /// microtask queue.
  ///
  /// This is the lower-level form of [`WindowHost::exec_script`] for callers that already have a
  /// `(&mut WindowHostState, &mut EventLoop<WindowHostState>)` pair (e.g. inside an event-loop task).
  ///
  /// Note: this does **not** automatically run a microtask checkpoint. Drive the event loop or call
  /// [`EventLoop::perform_microtask_checkpoint`] as needed.
  pub fn exec_script_in_event_loop(
    &mut self,
    event_loop: &mut EventLoop<WindowHostState>,
    source: &str,
  ) -> Result<vm_js::Value> {
    use crate::js::window_timers::VmJsEventLoopHooks;

    let mut hooks = VmJsEventLoopHooks::<WindowHostState>::new_with_host(self);
    hooks.set_event_loop(event_loop);
    let (vm_host, window) = self.vm_host_and_window_realm();
    let result = window.exec_script_with_host_and_hooks(vm_host, &mut hooks, source);

    if let Some(err) = hooks.finish(window.heap_mut()) {
      return Err(err);
    }

    match result {
      Ok(value) => Ok(value),
      Err(err) => Err(vm_error_format::vm_error_to_error(window.heap_mut(), err)),
    }
  }

  /// Execute a classic script (with an explicit source name) while integrating Promise jobs into the
  /// provided [`EventLoop`]'s microtask queue.
  pub fn exec_script_with_name_in_event_loop(
    &mut self,
    event_loop: &mut EventLoop<WindowHostState>,
    source_name: impl Into<Arc<str>>,
    source_text: impl Into<Arc<str>>,
  ) -> Result<vm_js::Value> {
    use crate::js::window_timers::VmJsEventLoopHooks;

    let source = Arc::new(vm_js::SourceText::new(source_name, source_text));
    let mut hooks = VmJsEventLoopHooks::<WindowHostState>::new_with_host(self);
    hooks.set_event_loop(event_loop);
    let (vm_host, window) = self.vm_host_and_window_realm();
    let result = window.exec_script_source_with_host_and_hooks(vm_host, &mut hooks, source);

    if let Some(err) = hooks.finish(window.heap_mut()) {
      return Err(err);
    }

    match result {
      Ok(value) => Ok(value),
      Err(err) => Err(vm_error_format::vm_error_to_error(window.heap_mut(), err)),
    }
  }
}

impl DomHost for WindowHostState {
  fn with_dom<R, F>(&self, f: F) -> R
  where
    F: FnOnce(&dom2::Document) -> R,
  {
    self.document.with_dom(f)
  }

  fn mutate_dom<R, F>(&mut self, f: F) -> R
  where
    F: FnOnce(&mut dom2::Document) -> (R, bool),
  {
    self.document.mutate_dom(f)
  }
}

impl CurrentScriptHost for WindowHostState {
  fn current_script_state(&self) -> &crate::js::CurrentScriptStateHandle {
    self.document.current_script_state()
  }
}

impl WindowRealmHost for WindowHostState {
  fn vm_host_and_window_realm(&mut self) -> (&mut dyn vm_js::VmHost, &mut WindowRealm) {
    (&mut self.document, &mut self.window)
  }

  fn webidl_bindings_host(&mut self) -> Option<&mut dyn webidl_vm_js::WebIdlBindingsHost> {
    Some(&mut self.webidl_bindings_host)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  use crate::resource::FetchedResource;
  use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
  use base64::Engine as _;
  use selectors::context::QuirksMode;
  use sha2::{Digest, Sha256};
  use std::collections::HashMap;
  use std::io::{Read, Write};
  use std::net::TcpListener;
  use std::sync::atomic::{AtomicUsize, Ordering};
  use std::sync::Mutex;
  use std::time::{Duration, Instant};
  use vm_js::{
    GcObject, PropertyDescriptor, PropertyKey, PropertyKind, Scope, TerminationReason, Value, Vm,
    VmError, VmHost, VmHostHooks,
  };

  fn get_global_prop(host: &mut WindowHost, name: &str) -> Value {
    let window = host.host_mut().window_mut();
    let (_vm, realm, heap) = window.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let global = realm.global_object();
    scope
      .push_root(Value::Object(global))
      .expect("push root global");
    let key_s = scope.alloc_string(name).expect("alloc prop name");
    scope
      .push_root(Value::String(key_s))
      .expect("push root prop name");
    let key = PropertyKey::from_string(key_s);
    scope
      .heap()
      .object_get_own_data_property_value(global, &key)
      .expect("get prop")
      .unwrap_or(Value::Undefined)
  }

  fn get_global_prop_utf8(host: &mut WindowHost, name: &str) -> Option<String> {
    let value = get_global_prop(host, name);
    let window = host.host_mut().window_mut();
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

  fn value_to_string(host: &WindowHost, value: Value) -> String {
    let Value::String(s) = value else {
      panic!("expected a string, got {value:?}");
    };
    host
      .host()
      .window()
      .heap()
      .get_string(s)
      .expect("heap should contain string")
      .to_utf8_lossy()
  }

  fn value_to_string_from_host_state(host: &WindowHostState, value: Value) -> String {
    let Value::String(s) = value else {
      panic!("expected a string, got {value:?}");
    };
    host
      .window()
      .heap()
      .get_string(s)
      .expect("heap should contain string")
      .to_utf8_lossy()
  }

  fn get_global_prop_host_state(host: &mut WindowHostState, name: &str) -> Value {
    let window = host.window_mut();
    let (_vm, realm, heap) = window.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let global = realm.global_object();
    scope
      .push_root(Value::Object(global))
      .expect("push root global");
    let key_s = scope.alloc_string(name).expect("alloc prop name");
    scope
      .push_root(Value::String(key_s))
      .expect("push root prop name");
    let key = PropertyKey::from_string(key_s);
    scope
      .heap()
      .object_get_own_data_property_value(global, &key)
      .expect("get global prop")
      .unwrap_or(Value::Undefined)
  }

  fn get_global_prop_utf8_host_state(host: &mut WindowHostState, name: &str) -> Option<String> {
    let value = get_global_prop_host_state(host, name);
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

  #[derive(Default)]
  struct MapResourceFetcher {
    entries: Mutex<HashMap<String, FetchedResource>>,
  }

  impl MapResourceFetcher {
    fn insert(&self, url: &str, resource: FetchedResource) {
      self
        .entries
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(url.to_string(), resource);
    }
  }

  impl ResourceFetcher for MapResourceFetcher {
    fn fetch(&self, url: &str) -> Result<FetchedResource> {
      self
        .entries
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(url)
        .cloned()
        .ok_or_else(|| Error::Other(format!("no entry for url={url}")))
    }
  }

  #[derive(Default)]
  struct RecordingFetcher {
    calls: Mutex<Vec<String>>,
  }

  impl RecordingFetcher {
    fn calls(&self) -> Vec<String> {
      self
        .calls
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
    }

    fn ok_response(url: &str) -> FetchedResource {
      let mut res = FetchedResource::new(b"ok".to_vec(), Some("text/plain".to_string()));
      res.status = Some(200);
      res.final_url = Some(url.to_string());
      res
    }
  }

  impl ResourceFetcher for RecordingFetcher {
    fn fetch(&self, url: &str) -> Result<FetchedResource> {
      self
        .calls
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(url.to_string());
      Ok(Self::ok_response(url))
    }

    fn fetch_http_request(&self, req: crate::resource::HttpRequest<'_>) -> Result<FetchedResource> {
      let url = req.fetch.url.to_string();
      self
        .calls
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(url.clone());
      Ok(Self::ok_response(&url))
    }
  }

  #[test]
  fn window_realm_respects_max_stack_depth() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new_with_options(
      dom,
      "https://example.invalid/",
      JsExecutionOptions {
        max_stack_depth: Some(16),
        ..JsExecutionOptions::default()
      },
    )?;
    let window = host.host_mut().window_mut();
    let err = window
      .exec_script("function f(){return f()} f()")
      .expect_err("expected recursion to terminate");
    match err {
      VmError::Termination(term) => {
        assert_eq!(term.reason, TerminationReason::StackOverflow);
        assert_eq!(term.stack.len(), 16);
      }
      other => panic!("expected stack overflow termination, got {other:?}"),
    }
    Ok(())
  }

  #[test]
  fn window_realm_respects_max_vm_heap_bytes() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let limit = 7 * 1024 * 1024;
    let host = WindowHost::new_with_options(
      dom,
      "https://example.invalid/",
      JsExecutionOptions {
        max_vm_heap_bytes: Some(limit),
        ..JsExecutionOptions::default()
      },
    )?;
    let limits = host.host().window().heap().limits();
    assert_eq!(limits.max_bytes, limit);
    assert_eq!(limits.gc_threshold, (limit / 2).min(limit));
    Ok(())
  }

  #[test]
  fn generated_vmjs_url_search_params_installer_is_idempotent_and_does_not_clobber_dom(
  ) -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;

    let before = get_global_prop(&mut host, "URLSearchParams");
    {
      let window = host.host_mut().window_mut();
      let (vm, realm, heap) = window.vm_realm_and_heap_mut();
      crate::js::bindings::install_url_search_params_bindings_vm_js(vm, heap, realm)
        .map_err(|err| Error::Other(err.to_string()))?;
    }
    let after = get_global_prop(&mut host, "URLSearchParams");
    assert_eq!(
      before, after,
      "expected URLSearchParams installer to be idempotent (no clobber)"
    );

    let out = host.exec_script("new URLSearchParams('a=1').get('a')")?;
    assert_eq!(value_to_string(&host, out), "1");

    let el = host.exec_script("document.createElement('div')")?;
    assert!(
      matches!(el, Value::Object(_)),
      "expected document.createElement('div') to return an object"
    );

    Ok(())
  }

  #[test]
  fn generated_vmjs_window_ops_installer_does_not_clobber_existing_timers() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;

    let before = get_global_prop(&mut host, "setTimeout");
    {
      let window = host.host_mut().window_mut();
      let (vm, realm, heap) = window.vm_realm_and_heap_mut();
      crate::js::bindings::install_window_ops_bindings_vm_js(vm, heap, realm)
        .map_err(|err| Error::Other(err.to_string()))?;
    }
    let after = get_global_prop(&mut host, "setTimeout");
    assert_eq!(
      before, after,
      "expected Window ops installer to avoid clobbering existing setTimeout"
    );

    let el = host.exec_script("document.createElement('div')")?;
    assert!(
      matches!(el, Value::Object(_)),
      "expected document.createElement('div') to return an object"
    );

    Ok(())
  }

  #[test]
  fn generated_vmjs_node_installer_can_patch_prototype_chain_after_event_target_install(
  ) -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;
    {
      // Ensure we start from a clean slate even if other tests installed these bindings.
      let window = host.host_mut().window_mut();
      let (_vm, realm, heap) = window.vm_realm_and_heap_mut();
      let mut scope = heap.scope();
      let global = realm.global_object();
      scope
        .push_root(Value::Object(global))
        .map_err(|err| Error::Other(err.to_string()))?;
      for name in ["EventTarget", "Node"] {
        let key_s = scope
          .alloc_string(name)
          .map_err(|err| Error::Other(err.to_string()))?;
        scope
          .push_root(Value::String(key_s))
          .map_err(|err| Error::Other(err.to_string()))?;
        let key = PropertyKey::from_string(key_s);
        scope
          .delete_property_or_throw(global, key)
          .map_err(|err| Error::Other(err.to_string()))?;
      }
    }
    {
      let window = host.host_mut().window_mut();
      let (vm, realm, heap) = window.vm_realm_and_heap_mut();
      // Install Node first (without EventTarget present), then install EventTarget, then rerun the
      // Node installer. The second run should patch `Node.prototype` to inherit from
      // `EventTarget.prototype`.
      crate::js::bindings::install_node_bindings_vm_js(vm, heap, realm)
        .map_err(|err| Error::Other(err.to_string()))?;
      crate::js::bindings::install_event_target_bindings_vm_js(vm, heap, realm)
        .map_err(|err| Error::Other(err.to_string()))?;
      crate::js::bindings::install_node_bindings_vm_js(vm, heap, realm)
        .map_err(|err| Error::Other(err.to_string()))?;
    }

    let out =
      host.exec_script("Object.getPrototypeOf(Node.prototype) === EventTarget.prototype")?;
    assert_eq!(out, Value::Bool(true));

    Ok(())
  }

  #[test]
  fn window_host_state_exec_script_in_event_loop_sets_webidl_bindings_host_slot() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;

    // WindowRealm installs handcrafted URL bindings by default (`src/js/vmjs/window_url.rs`), which
    // do not use the WebIDL host slot. The generated bindings are idempotent and intentionally do
    // not clobber existing globals, so delete the existing constructors first to ensure the
    // executed script hits `webidl_vm_js::host_from_hooks()`.
    {
      let window = host.host_mut().window_mut();
      let (_vm, realm, heap) = window.vm_realm_and_heap_mut();
      let mut scope = heap.scope();
      let global = realm.global_object();
      scope
        .push_root(Value::Object(global))
        .map_err(|err| Error::Other(err.to_string()))?;
      for name in ["EventTarget", "URL", "URLSearchParams"] {
        let key_s = scope
          .alloc_string(name)
          .map_err(|err| Error::Other(err.to_string()))?;
        scope
          .push_root(Value::String(key_s))
          .map_err(|err| Error::Other(err.to_string()))?;
        let key = PropertyKey::from_string(key_s);
        scope
          .delete_property_or_throw(global, key)
          .map_err(|err| Error::Other(err.to_string()))?;
      }
    }
    {
      let window = host.host_mut().window_mut();
      let (vm, realm, heap) = window.vm_realm_and_heap_mut();
      crate::js::bindings::install_window_bindings_vm_js(vm, heap, realm)
        .map_err(|err| Error::Other(err.to_string()))?;
    }

    let got = {
      let (host_state, event_loop) = (&mut host.host, &mut host.event_loop);
      host_state.exec_script_in_event_loop(event_loop, "new URLSearchParams('a=1').get('a')")?
    };
    assert_eq!(value_to_string(&host, got), "1");

    let got = {
      let (host_state, event_loop) = (&mut host.host, &mut host.event_loop);
      host_state.exec_script_in_event_loop(
        event_loop,
        "new URLSearchParams([['a','1'],['b','2']]).get('b')",
      )?
    };
    assert_eq!(value_to_string(&host, got), "2");

    let got = {
      let (host_state, event_loop) = (&mut host.host, &mut host.event_loop);
      host_state.exec_script_in_event_loop(event_loop, "new URLSearchParams({a:'1'}).get('a')")?
    };
    assert_eq!(value_to_string(&host, got), "1");

    let got = {
      let (host_state, event_loop) = (&mut host.host, &mut host.event_loop);
      host_state.exec_script_in_event_loop(
        event_loop,
        "new URLSearchParams(new URLSearchParams('a=1')).get('a')",
      )?
    };
    assert_eq!(value_to_string(&host, got), "1");

    let got = {
      let (host_state, event_loop) = (&mut host.host, &mut host.event_loop);
      host_state
        .exec_script_in_event_loop(event_loop, "new URLSearchParams('a=1').keys().next().value")?
    };
    assert_eq!(value_to_string(&host, got), "a");

    let got = {
      let (host_state, event_loop) = (&mut host.host, &mut host.event_loop);
      host_state.exec_script_in_event_loop(event_loop, "URL.canParse('https://example.com/')")?
    };
    assert_eq!(got, Value::Bool(true));

    let got = {
      let (host_state, event_loop) = (&mut host.host, &mut host.event_loop);
      host_state.exec_script_in_event_loop(event_loop, "URL.parse('nope')")?
    };
    assert_eq!(got, Value::Null);

    let got = {
      let (host_state, event_loop) = (&mut host.host, &mut host.event_loop);
      host_state.exec_script_in_event_loop(event_loop, "URL.parse('https://example.com/').href")?
    };
    assert_eq!(value_to_string(&host, got), "https://example.com/");

    let got = {
      let (host_state, event_loop) = (&mut host.host, &mut host.event_loop);
      host_state.exec_script_in_event_loop(
        event_loop,
        r#"
        (() => {
          let t = new EventTarget();
          let n = 0;
          function f() { n++; }
          t.addEventListener('x', f);
          t.dispatchEvent({ type: 'x' });
          t.removeEventListener('x', f);
          t.dispatchEvent({ type: 'x' });
          return n;
        })()
        "#,
      )?
    };
    assert!(matches!(got, Value::Number(n) if n == 1.0));

    let got = {
      let (host_state, event_loop) = (&mut host.host, &mut host.event_loop);
      host_state.exec_script_in_event_loop(
        event_loop,
        r#"
        (() => {
          let t = new EventTarget();
          let n = 0;
          function f() { n++; }
          t.addEventListener('x', f, { once: true });
          t.dispatchEvent({ type: 'x' });
          t.dispatchEvent({ type: 'x' });
          return n;
        })()
        "#,
      )?
    };
    assert!(matches!(got, Value::Number(n) if n == 1.0));

    let got = {
      let (host_state, event_loop) = (&mut host.host, &mut host.event_loop);
      host_state.exec_script_with_name_in_event_loop(
        event_loop,
        "<test>",
        "new URLSearchParams('a=1').get('a')",
      )?
    };
    assert_eq!(value_to_string(&host, got), "1");

    Ok(())
  }

  #[test]
  fn webidl_window_timers_and_queue_microtask_run_via_event_loop() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;
    {
      let window = host.host_mut().window_mut();
      let (_vm, realm, heap) = window.vm_realm_and_heap_mut();
      let mut scope = heap.scope();
      let global = realm.global_object();
      scope
        .push_root(Value::Object(global))
        .map_err(|err| Error::Other(err.to_string()))?;
      for name in ["queueMicrotask", "setTimeout"] {
        let key_s = scope
          .alloc_string(name)
          .map_err(|err| Error::Other(err.to_string()))?;
        scope
          .push_root(Value::String(key_s))
          .map_err(|err| Error::Other(err.to_string()))?;
        let key = PropertyKey::from_string(key_s);
        scope
          .delete_property_or_throw(global, key)
          .map_err(|err| Error::Other(err.to_string()))?;
      }
    }
    {
      let window = host.host_mut().window_mut();
      let (vm, realm, heap) = window.vm_realm_and_heap_mut();
      crate::js::bindings::install_window_ops_bindings_vm_js(vm, heap, realm)
        .map_err(|err| Error::Other(err.to_string()))?;
    }

    host.exec_script(
      r#"
      globalThis.__micro = 0;
      globalThis.__timeout = 0;
      queueMicrotask(() => { globalThis.__micro = 1; });
      setTimeout(() => { globalThis.__timeout = 1; }, 0);
      "#,
    )?;

    assert!(matches!(
      get_global_prop(&mut host, "__micro"),
      Value::Number(n) if n == 0.0
    ));
    assert!(matches!(
      get_global_prop(&mut host, "__timeout"),
      Value::Number(n) if n == 0.0
    ));

    host.perform_microtask_checkpoint()?;

    assert!(matches!(
      get_global_prop(&mut host, "__micro"),
      Value::Number(n) if n == 1.0
    ));
    assert!(matches!(
      get_global_prop(&mut host, "__timeout"),
      Value::Number(n) if n == 0.0
    ));

    host.run_until_idle(RunLimits {
      max_tasks: 10,
      max_microtasks: 100,
      max_wall_time: Some(Duration::from_secs(1)),
    })?;

    assert!(matches!(
      get_global_prop(&mut host, "__timeout"),
      Value::Number(n) if n == 1.0
    ));
    Ok(())
  }

  #[test]
  fn window_host_state_registers_import_maps_and_respects_resolved_module_set() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHostState::new(dom, "https://example.invalid/base/page.html")?;

    host.register_import_map_using_document_base(r#"{"imports":{"foo":"/mapped.js"}}"#)?;

    let mapped = host
      .resolve_module_specifier_using_document_base("foo")
      .expect("resolve mapped bare specifier");
    assert_eq!(mapped.as_str(), "https://example.invalid/mapped.js");
    assert_eq!(host.import_maps().resolved_module_set().len(), 1);

    // URL-like specifiers resolve without an import map rule, but still participate in the resolved
    // module set (so later import map registrations cannot change their resolution).
    let direct = host
      .resolve_module_specifier_using_document_base("/direct.js")
      .expect("resolve url-like specifier");
    assert_eq!(direct.as_str(), "https://example.invalid/direct.js");
    assert_eq!(host.import_maps().resolved_module_set().len(), 2);

    // A later import map that would change the already-resolved module is ignored.
    host.register_import_map_using_document_base(r#"{"imports":{"/direct.js":"/changed.js"}}"#)?;
    let direct_again = host
      .resolve_module_specifier_using_document_base("/direct.js")
      .expect("resolve url-like specifier again");
    assert_eq!(direct_again, direct);

    Ok(())
  }

  #[test]
  fn window_host_state_set_document_base_url_updates_document_base_uri() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHostState::new(dom, "https://example.invalid/a/b.html")?;
    host.set_document_base_url(Some("https://example.invalid/dir/".to_string()));
    let mut event_loop = EventLoop::<WindowHostState>::new();
    let base_uri = host.exec_script_in_event_loop(&mut event_loop, "document.baseURI")?;
    assert_eq!(
      value_to_string_from_host_state(&host, base_uri),
      "https://example.invalid/dir/"
    );
    Ok(())
  }

  #[test]
  fn window_host_state_set_document_base_url_affects_fetch_relative_urls() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let fetcher = Arc::new(RecordingFetcher::default());
    let mut host =
      WindowHostState::new_with_fetcher(dom, "https://example.invalid/a/b.html", fetcher.clone())?;
    host.set_document_base_url(Some("https://example.invalid/dir/".to_string()));
    let mut event_loop = EventLoop::<WindowHostState>::new();

    host.exec_script_in_event_loop(
      &mut event_loop,
      r#"
      var g = this;
      g.__err = "";
      g.__text = "";
      fetch("x")
        .then(function (r) { return r.text(); })
        .then(function (t) { g.__text = t; })
        .catch(function (e) { g.__err = String(e && (e.stack || e.message) || e); });
      "#,
    )?;

    event_loop.run_until_idle(
      &mut host,
      RunLimits {
        max_tasks: 10,
        max_microtasks: 100,
        max_wall_time: Some(Duration::from_secs(5)),
      },
    )?;

    assert_eq!(
      get_global_prop_utf8_host_state(&mut host, "__err").unwrap_or_default(),
      ""
    );
    assert_eq!(
      get_global_prop_utf8_host_state(&mut host, "__text").as_deref(),
      Some("ok")
    );
    assert_eq!(
      fetcher.calls(),
      vec!["https://example.invalid/dir/x".to_string()]
    );
    Ok(())
  }

  #[test]
  fn window_host_dynamic_import_works_when_module_scripts_supported() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let fetcher = Arc::new(MapResourceFetcher::default());
    fetcher.insert(
      "https://example.invalid/mod.js",
      FetchedResource::new(
        "export default 42;".as_bytes().to_vec(),
        Some("application/javascript".to_string()),
      ),
    );

    let mut options = JsExecutionOptions::default();
    options.supports_module_scripts = true;
    let mut host =
      WindowHost::new_with_fetcher_and_options(dom, "https://example.invalid/", fetcher, options)?;

    host.exec_script(
      r#"
      globalThis.__x = 0;
      globalThis.__err = "";
      import("https://example.invalid/mod.js")
        .then(m => { globalThis.__x = m.default; })
        .catch(e => { globalThis.__err = String(e && e.message || e); });
      "#,
    )?;

    let _ = host.run_until_idle(RunLimits {
      max_tasks: 10,
      max_microtasks: 100,
      max_wall_time: Some(Duration::from_secs(5)),
    })?;

    assert_eq!(
      get_global_prop_utf8(&mut host, "__err").unwrap_or_default(),
      ""
    );
    assert!(matches!(
      get_global_prop(&mut host, "__x"),
      Value::Number(n) if n == 42.0
    ));
    Ok(())
  }

  #[test]
  fn window_host_dynamic_import_resolves_relative_specifiers_against_script_url() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let fetcher = Arc::new(MapResourceFetcher::default());
    fetcher.insert(
      "https://example.invalid/scripts/mod.js",
      FetchedResource::new(
        "export const url = import.meta.url; export default 42;"
          .as_bytes()
          .to_vec(),
        Some("application/javascript".to_string()),
      ),
    );

    let mut options = JsExecutionOptions::default();
    options.supports_module_scripts = true;
    let mut host = WindowHost::new_with_fetcher_and_options(
      dom,
      "https://example.invalid/index.html",
      fetcher,
      options,
    )?;

    // Execute a classic script with an explicit URL "source name" so dynamic `import()` can resolve
    // relative specifiers against the script URL (not the document URL).
    host.host.exec_script_with_name_in_event_loop(
      &mut host.event_loop,
      "https://example.invalid/scripts/main.js",
      r#"
      globalThis.__x = 0;
      globalThis.__url = "";
      globalThis.__err = "";
      import("./mod.js")
        .then(m => { globalThis.__x = m.default; globalThis.__url = m.url; })
        .catch(e => { globalThis.__err = String(e && e.message || e); });
      "#,
    )?;

    let _ = host.run_until_idle(RunLimits {
      max_tasks: 10,
      max_microtasks: 100,
      max_wall_time: Some(Duration::from_secs(5)),
    })?;

    assert_eq!(
      get_global_prop_utf8(&mut host, "__err").unwrap_or_default(),
      ""
    );
    assert!(matches!(
      get_global_prop(&mut host, "__x"),
      Value::Number(n) if n == 42.0
    ));
    assert_eq!(
      get_global_prop_utf8(&mut host, "__url").as_deref(),
      Some("https://example.invalid/scripts/mod.js")
    );
    Ok(())
  }

  #[test]
  fn window_host_dynamic_import_enforces_module_graph_module_count_budget() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let fetcher = Arc::new(MapResourceFetcher::default());
    fetcher.insert(
      "https://example.invalid/mod.js",
      FetchedResource::new(
        "import './dep.js'; export default 42;".as_bytes().to_vec(),
        Some("application/javascript".to_string()),
      ),
    );
    fetcher.insert(
      "https://example.invalid/dep.js",
      FetchedResource::new(
        "export const x = 1;".as_bytes().to_vec(),
        Some("application/javascript".to_string()),
      ),
    );

    let mut options = JsExecutionOptions::default();
    options.supports_module_scripts = true;
    options.max_module_graph_modules = 1; // only the entry module allowed
    let mut host =
      WindowHost::new_with_fetcher_and_options(dom, "https://example.invalid/", fetcher, options)?;

    host.exec_script(
      r#"
      globalThis.__x = 0;
      globalThis.__err = "";
      import("https://example.invalid/mod.js")
        .then(m => { globalThis.__x = m.default; })
        .catch(e => { globalThis.__err = String(e && e.message || e); });
      "#,
    )?;

    let _ = host.run_until_idle(RunLimits {
      max_tasks: 10,
      max_microtasks: 100,
      max_wall_time: Some(Duration::from_secs(5)),
    })?;

    let err = get_global_prop_utf8(&mut host, "__err").unwrap_or_default();
    assert!(
      err.contains("max_module_graph_modules"),
      "expected module count budget error, got: {err:?}"
    );
    assert!(matches!(get_global_prop(&mut host, "__x"), Value::Number(n) if n == 0.0));
    Ok(())
  }

  #[test]
  fn window_host_dynamic_import_resolves_bare_specifiers_via_import_maps() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let fetcher = Arc::new(MapResourceFetcher::default());
    fetcher.insert(
      "https://example.invalid/mod.js",
      FetchedResource::new(
        "export default 42;".as_bytes().to_vec(),
        Some("application/javascript".to_string()),
      ),
    );

    let mut options = JsExecutionOptions::default();
    options.supports_module_scripts = true;
    let mut host =
      WindowHost::new_with_fetcher_and_options(dom, "https://example.invalid/", fetcher, options)?;

    host.host_mut().register_import_map_using_document_base(
      r#"{"imports":{"foo":"https://example.invalid/mod.js"}}"#,
    )?;

    host.exec_script(
      r#"
      globalThis.__x = 0;
      globalThis.__err = "";
      import("foo")
        .then(m => { globalThis.__x = m.default; })
        .catch(e => { globalThis.__err = String(e && e.message || e); });
      "#,
    )?;

    let _ = host.run_until_idle(RunLimits {
      max_tasks: 10,
      max_microtasks: 100,
      max_wall_time: Some(Duration::from_secs(5)),
    })?;

    assert_eq!(
      get_global_prop_utf8(&mut host, "__err").unwrap_or_default(),
      ""
    );
    assert!(matches!(
      get_global_prop(&mut host, "__x"),
      Value::Number(n) if n == 42.0
    ));
    Ok(())
  }

  #[test]
  fn window_host_dynamic_import_rejects_when_module_scripts_not_supported() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let fetcher = Arc::new(MapResourceFetcher::default());

    let mut options = JsExecutionOptions::default();
    options.supports_module_scripts = false;
    let mut host =
      WindowHost::new_with_fetcher_and_options(dom, "https://example.invalid/", fetcher, options)?;

    host.exec_script(
      r#"
      globalThis.__x = 0;
      globalThis.__err = "";
      import("https://example.invalid/mod.js")
        .then(() => { globalThis.__x = 1; })
        .catch(e => { globalThis.__err = String(e && e.message || e); });
      "#,
    )?;

    let _ = host.run_until_idle(RunLimits {
      max_tasks: 10,
      max_microtasks: 100,
      max_wall_time: Some(Duration::from_secs(5)),
    })?;

    let err = get_global_prop_utf8(&mut host, "__err").unwrap_or_default();
    assert!(
      err.contains("module loading is not enabled for this realm"),
      "unexpected error: {err:?}"
    );
    assert!(matches!(get_global_prop(&mut host, "__x"), Value::Number(n) if n == 0.0));
    Ok(())
  }

  #[test]
  fn window_host_exec_script_exposes_document_current_script_via_host_context() -> Result<()> {
    let renderer_dom =
      crate::dom::parse_html("<!doctype html><html><body><script id=\"s\"></script></body></html>")
        .expect("parse_html");
    let mut host = WindowHost::from_renderer_dom(&renderer_dom, "https://example.invalid/")?;

    let no_current = host.exec_script("document.currentScript === null")?;
    assert_eq!(no_current, Value::Bool(true));

    let script_node = host
      .host()
      .dom()
      .get_element_by_id("s")
      .expect("expected #s script element");
    let current_script_state = host.host().document_host().current_script_handle().clone();
    let mut orchestrator = crate::js::ScriptOrchestrator::new();
    orchestrator.execute_with_current_script_state_resolved(
      &current_script_state,
      Some(script_node),
      || {
        let has_current = host.exec_script(
          "document.currentScript && document.currentScript.getAttribute('id') === 's'",
        )?;
        assert_eq!(has_current, Value::Bool(true));
        Ok(())
      },
    )?;

    let restored = host.exec_script("document.currentScript === null")?;
    assert_eq!(restored, Value::Bool(true));
    Ok(())
  }

  #[derive(Default)]
  struct CookieRecordingFetcher {
    cookies: Mutex<Vec<(String, String)>>,
  }

  impl CookieRecordingFetcher {
    fn cookie_header(&self) -> Option<String> {
      let lock = self
        .cookies
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
      if lock.is_empty() {
        return None;
      }
      Some(
        lock
          .iter()
          .map(|(name, value)| format!("{name}={value}"))
          .collect::<Vec<_>>()
          .join("; "),
      )
    }
  }

  impl ResourceFetcher for CookieRecordingFetcher {
    fn fetch(&self, url: &str) -> Result<FetchedResource> {
      Err(Error::Other(format!(
        "CookieRecordingFetcher does not support fetch: {url}"
      )))
    }

    fn cookie_header_value(&self, _url: &str) -> Option<String> {
      self.cookie_header()
    }

    fn store_cookie_from_document(&self, _url: &str, cookie_string: &str) {
      let first = cookie_string
        .split_once(';')
        .map(|(a, _)| a)
        .unwrap_or(cookie_string);
      let first = first.trim_matches(|c: char| c.is_ascii_whitespace());
      let Some((name, value)) = first.split_once('=') else {
        return;
      };
      let name = name.trim_matches(|c: char| c.is_ascii_whitespace());
      if name.is_empty() {
        return;
      }

      let mut lock = self
        .cookies
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
      if let Some(existing) = lock.iter_mut().find(|(n, _)| n == name) {
        existing.1 = value.to_string();
      } else {
        lock.push((name.to_string(), value.to_string()));
      }
    }
  }

  fn accept_with_deadline(
    listener: &TcpListener,
    deadline: Instant,
  ) -> std::io::Result<std::net::TcpStream> {
    use std::io::ErrorKind;

    loop {
      match listener.accept() {
        Ok((stream, _)) => return Ok(stream),
        Err(err) if err.kind() == ErrorKind::WouldBlock => {
          if Instant::now() >= deadline {
            return Err(std::io::Error::new(
              std::io::ErrorKind::TimedOut,
              "accept timed out",
            ));
          }
          std::thread::sleep(Duration::from_millis(10));
        }
        Err(err) => return Err(err),
      }
    }
  }

  fn read_http_request(stream: &mut std::net::TcpStream) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
      let n = stream.read(&mut tmp)?;
      if n == 0 {
        break;
      }
      buf.extend_from_slice(&tmp[..n]);
      if buf.windows(4).any(|w| w == b"\r\n\r\n") {
        break;
      }
      if buf.len() > 64 * 1024 {
        break;
      }
    }
    Ok(buf)
  }

  #[test]
  fn fetch_thenable_assimilation_runs_with_real_vm_host() -> Result<()> {
    let Ok(listener) = TcpListener::bind("127.0.0.1:0") else {
      // Some sandboxed CI environments may forbid binding sockets; skip in that case.
      return Ok(());
    };
    listener.set_nonblocking(true).expect("set_nonblocking");
    let addr = listener.local_addr().expect("local_addr");
    let url = format!("http://{addr}/");
    let server = std::thread::spawn(move || {
      let deadline = Instant::now() + Duration::from_secs(5);
      let mut stream = accept_with_deadline(&listener, deadline).expect("accept request");
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .expect("set_read_timeout");
      let _req = read_http_request(&mut stream).expect("read request");
      let body = b"ok";
      let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
      );
      stream.write_all(headers.as_bytes()).expect("write headers");
      stream.write_all(body).expect("write body");
    });

    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let fetcher = Arc::new(HttpFetcher::new().with_timeout(Duration::from_secs(2)));
    let mut host = WindowHost::new_with_fetcher(dom, url, fetcher)?;

    // Install the `recordHost` native into the global object so JS can assert a real VmHost is
    // threaded through thenable assimilation.
    install_record_host(&mut host);

    host.exec_script(
      r#"
      globalThis.__host_ok = false;
      globalThis.__result = "";
      globalThis.__err = "";

      // Make Response thenable so Promise resolution runs thenable assimilation during fetch's
      // internal `resolve(response)` call.
      Response.prototype.then = function(resolve, reject) {
        globalThis.__host_ok = recordHost();
        resolve("thenable_ok");
      };

      fetch("/")
        .then(v => { globalThis.__result = v; })
        .catch(e => { globalThis.__err = String(e && e.stack || e); });
      "#,
    )?;

    host.run_until_idle(RunLimits {
      max_tasks: 10,
      max_microtasks: 100,
      max_wall_time: Some(Duration::from_secs(5)),
    })?;

    assert!(
      get_global_prop_utf8(&mut host, "__err")
        .unwrap_or_default()
        .is_empty(),
      "fetch thenable test errored: {:?}",
      get_global_prop_utf8(&mut host, "__err")
    );
    assert!(matches!(
      get_global_prop(&mut host, "__host_ok"),
      Value::Bool(true)
    ));
    assert_eq!(
      get_global_prop_utf8(&mut host, "__result").as_deref(),
      Some("thenable_ok")
    );

    server.join().expect("server thread panicked");
    Ok(())
  }

  #[test]
  fn element_dataset_and_style_reflect_to_dom_attributes() -> Result<()> {
    // Build a tiny DOM with a single element so `document.getElementById` can find it.
    let mut dom = dom2::Document::new(QuirksMode::NoQuirks);
    let target = dom.create_element("div", "");
    dom
      .set_attribute(target, "id", "target")
      .expect("set id attribute");
    dom.append_child(dom.root(), target).expect("append child");

    let mut host = WindowHost::new(dom, "https://example.invalid/")?;

    host.exec_script(
      "const el = document.getElementById('target');\n\
       el.dataset.fooBar = 'baz';\n\
       el.dataset.removeMe = 'x';\n\
       delete el.dataset.removeMe;\n\
       el.style.setProperty('backgroundColor', 'red');",
    )?;

    let dom = host.host().dom();
    assert_eq!(
      dom
        .get_attribute(target, "data-foo-bar")
        .expect("get data-foo-bar"),
      Some("baz")
    );
    assert_eq!(
      dom
        .get_attribute(target, "data-remove-me")
        .expect("get data-remove-me"),
      None
    );
    assert_eq!(
      dom.get_attribute(target, "style").expect("get style"),
      Some("background-color: red;")
    );
    Ok(())
  }

  #[test]
  fn exec_script_installs_event_loop_for_queue_microtask() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;

    host.exec_script("var g = this; g.__x = 0; g.queueMicrotask(function () { g.__x = 1; });")?;

    assert!(matches!(get_global_prop(&mut host, "__x"), Value::Number(n) if n == 0.0));

    host.perform_microtask_checkpoint()?;

    assert!(matches!(get_global_prop(&mut host, "__x"), Value::Number(n) if n == 1.0));
    Ok(())
  }

  fn is_document_host_native(
    _vm: &mut Vm,
    _scope: &mut Scope<'_>,
    host: &mut dyn VmHost,
    _hooks: &mut dyn VmHostHooks,
    _callee: GcObject,
    _this: Value,
    _args: &[Value],
  ) -> std::result::Result<Value, VmError> {
    Ok(Value::Bool(
      host
        .as_any_mut()
        .downcast_mut::<DocumentHostState>()
        .is_some(),
    ))
  }

  fn record_host_native(
    _vm: &mut Vm,
    _scope: &mut Scope<'_>,
    host: &mut dyn VmHost,
    _hooks: &mut dyn VmHostHooks,
    _callee: GcObject,
    _this: Value,
    _args: &[Value],
  ) -> std::result::Result<Value, VmError> {
    if host
      .as_any_mut()
      .downcast_mut::<DocumentHostState>()
      .is_some()
    {
      Ok(Value::Bool(true))
    } else {
      Err(VmError::TypeError(
        "recordHost called without the embedder DocumentHostState VmHost context",
      ))
    }
  }

  fn install_record_host(host: &mut WindowHost) {
    install_record_host_in_window(host.host_mut().window_mut());
  }

  fn install_record_host_in_window(window: &mut WindowRealm) {
    let (vm, realm, heap) = window.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let global = realm.global_object();

    scope
      .push_root(Value::Object(global))
      .expect("push root global");

    let id = vm
      .register_native_call(record_host_native)
      .expect("register recordHost native");
    let name_s = scope
      .alloc_string("recordHost")
      .expect("alloc recordHost name");
    scope
      .push_root(Value::String(name_s))
      .expect("push root recordHost name");

    let func = scope
      .alloc_native_function(id, None, name_s, 0)
      .expect("alloc recordHost function");
    scope
      .heap_mut()
      .object_set_prototype(func, Some(realm.intrinsics().function_prototype()))
      .expect("set recordHost prototype");
    scope
      .push_root(Value::Object(func))
      .expect("push root recordHost function");

    let key = PropertyKey::from_string(name_s);
    scope
      .define_property(
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
      )
      .expect("define recordHost global");
  }

  #[test]
  fn exec_script_passes_real_vm_host_context() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;

    // Install a native function that can only return `true` if script execution passes the actual
    // `DocumentHostState` as the vm-js host context.
    {
      let window = host.host_mut().window_mut();
      let (vm, realm, heap) = window.vm_realm_and_heap_mut();

      let call_id = vm
        .register_native_call(is_document_host_native)
        .expect("register native call");

      let mut scope = heap.scope();
      let global = realm.global_object();
      scope
        .push_root(Value::Object(global))
        .expect("push root global");

      let name_s = scope
        .alloc_string("__fr_is_document_host")
        .expect("alloc name");
      scope
        .push_root(Value::String(name_s))
        .expect("push root name");
      let func = scope
        .alloc_native_function(call_id, None, name_s, 0)
        .expect("alloc native function");
      scope
        .push_root(Value::Object(func))
        .expect("push root func");
      let key = PropertyKey::from_string(name_s);
      scope
        .define_property(
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
        )
        .expect("define global native function");
    }

    host.exec_script(
      r#"
      var g = this;
      g.__immediate = __fr_is_document_host();
      g.__microtask = null;
      Promise.resolve().then(function () { g.__microtask = __fr_is_document_host(); });
      "#,
    )?;

    assert!(matches!(
      get_global_prop(&mut host, "__immediate"),
      Value::Bool(true)
    ));
    assert!(matches!(
      get_global_prop(&mut host, "__microtask"),
      Value::Null
    ));

    host.perform_microtask_checkpoint()?;

    assert!(matches!(
      get_global_prop(&mut host, "__microtask"),
      Value::Bool(true)
    ));
    Ok(())
  }

  #[test]
  fn create_error_construction_runs_with_real_vm_host() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;
    install_record_host(&mut host);

    host.exec_script(
      r#"
      globalThis.__host_ok = false;
      globalThis.__err = "";
      globalThis.__ctor = function(msg) {
        try { globalThis.__host_ok = recordHost(); }
        catch (e) { globalThis.__err = String(e && e.message || e); }
      };
      "#,
    )?;

    let create_error_result: Result<()> = {
      use crate::js::window_timers::VmJsEventLoopHooks;

      let (host_state, event_loop) = (&mut host.host, &mut host.event_loop);
      let mut hooks = VmJsEventLoopHooks::<WindowHostState>::new_with_host(host_state);
      hooks.set_event_loop(event_loop);
      let (vm_host, window) = host_state.vm_host_and_window_realm();
      let (vm, realm, heap) = window.vm_realm_and_heap_mut();
      let mut scope = heap.scope();
      let global = realm.global_object();
      scope
        .push_root(Value::Object(global))
        .expect("push root global");
      let key_s = scope.alloc_string("__ctor").expect("alloc __ctor");
      scope
        .push_root(Value::String(key_s))
        .expect("push root __ctor");
      let key = PropertyKey::from_string(key_s);
      let ctor_val = scope
        .heap()
        .object_get_own_data_property_value(global, &key)
        .expect("get __ctor")
        .unwrap_or(Value::Undefined);
      let Value::Object(ctor_obj) = ctor_val else {
        return Err(Error::Other("missing __ctor".to_string()));
      };
      scope
        .push_root(Value::Object(ctor_obj))
        .expect("push root __ctor function");
      let create_result = crate::js::window_realm::test_only_create_error(
        vm, &mut scope, vm_host, &mut hooks, ctor_obj, "boom",
      );
      drop(scope);
      let create_result = match create_result {
        Ok(_) => Ok(()),
        Err(err) => Err(vm_error_format::vm_error_to_error(window.heap_mut(), err)),
      };
      if let Some(err) = hooks.finish(window.heap_mut()) {
        return Err(err);
      }
      create_result
    };
    create_error_result?;

    assert_eq!(
      get_global_prop_utf8(&mut host, "__err").unwrap_or_default(),
      ""
    );
    assert!(matches!(
      get_global_prop(&mut host, "__host_ok"),
      Value::Bool(true)
    ));
    Ok(())
  }

  #[test]
  fn text_decoder_option_getter_runs_with_real_vm_host() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;
    install_record_host(&mut host);

    host.exec_script(
      r#"
      globalThis.__host_ok = false;
      globalThis.__err = "";
      try {
        const opts = {};
        Object.defineProperty(opts, "fatal", {
          get() {
            globalThis.__host_ok = recordHost();
            return true;
          }
        });
        new TextDecoder("utf-8", opts);
      } catch (e) {
        globalThis.__err = String(e && (e.stack || e.message) || e);
      }
      "#,
    )?;

    assert_eq!(
      get_global_prop_utf8(&mut host, "__err").unwrap_or_default(),
      ""
    );
    assert!(matches!(
      get_global_prop(&mut host, "__host_ok"),
      Value::Bool(true)
    ));
    Ok(())
  }

  #[test]
  fn blob_option_getter_runs_with_real_vm_host() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;
    install_record_host(&mut host);

    host.exec_script(
      r#"
      globalThis.__host_ok = false;
      globalThis.__err = "";
      try {
        const opts = {};
        Object.defineProperty(opts, "type", {
          get() {
            globalThis.__host_ok = recordHost();
            return "text/plain";
          }
        });
        new Blob(["hi"], opts);
      } catch (e) {
        globalThis.__err = String(e && (e.stack || e.message) || e);
      }
      "#,
    )?;

    assert_eq!(
      get_global_prop_utf8(&mut host, "__err").unwrap_or_default(),
      ""
    );
    assert!(matches!(
      get_global_prop(&mut host, "__host_ok"),
      Value::Bool(true)
    ));
    Ok(())
  }

  #[test]
  fn xhr_dispatch_event_getters_run_with_real_vm_host() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;
    install_record_host(&mut host);

    host.exec_script(
      r#"
      globalThis.__host_ok_type = false;
      globalThis.__host_ok_getter = false;
      globalThis.__host_ok_callback = false;
      globalThis.__err = "";
      try {
        const xhr = new XMLHttpRequest();
        Object.defineProperty(xhr, "onloadend", {
          get() {
            globalThis.__host_ok_getter = recordHost();
            return () => { globalThis.__host_ok_callback = recordHost(); };
          }
        });
        const ev = {};
        Object.defineProperty(ev, "type", {
          get() {
            globalThis.__host_ok_type = recordHost();
            return "loadend";
          }
        });
        xhr.dispatchEvent(ev);
      } catch (e) {
        globalThis.__err = String(e && (e.stack || e.message) || e);
      }
      "#,
    )?;

    assert_eq!(
      get_global_prop_utf8(&mut host, "__err").unwrap_or_default(),
      ""
    );
    assert!(matches!(
      get_global_prop(&mut host, "__host_ok_type"),
      Value::Bool(true)
    ));
    assert!(matches!(
      get_global_prop(&mut host, "__host_ok_getter"),
      Value::Bool(true)
    ));
    assert!(matches!(
      get_global_prop(&mut host, "__host_ok_callback"),
      Value::Bool(true)
    ));
    Ok(())
  }

  #[test]
  fn webidl_event_target_dispatch_runs_with_real_vm_host() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;
    install_record_host(&mut host);

    // Force the generated WebIDL EventTarget bindings to install (WindowRealm ships with a
    // handcrafted EventTarget implementation by default).
    {
      let window = host.host_mut().window_mut();
      let (_vm, realm, heap) = window.vm_realm_and_heap_mut();
      let mut scope = heap.scope();
      let global = realm.global_object();
      scope
        .push_root(Value::Object(global))
        .map_err(|err| Error::Other(err.to_string()))?;
      let key_s = scope
        .alloc_string("EventTarget")
        .map_err(|err| Error::Other(err.to_string()))?;
      scope
        .push_root(Value::String(key_s))
        .map_err(|err| Error::Other(err.to_string()))?;
      let key = PropertyKey::from_string(key_s);
      scope
        .delete_property_or_throw(global, key)
        .map_err(|err| Error::Other(err.to_string()))?;
    }
    {
      let window = host.host_mut().window_mut();
      let (vm, realm, heap) = window.vm_realm_and_heap_mut();
      crate::js::bindings::install_event_target_bindings_vm_js(vm, heap, realm)
        .map_err(|err| Error::Other(err.to_string()))?;
    }

    host.exec_script(
      r#"
      globalThis.__host_ok_cb = false;
      globalThis.__host_ok_type = false;
      globalThis.__err = "";
      try {
        const t = new EventTarget();
        t.addEventListener('x', () => { globalThis.__host_ok_cb = recordHost(); });
        const e = {};
        Object.defineProperty(e, "type", {
          get() {
            globalThis.__host_ok_type = recordHost();
            return "x";
          }
        });
        t.dispatchEvent(e);
      } catch (e) {
        globalThis.__err = String(e && (e.stack || e.message) || e);
      }
      "#,
    )?;

    assert_eq!(
      get_global_prop_utf8(&mut host, "__err").unwrap_or_default(),
      ""
    );
    assert!(matches!(
      get_global_prop(&mut host, "__host_ok_cb"),
      Value::Bool(true)
    ));
    assert!(matches!(
      get_global_prop(&mut host, "__host_ok_type"),
      Value::Bool(true)
    ));
    Ok(())
  }

  #[test]
  fn dispatch_event_listener_runs_with_real_vm_host() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;
    install_record_host(&mut host);

    host.exec_script(
      r#"
      globalThis.__host_ok = false;
      window.addEventListener('x', () => { globalThis.__host_ok = recordHost(); });
      window.dispatchEvent({ type: 'x' });
      "#,
    )?;

    assert!(matches!(
      get_global_prop(&mut host, "__host_ok"),
      Value::Bool(true)
    ));
    Ok(())
  }

  #[test]
  fn dispatch_event_handle_event_listener_runs_with_real_vm_host() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;
    install_record_host(&mut host);

    host.exec_script(
      r#"
      globalThis.__host_ok = false;
      window.addEventListener('x', { handleEvent() { globalThis.__host_ok = recordHost(); } });
      window.dispatchEvent({ type: 'x' });
      "#,
    )?;

    assert!(matches!(
      get_global_prop(&mut host, "__host_ok"),
      Value::Bool(true)
    ));
    Ok(())
  }

  #[test]
  fn abort_signal_onabort_runs_with_real_vm_host() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;
    install_record_host(&mut host);

    host.exec_script(
      r#"
      globalThis.__host_ok = false;
      var c = new AbortController();
      c.signal.onabort = () => { globalThis.__host_ok = recordHost(); };
      c.abort();
      "#,
    )?;

    assert!(matches!(
      get_global_prop(&mut host, "__host_ok"),
      Value::Bool(true)
    ));
    Ok(())
  }

  #[test]
  fn abort_signal_event_listener_runs_with_real_vm_host() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;
    install_record_host(&mut host);

    host.exec_script(
      r#"
      globalThis.__host_ok = false;
      var c = new AbortController();
      c.signal.addEventListener('abort', () => { globalThis.__host_ok = recordHost(); });
      c.abort();
      "#,
    )?;

    assert!(matches!(
      get_global_prop(&mut host, "__host_ok"),
      Value::Bool(true)
    ));
    Ok(())
  }

  #[test]
  fn headers_for_each_runs_with_real_vm_host() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;
    install_record_host(&mut host);

    host.exec_script(
      r#"
      globalThis.__host_ok = false;
      var h = new Headers([['a', '1']]);
      h.forEach(() => { globalThis.__host_ok = recordHost(); });
      "#,
    )?;

    assert!(matches!(
      get_global_prop(&mut host, "__host_ok"),
      Value::Bool(true)
    ));
    Ok(())
  }

  #[test]
  fn mutation_observer_callback_runs_with_real_vm_host() -> Result<()> {
    let renderer_dom =
      crate::dom::parse_html("<!doctype html><html><body><div id=target></div></body></html>")
        .expect("parse_html");
    let mut host = WindowHost::from_renderer_dom(&renderer_dom, "https://example.invalid/")?;
    install_record_host(&mut host);

    host.exec_script(
      r#"
      globalThis.__host_ok = false;
      globalThis.__calls = 0;
      const target = document.getElementById('target');
      new MutationObserver(() => { __calls++; __host_ok = recordHost(); }).observe(target, { childList: true });
      target.appendChild(document.createElement('span'));
      "#,
    )?;

    assert!(matches!(get_global_prop(&mut host, "__calls"), Value::Number(n) if n == 0.0));
    assert!(matches!(
      get_global_prop(&mut host, "__host_ok"),
      Value::Bool(false)
    ));

    host.perform_microtask_checkpoint()?;

    assert!(matches!(get_global_prop(&mut host, "__calls"), Value::Number(n) if n == 1.0));
    assert!(matches!(
      get_global_prop(&mut host, "__host_ok"),
      Value::Bool(true)
    ));
    Ok(())
  }

  #[test]
  fn mutation_observer_delivers_for_dataset_classlist_style_via_domhostvmjs_fast_path() -> Result<()> {
    let renderer_dom = crate::dom::parse_html("<!doctype html><html><body><div id=target></div></body></html>")
      .expect("parse_html");
    let mut host = WindowHost::from_renderer_dom(&renderer_dom, "https://example.invalid/")?;
 
    host.exec_script(
      "var g = this;\n\
       g.__calls = 0;\n\
       g.__len = 0;\n\
       g.__attr0 = null;\n\
       g.__attr1 = null;\n\
       g.__attr2 = null;\n\
       g.__attr3 = null;\n\
       const target = document.getElementById('target');\n\
       const obs = new MutationObserver(function (records) {\n\
         g.__calls++;\n\
         g.__len = records.length;\n\
         g.__attr0 = records[0].attributeName;\n\
         g.__attr1 = records[1].attributeName;\n\
         g.__attr2 = records[2].attributeName;\n\
         g.__attr3 = records[3].attributeName;\n\
       });\n\
       obs.observe(target, { attributes: true });\n\
       target.dataset.foo = 'a';\n\
       target.classList.add('x');\n\
       target.style.setProperty('color', 'red');\n\
       target.style.width = '1px';\n",
    )?;
 
    assert!(matches!(get_global_prop(&mut host, "__calls"), Value::Number(n) if n == 0.0));
    host.perform_microtask_checkpoint()?;
 
    assert!(matches!(get_global_prop(&mut host, "__calls"), Value::Number(n) if n == 1.0));
    assert!(matches!(get_global_prop(&mut host, "__len"), Value::Number(n) if n == 4.0));
    assert_eq!(
      get_global_prop_utf8(&mut host, "__attr0").as_deref(),
      Some("data-foo")
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__attr1").as_deref(),
      Some("class")
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__attr2").as_deref(),
      Some("style")
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__attr3").as_deref(),
      Some("style")
    );
 
    // No-op writes must not enqueue MutationObserver delivery.
    host.exec_script(
      "const el = document.getElementById('target');\n\
       el.dataset.foo = 'a';\n\
       el.classList.add('x');\n\
       el.style.setProperty('color', 'red');\n\
       el.style.width = '1px';\n",
    )?;
    host.perform_microtask_checkpoint()?;
    assert!(matches!(get_global_prop(&mut host, "__calls"), Value::Number(n) if n == 1.0));
 
    Ok(())
  }
 
  #[test]
  fn queue_microtask_callback_runs_with_real_vm_host() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;
    install_record_host(&mut host);

    host.exec_script(
      r#"
      globalThis.__host_ok = false;
      queueMicrotask(() => { globalThis.__host_ok = recordHost(); });
      "#,
    )?;

    assert!(matches!(
      get_global_prop(&mut host, "__host_ok"),
      Value::Bool(false)
    ));

    host.perform_microtask_checkpoint()?;

    assert!(matches!(
      get_global_prop(&mut host, "__host_ok"),
      Value::Bool(true)
    ));
    Ok(())
  }

  #[test]
  fn set_timeout_callback_runs_with_real_vm_host() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;
    install_record_host(&mut host);

    host.exec_script(
      r#"
      globalThis.__host_ok = false;
      setTimeout(() => { globalThis.__host_ok = recordHost(); }, 0);
      "#,
    )?;

    assert!(matches!(
      get_global_prop(&mut host, "__host_ok"),
      Value::Bool(false)
    ));

    let _ = host.run_until_idle(RunLimits {
      max_tasks: 10,
      max_microtasks: 10,
      max_wall_time: Some(Duration::from_secs(5)),
    })?;

    assert!(matches!(
      get_global_prop(&mut host, "__host_ok"),
      Value::Bool(true)
    ));
    Ok(())
  }

  #[test]
  fn set_interval_callback_runs_with_real_vm_host() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;
    install_record_host(&mut host);

    host.exec_script(
      r#"
      globalThis.__calls = 0;
      globalThis.__host_ok = false;
      let id = setInterval(() => {
        globalThis.__calls++;
        globalThis.__host_ok = recordHost();
        clearInterval(id);
      }, 0);
      "#,
    )?;

    assert!(matches!(get_global_prop(&mut host, "__calls"), Value::Number(n) if n == 0.0));
    assert!(matches!(
      get_global_prop(&mut host, "__host_ok"),
      Value::Bool(false)
    ));

    let _ = host.run_until_idle(RunLimits {
      max_tasks: 10,
      max_microtasks: 10,
      max_wall_time: Some(Duration::from_secs(5)),
    })?;

    assert!(matches!(get_global_prop(&mut host, "__calls"), Value::Number(n) if n == 1.0));
    assert!(matches!(
      get_global_prop(&mut host, "__host_ok"),
      Value::Bool(true)
    ));
    Ok(())
  }

  #[test]
  fn request_animation_frame_callback_runs_with_real_vm_host() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;
    install_record_host(&mut host);

    host.exec_script(
      r#"
      globalThis.__host_ok = false;
      requestAnimationFrame(() => { globalThis.__host_ok = recordHost(); });
      "#,
    )?;

    assert!(matches!(
      get_global_prop(&mut host, "__host_ok"),
      Value::Bool(false)
    ));

    // `requestAnimationFrame` callbacks are queued separately from task/microtask queues.
    {
      let WindowHost {
        host: host_state,
        event_loop,
      } = &mut host;
      let _ = event_loop.run_animation_frame(host_state)?;
    }

    assert!(matches!(
      get_global_prop(&mut host, "__host_ok"),
      Value::Bool(true)
    ));
    Ok(())
  }

  #[test]
  fn mutation_observer_queue_microtask_scheduling_runs_with_real_vm_host() -> Result<()> {
    // `WindowHost` always installs `install_window_timers_bindings`, which defines the internal
    // `__fastrender_queue_microtask` as a non-writable/non-configurable property. That prevents
    // tests from monkey-patching the scheduling primitive directly.
    //
    // Instead, construct a minimal `WindowRealm` with DOM shims but *without* timer bindings and
    // define a userland `queueMicrotask` implementation that calls `recordHost()`.
    // `queue_mutation_observer_microtask` should fall back to this when the internal key is absent.

    struct NoTimersHostState {
      document: DocumentHostState,
      window: WindowRealm,
    }

    impl WindowRealmHost for NoTimersHostState {
      fn vm_host_and_window_realm(&mut self) -> (&mut dyn vm_js::VmHost, &mut WindowRealm) {
        (&mut self.document, &mut self.window)
      }
    }

    impl NoTimersHostState {
      fn new(dom: dom2::Document, document_url: &str) -> Result<Self> {
        let clock: Arc<dyn Clock> = Arc::new(RealClock::default());
        let document = DocumentHostState::new(dom);
        let window = WindowRealm::new(
          WindowRealmConfig::new(document_url)
            .with_current_script_state(document.current_script_state().clone())
            .with_clock(clock),
        )
        .map_err(|err| Error::Other(err.to_string()))?;

        Ok(Self { document, window })
      }

      fn exec_script(&mut self, event_loop: &mut EventLoop<Self>, source: &str) -> Result<Value> {
        use crate::js::window_timers::VmJsEventLoopHooks;

        let mut hooks = VmJsEventLoopHooks::<Self>::new_with_host(self);
        hooks.set_event_loop(event_loop);
        let (vm_host, window) = self.vm_host_and_window_realm();
        let result = window.exec_script_with_host_and_hooks(vm_host, &mut hooks, source);

        if let Some(err) = hooks.finish(window.heap_mut()) {
          return Err(err);
        }

        match result {
          Ok(value) => Ok(value),
          Err(err) => Err(vm_error_format::vm_error_to_error(window.heap_mut(), err)),
        }
      }

      fn get_global_prop(&mut self, name: &str) -> Value {
        let (_vm, realm, heap) = self.window.vm_realm_and_heap_mut();
        let mut scope = heap.scope();
        let global = realm.global_object();
        scope
          .push_root(Value::Object(global))
          .expect("push root global");
        let key_s = scope.alloc_string(name).expect("alloc prop name");
        scope
          .push_root(Value::String(key_s))
          .expect("push root prop name");
        let key = PropertyKey::from_string(key_s);
        scope
          .heap()
          .object_get_own_data_property_value(global, &key)
          .expect("get global prop")
          .unwrap_or(Value::Undefined)
      }

      fn get_global_prop_utf8(&mut self, name: &str) -> Option<String> {
        let (_vm, realm, heap) = self.window.vm_realm_and_heap_mut();
        let mut scope = heap.scope();
        let global = realm.global_object();
        scope
          .push_root(Value::Object(global))
          .expect("push root global");
        let key_s = scope.alloc_string(name).expect("alloc prop name");
        scope
          .push_root(Value::String(key_s))
          .expect("push root prop name");
        let key = PropertyKey::from_string(key_s);
        let v = scope
          .heap()
          .object_get_own_data_property_value(global, &key)
          .expect("get global prop")
          .unwrap_or(Value::Undefined);
        let s = match v {
          Value::String(s) => s,
          _ => return None,
        };
        Some(scope.heap().get_string(s).ok()?.to_utf8_lossy())
      }
    }

    let renderer_dom =
      crate::dom::parse_html("<!doctype html><html><body><div id=target></div></body></html>")
        .expect("parse_html");
    let dom = dom2::Document::from_renderer_dom(&renderer_dom);
    let mut host = NoTimersHostState::new(dom, "https://example.invalid/")?;

    install_record_host_in_window(&mut host.window);
    let mut event_loop = EventLoop::<NoTimersHostState>::new();

    host.exec_script(
      &mut event_loop,
      r#"
      globalThis.__host_ok = false;
      globalThis.__err = "";

      // `queue_mutation_observer_microtask` should fall back to this userland implementation.
      globalThis.queueMicrotask = function () {
        try { globalThis.__host_ok = recordHost(); }
        catch (e) { globalThis.__err = String(e && e.message || e); }
      };

      const target = document.getElementById('target');
      new MutationObserver(() => {}).observe(target, { childList: true });
      target.appendChild(document.createElement('span'));
      "#,
    )?;

    assert_eq!(host.get_global_prop_utf8("__err").unwrap_or_default(), "");
    assert!(matches!(
      host.get_global_prop("__host_ok"),
      Value::Bool(true)
    ));
    Ok(())
  }

  #[test]
  fn exec_script_drains_promise_jobs_at_microtask_checkpoint() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;

    // Nested Promise job: the inner `then` must run in the same microtask checkpoint.
    host.exec_script(
      "var g = this; g.__x = 0; Promise.resolve().then(function () { g.__x = 1; Promise.resolve().then(function () { g.__x = 2; }); });",
    )?;

    assert!(matches!(get_global_prop(&mut host, "__x"), Value::Number(n) if n == 0.0));

    host.perform_microtask_checkpoint()?;

    assert!(matches!(get_global_prop(&mut host, "__x"), Value::Number(n) if n == 2.0));
    Ok(())
  }

  #[test]
  fn exec_script_preserves_microtask_order_between_promise_and_queue_microtask() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;

    // Both Promise jobs and `queueMicrotask` are microtasks in HTML. They must share the same FIFO
    // microtask queue so ordering matches enqueue order.
    host.exec_script(
      "var g = this; g.__x = 0; Promise.resolve().then(function () { g.__x = g.__x * 10 + 1; }); queueMicrotask(function () { g.__x = g.__x * 10 + 2; });",
    )?;

    assert!(matches!(get_global_prop(&mut host, "__x"), Value::Number(n) if n == 0.0));

    host.perform_microtask_checkpoint()?;

    // If Promise jobs are incorrectly drained after `queueMicrotask` callbacks, the result would be
    // `21` instead of `12`.
    assert!(matches!(get_global_prop(&mut host, "__x"), Value::Number(n) if n == 12.0));
    Ok(())
  }

  #[test]
  fn mutation_observer_delivers_attribute_records_via_microtask_checkpoint() -> Result<()> {
    let renderer_dom =
      crate::dom::parse_html("<!doctype html><html><body><div id=target></div></body></html>")
        .expect("parse_html");
    let mut host = WindowHost::from_renderer_dom(&renderer_dom, "https://example.invalid/")?;

    host.exec_script(
      "var g = this;\n\
       g.__calls = 0;\n\
       g.__len = 0;\n\
       g.__type0 = null;\n\
       g.__attr0 = null;\n\
       g.__old0_is_null = false;\n\
       g.__old1 = null;\n\
       g.__target_eq = false;\n\
       g.__observer_eq = false;\n\
       g.__this_eq = false;\n\
       const target = document.getElementById('target');\n\
       const obs = new MutationObserver(function (records, observer) {\n\
         g.__calls++;\n\
         g.__len = records.length;\n\
         g.__type0 = records[0].type;\n\
         g.__attr0 = records[0].attributeName;\n\
         g.__old0_is_null = (records[0].oldValue === null);\n\
         g.__old1 = records[1].oldValue;\n\
         g.__target_eq = (records[0].target === target);\n\
         g.__observer_eq = (observer === obs);\n\
         g.__this_eq = (this === obs);\n\
       });\n\
       obs.observe(target, { attributes: true, attributeOldValue: true });\n\
       target.setAttribute('DATA-X', 'a');\n\
       target.setAttribute('DATA-X', 'b');\n",
    )?;

    assert!(matches!(get_global_prop(&mut host, "__calls"), Value::Number(n) if n == 0.0));
    host.perform_microtask_checkpoint()?;

    assert!(matches!(get_global_prop(&mut host, "__calls"), Value::Number(n) if n == 1.0));
    assert!(matches!(get_global_prop(&mut host, "__len"), Value::Number(n) if n == 2.0));
    assert_eq!(
      get_global_prop_utf8(&mut host, "__type0").as_deref(),
      Some("attributes")
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__attr0").as_deref(),
      Some("data-x")
    );
    assert_eq!(
      get_global_prop(&mut host, "__old0_is_null"),
      Value::Bool(true)
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__old1").as_deref(),
      Some("a")
    );
    assert_eq!(get_global_prop(&mut host, "__target_eq"), Value::Bool(true));
    assert_eq!(
      get_global_prop(&mut host, "__observer_eq"),
      Value::Bool(true)
    );
    assert_eq!(get_global_prop(&mut host, "__this_eq"), Value::Bool(true));

    Ok(())
  }

  #[test]
  fn mutation_observer_attribute_old_value_implies_attributes_option() -> Result<()> {
    let renderer_dom =
      crate::dom::parse_html("<!doctype html><html><body><div id=target></div></body></html>")
        .expect("parse_html");
    let mut host = WindowHost::from_renderer_dom(&renderer_dom, "https://example.invalid/")?;

    // Per the DOM Standard, specifying `attributeOldValue` without an explicit `attributes` member
    // implies `attributes: true`.
    host.exec_script(
      "var g = this;\n\
       g.__calls = 0;\n\
       g.__len = 0;\n\
       g.__type0 = null;\n\
       g.__attr0 = null;\n\
       g.__old0_is_null = false;\n\
       const target = document.getElementById('target');\n\
       const obs = new MutationObserver(function (records) {\n\
         g.__calls++;\n\
         g.__len = records.length;\n\
         g.__type0 = records[0].type;\n\
         g.__attr0 = records[0].attributeName;\n\
         g.__old0_is_null = (records[0].oldValue === null);\n\
       });\n\
       obs.observe(target, { attributeOldValue: true });\n\
       target.setAttribute('data-q', '1');\n",
    )?;

    host.perform_microtask_checkpoint()?;
    assert!(matches!(get_global_prop(&mut host, "__calls"), Value::Number(n) if n == 1.0));
    assert!(matches!(get_global_prop(&mut host, "__len"), Value::Number(n) if n == 1.0));
    assert_eq!(
      get_global_prop_utf8(&mut host, "__type0").as_deref(),
      Some("attributes")
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__attr0").as_deref(),
      Some("data-q")
    );
    assert_eq!(
      get_global_prop(&mut host, "__old0_is_null"),
      Value::Bool(true)
    );

    Ok(())
  }

  #[test]
  fn mutation_observer_delivers_child_list_records_via_microtask_checkpoint() -> Result<()> {
    let renderer_dom =
      crate::dom::parse_html("<!doctype html><html><body><div id=target></div></body></html>")
        .expect("parse_html");
    let mut host = WindowHost::from_renderer_dom(&renderer_dom, "https://example.invalid/")?;

    host.exec_script(
      "var g = this;\n\
       g.__calls = 0;\n\
       g.__type0 = null;\n\
       g.__added_len = 0;\n\
       g.__removed_len = 0;\n\
       g.__target_eq = false;\n\
       const target = document.getElementById('target');\n\
       const child = document.createElement('span');\n\
       const obs = new MutationObserver(function (records) {\n\
         g.__calls++;\n\
         g.__type0 = records[0].type;\n\
         g.__added_len = records[0].addedNodes.length;\n\
         g.__removed_len = records[0].removedNodes.length;\n\
         g.__target_eq = (records[0].target === target);\n\
       });\n\
       obs.observe(target, { childList: true });\n\
       target.appendChild(child);\n",
    )?;

    assert!(matches!(get_global_prop(&mut host, "__calls"), Value::Number(n) if n == 0.0));
    host.perform_microtask_checkpoint()?;

    assert!(matches!(get_global_prop(&mut host, "__calls"), Value::Number(n) if n == 1.0));
    assert_eq!(
      get_global_prop_utf8(&mut host, "__type0").as_deref(),
      Some("childList")
    );
    assert!(matches!(get_global_prop(&mut host, "__added_len"), Value::Number(n) if n == 1.0));
    assert!(matches!(get_global_prop(&mut host, "__removed_len"), Value::Number(n) if n == 0.0));
    assert_eq!(get_global_prop(&mut host, "__target_eq"), Value::Bool(true));

    Ok(())
  }

  #[test]
  fn mutation_observer_move_within_parent_queues_separate_remove_and_add_records() -> Result<()> {
    let renderer_dom =
      crate::dom::parse_html("<!doctype html><html><body><div id=target></div></body></html>")
        .expect("parse_html");
    let mut host = WindowHost::from_renderer_dom(&renderer_dom, "https://example.invalid/")?;

    host.exec_script(
      "var g = this;\n\
       g.__calls = 0;\n\
       g.__len = 0;\n\
       g.__added0 = 0;\n\
       g.__removed0 = 0;\n\
       g.__added1 = 0;\n\
       g.__removed1 = 0;\n\
       const target = document.getElementById('target');\n\
       const a = document.createElement('span');\n\
       const b = document.createElement('span');\n\
       target.appendChild(a);\n\
       target.appendChild(b);\n\
       const obs = new MutationObserver(function (records) {\n\
         g.__calls++;\n\
         g.__len = records.length;\n\
         if (records.length >= 2) {\n\
           g.__added0 = records[0].addedNodes.length;\n\
           g.__removed0 = records[0].removedNodes.length;\n\
           g.__added1 = records[1].addedNodes.length;\n\
           g.__removed1 = records[1].removedNodes.length;\n\
         }\n\
       });\n\
       obs.observe(target, { childList: true });\n\
       target.appendChild(a);\n",
    )?;

    host.perform_microtask_checkpoint()?;
    assert!(matches!(get_global_prop(&mut host, "__calls"), Value::Number(n) if n == 1.0));
    assert!(matches!(get_global_prop(&mut host, "__len"), Value::Number(n) if n == 2.0));
    assert!(matches!(get_global_prop(&mut host, "__added0"), Value::Number(n) if n == 0.0));
    assert!(matches!(get_global_prop(&mut host, "__removed0"), Value::Number(n) if n == 1.0));
    assert!(matches!(get_global_prop(&mut host, "__added1"), Value::Number(n) if n == 1.0));
    assert!(matches!(get_global_prop(&mut host, "__removed1"), Value::Number(n) if n == 0.0));
    Ok(())
  }

  #[test]
  fn mutation_observer_subtree_option_observes_descendant_attributes() -> Result<()> {
    let renderer_dom = crate::dom::parse_html(
      "<!doctype html><html><body><div id=root><div id=target></div></div></body></html>",
    )
    .expect("parse_html");
    let mut host = WindowHost::from_renderer_dom(&renderer_dom, "https://example.invalid/")?;

    host.exec_script(
      "var g = this;\n\
       g.__calls = 0;\n\
       g.__target_eq = false;\n\
       const root = document.getElementById('root');\n\
       const target = document.getElementById('target');\n\
       const obs = new MutationObserver(function (records) {\n\
         g.__calls++;\n\
         g.__target_eq = (records[0].target === target);\n\
       });\n\
       obs.observe(root, { attributes: true, subtree: true });\n\
       target.setAttribute('data-y', '1');\n",
    )?;

    host.perform_microtask_checkpoint()?;
    assert!(matches!(get_global_prop(&mut host, "__calls"), Value::Number(n) if n == 1.0));
    assert_eq!(get_global_prop(&mut host, "__target_eq"), Value::Bool(true));
    Ok(())
  }

  #[test]
  fn mutation_observer_take_records_drains_queue() -> Result<()> {
    let renderer_dom =
      crate::dom::parse_html("<!doctype html><html><body><div id=target></div></body></html>")
        .expect("parse_html");
    let mut host = WindowHost::from_renderer_dom(&renderer_dom, "https://example.invalid/")?;

    host.exec_script(
      "var g = this;\n\
       g.__calls = 0;\n\
       g.__taken_len = 0;\n\
       const target = document.getElementById('target');\n\
       const obs = new MutationObserver(function () { g.__calls++; });\n\
       obs.observe(target, { attributes: true });\n\
       target.setAttribute('data-z', '1');\n\
       g.__taken_len = obs.takeRecords().length;\n",
    )?;

    assert!(matches!(
      get_global_prop(&mut host, "__taken_len"),
      Value::Number(n) if n == 1.0
    ));
    host.perform_microtask_checkpoint()?;
    assert!(matches!(
      get_global_prop(&mut host, "__calls"),
      Value::Number(n) if n == 0.0
    ));
    Ok(())
  }

  #[test]
  fn mutation_observer_disconnect_stops_future_records() -> Result<()> {
    let renderer_dom =
      crate::dom::parse_html("<!doctype html><html><body><div id=target></div></body></html>")
        .expect("parse_html");
    let mut host = WindowHost::from_renderer_dom(&renderer_dom, "https://example.invalid/")?;

    host.exec_script(
      "var g = this;\n\
       g.__calls = 0;\n\
       const target = document.getElementById('target');\n\
       const obs = new MutationObserver(function () { g.__calls++; });\n\
       obs.observe(target, { attributes: true });\n\
       obs.disconnect();\n\
       target.setAttribute('data-a', '1');\n",
    )?;

    host.perform_microtask_checkpoint()?;
    assert!(matches!(
      get_global_prop(&mut host, "__calls"),
      Value::Number(n) if n == 0.0
    ));
    Ok(())
  }

  #[test]
  fn document_cookie_round_trip_is_deterministic() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;

    host.exec_script("document.cookie = 'b=c; Path=/'; document.cookie = 'a=b';")?;

    let cookie = host.exec_script("document.cookie")?;
    assert_eq!(value_to_string(&host, cookie), "a=b; b=c");
    Ok(())
  }

  #[test]
  fn document_cookie_syncs_with_fetcher_cookie_store() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let fetcher = Arc::new(CookieRecordingFetcher::default());
    fetcher.store_cookie_from_document("https://example.invalid/", "z=1");
    let mut host = WindowHost::new_with_fetcher(dom, "https://example.invalid/", fetcher.clone())?;

    let cookie = host.exec_script("document.cookie")?;
    assert_eq!(value_to_string(&host, cookie), "z=1");

    host.exec_script("document.cookie = 'b=c; Path=/'; document.cookie = 'a=b';")?;

    assert_eq!(
      fetcher
        .cookie_header_value("https://example.invalid/")
        .unwrap_or_default(),
      "z=1; b=c; a=b"
    );

    let cookie = host.exec_script("document.cookie")?;
    assert_eq!(value_to_string(&host, cookie), "a=b; b=c; z=1");
    Ok(())
  }

  #[test]
  fn document_cookie_fetcher_sync_handles_empty_cookie_header() -> Result<()> {
    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(HttpFetcher::new());
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new_with_fetcher(dom, "https://example.invalid/", fetcher.clone())?;

    // Cookie is scoped to `/sub`, so it should not be visible on the document at `/`.
    host.exec_script("document.cookie = 'a=b; Path=/sub';")?;
    let cookie = host.exec_script("document.cookie")?;
    assert_eq!(value_to_string(&host, cookie), "");

    // A separate document whose URL path matches the cookie should observe it.
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host_sub = WindowHost::new_with_fetcher(dom, "https://example.invalid/sub", fetcher)?;
    let cookie = host_sub.exec_script("document.cookie")?;
    assert_eq!(value_to_string(&host_sub, cookie), "a=b");
    Ok(())
  }

  #[test]
  fn fetch_includes_cookies_from_set_cookie_and_document_cookie() -> Result<()> {
    let Ok(listener) = TcpListener::bind("127.0.0.1:0") else {
      // Some sandboxed CI environments may forbid binding sockets; skip in that case.
      return Ok(());
    };
    listener.set_nonblocking(true).expect("set_nonblocking");
    let addr = listener.local_addr().expect("local_addr");
    let url = format!("http://{addr}/");
    let server = std::thread::spawn(move || {
      let deadline = Instant::now() + Duration::from_secs(5);

      // First request: respond with Set-Cookie so subsequent requests should include it.
      let mut stream = accept_with_deadline(&listener, deadline).expect("accept first request");
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .expect("set_read_timeout");
      let _req1 = read_http_request(&mut stream).expect("read first request");
      let body = b"first";
      let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nSet-Cookie: a=b; Path=/\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
      );
      stream.write_all(headers.as_bytes()).expect("write headers");
      stream.write_all(body).expect("write body");
      drop(stream);

      // Second request must include both the Set-Cookie cookie and the document.cookie cookie.
      let mut stream = accept_with_deadline(&listener, deadline).expect("accept second request");
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .expect("set_read_timeout");
      let req2 = read_http_request(&mut stream).expect("read second request");
      let req2_s = String::from_utf8_lossy(&req2).to_ascii_lowercase();
      assert!(
        req2_s.contains("cookie:") && req2_s.contains("a=b") && req2_s.contains("c=d"),
        "expected second fetch request to include cookies a=b and c=d, got:\\n{req2_s}"
      );

      let body = b"second";
      let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
      );
      stream.write_all(headers.as_bytes()).expect("write headers");
      stream.write_all(body).expect("write body");
    });

    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let fetcher = Arc::new(HttpFetcher::new().with_timeout(Duration::from_secs(2)));
    let mut host = WindowHost::new_with_fetcher(dom, url, fetcher)?;

    host.exec_script(
      r#"
      var g = this;
      fetch("/set")
        .then(function (r) { return r.text(); })
        .then(function (_t) {
          document.cookie = "c=d; Path=/";
          return fetch("/check").then(function (r) { return r.text(); });
        })
        .then(function (t) {
          g.__fetch_text = t;
          g.__cookie = document.cookie;
        })
        .catch(function (e) {
          g.__err = String(e && e.stack || e);
        });
      "#,
    )?;

    host.run_until_idle(RunLimits {
      max_tasks: 10,
      max_microtasks: 100,
      max_wall_time: Some(Duration::from_secs(5)),
    })?;

    if let Some(err) = get_global_prop_utf8(&mut host, "__err") {
      panic!("fetch script errored: {err}");
    }

    assert_eq!(
      get_global_prop_utf8(&mut host, "__fetch_text").as_deref(),
      Some("second")
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__cookie").as_deref(),
      Some("a=b; c=d")
    );

    server.join().expect("server thread panicked");
    Ok(())
  }

  #[test]
  fn fetch_redirect_modes_surface_response_metadata() -> Result<()> {
    let Ok(listener) = TcpListener::bind("127.0.0.1:0") else {
      // Some sandboxed CI environments may forbid binding sockets; skip in that case.
      return Ok(());
    };
    listener.set_nonblocking(true).expect("set_nonblocking");
    let addr = listener.local_addr().expect("local_addr");
    let url = format!("http://{addr}/");
    let server = std::thread::spawn(move || {
      let deadline = Instant::now() + Duration::from_secs(5);
      let mut paths: Vec<String> = Vec::new();

      for i in 0..4 {
        let mut stream = accept_with_deadline(&listener, deadline)
          .unwrap_or_else(|_| panic!("accept request {i}"));
        stream
          .set_read_timeout(Some(Duration::from_millis(500)))
          .expect("set_read_timeout");
        let req = read_http_request(&mut stream).unwrap_or_else(|_| panic!("read request {i}"));
        let req_s = String::from_utf8_lossy(&req);
        let first_line = req_s.lines().next().unwrap_or("");
        let path = first_line
          .split_whitespace()
          .nth(1)
          .unwrap_or("")
          .to_string();
        paths.push(path.clone());

        match path.as_str() {
          "/redir" => {
            let body = b"redir";
            let headers = format!(
              "HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
              body.len()
            );
            stream.write_all(headers.as_bytes()).expect("write headers");
            stream.write_all(body).expect("write body");
          }
          "/final" => {
            let body = b"final";
            let headers = format!(
              "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
              body.len()
            );
            stream.write_all(headers.as_bytes()).expect("write headers");
            stream.write_all(body).expect("write body");
          }
          _ => {
            let body = b"not found";
            let headers = format!(
              "HTTP/1.1 404 Not Found\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
              body.len()
            );
            stream.write_all(headers.as_bytes()).expect("write headers");
            stream.write_all(body).expect("write body");
          }
        }
      }

      assert_eq!(
        paths,
        vec![
          "/redir".to_string(),
          "/redir".to_string(),
          "/final".to_string(),
          "/redir".to_string()
        ],
        "unexpected redirect request sequence"
      );
    });

    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let fetcher = Arc::new(HttpFetcher::new().with_timeout(Duration::from_secs(2)));
    let mut host = WindowHost::new_with_fetcher(dom, url, fetcher)?;

    host.exec_script(
      r#"
      var g = this;
      fetch("/redir", { redirect: "manual" })
        .then(function (r) {
          g.__manual_type = r.type;
          g.__manual_status = r.status;
          g.__manual_url = r.url;
          g.__manual_redirected = r.redirected;
          return fetch("/redir");
        })
        .then(function (r) {
          g.__follow_type = r.type;
          g.__follow_status = r.status;
          g.__follow_url = r.url;
          g.__follow_redirected = r.redirected;
          return fetch("/redir", { redirect: "error" });
        })
        .then(function (_r) {
          g.__redirect_error = "did_not_throw";
        })
        .catch(function (e) {
          g.__redirect_error = String(e && (e.stack || e.message) || e);
        });
      "#,
    )?;

    host.run_until_idle(RunLimits {
      max_tasks: 20,
      max_microtasks: 200,
      max_wall_time: Some(Duration::from_secs(5)),
    })?;

    assert_eq!(
      get_global_prop_utf8(&mut host, "__manual_type").as_deref(),
      Some("opaqueredirect")
    );
    assert!(matches!(
      get_global_prop(&mut host, "__manual_status"),
      Value::Number(n) if n == 0.0
    ));
    assert_eq!(
      get_global_prop_utf8(&mut host, "__manual_url").as_deref(),
      Some("")
    );
    assert!(matches!(
      get_global_prop(&mut host, "__manual_redirected"),
      Value::Bool(false)
    ));

    assert_eq!(
      get_global_prop_utf8(&mut host, "__follow_type").as_deref(),
      Some("basic")
    );
    assert!(matches!(
      get_global_prop(&mut host, "__follow_status"),
      Value::Number(n) if n == 200.0
    ));
    let follow_url = get_global_prop_utf8(&mut host, "__follow_url").unwrap_or_default();
    assert!(
      follow_url.ends_with("/final"),
      "expected follow response URL to end with /final, got {follow_url:?}"
    );
    assert!(matches!(
      get_global_prop(&mut host, "__follow_redirected"),
      Value::Bool(true)
    ));

    let redirect_error = get_global_prop_utf8(&mut host, "__redirect_error").unwrap_or_default();
    assert!(
      redirect_error.to_ascii_lowercase().contains("redirect"),
      "expected redirect=\"error\" fetch to reject, got {redirect_error:?}"
    );

    server.join().expect("server thread panicked");
    Ok(())
  }

  #[test]
  fn window_realm_supports_event_constructors_and_create_event() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;

    host.exec_script(
      r#"
      var e1 = document.createEvent("Event");
      e1.initEvent("hello", true, false);
      this.__e1_type = e1.type;
      this.__e1_bubbles = e1.bubbles;
      this.__e1_cancelable = e1.cancelable;

      var e2 = document.createEvent("CustomEvent");
      e2.initCustomEvent("world", false, true, 123);
      this.__e2_type = e2.type;
      this.__e2_detail = e2.detail;

      var e3 = new CustomEvent("ctor", { detail: 456 });
      this.__e3_type = e3.type;
      this.__e3_detail = e3.detail;

      try {
        document.createEvent("NoSuchEvent");
        this.__unsupported = "did_not_throw";
      } catch (e) {
        this.__unsupported = e && e.name;
      }
    "#,
    )?;

    assert_eq!(
      get_global_prop_utf8(&mut host, "__e1_type").as_deref(),
      Some("hello")
    );
    assert!(matches!(
      get_global_prop(&mut host, "__e1_bubbles"),
      Value::Bool(true)
    ));
    assert!(matches!(
      get_global_prop(&mut host, "__e1_cancelable"),
      Value::Bool(false)
    ));

    assert_eq!(
      get_global_prop_utf8(&mut host, "__e2_type").as_deref(),
      Some("world")
    );
    assert!(matches!(
      get_global_prop(&mut host, "__e2_detail"),
      Value::Number(n) if n == 123.0
    ));

    assert_eq!(
      get_global_prop_utf8(&mut host, "__e3_type").as_deref(),
      Some("ctor")
    );
    assert!(matches!(
      get_global_prop(&mut host, "__e3_detail"),
      Value::Number(n) if n == 456.0
    ));

    assert_eq!(
      get_global_prop_utf8(&mut host, "__unsupported").as_deref(),
      Some("NotSupportedError")
    );

    Ok(())
  }

  #[test]
  fn window_onload_handler_runs_on_load_event_dispatch() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;

    host.exec_script(
      "globalThis.__called = false;\n\
       globalThis.onload = function (e) {\n\
         globalThis.__called = (\n\
           this === globalThis &&\n\
           e && e.type === 'load' &&\n\
           e.target === globalThis &&\n\
           e.currentTarget === globalThis &&\n\
           e.eventPhase === 2\n\
         );\n\
       };\n\
       globalThis.dispatchEvent(new Event('load'));\n",
    )?;

    assert!(matches!(get_global_prop(&mut host, "__called"), Value::Bool(true)));
    Ok(())
  }

  #[test]
  fn document_onvisibilitychange_handler_runs_on_visibilitychange_event_dispatch() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;

    host.exec_script(
      "globalThis.__called = false;\n\
       document.onvisibilitychange = function (e) {\n\
         globalThis.__called = (\n\
           this === document &&\n\
           e && e.type === 'visibilitychange' &&\n\
           e.target === document &&\n\
           e.currentTarget === document &&\n\
           e.eventPhase === 2\n\
         );\n\
       };\n\
       document.dispatchEvent(new Event('visibilitychange'));\n",
    )?;

    assert!(matches!(get_global_prop(&mut host, "__called"), Value::Bool(true)));
    Ok(())
  }

  #[test]
  fn node_onclick_handler_runs_on_click_event_dispatch() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;

    host.exec_script(
      "globalThis.__called = false;\n\
       var el = document.createElement('div');\n\
       el.onclick = function (e) {\n\
         globalThis.__called = (\n\
           this === el &&\n\
           e && e.type === 'click' &&\n\
           e.target === el &&\n\
           e.currentTarget === el &&\n\
           e.eventPhase === 2\n\
         );\n\
       };\n\
       el.dispatchEvent(new Event('click'));\n",
    )?;

    assert!(matches!(get_global_prop(&mut host, "__called"), Value::Bool(true)));
    Ok(())
  }

  #[test]
  fn window_onerror_handler_uses_special_signature_and_return_true_cancels() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;

    host.exec_script(
      "globalThis.__argc = 0;\n\
       globalThis.__a0 = '';\n\
       globalThis.__a1 = '';\n\
       globalThis.__a2 = 0;\n\
       globalThis.__a3 = 0;\n\
       globalThis.__a4_code = 0;\n\
       globalThis.__dispatch_result = null;\n\
       globalThis.__default_prevented = false;\n\
       globalThis.onerror = function (message, filename, lineno, colno, error) {\n\
         globalThis.__argc = arguments.length;\n\
         globalThis.__a0 = String(message);\n\
         globalThis.__a1 = String(filename);\n\
         globalThis.__a2 = lineno;\n\
         globalThis.__a3 = colno;\n\
         globalThis.__a4_code = error && error.code;\n\
         return true;\n\
       };\n\
       var e = new Event('error', { cancelable: true });\n\
       e.message = 'boom';\n\
       e.filename = 'file.js';\n\
       e.lineno = 10;\n\
       e.colno = 20;\n\
       e.error = { code: 42 };\n\
       globalThis.__dispatch_result = globalThis.dispatchEvent(e);\n\
       globalThis.__default_prevented = e.defaultPrevented;\n",
    )?;

    assert!(matches!(
      get_global_prop(&mut host, "__argc"),
      Value::Number(n) if n == 5.0
    ));
    assert_eq!(get_global_prop_utf8(&mut host, "__a0").as_deref(), Some("boom"));
    assert_eq!(
      get_global_prop_utf8(&mut host, "__a1").as_deref(),
      Some("file.js")
    );
    assert!(matches!(
      get_global_prop(&mut host, "__a2"),
      Value::Number(n) if n == 10.0
    ));
    assert!(matches!(
      get_global_prop(&mut host, "__a3"),
      Value::Number(n) if n == 20.0
    ));
    assert!(matches!(
      get_global_prop(&mut host, "__a4_code"),
      Value::Number(n) if n == 42.0
    ));
    assert!(matches!(
      get_global_prop(&mut host, "__dispatch_result"),
      Value::Bool(false)
    ));
    assert!(matches!(
      get_global_prop(&mut host, "__default_prevented"),
      Value::Bool(true)
    ));
    Ok(())
  }

  #[test]
  fn unhandled_promise_rejection_dispatches_unhandledrejection_event() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;

    host.queue_task(TaskSource::Script, |host_state, event_loop| {
      host_state.exec_script_in_event_loop(
        event_loop,
        "this.__unhandled = undefined;\n\
         addEventListener('unhandledrejection', function (e) { this.__unhandled = e.reason; });\n\
         Promise.reject('x');\n",
      )?;
      Ok(())
    })?;

    assert_eq!(
      host.run_until_idle(RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__unhandled").as_deref(),
      Some("x")
    );
    Ok(())
  }

  #[test]
  fn unhandledrejection_listener_runs_with_real_vm_host() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;
    install_record_host(&mut host);

    host.queue_task(TaskSource::Script, |host_state, event_loop| {
      host_state.exec_script_in_event_loop(
        event_loop,
        "globalThis.__host_ok = false;\n\
         addEventListener('unhandledrejection', function () { globalThis.__host_ok = recordHost(); });\n\
         Promise.reject('x');\n",
      )?;
      Ok(())
    })?;

    assert_eq!(
      host.run_until_idle(RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert!(matches!(
      get_global_prop(&mut host, "__host_ok"),
      Value::Bool(true)
    ));
    Ok(())
  }

  #[test]
  fn unhandledrejection_event_supports_prevent_default() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;

    host.queue_task(TaskSource::Script, |host_state, event_loop| {
      host_state.exec_script_in_event_loop(
        event_loop,
        "this.__default_prevented = false;\n\
         addEventListener('unhandledrejection', function (e) {\n\
           e.preventDefault();\n\
           this.__default_prevented = e.defaultPrevented;\n\
         });\n\
         Promise.reject('x');\n",
      )?;
      Ok(())
    })?;

    assert_eq!(
      host.run_until_idle(RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert!(matches!(
      get_global_prop(&mut host, "__default_prevented"),
      Value::Bool(true)
    ));
    Ok(())
  }

  #[test]
  fn onunhandledrejection_handler_runs_and_return_false_cancels() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;

    host.queue_task(TaskSource::Script, |host_state, event_loop| {
      host_state.exec_script_in_event_loop(
        event_loop,
        "this.__called = false;\n\
         this.__default_prevented = false;\n\
         this.__reason = undefined;\n\
         this.onunhandledrejection = function (e) {\n\
           this.__called = true;\n\
           this.__reason = e.reason;\n\
           // Read `defaultPrevented` after dispatch completes.\n\
           queueMicrotask(() => { this.__default_prevented = e.defaultPrevented; });\n\
           return false;\n\
         };\n\
         Promise.reject('x');\n",
      )?;
      Ok(())
    })?;

    assert_eq!(
      host.run_until_idle(RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert!(matches!(
      get_global_prop(&mut host, "__called"),
      Value::Bool(true)
    ));
    assert_eq!(
      get_global_prop_utf8(&mut host, "__reason").as_deref(),
      Some("x")
    );
    assert!(matches!(
      get_global_prop(&mut host, "__default_prevented"),
      Value::Bool(true)
    ));

    Ok(())
  }

  #[test]
  fn onrejectionhandled_handler_runs() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;

    host.queue_task(TaskSource::Script, |host_state, event_loop| {
      host_state.exec_script_in_event_loop(
        event_loop,
        "this.__called = false;\n\
         this.__reason = undefined;\n\
         this.onrejectionhandled = function (e) {\n\
           this.__called = true;\n\
           this.__reason = e.reason;\n\
         };\n\
         var p = Promise.reject('x');\n\
         setTimeout(function () { p.catch(function () {}); }, 0);\n",
      )?;
      Ok(())
    })?;

    assert_eq!(
      host.run_until_idle(RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert!(matches!(
      get_global_prop(&mut host, "__called"),
      Value::Bool(true)
    ));
    assert_eq!(
      get_global_prop_utf8(&mut host, "__reason").as_deref(),
      Some("x")
    );
    Ok(())
  }

  #[test]
  fn promise_rejection_events_use_promise_rejection_event_and_are_read_only() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;

    host.queue_task(TaskSource::Script, |host_state, event_loop| {
      host_state.exec_script_in_event_loop(
        event_loop,
        "this.__has_ctor = (typeof PromiseRejectionEvent === 'function');\n\
         this.__ctor_name = undefined;\n\
         this.__is_instance = false;\n\
         this.__promise_then = false;\n\
         this.__promise_then_after = false;\n\
         this.__reason = undefined;\n\
         this.__reason_after = undefined;\n\
         this.__reason_assign_err = undefined;\n\
         this.__promise_assign_err = undefined;\n\
         addEventListener('unhandledrejection', function (e) {\n\
           \"use strict\";\n\
           this.__ctor_name = e && e.constructor && e.constructor.name;\n\
           this.__is_instance = (typeof PromiseRejectionEvent === 'function') && (e instanceof PromiseRejectionEvent);\n\
           this.__promise_then = !!(e.promise && typeof e.promise.then === 'function');\n\
           this.__reason = e.reason;\n\
           try { e.reason = 'y'; } catch (err) { this.__reason_assign_err = err && err.name; }\n\
           try { e.promise = null; } catch (err) { this.__promise_assign_err = err && err.name; }\n\
           this.__reason_after = e.reason;\n\
           this.__promise_then_after = !!(e.promise && typeof e.promise.then === 'function');\n\
         });\n\
         Promise.reject('x');\n",
      )?;
      Ok(())
    })?;

    assert_eq!(
      host.run_until_idle(RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );

    assert!(matches!(
      get_global_prop(&mut host, "__has_ctor"),
      Value::Bool(true)
    ));
    assert_eq!(
      get_global_prop_utf8(&mut host, "__ctor_name").as_deref(),
      Some("PromiseRejectionEvent")
    );
    assert!(matches!(
      get_global_prop(&mut host, "__is_instance"),
      Value::Bool(true)
    ));
    assert!(matches!(
      get_global_prop(&mut host, "__promise_then"),
      Value::Bool(true)
    ));
    assert_eq!(
      get_global_prop_utf8(&mut host, "__reason").as_deref(),
      Some("x")
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__reason_assign_err").as_deref(),
      Some("TypeError")
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__promise_assign_err").as_deref(),
      Some("TypeError")
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__reason_after").as_deref(),
      Some("x")
    );
    assert!(matches!(
      get_global_prop(&mut host, "__promise_then_after"),
      Value::Bool(true)
    ));

    Ok(())
  }

  #[test]
  fn error_beforeunload_and_pagetransition_event_constructors_exist_and_roundtrip_init() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;

    host.queue_task(TaskSource::Script, |host_state, event_loop| {
      host_state.exec_script_in_event_loop(
        event_loop,
        "this.__has_error_event_ctor = (typeof ErrorEvent === 'function');\n\
         this.__has_beforeunload_event_ctor = (typeof BeforeUnloadEvent === 'function');\n\
         this.__has_pagetransition_event_ctor = (typeof PageTransitionEvent === 'function');\n\
         this.__listener_message = undefined;\n\
         addEventListener('error', (e) => { this.__listener_message = e && e.message; });\n\
         var e1 = new ErrorEvent('error', {\n\
           message: 'boom',\n\
           filename: 'https://example.invalid/app.js',\n\
           lineno: 10,\n\
           colno: 20,\n\
           error: 123,\n\
           bubbles: true,\n\
           cancelable: true,\n\
           composed: true,\n\
         });\n\
         this.__e1_is_instance = (e1 instanceof ErrorEvent);\n\
         this.__e1_message = e1.message;\n\
         this.__e1_filename = e1.filename;\n\
         this.__e1_lineno = e1.lineno;\n\
         this.__e1_colno = e1.colno;\n\
         this.__e1_error = e1.error;\n\
         this.__e1_bubbles = e1.bubbles;\n\
         this.__e1_cancelable = e1.cancelable;\n\
         this.__e1_composed = e1.composed;\n\
         dispatchEvent(e1);\n\
         var e2 = new BeforeUnloadEvent('beforeunload', { returnValue: 'bye', cancelable: true });\n\
         this.__e2_is_instance = (e2 instanceof BeforeUnloadEvent);\n\
         this.__e2_return_value = e2.returnValue;\n\
         e2.returnValue = 'changed';\n\
         this.__e2_return_value_after = e2.returnValue;\n\
         var e3 = new PageTransitionEvent('pageshow', { persisted: true, bubbles: true });\n\
         this.__e3_is_instance = (e3 instanceof PageTransitionEvent);\n\
         this.__e3_persisted = e3.persisted;\n\
         this.__e3_bubbles = e3.bubbles;\n",
      )?;
      Ok(())
    })?;

    assert_eq!(
      host.run_until_idle(RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );

    assert!(matches!(
      get_global_prop(&mut host, "__has_error_event_ctor"),
      Value::Bool(true)
    ));
    assert!(matches!(
      get_global_prop(&mut host, "__has_beforeunload_event_ctor"),
      Value::Bool(true)
    ));
    assert!(matches!(
      get_global_prop(&mut host, "__has_pagetransition_event_ctor"),
      Value::Bool(true)
    ));

    assert!(matches!(
      get_global_prop(&mut host, "__e1_is_instance"),
      Value::Bool(true)
    ));
    assert_eq!(
      get_global_prop_utf8(&mut host, "__e1_message").as_deref(),
      Some("boom")
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__e1_filename").as_deref(),
      Some("https://example.invalid/app.js")
    );
    assert!(matches!(
      get_global_prop(&mut host, "__e1_lineno"),
      Value::Number(n) if (n - 10.0).abs() < f64::EPSILON
    ));
    assert!(matches!(
      get_global_prop(&mut host, "__e1_colno"),
      Value::Number(n) if (n - 20.0).abs() < f64::EPSILON
    ));
    assert!(matches!(
      get_global_prop(&mut host, "__e1_error"),
      Value::Number(n) if (n - 123.0).abs() < f64::EPSILON
    ));
    assert!(matches!(
      get_global_prop(&mut host, "__e1_bubbles"),
      Value::Bool(true)
    ));
    assert!(matches!(
      get_global_prop(&mut host, "__e1_cancelable"),
      Value::Bool(true)
    ));
    assert!(matches!(
      get_global_prop(&mut host, "__e1_composed"),
      Value::Bool(true)
    ));

    assert_eq!(
      get_global_prop_utf8(&mut host, "__listener_message").as_deref(),
      Some("boom")
    );

    assert!(matches!(
      get_global_prop(&mut host, "__e2_is_instance"),
      Value::Bool(true)
    ));
    assert_eq!(
      get_global_prop_utf8(&mut host, "__e2_return_value").as_deref(),
      Some("bye")
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__e2_return_value_after").as_deref(),
      Some("changed")
    );

    assert!(matches!(
      get_global_prop(&mut host, "__e3_is_instance"),
      Value::Bool(true)
    ));
    assert!(matches!(
      get_global_prop(&mut host, "__e3_persisted"),
      Value::Bool(true)
    ));
    assert!(matches!(
      get_global_prop(&mut host, "__e3_bubbles"),
      Value::Bool(true)
    ));

    Ok(())
  }

  #[test]
  fn handled_after_notification_dispatches_rejectionhandled_event() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;

    host.queue_task(TaskSource::Script, |host_state, event_loop| {
      host_state.exec_script_in_event_loop(
        event_loop,
        "this.__order = '';\n\
         this.__unhandled = undefined;\n\
         this.__handled = undefined;\n\
         addEventListener('unhandledrejection', function (e) {\n\
           this.__order += 'u';\n\
           this.__unhandled = e.reason;\n\
         });\n\
         addEventListener('rejectionhandled', function (e) {\n\
           this.__order += 'h';\n\
           this.__handled = e.reason;\n\
         });\n\
         var p = Promise.reject('x');\n\
         setTimeout(function () { p.catch(function () {}); }, 0);\n",
      )?;
      Ok(())
    })?;

    assert_eq!(
      host.run_until_idle(RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__order").as_deref(),
      Some("uh")
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__unhandled").as_deref(),
      Some("x")
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__handled").as_deref(),
      Some("x")
    );
    Ok(())
  }

  #[test]
  fn synchronously_handled_rejection_does_not_dispatch_unhandledrejection() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;

    host.queue_task(TaskSource::Script, |host_state, event_loop| {
      host_state.exec_script_in_event_loop(
        event_loop,
        "this.__fired = false;\n\
         addEventListener('unhandledrejection', function () { this.__fired = true; });\n\
         Promise.reject('x').catch(function () {});\n",
      )?;
      Ok(())
    })?;

    assert_eq!(
      host.run_until_idle(RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert!(matches!(
      get_global_prop(&mut host, "__fired"),
      Value::Bool(false)
    ));
    Ok(())
  }

  #[test]
  fn window_host_event_target_dispatch_uses_shared_dom_events() -> Result<()> {
    let renderer_dom = crate::dom::parse_html("<!doctype html><html><body></body></html>").unwrap();
    let dom = dom2::Document::from_renderer_dom(&renderer_dom);
    // This test exercises a fairly deep chain of JS<->Rust native calls (DOM wrapper creation,
    // listener registration, and event propagation). The default per-script wall-time budget is
    // tuned for hostile input and can be a bit too tight in debug builds.
    let mut opts = JsExecutionOptions::default();
    opts.event_loop_run_limits.max_wall_time = Some(Duration::from_secs(2));
    let mut host = WindowHost::new_with_options(dom, "https://example.invalid/", opts)?;

    let log = host.exec_script(
      "(() => {\n\
        let log = '';\n\
        const win = this;\n\
        const doc = document;\n\
        const html = document.documentElement;\n\
        const body = document.body;\n\
        const div = document.createElement('div');\n\
        body.appendChild(div);\n\
\n\
        function rec(label) {\n\
          return function (e) {\n\
            const ct = this === win ? 'win'\n\
              : this === doc ? 'doc'\n\
              : this === html ? 'html'\n\
              : this === body ? 'body'\n\
              : this === div ? 'div'\n\
              : 'other';\n\
            if (log) log += ',';\n\
            log += label + ':' + ct + ':' + e.eventPhase + ':' + (e.target === div) + ':' + (e.currentTarget === this);\n\
          };\n\
        }\n\
\n\
        win.addEventListener('x', rec('wC'), true);\n\
        doc.addEventListener('x', rec('dC'), true);\n\
        html.addEventListener('x', rec('hC'), true);\n\
        body.addEventListener('x', rec('bC'), true);\n\
        div.addEventListener('x', rec('tC'), true);\n\
        div.addEventListener('x', rec('tB'), false);\n\
        body.addEventListener('x', rec('bB'), false);\n\
        html.addEventListener('x', rec('hB'), false);\n\
        doc.addEventListener('x', rec('dB'), false);\n\
        win.addEventListener('x', rec('wB'), false);\n\
\n\
        div.dispatchEvent(new Event('x', { bubbles: true }));\n\
        return log;\n\
      })()",
    )?;

    assert_eq!(
      value_to_string(&host, log),
      "wC:win:1:true:true,dC:doc:1:true:true,hC:html:1:true:true,bC:body:1:true:true,tC:div:2:true:true,tB:div:2:true:true,bB:body:3:true:true,hB:html:3:true:true,dB:doc:3:true:true,wB:win:3:true:true"
    );

    Ok(())
  }

  #[test]
  fn dynamic_external_script_sri_mismatch_blocks_execution() -> Result<()> {
    let renderer_dom =
      crate::dom::parse_html("<!doctype html><html><head></head><body></body></html>").unwrap();
    let dom = dom2::Document::from_renderer_dom(&renderer_dom);

    let script_source = "this.__ran = true;";
    let script_url = "https://example.invalid/app.js";

    let mut resource = FetchedResource::new(
      script_source.as_bytes().to_vec(),
      Some("application/javascript".to_string()),
    );
    resource.status = Some(200);

    let fetcher = Arc::new(MapResourceFetcher::default());
    fetcher.insert(script_url, resource);

    let mut host = WindowHost::new_with_fetcher(dom, "https://example.invalid/", fetcher)?;

    let wrong_digest = BASE64_STANDARD.encode(Sha256::digest(b"other"));
    let integrity = format!("sha256-{wrong_digest}");
    host.exec_script(&format!(
      "(() => {{\n\
        this.__ran = false;\n\
        const s = document.createElement('script');\n\
        s.src = '{script_url}';\n\
        s.setAttribute('integrity', '{integrity}');\n\
        document.head.appendChild(s);\n\
      }})()"
    ))?;

    let err = host
      .run_until_idle(RunLimits::unbounded())
      .expect_err("expected SRI mismatch to block dynamic script execution");
    let Error::Other(msg) = err else {
      panic!("expected Error::Other, got {err:?}");
    };
    assert!(
      msg.contains("SRI blocked script"),
      "expected SRI error message, got {msg:?}"
    );
    assert!(matches!(
      get_global_prop(&mut host, "__ran"),
      Value::Bool(false)
    ));
    Ok(())
  }

  #[test]
  fn dynamic_external_script_sri_oversized_integrity_attribute_blocks_execution() -> Result<()> {
    let renderer_dom =
      crate::dom::parse_html("<!doctype html><html><head></head><body></body></html>").unwrap();
    let dom = dom2::Document::from_renderer_dom(&renderer_dom);

    let script_source = "this.__ran = true;";
    let script_url = "https://example.invalid/app.js";

    let mut resource = FetchedResource::new(
      script_source.as_bytes().to_vec(),
      Some("application/javascript".to_string()),
    );
    resource.status = Some(200);

    let fetcher = Arc::new(MapResourceFetcher::default());
    fetcher.insert(script_url, resource);

    let mut host = WindowHost::new_with_fetcher(dom, "https://example.invalid/", fetcher)?;

    let integrity = "a".repeat(crate::js::sri::MAX_INTEGRITY_ATTRIBUTE_BYTES + 1);
    host.exec_script(&format!(
      "(() => {{\n\
        this.__ran = false;\n\
        const s = document.createElement('script');\n\
        s.src = '{script_url}';\n\
        s.setAttribute('integrity', '{integrity}');\n\
        document.head.appendChild(s);\n\
      }})()"
    ))?;

    let err = host
      .run_until_idle(RunLimits::unbounded())
      .expect_err("expected oversized integrity attribute to block dynamic script execution");
    let Error::Other(msg) = err else {
      panic!("expected Error::Other, got {err:?}");
    };
    assert!(
      msg.contains("integrity attribute exceeded max length"),
      "expected oversized integrity error message, got {msg:?}"
    );
    assert!(matches!(
      get_global_prop(&mut host, "__ran"),
      Value::Bool(false)
    ));
    Ok(())
  }

  #[test]
  fn dynamic_inline_script_oversized_integrity_attribute_does_not_execute_and_is_not_started(
  ) -> Result<()> {
    let renderer_dom =
      crate::dom::parse_html("<!doctype html><html><head></head><body></body></html>").unwrap();
    let dom = dom2::Document::from_renderer_dom(&renderer_dom);

    let mut host = WindowHost::new(dom, "https://example.invalid/")?;

    let integrity = "a".repeat(crate::js::sri::MAX_INTEGRITY_ATTRIBUTE_BYTES + 1);
    host.exec_script(&format!(
      "(() => {{\n\
        this.__ran = false;\n\
        const s = document.createElement('script');\n\
        s.setAttribute('id', 's');\n\
        s.setAttribute('integrity', '{integrity}');\n\
        s.appendChild(document.createTextNode('this.__ran = true;'));\n\
        document.head.appendChild(s);\n\
      }})()"
    ))?;

    assert!(matches!(
      get_global_prop(&mut host, "__ran"),
      Value::Bool(false)
    ));
    let script = host
      .host()
      .dom()
      .get_element_by_id("s")
      .expect("expected #s script");
    assert!(
      !host.host().dom().node(script).script_already_started,
      "expected scripts with invalid integrity metadata not to be marked already started"
    );
    Ok(())
  }

  #[test]
  fn dynamic_external_script_sri_cross_origin_without_crossorigin_blocks_execution() -> Result<()> {
    let renderer_dom =
      crate::dom::parse_html("<!doctype html><html><head></head><body></body></html>").unwrap();
    let dom = dom2::Document::from_renderer_dom(&renderer_dom);

    let script_source = "this.__ran = true;";
    let script_url = "https://cross-origin.invalid/app.js";

    let mut resource = FetchedResource::new(
      script_source.as_bytes().to_vec(),
      Some("application/javascript".to_string()),
    );
    resource.status = Some(200);

    let fetcher = Arc::new(MapResourceFetcher::default());
    fetcher.insert(script_url, resource);

    let mut host = WindowHost::new_with_fetcher(dom, "https://example.invalid/", fetcher)?;

    let digest = BASE64_STANDARD.encode(Sha256::digest(script_source.as_bytes()));
    let integrity = format!("sha256-{digest}");
    host.exec_script(&format!(
      "(() => {{\n\
        this.__ran = false;\n\
        const s = document.createElement('script');\n\
        s.src = '{script_url}';\n\
        s.setAttribute('integrity', '{integrity}');\n\
        document.head.appendChild(s);\n\
      }})()"
    ))?;

    let err = host
      .run_until_idle(RunLimits::unbounded())
      .expect_err("expected cross-origin SRI without crossorigin to block execution");
    let Error::Other(msg) = err else {
      panic!("expected Error::Other, got {err:?}");
    };
    assert_eq!(
      msg,
      format!(
        "SRI blocked script {script_url}: cross-origin integrity requires a CORS-enabled fetch (missing crossorigin attribute)"
      )
    );
    assert!(matches!(
      get_global_prop(&mut host, "__ran"),
      Value::Bool(false)
    ));
    Ok(())
  }

  #[test]
  fn dynamic_external_script_sri_match_executes() -> Result<()> {
    let renderer_dom =
      crate::dom::parse_html("<!doctype html><html><head></head><body></body></html>").unwrap();
    let dom = dom2::Document::from_renderer_dom(&renderer_dom);

    let script_source = "this.__ran = true;";
    let script_url = "https://example.invalid/app.js";

    let mut resource = FetchedResource::new(
      script_source.as_bytes().to_vec(),
      Some("application/javascript".to_string()),
    );
    resource.status = Some(200);

    let fetcher = Arc::new(MapResourceFetcher::default());
    fetcher.insert(script_url, resource);

    let mut host = WindowHost::new_with_fetcher(dom, "https://example.invalid/", fetcher)?;

    let digest = BASE64_STANDARD.encode(Sha256::digest(script_source.as_bytes()));
    let integrity = format!("sha256-{digest}");
    host.exec_script(&format!(
      "(() => {{\n\
        this.__ran = false;\n\
        const s = document.createElement('script');\n\
        s.src = '{script_url}';\n\
        s.setAttribute('integrity', '{integrity}');\n\
        document.head.appendChild(s);\n\
      }})()"
    ))?;

    assert_eq!(
      host.run_until_idle(RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert!(matches!(
      get_global_prop(&mut host, "__ran"),
      Value::Bool(true)
    ));
    Ok(())
  }

  #[test]
  fn exec_script_error_includes_stack_trace() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;

    let err = host
      .exec_script("1;\nthrow \"boom\";")
      .expect_err("expected script to throw");
    let Error::Other(msg) = err else {
      panic!("expected Error::Other, got {err:?}");
    };

    assert!(
      msg.contains("boom"),
      "expected message to include thrown string, got {msg:?}"
    );
    assert!(
      msg.contains("at "),
      "expected message to include stack trace, got {msg:?}"
    );
    assert!(
      msg.contains(":2:1"),
      "expected stack trace to include line/col 2:1, got {msg:?}"
    );
    Ok(())
  }

  #[test]
  fn abort_controller_exists_and_dispatches_abort_event() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;

    host.exec_script(
      r#"
      var g = this;
      g.__has_abort_controller = (typeof AbortController === 'function');
      var c = new AbortController();
      g.__abort_fired = false;
      g.__onabort_fired = false;
      c.signal.addEventListener('abort', function () { g.__abort_fired = true; });
      c.signal.onabort = function () { g.__onabort_fired = true; };
      c.abort();
      g.__aborted = c.signal.aborted;
      g.__reason_name = c.signal.reason && c.signal.reason.name;
      "#,
    )?;

    assert!(matches!(
      get_global_prop(&mut host, "__has_abort_controller"),
      Value::Bool(true)
    ));
    assert!(matches!(
      get_global_prop(&mut host, "__abort_fired"),
      Value::Bool(true)
    ));
    assert!(matches!(
      get_global_prop(&mut host, "__onabort_fired"),
      Value::Bool(true)
    ));
    assert!(matches!(
      get_global_prop(&mut host, "__aborted"),
      Value::Bool(true)
    ));
    assert_eq!(
      get_global_prop_utf8(&mut host, "__reason_name").as_deref(),
      Some("AbortError")
    );
    Ok(())
  }

  #[test]
  fn js_budget_terminates_infinite_loop_in_top_level_script() -> Result<()> {
    let mut js_options = JsExecutionOptions::default();
    js_options.max_instruction_count = Some(10_000);
    js_options.event_loop_run_limits.max_wall_time = None;

    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host =
      WindowHost::new_with_js_execution_options(dom, "https://example.invalid/", js_options)?;

    let err = host
      .exec_script("while (true) {}")
      .expect_err("expected infinite loop to terminate");
    assert!(
      err.to_string().contains("out of fuel"),
      "expected out-of-fuel termination, got {err}"
    );
    Ok(())
  }

  #[test]
  fn abort_signal_timeout_zero_aborts_on_next_turn() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;

    host.exec_script(
      r#"
      var g = this;
      g.__timeout_signal = AbortSignal.timeout(0);
      g.__timeout_fired = false;
      g.__timeout_signal.addEventListener('abort', function () { g.__timeout_fired = true; });
      g.__timeout_aborted_before = g.__timeout_signal.aborted;
      "#,
    )?;

    assert!(matches!(
      get_global_prop(&mut host, "__timeout_aborted_before"),
      Value::Bool(false)
    ));
    assert!(matches!(
      get_global_prop(&mut host, "__timeout_fired"),
      Value::Bool(false)
    ));

    host.run_until_idle(RunLimits {
      max_tasks: 10,
      max_microtasks: 100,
      max_wall_time: Some(Duration::from_secs(1)),
    })?;

    let aborted_after = host.exec_script("__timeout_signal.aborted")?;
    assert!(matches!(aborted_after, Value::Bool(true)));
    assert!(matches!(
      get_global_prop(&mut host, "__timeout_fired"),
      Value::Bool(true)
    ));
    Ok(())
  }

  #[derive(Default)]
  struct CountingFetcher {
    calls: AtomicUsize,
  }

  impl ResourceFetcher for CountingFetcher {
    fn fetch(&self, url: &str) -> Result<FetchedResource> {
      self.calls.fetch_add(1, Ordering::Relaxed);
      Err(Error::Other(format!(
        "CountingFetcher does not support fetch: {url}"
      )))
    }
  }

  #[test]
  fn fetch_rejects_when_signal_is_pre_aborted() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let fetcher = Arc::new(CountingFetcher::default());
    let mut host = WindowHost::new_with_fetcher(dom, "https://example.invalid/", fetcher.clone())?;

    host.exec_script(
      r#"
      var g = this;
      var c = new AbortController();
      c.abort();
      fetch("/", { signal: c.signal }).catch(function (e) {
        g.__fetch_err_name = e && e.name;
      });
      "#,
    )?;

    // Rejection happens synchronously (no networking task enqueued), but Promise reactions are
    // microtasks.
    host.perform_microtask_checkpoint()?;

    assert_eq!(
      get_global_prop_utf8(&mut host, "__fetch_err_name").as_deref(),
      Some("AbortError")
    );
    assert_eq!(fetcher.calls.load(Ordering::Relaxed), 0);
    assert!(host.event_loop().is_idle());
    Ok(())
  }

  #[test]
  fn js_budget_terminates_recursion_via_stack_depth_limit() -> Result<()> {
    let mut js_options = JsExecutionOptions::default();
    js_options.max_stack_depth = Some(64);
    js_options.event_loop_run_limits.max_wall_time = None;

    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host =
      WindowHost::new_with_js_execution_options(dom, "https://example.invalid/", js_options)?;

    let err = host
      .exec_script("function f() { return f(); }\nf();")
      .expect_err("expected recursion to terminate");
    assert!(
      err.to_string().contains("stack overflow"),
      "expected stack overflow termination, got {err}"
    );
    Ok(())
  }

  #[test]
  fn fetch_can_be_aborted_after_scheduling_before_execution() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let fetcher = Arc::new(CountingFetcher::default());
    let mut host = WindowHost::new_with_fetcher(dom, "https://example.invalid/", fetcher.clone())?;

    host.exec_script(
      r#"
      var g = this;
      var c = new AbortController();
      fetch("/", { signal: c.signal }).catch(function (e) {
        g.__fetch2_err_name = e && e.name;
      });
      c.abort();
      "#,
    )?;

    host.run_until_idle(RunLimits {
      max_tasks: 10,
      max_microtasks: 100,
      max_wall_time: Some(Duration::from_secs(1)),
    })?;

    assert_eq!(
      get_global_prop_utf8(&mut host, "__fetch2_err_name").as_deref(),
      Some("AbortError")
    );
    assert_eq!(fetcher.calls.load(Ordering::Relaxed), 0);
    Ok(())
  }

  #[test]
  fn js_budget_terminates_infinite_loop_in_promise_job() -> Result<()> {
    let mut js_options = JsExecutionOptions::default();
    js_options.max_instruction_count = Some(10_000);
    js_options.event_loop_run_limits.max_wall_time = None;

    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host =
      WindowHost::new_with_js_execution_options(dom, "https://example.invalid/", js_options)?;

    host.exec_script("Promise.resolve().then(function () { while (true) {} });")?;
    let err = host
      .perform_microtask_checkpoint()
      .expect_err("expected Promise job to terminate");
    assert!(
      err.to_string().contains("out of fuel"),
      "expected out-of-fuel termination, got {err}"
    );
    Ok(())
  }

  #[test]
  fn request_exposes_signal_and_clone_preserves_it() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;

    host.exec_script(
      r#"
      var g = this;
      var c = new AbortController();
      var r1 = new Request("/", { signal: c.signal });
      var r2 = r1.clone();
      g.__req_signal_same = (r1.signal === c.signal) && (r2.signal === c.signal);
      "#,
    )?;

    assert!(matches!(
      get_global_prop(&mut host, "__req_signal_same"),
      Value::Bool(true)
    ));
    Ok(())
  }

  #[test]
  fn js_heap_limit_terminates_large_allocations() -> Result<()> {
    // WindowRealm initialization can allocate a non-trivial amount of heap memory. Find a small heap
    // cap that still allows initialization, then verify that we can deterministically trigger an
    // allocation failure from JS.
    let mut max_bytes = 1024usize;
    let mut host = loop {
      let mut js_options = JsExecutionOptions::default();
      js_options.max_vm_heap_bytes = Some(max_bytes);
      js_options.event_loop_run_limits.max_wall_time = None;

      let dom = dom2::Document::new(QuirksMode::NoQuirks);
      match WindowHost::new_with_js_execution_options(dom, "https://example.invalid/", js_options) {
        Ok(host) => break host,
        Err(_) => {
          max_bytes = max_bytes.saturating_mul(2);
          assert!(
            max_bytes <= 64 * 1024 * 1024,
            "failed to find a heap limit that allows WindowHost initialization"
          );
        }
      }
    };

    let err = host
      // Avoid relying on Array.prototype methods (which may be unimplemented in our JS VM): keep
      // allocating reachable objects until the VM hits its heap limit.
      .exec_script("var o = {}; while (true) { o = { next: o }; }")
      .expect_err("expected heap allocations to exceed vm heap limit");
    assert!(
      err.to_string().contains("out of memory"),
      "expected out-of-memory error, got {err}"
    );
    Ok(())
  }

  #[test]
  fn dynamic_inline_script_executes_as_task_on_insertion() -> Result<()> {
    let mut dom = dom2::Document::new(QuirksMode::NoQuirks);
    let container = dom.create_element("div", "");
    dom
      .set_attribute(container, "id", "c")
      .expect("set container id");
    dom
      .append_child(dom.root(), container)
      .expect("append container");
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;

    host.exec_script(
      r#"
      globalThis.__log = "";
      const container = document.getElementById("c");
      const s = document.createElement("script");
      s.textContent = "globalThis.__log += 'S';";
      container.appendChild(s);
      globalThis.__log += "A";
      "#,
    )?;

    assert_eq!(
      get_global_prop_utf8(&mut host, "__log").as_deref(),
      Some("A")
    );

    host.run_until_idle(RunLimits {
      max_tasks: 10,
      max_microtasks: 100,
      max_wall_time: Some(Duration::from_secs(1)),
    })?;

    assert_eq!(
      get_global_prop_utf8(&mut host, "__log").as_deref(),
      Some("AS")
    );
    Ok(())
  }

  #[test]
  fn dynamic_script_executes_once_after_text_content_set() -> Result<()> {
    let mut dom = dom2::Document::new(QuirksMode::NoQuirks);
    let container = dom.create_element("div", "");
    dom
      .set_attribute(container, "id", "c")
      .expect("set container id");
    dom
      .append_child(dom.root(), container)
      .expect("append container");
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;

    host.exec_script(
      r#"
      globalThis.__log = "";
      const container = document.getElementById("c");
      const s = document.createElement("script");
      container.appendChild(s);
      // Empty scripts must not start on insertion; they should start once content becomes non-empty.
      s.textContent = "globalThis.__log += 'S';";
      globalThis.__log += "A";
      globalThis.__s = s;
      "#,
    )?;

    assert_eq!(
      get_global_prop_utf8(&mut host, "__log").as_deref(),
      Some("A")
    );

    host.run_until_idle(RunLimits {
      max_tasks: 10,
      max_microtasks: 100,
      max_wall_time: Some(Duration::from_secs(1)),
    })?;

    assert_eq!(
      get_global_prop_utf8(&mut host, "__log").as_deref(),
      Some("AS")
    );

    // Mutating the script again must not re-execute it.
    host.exec_script(
      r#"
      globalThis.__s.textContent = "globalThis.__log += 'X';";
      globalThis.__log += "B";
      "#,
    )?;

    host.run_until_idle(RunLimits {
      max_tasks: 10,
      max_microtasks: 100,
      max_wall_time: Some(Duration::from_secs(1)),
    })?;

    assert_eq!(
      get_global_prop_utf8(&mut host, "__log").as_deref(),
      Some("ASB")
    );
    Ok(())
  }

  #[derive(Default)]
  struct ScriptMapFetcher {
    entries: Mutex<HashMap<String, Vec<u8>>>,
  }

  impl ScriptMapFetcher {
    fn insert(&self, url: &str, bytes: Vec<u8>) {
      self
        .entries
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(url.to_string(), bytes);
    }
  }

  impl ResourceFetcher for ScriptMapFetcher {
    fn fetch(&self, url: &str) -> Result<FetchedResource> {
      let bytes = self
        .entries
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(url)
        .cloned()
        .ok_or_else(|| Error::Other(format!("no entry for url={url}")))?;
      Ok(FetchedResource {
        bytes,
        content_type: None,
        nosniff: false,
        content_encoding: None,
        status: None,
        etag: None,
        last_modified: None,
        access_control_allow_origin: None,
        timing_allow_origin: None,
        vary: None,
        response_referrer_policy: None,
        access_control_allow_credentials: false,
        final_url: None,
        cache_policy: None,
        response_headers: None,
      })
    }
  }

  #[test]
  fn dynamic_script_executes_once_after_src_set() -> Result<()> {
    let mut dom = dom2::Document::new(QuirksMode::NoQuirks);
    let container = dom.create_element("div", "");
    dom
      .set_attribute(container, "id", "c")
      .expect("set container id");
    dom
      .append_child(dom.root(), container)
      .expect("append container");
    let fetcher = Arc::new(ScriptMapFetcher::default());
    fetcher.insert(
      "https://example.invalid/a.js",
      b"globalThis.__log += 'S';".to_vec(),
    );
    fetcher.insert(
      "https://example.invalid/b.js",
      b"globalThis.__log += 'X';".to_vec(),
    );
    let mut host = WindowHost::new_with_fetcher(dom, "https://example.invalid/", fetcher)?;

    host.exec_script(
      r#"
      globalThis.__log = "";
      const container = document.getElementById("c");
      const s = document.createElement("script");
      container.appendChild(s);
      s.src = "https://example.invalid/a.js";
      globalThis.__log += "A";
      globalThis.__s = s;
      "#,
    )?;

    assert_eq!(
      get_global_prop_utf8(&mut host, "__log").as_deref(),
      Some("A")
    );

    host.run_until_idle(RunLimits {
      max_tasks: 10,
      max_microtasks: 100,
      max_wall_time: Some(Duration::from_secs(1)),
    })?;

    assert_eq!(
      get_global_prop_utf8(&mut host, "__log").as_deref(),
      Some("AS")
    );

    host.exec_script(
      r#"
      globalThis.__s.src = "https://example.invalid/b.js";
      globalThis.__log += "B";
      "#,
    )?;

    host.run_until_idle(RunLimits {
      max_tasks: 10,
      max_microtasks: 100,
      max_wall_time: Some(Duration::from_secs(1)),
    })?;

    assert_eq!(
      get_global_prop_utf8(&mut host, "__log").as_deref(),
      Some("ASB")
    );
    Ok(())
  }

  #[test]
  fn dynamic_external_script_charset_attribute_controls_decoding() -> Result<()> {
    let mut dom = dom2::Document::new(QuirksMode::NoQuirks);
    let container = dom.create_element("div", "");
    dom
      .set_attribute(container, "id", "c")
      .expect("set container id");
    dom
      .append_child(dom.root(), container)
      .expect("append container");

    // Return SHIFT_JIS-encoded bytes to ensure the dynamic script loader honors the `<script charset>`
    // fallback encoding.
    let fetcher = Arc::new(ScriptMapFetcher::default());
    let encoded = encoding_rs::SHIFT_JIS
      .encode("globalThis.__log += 'デ';")
      .0
      .into_owned();
    fetcher.insert("https://example.invalid/a.js", encoded);

    let mut host = WindowHost::new_with_fetcher(dom, "https://example.invalid/", fetcher)?;

    host.exec_script(
      r#"
      globalThis.__log = "";
      const container = document.getElementById("c");
      const s = document.createElement("script");
      container.appendChild(s);
      s.charset = "shift_jis";
      s.src = "https://example.invalid/a.js";
      globalThis.__log += "A";
      "#,
    )?;

    assert_eq!(
      get_global_prop_utf8(&mut host, "__log").as_deref(),
      Some("A")
    );

    host.run_until_idle(RunLimits {
      max_tasks: 10,
      max_microtasks: 100,
      max_wall_time: Some(Duration::from_secs(1)),
    })?;

    assert_eq!(
      get_global_prop_utf8(&mut host, "__log").as_deref(),
      Some("Aデ")
    );
    Ok(())
  }
}

#[cfg(test)]
mod import_map_tests {
  use super::WindowHostState;
  use crate::dom2;
  use crate::js::import_maps::{ImportMapError, SpecifierAsUrlKind};
  use selectors::context::QuirksMode;
  use url::Url;

  #[test]
  fn window_host_state_starts_with_empty_import_map_state() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let host = WindowHostState::new(dom, "https://example.com/index.html").expect("new host state");

    let state = host.import_map_state();
    assert!(state.import_map.imports.is_empty());
    assert!(state.import_map.scopes.is_empty());
    assert!(state.import_map.integrity.is_empty());
    assert!(state.resolved_module_set().is_empty());
  }

  #[test]
  fn window_host_can_register_import_map_and_resolve_specifier() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host =
      WindowHostState::new(dom, "https://example.com/index.html").expect("new host state");
    let base_url = Url::parse("https://example.com/index.html").expect("parse base URL");

    let warnings = host
      .register_import_map_string(
        r#"{ "imports": { "react": "/vendor/react.js" } }"#,
        &base_url,
      )
      .expect("register import map should succeed");
    assert!(
      warnings.is_empty(),
      "expected no warnings, got {warnings:?}"
    );
    assert!(host.import_map_state().resolved_module_set().is_empty());

    let resolved = host
      .resolve_module_specifier_with_import_maps("react", &base_url)
      .expect("resolve should succeed");
    assert_eq!(
      resolved,
      Url::parse("https://example.com/vendor/react.js").expect("parse expected URL")
    );

    let records = host.import_map_state().resolved_module_set();
    assert_eq!(records.len(), 1);
    assert_eq!(
      records[0].serialized_base_url.as_deref(),
      Some("https://example.com/index.html")
    );
    assert_eq!(records[0].specifier, "react");
    assert_eq!(records[0].as_url_kind, SpecifierAsUrlKind::NotUrl);
  }

  #[test]
  fn window_host_register_import_map_propagates_errors() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host =
      WindowHostState::new(dom, "https://example.com/index.html").expect("new host state");
    let base_url = Url::parse("https://example.com/index.html").expect("parse base URL");

    let err = host
      .register_import_map_string("{", &base_url)
      .expect_err("expected invalid JSON to error");
    assert!(
      matches!(err, ImportMapError::Json(_)),
      "unexpected error: {err:?}"
    );
  }
}
