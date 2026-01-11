use crate::dom::HTML_NAMESPACE;
use crate::debug::trace::TraceHandle;
use crate::dom2::{Document, NodeId, NodeKind};
use crate::error::{Error, Result};
use crate::html::base_url_tracker::BaseUrlTracker;
use crate::html::content_security_policy::CspPolicy;
use crate::html::document_write::with_active_streaming_parser;
use crate::html::encoding::decode_html_bytes;
use crate::html::streaming_parser::{StreamingHtmlParser, StreamingParserYield};
use crate::resource::ResourceFetcher;
use crate::js::{
  CurrentScriptHost, CurrentScriptStateHandle, DocumentLifecycle, DocumentLifecycleHost,
  DocumentReadyState, DocumentWriteState, DomHost, EventLoop, JsDomEvents, JsExecutionOptions,
  LoadBlockerKind, LocationNavigationRequest, RunAnimationFrameOutcome, RunLimits, RunUntilIdleOutcome,
  RunUntilIdleStopReason, ScriptBlockExecutor, ScriptBlockingStyleSheetSet, ScriptElementSpec, ScriptId,
  ScriptOrchestrator, ScriptScheduler, ScriptSchedulerAction, ScriptType, TaskSource,
};
use crate::js::runtime::with_event_loop;
use crate::js::script_encoding::decode_classic_script_bytes;
use crate::css::encoding::decode_css_bytes_cow;
use crate::css::parser::parse_stylesheet_with_media;
use crate::css::types::CssImportLoader;
use crate::render_control::{DeadlineGuard, RenderDeadline};
use crate::resource::{origin_from_url, FetchDestination, FetchRequest, ReferrerPolicy};
use crate::style::media::{MediaContext, MediaQuery, MediaQueryCache, MediaType};
use crate::ui::TabHistory;
use crate::web::events::{Event, EventInit, EventTargetId};

use encoding_rs::{Encoding, UTF_8};

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use selectors::context::QuirksMode;
use url::Url;

use super::{BrowserDocumentDom2, Pixmap, RenderOptions, RunUntilStableOutcome, RunUntilStableStopReason};

const MODULE_GRAPH_FETCH_UNSUPPORTED_MESSAGE: &str =
  "module graph fetching is not supported by this BrowserTabJsExecutor";

pub trait BrowserTabJsExecutor {
  fn execute_classic_script(
    &mut self,
    script_text: &str,
    spec: &ScriptElementSpec,
    current_script: Option<NodeId>,
    document: &mut BrowserDocumentDom2,
    event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()>;

  fn execute_module_script(
    &mut self,
    _script_text: &str,
    _spec: &ScriptElementSpec,
    _current_script: Option<NodeId>,
    _document: &mut BrowserDocumentDom2,
    _event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    Err(Error::Other(
      "module script execution is not supported by this BrowserTabJsExecutor".to_string(),
    ))
  }

  /// Returns `true` if this executor supports module graph prefetch via [`Self::fetch_module_graph`].
  ///
  /// Module graph fetching is optional: `BrowserTabHost` uses it to start loading module dependency
  /// graphs early, but will fall back to fetching only the entry module source when this returns
  /// `false`.
  fn supports_module_graph_fetch(&self) -> bool {
    false
  }

  /// Fetch/instantiate the module graph for a module `<script>` element without evaluating it.
  ///
  /// This is used by the HTML-like script scheduler so module scripts can start loading their
  /// dependency graphs as early as possible, while still deferring evaluation based on
  /// `async`/parser-inserted ordering rules.
  ///
  /// Executors that support module graph prefetch should override this and return `true` from
  /// [`Self::supports_module_graph_fetch`].
  fn fetch_module_graph(
    &mut self,
    _spec: &ScriptElementSpec,
    _fetcher: Arc<dyn ResourceFetcher>,
    _document: &mut BrowserDocumentDom2,
    _event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    Err(Error::Other(
      MODULE_GRAPH_FETCH_UNSUPPORTED_MESSAGE.to_string(),
    ))
  }

  /// Process an inline `<script type="importmap">` script.
  ///
  /// HTML import maps are not JavaScript; they register/merge into per-document import map state
  /// used by subsequent module resolution.
  ///
  /// The default implementation is a no-op so custom/test executors do not need to implement import
  /// map support.
  fn execute_import_map_script(
    &mut self,
    _script_text: &str,
    _spec: &ScriptElementSpec,
    _current_script: Option<NodeId>,
    _document: &mut BrowserDocumentDom2,
    _event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    Ok(())
  }

  /// Returns and clears any navigation request emitted by the JS embedding (for example via
  /// `window.location.href = ...`).
  fn take_navigation_request(&mut self) -> Option<LocationNavigationRequest> {
    None
  }

  /// Notify the executor that the document base URL (`Document.baseURI`) has changed.
  ///
  /// Hosts call this during streaming HTML parsing when a `<base href>` element is encountered so
  /// subsequent JS-visible URL resolution (`fetch("rel")`, `location.href="rel"`, etc) uses the
  /// updated base.
  fn on_document_base_url_updated(&mut self, _base_url: Option<&str>) {}

  /// Notifies the executor that a new document has been committed.
  ///
  /// Implementations can use this to reset per-document JS state (e.g. recreate a JS realm with the
  /// updated `document.URL`).
  fn on_navigation_committed(&mut self, _document_url: Option<&str>) {}

  /// Reset the executor's JS state for a new navigation.
  ///
  /// Navigation in browsers creates a fresh global object / realm for each new document. Embeddings
  /// that hold JS runtime state (e.g. `vm-js` realms) should override this to tear down any
  /// per-document state (including rooted callbacks) and reinitialize against the new document.
  ///
  /// The provided [`CurrentScriptStateHandle`] is stable for the lifetime of the tab; it is cleared
  /// before this hook is invoked.
  fn reset_for_navigation(
    &mut self,
    document_url: Option<&str>,
    document: &mut BrowserDocumentDom2,
    current_script: &CurrentScriptStateHandle,
    js_execution_options: JsExecutionOptions,
  ) -> Result<()> {
    let _ = (document_url, document, current_script, js_execution_options);
    Ok(())
  }

  /// Provide the executor with the active WebIDL bindings host (if any).
  ///
  /// Some executors construct `VmJsEventLoopHooks` using only a `&mut dyn VmHost` context and
  /// therefore cannot access `WindowRealmHost::webidl_bindings_host()` directly.
  fn set_webidl_bindings_host(&mut self, _host: &mut dyn webidl_vm_js::WebIdlBindingsHost) {}

  /// Dispatch a document lifecycle event (e.g. `DOMContentLoaded`, `load`) into the JS environment.
  ///
  /// Hosts invoke this hook from their [`DocumentLifecycleHost`] implementation so that JS event
  /// listeners registered via the executor's DOM bindings can observe lifecycle events.
  fn dispatch_lifecycle_event(
    &mut self,
    target: EventTargetId,
    event: &Event,
    _document: &mut BrowserDocumentDom2,
  ) -> Result<()> {
    let _ = (target, event);
    Ok(())
  }

  /// Returns the underlying [`crate::js::WindowRealm`] if this executor is backed by `vm-js`.
  ///
  /// This is used by timer/microtask bindings (`queueMicrotask`, `setTimeout`, etc) to execute
  /// queued JS callbacks on the correct realm.
  fn window_realm_mut(&mut self) -> Option<&mut crate::js::WindowRealm> {
    None
  }

  /// Optional DOM event listener invoker.
  ///
  /// When the executor exposes `EventTarget.addEventListener`/`removeEventListener` shims and stores
  /// listener registrations in the document's [`crate::web::events::EventListenerRegistry`], it
  /// should also provide an [`crate::web::events::EventListenerInvoker`] so Rust-driven event
  /// dispatch (e.g. user interaction) can call the corresponding JS callbacks.
  fn event_listener_invoker(
    &self,
  ) -> Option<Box<dyn crate::web::events::EventListenerInvoker>> {
    None
  }
}

#[derive(Debug, Clone)]
struct ScriptEntry {
  node_id: NodeId,
  spec: ScriptElementSpec,
}

/// RAII guard that increments a host-local "JS execution depth" counter.
///
/// HTML gates certain microtask checkpoints based on whether the **JavaScript execution context
/// stack is empty**. Parsing and navigation can run inside event-loop tasks, so the event loop's
/// "currently running task" state is not equivalent to the JS execution context stack.
struct JsExecutionGuard {
  depth: Rc<Cell<usize>>,
}

impl JsExecutionGuard {
  fn enter(depth: &Rc<Cell<usize>>) -> Self {
    depth.set(depth.get().saturating_add(1));
    Self {
      depth: Rc::clone(depth),
    }
  }
}

impl Drop for JsExecutionGuard {
  fn drop(&mut self) {
    let current = self.depth.get();
    self.depth.set(current.saturating_sub(1));
  }
}

struct NoopEventInvoker;

impl crate::web::events::EventListenerInvoker for NoopEventInvoker {
  fn invoke(
    &mut self,
    _listener_id: crate::web::events::ListenerId,
    _event: &mut crate::web::events::Event,
  ) -> std::result::Result<(), crate::web::events::DomError> {
    Ok(())
  }
}

#[derive(Debug, Clone)]
struct PendingParserBlockingScript {
  script_id: ScriptId,
  source_text: String,
}

#[derive(Clone)]
struct ScriptSourceOverrideFetcher {
  overrides: Arc<Mutex<HashMap<String, String>>>,
  inner: Arc<dyn ResourceFetcher>,
}

impl ScriptSourceOverrideFetcher {
  fn override_bytes(&self, url: &str) -> Option<Vec<u8>> {
    self
      .overrides
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .get(url)
      .map(|s| s.as_bytes().to_vec())
  }

  fn override_resource(url: &str, mut bytes: Vec<u8>) -> crate::resource::FetchedResource {
    let mut res = crate::resource::FetchedResource::new(
      std::mem::take(&mut bytes),
      Some("application/javascript".to_string()),
    );
    // Mirror HTTP fetches so downstream validations (status/CORS) remain deterministic.
    res.status = Some(200);
    res.final_url = Some(url.to_string());
    // Allow CORS-mode scripts/modules to pass enforcement when enabled.
    res.access_control_allow_origin = Some("*".to_string());
    res.access_control_allow_credentials = true;
    res
  }
}

impl ResourceFetcher for ScriptSourceOverrideFetcher {
  fn fetch(&self, url: &str) -> Result<crate::resource::FetchedResource> {
    if let Some(bytes) = self.override_bytes(url) {
      return Ok(Self::override_resource(url, bytes));
    }
    self.inner.fetch(url)
  }

  fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<crate::resource::FetchedResource> {
    if let Some(bytes) = self.override_bytes(req.url) {
      return Ok(Self::override_resource(req.url, bytes));
    }
    self.inner.fetch_with_request(req)
  }

  fn fetch_partial_with_request(
    &self,
    req: FetchRequest<'_>,
    max_bytes: usize,
  ) -> Result<crate::resource::FetchedResource> {
    if let Some(mut bytes) = self.override_bytes(req.url) {
      if bytes.len() > max_bytes {
        bytes.truncate(max_bytes);
      }
      return Ok(Self::override_resource(req.url, bytes));
    }
    self.inner.fetch_partial_with_request(req, max_bytes)
  }

  fn request_header_value(&self, req: FetchRequest<'_>, header_name: &str) -> Option<String> {
    self.inner.request_header_value(req, header_name)
  }

  fn cookie_header_value(&self, url: &str) -> Option<String> {
    self.inner.cookie_header_value(url)
  }

  fn store_cookie_from_document(&self, url: &str, cookie_string: &str) {
    self.inner.store_cookie_from_document(url, cookie_string)
  }
}

struct StreamingParseState {
  parser: StreamingHtmlParser,
  input: String,
  input_offset: usize,
  eof_set: bool,
  deadline: Option<RenderDeadline>,
  parse_task_scheduled: bool,
  resume_task_scheduled: bool,
  /// Whether the streaming parser's DOM has been snapshotted into the host DOM at least once.
  ///
  /// Until we commit the streaming DOM, the host document may contain an unrelated initial DOM
  /// (e.g. the renderer's empty-document scaffold). Syncing that DOM *into* the streaming parser's
  /// live sink would corrupt html5ever's internal handle state (and can introduce multiple `<html>`
  /// roots).
  host_snapshot_committed: bool,
  last_synced_host_dom_generation: u64,
}

pub struct BrowserTabHost {
  trace: TraceHandle,
  document: Box<BrowserDocumentDom2>,
  executor: Box<dyn BrowserTabJsExecutor>,
  event_invoker: Box<dyn crate::web::events::EventListenerInvoker>,
  js_events: JsDomEvents,
  current_script: CurrentScriptStateHandle,
  orchestrator: ScriptOrchestrator,
  scheduler: ScriptScheduler<NodeId>,
  scripts: HashMap<ScriptId, ScriptEntry>,
  scheduled_script_nodes: HashSet<NodeId>,
  deferred_scripts: HashSet<ScriptId>,
  executed: HashSet<ScriptId>,
  pending_script_load_blockers: HashSet<ScriptId>,
  parser_blocked_on: Option<ScriptId>,
  document_url: Option<String>,
  /// Current document base URL used for resolving *JS-visible* relative URLs.
  ///
  /// This reflects HTML's "document base URL" concept and updates when `<base href>` elements are
  /// encountered during streaming parsing.
  ///
  /// Note: this is intentionally distinct from `document_url`:
  /// - `document_url` is the stable URL used for referrer/origin semantics.
  /// - `base_url` is the mutable base used for resolving relative URLs in scripts.
  base_url: Option<String>,
  document_origin: Option<crate::resource::DocumentOrigin>,
  document_referrer_policy: ReferrerPolicy,
  csp: Option<CspPolicy>,
  pending_navigation: Option<LocationNavigationRequest>,
  /// Root render deadline captured when `pending_navigation` is set.
  ///
  /// Navigation requests are often produced while a render deadline is active (during streaming
  /// parsing or event-loop driven script execution). When the host later commits the navigation
  /// outside that immediate deadline scope, we must preserve the original deadline start instant so
  /// `RenderOptions::{timeout,cancel_callback}` cannot be bypassed by resetting the clock at commit
  /// time.
  pending_navigation_deadline: Option<RenderDeadline>,
  html_sources: HashMap<String, String>,
  external_script_sources: Arc<Mutex<HashMap<String, String>>>,
  script_blocking_stylesheets: ScriptBlockingStyleSheetSet,
  stylesheet_keys_by_node: HashMap<NodeId, usize>,
  next_stylesheet_key: usize,
  stylesheet_media_context: MediaContext,
  stylesheet_media_query_cache: MediaQueryCache,
  js_execution_options: JsExecutionOptions,
  document_write_state: DocumentWriteState,
  js_execution_depth: Rc<Cell<usize>>,
  lifecycle: DocumentLifecycle,
  webidl_bindings_host: Box<BrowserTabWebIdlBindingsHost>,
  last_dynamic_script_discovery_generation: u64,
  /// Whether we are currently running a streaming HTML parse (even if the parser state is
  /// temporarily moved out of `streaming_parse` by `parse_until_blocked`).
  streaming_parse_active: bool,
  /// Re-entrancy guard for `parse_until_blocked`.
  ///
  /// Some networking tasks (e.g. script-blocking stylesheet loads) attempt to resume parsing by
  /// queueing another parse task. When we are already actively parsing on the stack, re-queueing
  /// parsing would lead to nested parses of the same `StreamingHtmlParser` state. Guard against
  /// that by treating such resume requests as a no-op: the active parse will continue naturally.
  streaming_parse_in_progress: bool,
  streaming_parse: Option<StreamingParseState>,
  pending_parser_blocking_script: Option<PendingParserBlockingScript>,
}

#[derive(Debug, Default)]
struct BrowserTabWebIdlBindingsHost;

impl webidl_vm_js::WebIdlBindingsHost for BrowserTabWebIdlBindingsHost {
  fn call_operation(
    &mut self,
    _vm: &mut vm_js::Vm,
    _scope: &mut vm_js::Scope<'_>,
    _receiver: Option<vm_js::Value>,
    _interface: &'static str,
    _operation: &'static str,
    _overload: usize,
    _args: &[vm_js::Value],
  ) -> std::result::Result<vm_js::Value, vm_js::VmError> {
    Err(vm_js::VmError::Unimplemented(
      "BrowserTab does not implement WebIDL binding dispatch",
    ))
  }

  fn call_constructor(
    &mut self,
    _vm: &mut vm_js::Vm,
    _scope: &mut vm_js::Scope<'_>,
    _interface: &'static str,
    _overload: usize,
    _args: &[vm_js::Value],
    _new_target: vm_js::Value,
  ) -> std::result::Result<vm_js::Value, vm_js::VmError> {
    Err(vm_js::VmError::Unimplemented(
      "BrowserTab does not implement WebIDL binding dispatch",
    ))
  }
}

impl BrowserTabHost {
  fn new(
    document: BrowserDocumentDom2,
    mut executor: Box<dyn BrowserTabJsExecutor>,
    trace: TraceHandle,
    js_execution_options: JsExecutionOptions,
  ) -> Result<Self> {
    let mut webidl_bindings_host = Box::new(BrowserTabWebIdlBindingsHost::default());
    executor.set_webidl_bindings_host(webidl_bindings_host.as_mut());
    let event_invoker = executor
      .event_listener_invoker()
      .unwrap_or_else(|| Box::new(NoopEventInvoker));
    let current_script = CurrentScriptStateHandle::default();
    let mut document_write_state = DocumentWriteState::default();
    document_write_state.update_limits(js_execution_options);
    Ok(Self {
      trace,
      document: Box::new(document),
      executor,
      event_invoker,
      js_events: JsDomEvents::new()?,
      current_script,
      orchestrator: ScriptOrchestrator::new(),
      scheduler: ScriptScheduler::with_options(js_execution_options),
      scripts: HashMap::new(),
      scheduled_script_nodes: HashSet::new(),
      deferred_scripts: HashSet::new(),
      executed: HashSet::new(),
      pending_script_load_blockers: HashSet::new(),
      parser_blocked_on: None,
      document_url: None,
      base_url: None,
      document_origin: None,
      document_referrer_policy: ReferrerPolicy::default(),
      csp: None,
      pending_navigation: None,
      pending_navigation_deadline: None,
      html_sources: HashMap::new(),
      external_script_sources: Arc::new(Mutex::new(HashMap::new())),
      script_blocking_stylesheets: ScriptBlockingStyleSheetSet::new(),
      stylesheet_keys_by_node: HashMap::new(),
      next_stylesheet_key: 0,
      stylesheet_media_context: MediaContext::default(),
      stylesheet_media_query_cache: MediaQueryCache::default(),
      js_execution_options,
      document_write_state,
      js_execution_depth: Rc::new(Cell::new(0)),
      lifecycle: DocumentLifecycle::new(),
      webidl_bindings_host,
      last_dynamic_script_discovery_generation: 0,
      streaming_parse_active: false,
      streaming_parse_in_progress: false,
      streaming_parse: None,
      pending_parser_blocking_script: None,
    })
  }

  fn with_installed_document_write_state<R>(
    &mut self,
    f: impl FnOnce(&mut Self) -> Result<R>,
  ) -> Result<R> {
    // Avoid double-borrowing `self` by temporarily moving the state out of the host, then installing
    // it in TLS for the duration of the call.
    let mut state = std::mem::take(&mut self.document_write_state);
    let result = crate::js::with_document_write_state(&mut state, || f(self));
    self.document_write_state = state;
    result
  }

  fn register_html_source(&mut self, url: String, html: String) {
    self.html_sources.insert(url, html);
  }

  fn register_external_script_source(&mut self, url: String, source: String) {
    self
      .external_script_sources
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .insert(url, source);
  }

  fn set_event_invoker(&mut self, invoker: Box<dyn crate::web::events::EventListenerInvoker>) {
    self.event_invoker = invoker;
  }

  fn dispatch_dom_event(&mut self, target: EventTargetId, mut event: Event) -> Result<bool> {
    let dom: &crate::dom2::Document = self.document.dom();
    crate::web::events::dispatch_event(
      target,
      &mut event,
      dom,
      dom.events(),
      self.event_invoker.as_mut(),
    )
    .map_err(|err| Error::Other(err.to_string()))
  }

  fn dispatch_script_event(&mut self, script_node_id: NodeId, type_: &str) -> Result<()> {
    let mut event = Event::new(
      type_,
      EventInit {
        bubbles: false,
        cancelable: false,
        composed: false,
      },
    );
    event.is_trusted = true;
    let _default_not_prevented = self.dispatch_dom_event(EventTargetId::Node(script_node_id), event)?;
    Ok(())
  }

  fn dispatch_script_event_in_event_loop(
    &mut self,
    script_node_id: NodeId,
    type_: &str,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<()> {
    // Install the active event loop in TLS so `vm-js` Web APIs like `queueMicrotask`/`setTimeout`
    // called from `<script>` load/error event listeners can schedule work.
    with_event_loop(event_loop, || self.dispatch_script_event(script_node_id, type_))
  }

  fn dispatch_script_error_event_in_event_loop(
    &mut self,
    script_node_id: NodeId,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<()> {
    self.dispatch_script_event_in_event_loop(script_node_id, "error", event_loop)
  }

  pub fn dom(&self) -> &Document {
    self.document.dom()
  }

  pub fn document_is_dirty(&self) -> bool {
    self.document.is_dirty()
  }

  pub fn dom_mut(&mut self) -> &mut Document {
    self.document.dom_mut()
  }

  pub fn current_script_node(&self) -> Option<NodeId> {
    self.current_script.borrow().current_script
  }

  fn reset_scripting_state(
    &mut self,
    document_url: Option<String>,
    document_referrer_policy: ReferrerPolicy,
  ) -> Result<()> {
    self.current_script.reset();
    self.orchestrator = ScriptOrchestrator::new();
    self.scheduler = ScriptScheduler::with_options(self.js_execution_options);
    self.scripts.clear();
    self.scheduled_script_nodes.clear();
    self.deferred_scripts.clear();
    self.executed.clear();
    self.pending_script_load_blockers.clear();
    self.parser_blocked_on = None;
    self.document_url = document_url.clone();
    self.base_url = document_url;
    self.document_origin = self
      .document_url
      .as_deref()
      .and_then(|url| origin_from_url(url));
    self.document_referrer_policy = document_referrer_policy;
    self.pending_navigation = None;
    self.pending_navigation_deadline = None;
    self.executor.on_navigation_committed(self.document_url.as_deref());
    // Mirror the renderer's view of the active CSP policy (populated when navigating to a URL).
    // HTML-string entry points may overwrite this with `<meta http-equiv="Content-Security-Policy">`
    // extraction before scripts run.
    self.csp = self
      .document
      .renderer_mut()
      .resource_context
      .as_ref()
      .and_then(|ctx| ctx.csp.clone());
    self.js_events = JsDomEvents::new()?;
    self.js_execution_depth.set(0);
    self.lifecycle = DocumentLifecycle::new();
    self.last_dynamic_script_discovery_generation = 0;
    self.document_write_state.reset_for_navigation();
    self.document_write_state.update_limits(self.js_execution_options);
    self.script_blocking_stylesheets = ScriptBlockingStyleSheetSet::new();
    self.stylesheet_keys_by_node.clear();
    self.next_stylesheet_key = 0;
    self.stylesheet_media_query_cache = MediaQueryCache::default();
    self.streaming_parse_active = false;
    self.streaming_parse = None;
    self.pending_parser_blocking_script = None;
    self.executor.reset_for_navigation(
      self.document_url.as_deref(),
      &mut self.document,
      &self.current_script,
      self.js_execution_options,
    )?;
    // Ensure the executor's JS realm starts with a base URL consistent with the new navigation
    // (document URL by default, unless later updated by `<base href>`).
    self
      .executor
      .on_document_base_url_updated(self.base_url.as_deref());
    Ok(())
  }

  fn update_stylesheet_media_context(&mut self, options: &RenderOptions) {
    // Preserve the renderer's defaults for most fields; script-blocking stylesheet semantics only
    // require correct media type + media query evaluation.
    let (viewport_w, viewport_h) = options.viewport.unwrap_or((1024, 768));
    let width = viewport_w as f32;
    let height = viewport_h as f32;
    let mut ctx = match options.media_type {
      MediaType::Print => MediaContext::print(width, height),
      _ => MediaContext::screen(width, height),
    };
    ctx.media_type = options.media_type;
    if let Some(dpr) = options.device_pixel_ratio {
      ctx.device_pixel_ratio = dpr;
    }
    self.stylesheet_media_context = ctx;
    // Cached query results depend on the context fingerprint, so clear on updates.
    self.stylesheet_media_query_cache = MediaQueryCache::default();
  }

  fn stylesheet_media_matches_link(&mut self, dom: &Document, link: NodeId) -> bool {
    let NodeKind::Element { attributes, .. } = &dom.node(link).kind else {
      return true;
    };
    let Some(media_attr) = attributes
      .iter()
      .find(|(name, _)| name.eq_ignore_ascii_case("media"))
      .map(|(_, value)| value.as_str())
    else {
      return true;
    };
    let trimmed = super::trim_ascii_whitespace(media_attr);
    if trimmed.is_empty() {
      return true;
    }
    let Ok(queries) = MediaQuery::parse_list(trimmed) else {
      return false;
    };
    self
      .stylesheet_media_context
      .evaluate_list_with_cache(&queries, Some(&mut self.stylesheet_media_query_cache))
  }
  fn load_stylesheet_and_imports(&mut self, url: &str) -> Result<()> {
    let fetcher = self.document.fetcher();
    let mut req = FetchRequest::new(url, FetchDestination::Style);
    if let Some(referrer) = self.document_url.as_deref() {
      req = req.with_referrer_url(referrer);
    }
    if let Some(origin) = self.document_origin.as_ref() {
      req = req.with_client_origin(origin);
    }
    req = req.with_referrer_policy(self.document_referrer_policy);
    let resource = fetcher.fetch_with_request(req)?;
    let final_url = resource.final_url.clone().unwrap_or_else(|| url.to_string());

    let css = decode_css_bytes_cow(&resource.bytes, resource.content_type.as_deref());
    let ctx = &self.stylesheet_media_context;
    let cache = &mut self.stylesheet_media_query_cache;
    let mut sheet = parse_stylesheet_with_media(&css, ctx, Some(cache))?;

    if sheet.contains_imports() {
      struct ImportLoader {
        fetcher: Arc<dyn ResourceFetcher>,
        document_url: Option<String>,
        document_origin: Option<crate::resource::DocumentOrigin>,
        document_referrer_policy: ReferrerPolicy,
      }

      impl CssImportLoader for ImportLoader {
        fn load(&self, url: &str) -> Result<String> {
          let mut req = FetchRequest::new(url, FetchDestination::Style);
          if let Some(referrer) = self.document_url.as_deref() {
            req = req.with_referrer_url(referrer);
          }
          if let Some(origin) = self.document_origin.as_ref() {
            req = req.with_client_origin(origin);
          }
          req = req.with_referrer_policy(self.document_referrer_policy);
          let resource = self.fetcher.fetch_with_request(req)?;
          Ok(decode_css_bytes_cow(&resource.bytes, resource.content_type.as_deref()).into_owned())
        }

        fn load_with_importer(
          &self,
          url: &str,
          importer_url: Option<&str>,
        ) -> Result<crate::css::loader::FetchedStylesheet> {
          let mut req = FetchRequest::new(url, FetchDestination::Style);
          if let Some(referrer) = importer_url.or(self.document_url.as_deref()) {
            req = req.with_referrer_url(referrer);
          }
          if let Some(origin) = self.document_origin.as_ref() {
            req = req.with_client_origin(origin);
          }
          req = req.with_referrer_policy(self.document_referrer_policy);
          let resource = self.fetcher.fetch_with_request(req)?;
          let css =
            decode_css_bytes_cow(&resource.bytes, resource.content_type.as_deref()).into_owned();
          Ok(crate::css::loader::FetchedStylesheet::new(
            css,
            resource.final_url.clone(),
          ))
        }
      }

      let loader = ImportLoader {
        fetcher: Arc::clone(&fetcher),
        document_url: self.document_url.clone(),
        document_origin: self.document_origin.clone(),
        document_referrer_policy: self.document_referrer_policy,
      };
      let importer_url = self.document_url.as_deref();
      sheet = sheet
        .resolve_imports_owned_with_cache_with_importer_url(
          &loader,
          Some(final_url.as_str()),
          importer_url,
          ctx,
          Some(cache),
        )
        .map_err(Error::Render)?;
    }

    let _ = sheet;
    Ok(())
  }

  fn should_delay_parser_blocking_script(&self, script_id: ScriptId) -> bool {
    let Some(entry) = self.scripts.get(&script_id) else {
      return false;
    };
    let spec = &entry.spec;
    if spec.script_type != ScriptType::Classic {
      return false;
    }
    if !spec.parser_inserted {
      return false;
    }
    if !spec.src_attr_present {
      // Inline scripts are always parser-blocking; `async`/`defer` do not apply.
      return true;
    }
    // External scripts block the parser only when neither async nor defer are set.
    !spec.async_attr && !spec.defer_attr
  }

  fn queue_parse_task(&mut self, event_loop: &mut EventLoop<Self>) -> Result<()> {
    if self.streaming_parse_in_progress {
      // Avoid scheduling nested parse tasks when parsing is already on the stack (for example when a
      // networking task completes while `parse_until_blocked` is interleaving event-loop turns).
      return Ok(());
    }
    let Some(state) = self.streaming_parse.as_mut() else {
      return Ok(());
    };
    if state.parse_task_scheduled || state.resume_task_scheduled {
      return Ok(());
    }
    state.parse_task_scheduled = true;

    let queued = event_loop.queue_task(TaskSource::DOMManipulation, |host, event_loop| {
      let task_result = host.parse_until_blocked(event_loop);
      if let Some(state) = host.streaming_parse.as_mut() {
        state.parse_task_scheduled = false;
      }
      let should_continue = task_result?;
      if should_continue {
        host.queue_parse_resume_task(event_loop)?;
      }
      Ok(())
    });
    if let Err(err) = queued {
      if let Some(state) = self.streaming_parse.as_mut() {
        state.parse_task_scheduled = false;
      }
      return Err(err);
    }
    Ok(())
  }

  fn queue_parse_resume_task(&mut self, event_loop: &mut EventLoop<Self>) -> Result<()> {
    let Some(state) = self.streaming_parse.as_mut() else {
      return Ok(());
    };
    if state.resume_task_scheduled {
      return Ok(());
    }
    state.resume_task_scheduled = true;

    let queued = event_loop.queue_task(TaskSource::DOMManipulation, |host, event_loop| {
      if let Some(state) = host.streaming_parse.as_mut() {
        state.resume_task_scheduled = false;
      }
      host.queue_parse_task(event_loop)
    });
    if let Err(err) = queued {
      if let Some(state) = self.streaming_parse.as_mut() {
        state.resume_task_scheduled = false;
      }
      return Err(err);
    }
    Ok(())
  }

  fn start_script_blocking_stylesheet_load(
    &mut self,
    link_node_id: NodeId,
    url: String,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<()> {
    // Skip if we already started (or completed) a load for this node.
    if self.stylesheet_keys_by_node.contains_key(&link_node_id) {
      return Ok(());
    }

    // HTML: stylesheets with non-matching `media` attributes are not "script-blocking".
    //
    // Avoid wedging parser-blocking scripts behind `<link rel=stylesheet media="print">` when
    // rendering for screen media. These stylesheets also do not affect our current rendering and
    // stylesheet import parsing logic, so skip fetching entirely.
    if let Ok(Some(media_attr)) = self.document.dom().get_attribute(link_node_id, "media") {
      let trimmed = super::trim_ascii_whitespace(media_attr);
      if !trimmed.is_empty() {
        let matches = match MediaQuery::parse_list(trimmed) {
          Ok(list) => self
            .stylesheet_media_context
            .evaluate_list_with_cache(&list, Some(&mut self.stylesheet_media_query_cache)),
          Err(_) => false,
        };
        if !matches {
          return Ok(());
        }
      }
    }

    if self.script_blocking_stylesheets.len() >= self.js_execution_options.max_pending_blocking_stylesheets {
      return Err(Error::Other(format!(
        "Exceeded max_pending_blocking_stylesheets (len={}, limit={})",
        self.script_blocking_stylesheets.len(),
        self.js_execution_options.max_pending_blocking_stylesheets
      )));
    }

    let key = self.next_stylesheet_key;
    self.next_stylesheet_key = self.next_stylesheet_key.wrapping_add(1);
    self.stylesheet_keys_by_node.insert(link_node_id, key);
    let inserted = self
      .script_blocking_stylesheets
      .register_blocking_stylesheet(key);

    let queued = event_loop.queue_task(TaskSource::Networking, move |host, event_loop| {
      let load_result = host.load_stylesheet_and_imports(&url);
      let removed = host.script_blocking_stylesheets.unregister_blocking_stylesheet(key);
      if inserted && removed {
        host
          .lifecycle
          .load_blocker_completed(LoadBlockerKind::StyleSheet, event_loop)?;
      }
      if !host.script_blocking_stylesheets.has_blocking_stylesheet() {
        // Wake parser-blocking scripts/parsing if this was the last blocking stylesheet.
        if let Err(err) = host.queue_parse_task(event_loop) {
          // Fallback: if we cannot queue a parse task (queue limits), resume immediately to avoid
          // deadlocking parser-blocking scripts.
          let _ = err;
          while host.parse_until_blocked(event_loop)? {}
        }
      }
      match load_result {
        Ok(()) => Ok(()),
        Err(err @ Error::Render(_)) => Err(err),
        Err(_) => Ok(()),
      }
    });
    if let Err(err) = queued {
      // If we cannot queue the networking task, do not leave script/blocking + load/lifecycle
      // tracking state wedged.
      self.script_blocking_stylesheets.unregister_blocking_stylesheet(key);
      self.stylesheet_keys_by_node.remove(&link_node_id);
      return Err(err);
    }
    if inserted {
      self
        .lifecycle
        .register_pending_load_blocker(LoadBlockerKind::StyleSheet);
    }

    Ok(())
  }

  fn commit_streaming_parser_dom_snapshot_to_host(
    &mut self,
    state: &mut StreamingParseState,
  ) -> Result<()> {
    let (snapshot, base_url) = {
      let Some(doc) = state.parser.document() else {
        return Err(Error::Other(
          "StreamingHtmlParser document unavailable while parsing is in progress".to_string(),
        ));
      };
      (doc.clone_with_events(), state.parser.current_base_url())
    };
 
    self.mutate_dom(|dom| {
      *dom = snapshot;
      ((), true)
    });
 
    self.base_url = base_url;
    self
      .executor
      .on_document_base_url_updated(self.base_url.as_deref());
    {
      let renderer = self.document.renderer_mut();
      match self.base_url.clone() {
        Some(url) => renderer.set_base_url(url),
        None => renderer.clear_base_url(),
      }
    }
 
    state.host_snapshot_committed = true;
    state.last_synced_host_dom_generation = self.document.dom_mutation_generation();
    Ok(())
  }
 
  fn sync_host_dom_to_streaming_parser(&mut self, state: &mut StreamingParseState) -> Result<()> {
    if !state.host_snapshot_committed {
      return Ok(());
    }
 
    let updated = self.dom().clone_with_events();
    let Some(mut doc) = state.parser.document_mut() else {
      return Err(Error::Other(
        "StreamingHtmlParser document unavailable while parsing is in progress".to_string(),
      ));
    };
    *doc = updated;
    state.last_synced_host_dom_generation = self.document.dom_mutation_generation();
    Ok(())
  }
 
  fn parse_until_blocked(&mut self, event_loop: &mut EventLoop<Self>) -> Result<bool> {
    const INPUT_CHUNK_BYTES: usize = 8 * 1024;

    if self.streaming_parse_in_progress {
      // Re-entrancy guard: when parsing is already active, callers should rely on the outer parse
      // loop to continue. This can happen if a parse-resume hook tries to parse synchronously while
      // we are already parsing on the stack.
      return Ok(false);
    }

    let Some(mut state) = self.streaming_parse.take() else {
      return Ok(false);
    };

    // Mark parsing as in-progress for the duration of this call so tasks triggered while parsing
    // (e.g. stylesheet fetch completion) do not attempt to schedule nested parse tasks.
    struct StreamingParseInProgressGuard(*mut bool);
    impl Drop for StreamingParseInProgressGuard {
      fn drop(&mut self) {
        // SAFETY: The guard is created from a stable pointer to `BrowserTabHost`'s field and dropped
        // before `self` can be moved.
        unsafe {
          *self.0 = false;
        }
      }
    }
    self.streaming_parse_in_progress = true;
    let _parse_guard = StreamingParseInProgressGuard(&mut self.streaming_parse_in_progress as *mut bool);

    // Ensure any render deadline configured for streaming parsing remains active even when parsing
    // is resumed via event-loop tasks (e.g. after script-blocking stylesheets load).
    let _deadline_guard = state
      .deadline
      .as_ref()
      .map(|deadline| DeadlineGuard::install(Some(deadline)));

    // Sync any DOM mutations from tasks that executed since the last parse slice back into the
    // streaming parser's live DOM before resuming parsing.
    if state.host_snapshot_committed
      && state.last_synced_host_dom_generation != self.document.dom_mutation_generation()
    {
      self.sync_host_dom_to_streaming_parser(&mut state)?;
    }

    enum Outcome {
      Blocked,
      Finished,
      AbortedForNavigation,
      BudgetExhausted,
    }
 
    let outcome = (|| -> Result<Outcome> {
      let mut remaining = self.js_execution_options.dom_parse_budget.max_pump_iterations;
      while remaining > 0 {
        // Flush any `document.write` / `document.writeln` data that was buffered during script
        // execution (or tasks) into the parser input stream before continuing.
        //
        // Note: `StreamingHtmlParser::push_front_str` injects text before any buffered "remaining
        // input", matching HTML's `document.write` insertion semantics.
        let pending = self.document_write_state.take_pending_html();
        if !pending.is_empty() {
          state.parser.push_front_str(&pending);
        }
 
        // If a parser-blocking script was delayed because there were pending script-blocking
        // stylesheets, retry execution now that we are resuming parsing.
        if let Some(pending) = self.pending_parser_blocking_script.take() {
          if self.script_blocking_stylesheets.has_blocking_stylesheet() {
            self.pending_parser_blocking_script = Some(pending);
            return Ok(Outcome::Blocked);
          }

          with_active_streaming_parser(&state.parser, || -> Result<()> {
            let PendingParserBlockingScript {
              script_id,
              source_text,
            } = pending;

            let entry = self.scripts.get(&script_id).cloned();
            let generation_before = self.document.dom_mutation_generation();

            let exec_result = {
              let _guard = JsExecutionGuard::enter(&self.js_execution_depth);
              self.execute_script(script_id, &source_text, event_loop)
            };
            // Ensure parser blocking is cleared even if script execution fails.
            self.finish_script_execution(script_id, event_loop)?;

            // HTML: "clean up after running script" performs a microtask checkpoint only when the JS
            // execution context stack is empty. Nested (re-entrant) script execution must not drain
            // microtasks until the outermost script returns.
            //
            // Additionally, `<script>` load/error events are fired after the script (and its
            // microtasks) complete, so ensure the checkpoint runs before event dispatch.
            if matches!(&exec_result, Err(Error::Render(_))) {
              if let Some(entry) = entry {
                self.dispatch_script_event_in_event_loop(entry.node_id, "error", event_loop)?;
              }
              return exec_result;
            }

            let microtask_err = if self.js_execution_depth.get() == 0 {
              self
                .with_installed_document_write_state(|host| event_loop.perform_microtask_checkpoint(host))
                .err()
            } else {
              None
            };

            match exec_result {
              Ok(()) => {
                if let Some(entry) = entry {
                  if entry.spec.src_attr_present
                    && entry.spec.src.as_deref().is_some_and(|s| !s.is_empty())
                  {
                    self.dispatch_script_event_in_event_loop(entry.node_id, "load", event_loop)?;
                  }
                }
              }
              Err(err) => {
                let Some(entry) = entry else {
                  return Err(err);
                };
                self.dispatch_script_event_in_event_loop(entry.node_id, "error", event_loop)?;
                // Uncaught script exceptions should not abort parsing/task scheduling (browser
                // behavior). Still propagate host-level render timeouts/cancellation.
                if matches!(err, Error::Render(_)) {
                  return Err(err);
                }
              }
            }

            if let Some(err) = microtask_err {
              return Err(err);
            }

            if generation_before != self.document.dom_mutation_generation() {
              self.discover_dynamic_scripts(event_loop)?;
            }
            Ok(())
          })?;

          if self.pending_navigation.is_some() {
            // Abort the current parse/execution; the caller will commit the navigation.
            return Ok(Outcome::AbortedForNavigation);
          }

          // Sync any DOM mutations from the executed script back into the streaming parser's live
          // DOM before resuming parsing.
          self.sync_host_dom_to_streaming_parser(&mut state)?;
          continue;
        }

        let yield_result = state.parser.pump()?;
        remaining = remaining.saturating_sub(1);

        // Start fetching any script-blocking stylesheet links discovered during this parse step.
        //
        // Per HTML, a stylesheet only blocks parser-inserted scripts when it applies to the
        // document. In particular, non-matching `media=` stylesheets must not block scripts and can
        // be skipped.
        //
        // Note: link node ids are produced by the streaming parser's live DOM. The host `dom2`
        // document is only synchronized at script-yield points, so consult the parser document for
        // `media=` matching before deciding whether to load/block.
        let pending_stylesheet_links = state.parser.take_pending_stylesheet_links();
        if !pending_stylesheet_links.is_empty() {
          if let Some(doc) = state.parser.document() {
            for (node_id, url) in pending_stylesheet_links {
              if self.stylesheet_media_matches_link(&doc, node_id) {
                self.start_script_blocking_stylesheet_load(node_id, url, event_loop)?;
              }
            }
          } else {
            // Streaming parser should always have an active document sink, but be defensive and
            // avoid deadlocking parser-blocking scripts by treating stylesheets as non-blocking.
          }
        }

        match yield_result {
          StreamingParserYield::NeedMoreInput => {
            if state.input_offset < state.input.len() {
              let mut end = (state.input_offset + INPUT_CHUNK_BYTES).min(state.input.len());
              while end < state.input.len() && !state.input.is_char_boundary(end) {
                end += 1;
              }
              debug_assert!(state.input.is_char_boundary(state.input_offset));
              debug_assert!(state.input.is_char_boundary(end));
              state.parser.push_str(&state.input[state.input_offset..end]);
              state.input_offset = end;
              continue;
            }

            if !state.eof_set {
              state.parser.set_eof();
              state.eof_set = true;
              continue;
            }

            return Err(Error::Other(
              "StreamingHtmlParser unexpectedly requested more input after EOF".to_string(),
            ));
          }
          StreamingParserYield::Script {
            script,
            base_url_at_this_point,
          } => {
            self.commit_streaming_parser_dom_snapshot_to_host(&mut state)?;

            // HTML: before preparing a parser-inserted script at a script end-tag boundary,
            // perform a microtask checkpoint when the JS execution context stack is empty.
            //
            // Microtasks may mutate the document (including removing/detaching this `<script>`
            // element), so this must occur before we check `is_connected_for_scripting` and build
            // the final `ScriptElementSpec`.
            if self.js_execution_depth.get() == 0 {
              with_active_streaming_parser(&state.parser, || {
                self.with_installed_document_write_state(|host| event_loop.perform_microtask_checkpoint(host))
              })?;
            }
            if self.pending_navigation.is_some() {
              return Ok(Outcome::AbortedForNavigation);
            }

            if !self.dom().is_connected_for_scripting(script) {
              self.mutate_dom(|dom| {
                if let NodeKind::Element {
                  tag_name,
                  namespace,
                  ..
                } = &dom.node(script).kind
                {
                  if tag_name.eq_ignore_ascii_case("script")
                    && (namespace.is_empty() || namespace == HTML_NAMESPACE)
                  {
                    let parser_document = dom.node(script).script_parser_document;
                    dom.node_mut(script).script_parser_document = false;
                    if parser_document && !dom.has_attribute(script, "async").unwrap_or(false) {
                      dom.node_mut(script).script_force_async = true;
                    }
                  }
                }
                ((), false)
              });

              self.sync_host_dom_to_streaming_parser(&mut state)?;
              continue;
            }

            let base = BaseUrlTracker::new(base_url_at_this_point.as_deref());
            let spec = crate::js::streaming_dom2::build_parser_inserted_script_element_spec_dom2(
              self.dom(),
              script,
              &base,
            );
 
            // HTML: "prepare the script element" can return early without executing the script
            // (e.g. unsupported `type`, empty inline script). In that case, the spec clears the
            // "parser document" internal slot (and may set force-async) so future mutations/insertion
            // treat the element like a dynamic script.
            let should_run = self.mutate_dom(|dom| {
              (
                crate::js::prepare_script_element_dom2(dom, script, &spec),
                /* changed */ false,
              )
            });
            if !should_run {
              // Sync any DOM mutations (including internal-slot updates above) back into the
              // streaming parser's live DOM before resuming parsing.
              self.sync_host_dom_to_streaming_parser(&mut state)?;
              continue;
            }

            // In real browsers, async external scripts can execute before later parser-inserted
            // scripts when they load quickly (e.g. from cache). FastRender does not have true
            // background network concurrency, so we give the event loop a chance to service the
            // async fetch + execution task before resuming parsing. This keeps deterministic
            // fixtures (especially `file://` tests) aligned with web semantics.
            let should_spin_for_async = spec.script_type == ScriptType::Classic
              && spec.src_attr_present
              && spec.async_attr
              && spec
                .src
                .as_deref()
                .filter(|src| !src.is_empty())
                .is_some_and(|src| {
                  // Avoid eager network fetches during parsing when the script source is not
                  // immediately available. Many tests register in-memory script sources *after*
                  // construction (before running the event loop), so only spin for "fast" sources.
                  self
                    .external_script_sources
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .contains_key(src)
                    || Url::parse(src)
                      .ok()
                      .is_some_and(|parsed| parsed.scheme() == "file")
                });
            let base_url_at_discovery = spec.base_url.clone();

            let script_id = with_active_streaming_parser(&state.parser, || {
              self.register_and_schedule_script(script, spec, base_url_at_discovery, event_loop)
            })?;

            if should_spin_for_async {
              // Stop spinning if the script has executed or a navigation request was issued.
              let _ = with_active_streaming_parser(&state.parser, || {
                event_loop.spin_until(self, self.js_execution_options.event_loop_run_limits, |host| {
                  host.pending_navigation.is_none() && !host.executed.contains(&script_id)
                })
              })?;
            }

            if self.pending_navigation.is_some() {
              // Abort the current parse/execution; the caller will commit the navigation.
              return Ok(Outcome::AbortedForNavigation);
            }

            if self.pending_parser_blocking_script.is_some() || self.parser_blocked_on.is_some() {
              // Parsing is blocked (either on a stylesheet-blocking script, or another parser
              // block). Sync any microtask mutations back into the streaming parser's live DOM so
              // parsing resumes with an up-to-date tree once unblocked.
              let pending = self.document_write_state.take_pending_html();
              if !pending.is_empty() {
                state.parser.push_front_str(&pending);
              }
              self.sync_host_dom_to_streaming_parser(&mut state)?;
              return Ok(Outcome::Blocked);
            }
 
            // Sync any DOM mutations from the executed script back into the streaming parser's live
            // DOM before resuming parsing.
            self.sync_host_dom_to_streaming_parser(&mut state)?;

            // Note: when parsing is initiated outside the event loop (e.g. HTML-string entry points),
            // we intentionally avoid running pending networking tasks here. This keeps construction
            // deterministic and ensures embeddings can attach listeners (e.g. `<script>` error/load)
            // before async fetch tasks are serviced.
          }
          StreamingParserYield::Finished { document } => {
            // Parsing has completed; any subsequent scripts (deferred/async) should treat
            // `document.write` as a deterministic no-op instead of implicitly rewriting the
            // document.
            self.document_write_state.set_parsing_active(false);
            let final_base_url = state.parser.current_base_url();
            // Persist the final base URL after parsing completes so any later JS-visible URL
            // resolution uses the post-parse `<base href>` result.
            self.base_url = final_base_url.clone();
            self
              .executor
              .on_document_base_url_updated(self.base_url.as_deref());

            self.mutate_dom(|dom| {
              *dom = document;
              ((), true)
            });

            let actions = self.scheduler.parsing_completed()?;
            self.apply_scheduler_actions(actions, event_loop)?;
            self.notify_parsing_completed(event_loop)?;

            // Update the renderer's base URL hint to match the parse-time base URL after processing
            // the full document.
            let renderer = self.document.renderer_mut();
            match final_base_url {
              Some(url) => renderer.set_base_url(url),
              None => renderer.clear_base_url(),
            }

            return Ok(Outcome::Finished);
          }
        }
      }
      Ok(Outcome::BudgetExhausted)
    })();

    match outcome {
      Ok(Outcome::Blocked) => {
        self.streaming_parse = Some(state);
        Ok(false)
      }
      Ok(Outcome::BudgetExhausted) => {
        // Budget exhausted: snapshot parser DOM into the host so other tasks observe the most recent
        // parsed DOM state, then yield back to the event loop.
        self.commit_streaming_parser_dom_snapshot_to_host(&mut state)?;
        self.streaming_parse = Some(state);
        Ok(true)
      }
      Ok(Outcome::Finished | Outcome::AbortedForNavigation) => {
        self.streaming_parse_active = false;
        self.document_write_state.set_parsing_active(false);
        Ok(false)
      }
      Err(err) => {
        self.streaming_parse_active = false;
        self.document_write_state.set_parsing_active(false);
        Err(err)
      }
    }
  }
  fn fail_external_script_fetch(
    &mut self,
    script_id: ScriptId,
    script_node: NodeId,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<()> {
    // HTML: external script fetch failure should dispatch an `error` event and the script should not
    // execute.
    self.dispatch_script_event_in_event_loop(script_node, "error", event_loop)?;
    // Mark the element as already-started so future scheduling attempts short-circuit.
    self
      .mutate_dom(|dom| (dom.set_script_already_started(script_node, true), false))
      .map_err(|err| Error::Other(err.to_string()))?;

    let actions = self.scheduler.fetch_failed(script_id)?;
    // Treat the script as "done" for parser blocking + deferred-script lifecycle gates.
    self.finish_script_execution(script_id, event_loop)?;
    self.apply_scheduler_actions(actions, event_loop)?;
    Ok(())
  }

  fn discover_scripts_best_effort(
    &mut self,
    document_url: Option<&str>,
  ) -> Vec<(NodeId, ScriptElementSpec)> {
    fn is_html_namespace(namespace: &str) -> bool {
      namespace.is_empty() || namespace == HTML_NAMESPACE
    }

    let dom = self.document.dom();
    let mut base_url_tracker = BaseUrlTracker::new(document_url);
    let mut out: Vec<(NodeId, ScriptElementSpec)> = Vec::new();

    let mut stack: Vec<(NodeId, bool, bool, bool)> = Vec::new();
    stack.push((dom.root(), false, false, false));

    while let Some((id, in_head, in_foreign_namespace, in_template)) = stack.pop() {
      let node = dom.node(id);

      // Shadow roots are treated as separate trees for script discovery/execution.
      if matches!(node.kind, NodeKind::ShadowRoot { .. }) {
        continue;
      }

      // HTML: "prepare a script" early-outs when the script element is not connected. Be robust
      // against partially-detached nodes that may still appear in a parent's `children` list.
      if !dom.is_connected_for_scripting(id) {
        continue;
      }

      let mut next_in_head = in_head;
      let mut next_in_template = in_template;
      let mut next_in_foreign_namespace = in_foreign_namespace;

      match &node.kind {
        NodeKind::Element {
          tag_name,
          namespace,
          attributes,
        } => {
          base_url_tracker.on_element_inserted(
            tag_name,
            namespace,
            attributes,
            in_head,
            in_foreign_namespace,
            in_template,
          );

          if tag_name.eq_ignore_ascii_case("script") && is_html_namespace(namespace) {
            let spec = crate::js::streaming_dom2::build_parser_inserted_script_element_spec_dom2(
              dom,
              id,
              &base_url_tracker,
            );
            out.push((id, spec));
          }

          let is_head = tag_name.eq_ignore_ascii_case("head") && is_html_namespace(namespace);
          next_in_head = in_head || is_head;
          let is_template = tag_name.eq_ignore_ascii_case("template") && is_html_namespace(namespace);
          next_in_template = in_template || is_template;
          next_in_foreign_namespace = in_foreign_namespace || !is_html_namespace(namespace);
        }
        NodeKind::Slot {
          namespace,
          attributes,
          ..
        } => {
          base_url_tracker.on_element_inserted(
            "slot",
            namespace,
            attributes,
            in_head,
            in_foreign_namespace,
            in_template,
          );
          next_in_foreign_namespace = in_foreign_namespace || !is_html_namespace(namespace);
        }
        _ => {}
      }

      // Inert subtrees (template contents) should not be traversed for script execution.
      if node.inert_subtree {
        continue;
      }

      // Push children in reverse so we traverse left-to-right in document order.
      for &child in node.children.iter().rev() {
        stack.push((child, next_in_head, next_in_foreign_namespace, next_in_template));
      }
    }

    // Persist the final base URL so subsequent JS-visible URL resolution uses it.
    self.base_url = base_url_tracker.current_base_url();
    self
      .executor
      .on_document_base_url_updated(self.base_url.as_deref());
    out
  }

  fn discover_dynamic_scripts(&mut self, event_loop: &mut EventLoop<Self>) -> Result<()> {
    // Avoid O(N) scans on every hook call by gating discovery on the document's mutation counter.
    let generation = self.document.dom_mutation_generation();
    if generation == self.last_dynamic_script_discovery_generation {
      return Ok(());
    }
    self.last_dynamic_script_discovery_generation = generation;

    fn is_html_namespace(namespace: &str) -> bool {
      namespace.is_empty() || namespace == HTML_NAMESPACE
    }

    let document_url = self.document_url.as_deref();
    let discovered: Vec<(NodeId, ScriptElementSpec)> = {
      let dom = self.document.dom();
      let mut base_url_tracker = BaseUrlTracker::new(document_url);
      let mut discovered: Vec<(NodeId, ScriptElementSpec)> = Vec::new();

      let mut stack: Vec<(NodeId, bool, bool, bool)> = Vec::new();
      stack.push((dom.root(), false, false, false));

      while let Some((id, in_head, in_foreign_namespace, in_template)) = stack.pop() {
        let node = dom.node(id);

        // Shadow roots are treated as separate trees for script discovery/execution.
        if matches!(node.kind, NodeKind::ShadowRoot { .. }) {
          continue;
        }

        // HTML: "prepare a script" early-outs when the script element is not connected. Be robust
        // against partially-detached nodes that may still appear in a parent's `children` list.
        if !dom.is_connected_for_scripting(id) {
          continue;
        }

        let mut next_in_head = in_head;
        let mut next_in_template = in_template;
        let mut next_in_foreign_namespace = in_foreign_namespace;

        match &node.kind {
          NodeKind::Element {
            tag_name,
            namespace,
            attributes,
          } => {
            base_url_tracker.on_element_inserted(
              tag_name,
              namespace,
              attributes,
              in_head,
              in_foreign_namespace,
              in_template,
            );

            if tag_name.eq_ignore_ascii_case("script")
              && is_html_namespace(namespace)
              // Only schedule scripts that were dynamically inserted (e.g. via DOM APIs).
              //
              // Parser-inserted scripts are discovered explicitly by the streaming parser (or via
              // `discover_scripts_best_effort`). Executing them mutates script internal slots which
              // bumps the document mutation generation; if we treated that change as a signal to
              // rediscover *all* scripts, we'd double-schedule remaining parser-inserted scripts.
              && !node.script_parser_document
              && !node.script_already_started
              && !self.scheduled_script_nodes.contains(&id)
            {
              let mut spec =
                crate::js::streaming_dom2::build_parser_inserted_script_element_spec_dom2(
                  dom,
                  id,
                  &base_url_tracker,
                );
              spec.parser_inserted = false;
              spec.force_async = node.script_force_async;
              discovered.push((id, spec));
            }

            let is_head = tag_name.eq_ignore_ascii_case("head") && is_html_namespace(namespace);
            next_in_head = in_head || is_head;
            let is_template =
              tag_name.eq_ignore_ascii_case("template") && is_html_namespace(namespace);
            next_in_template = in_template || is_template;
            next_in_foreign_namespace = in_foreign_namespace || !is_html_namespace(namespace);
          }
          NodeKind::Slot {
            namespace,
            attributes,
            ..
          } => {
            base_url_tracker.on_element_inserted(
              "slot",
              namespace,
              attributes,
              in_head,
              in_foreign_namespace,
              in_template,
            );
            next_in_foreign_namespace = in_foreign_namespace || !is_html_namespace(namespace);
          }
          _ => {}
        }

        // Inert subtrees (template contents) should not be traversed for script execution.
        if node.inert_subtree {
          continue;
        }

        // Push children in reverse so we traverse left-to-right in document order.
        for &child in node.children.iter().rev() {
          stack.push((child, next_in_head, next_in_foreign_namespace, next_in_template));
        }
      }

      discovered
    };

    for (node_id, spec) in discovered {
      let base_url_at_discovery = spec.base_url.clone();
      self.register_and_schedule_dynamic_script(node_id, spec, base_url_at_discovery, event_loop)?;
    }

    Ok(())
  }

  fn register_and_schedule_script(
    &mut self,
    node_id: NodeId,
    mut spec: ScriptElementSpec,
    base_url_at_discovery: Option<String>,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<ScriptId> {
    // HTML `</script>` handling performs a microtask checkpoint *before* preparing the script, but
    // only when the JS execution context stack is empty.
    //
    // This matters even when the script ultimately does not run (e.g. empty inline scripts), as the
    // checkpoint can mutate the document (including this `<script>` element).
    //
    // When using the streaming parser pipeline, the checkpoint is performed at the script
    // end-tag boundary (before we compute the final `ScriptElementSpec`). Avoid duplicating the
    // checkpoint here during an active streaming parse.
    if spec.parser_inserted && !self.streaming_parse_active && self.js_execution_depth.get() == 0 {
      self.with_installed_document_write_state(|host| event_loop.perform_microtask_checkpoint(host))?;
    }

    // HTML: "prepare the script element" can return early without executing the script (e.g.
    // unsupported `type`, empty inline script). In that case the spec clears the "parser document"
    // internal slot (and may set force-async) so future mutations/insertion treat the element like a
    // dynamic script.
    //
    // Note: we do this *before* CSP handling so empty inline scripts early-out before CSP checks
    // (matching the spec's step ordering).
    let should_run = self.mutate_dom(|dom| {
      (
        crate::js::prepare_script_element_dom2(dom, node_id, &spec),
        /* changed */ false,
      )
    });
    if !should_run {
      let discovered =
        self
          .scheduler
          .discovered_parser_script(spec, node_id, base_url_at_discovery)?;
      return Ok(discovered.id);
    }

    fn trim_ascii_whitespace(value: &str) -> &str {
      value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
    }

    let nonce_attr = self
      .document
      .dom()
      .get_attribute(node_id, "nonce")
      .ok()
      .flatten()
      .map(trim_ascii_whitespace)
      .filter(|value| !value.is_empty());

    if spec.script_type == ScriptType::Classic {
      if let Some(csp) = self.csp.as_ref() {
        let document_origin = self.document_origin.as_ref();

        // Inline classic scripts.
        //
        // Note: HTML uses the *presence* of the `src` attribute to suppress inline execution, even
        // if the value is empty/invalid. Mirror the scheduler's `src_attr_present` semantics here.
        if !spec.src_attr_present {
          if !csp.allows_inline_script(nonce_attr, &spec.inline_text) {
            let mut span = self.trace.span("js.script.csp_block", "js");
            span.arg_u64("node_id", node_id.index() as u64);
            span.arg_str("kind", "inline");
            if let Some(nonce) = nonce_attr {
              span.arg_str("nonce", nonce);
            }
            // Suppress execution by forcing the "external src attribute present but invalid" path.
            spec.src_attr_present = true;
            spec.src = None;
          }
        } else if let Some(src) = spec.src.as_deref().filter(|s| !s.is_empty()) {
          match Url::parse(src) {
            Ok(url) => {
              if !csp.allows_script_url(document_origin, nonce_attr, &url) {
                let mut span = self.trace.span("js.script.csp_block", "js");
                span.arg_u64("node_id", node_id.index() as u64);
                span.arg_str("kind", "external");
                span.arg_str("url", src);
                if let Some(nonce) = nonce_attr {
                  span.arg_str("nonce", nonce);
                }
                spec.src = None;
              }
            }
            Err(_) => {
              // Conservatively block unparseable URLs when a CSP policy is present.
              let mut span = self.trace.span("js.script.csp_block", "js");
              span.arg_u64("node_id", node_id.index() as u64);
              span.arg_str("kind", "external");
              span.arg_str("url", src);
              span.arg_str("reason", "invalid_url");
              if let Some(nonce) = nonce_attr {
                span.arg_str("nonce", nonce);
              }
              spec.src = None;
            }
          }
        }
      }
    }

    let spec_for_table = spec.clone();
    let nomodule_blocked = spec_for_table.is_suppressed_by_nomodule(&self.js_execution_options);
    let discovered = self
      .scheduler
      .discovered_parser_script(spec, node_id, base_url_at_discovery)?;
    if discovered.actions.is_empty() {
      // Most of the time an empty action list means the script should be ignored (e.g. unknown type
      // or `nomodule` suppression). However, parser-inserted *inline* module scripts are deferred by
      // default and only become runnable once parsing completes, so the scheduler will produce work
      // later via `parsing_completed()`. In that case we must still register the script in the host
      // tables so later scheduler actions can resolve `script_id -> ScriptElementSpec`.
      let should_register_without_actions = self.js_execution_options.supports_module_scripts
        && spec_for_table.script_type == ScriptType::Module
        && spec_for_table.parser_inserted
        && !spec_for_table.async_attr
        && !spec_for_table.src_attr_present;
      if !should_register_without_actions {
        return Ok(discovered.id);
      }
    }
    let is_deferred = match spec_for_table.script_type {
      ScriptType::Classic => {
        spec_for_table.parser_inserted
          && spec_for_table.src_attr_present
          && spec_for_table.src.as_deref().is_some_and(|src| !src.is_empty())
          && spec_for_table.defer_attr
          && !spec_for_table.async_attr
          && !nomodule_blocked
      }
      ScriptType::Module => {
        // Parser-inserted module scripts are deferred-by-default when `async` is absent (the `defer`
        // attribute has no effect). They should delay `DOMContentLoaded` like classic `defer`.
        let has_executable_source = if spec_for_table.src_attr_present {
          spec_for_table
            .src
            .as_deref()
            .is_some_and(|src| !src.is_empty())
        } else {
          true
        };
        spec_for_table.parser_inserted && !spec_for_table.async_attr && has_executable_source
      }
      ScriptType::ImportMap | ScriptType::Unknown => false,
    };
    let should_check_inline_source = !spec_for_table.src_attr_present
      && matches!(
        spec_for_table.script_type,
        ScriptType::Classic | ScriptType::Module | ScriptType::ImportMap
      )
      && !(spec_for_table.script_type == ScriptType::Classic && nomodule_blocked);
    if should_check_inline_source {
      self
        .js_execution_options
        .check_script_source(&spec_for_table.inline_text, "source=inline")?;
    }
    if spec_for_table.script_type == ScriptType::ImportMap && !spec_for_table.src_attr_present {
      self
        .js_execution_options
        .check_script_source(&spec_for_table.inline_text, "source=importmap")?;
    }
    self.scripts.insert(
      discovered.id,
      ScriptEntry {
        node_id,
        spec: spec_for_table,
      },
    );
    self.scheduled_script_nodes.insert(node_id);
    if is_deferred {
      self.lifecycle.register_deferred_script();
      self.deferred_scripts.insert(discovered.id);
    }
    self.apply_scheduler_actions(discovered.actions, event_loop)?;
    Ok(discovered.id)
  }

  fn register_and_schedule_dynamic_script(
    &mut self,
    node_id: NodeId,
    spec: ScriptElementSpec,
    base_url_at_discovery: Option<String>,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<ScriptId> {
    let spec_for_table = spec.clone();
    let failed_to_run = (!spec_for_table.src_attr_present && spec_for_table.inline_text.is_empty())
      || spec_for_table.script_type == ScriptType::Unknown;

    if matches!(
      spec_for_table.script_type,
      ScriptType::Classic | ScriptType::Module | ScriptType::ImportMap
    )
      && !spec_for_table.src_attr_present
    {
      self
        .js_execution_options
        .check_script_source(&spec_for_table.inline_text, "source=inline")?;
    }
    if spec_for_table.script_type == ScriptType::ImportMap && !spec_for_table.src_attr_present {
      self
        .js_execution_options
        .check_script_source(&spec_for_table.inline_text, "source=importmap")?;
    }
    let discovered = self
      .scheduler
      .discovered_script(spec, node_id, base_url_at_discovery)?;
    if failed_to_run && discovered.actions.is_empty() {
      // HTML "prepare the script element" early-outs without marking the script as started when it
      // does not run. Keep it eligible for later mutations (src/children changed steps).
      return Ok(discovered.id);
    }
    self.scripts.insert(
      discovered.id,
      ScriptEntry {
        node_id,
        spec: spec_for_table,
      },
    );
    self.scheduled_script_nodes.insert(node_id);
    self.apply_scheduler_actions(discovered.actions, event_loop)?;
    Ok(discovered.id)
  }

  fn apply_scheduler_actions(
    &mut self,
    actions: Vec<ScriptSchedulerAction<NodeId>>,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<()> {
    for action in actions {
      if self.pending_navigation.is_some() {
        break;
      }
      match action {
        ScriptSchedulerAction::StartFetch {
          script_id,
          url,
          destination,
          ..
        } => {
          if !self.pending_script_load_blockers.insert(script_id) {
            return Err(Error::Other(format!(
              "ScriptScheduler requested StartFetch more than once for script_id={}",
              script_id.as_u64()
            )));
          }
          self
            .lifecycle
            .register_pending_load_blocker(LoadBlockerKind::Script);
          self.start_fetch(script_id, url, destination, event_loop)?;
        }
        ScriptSchedulerAction::StartModuleGraphFetch { script_id, .. } => {
          if !self.pending_script_load_blockers.insert(script_id) {
            return Err(Error::Other(format!(
              "ScriptScheduler requested StartModuleGraphFetch more than once for script_id={}",
              script_id.as_u64()
            )));
          }
          self
            .lifecycle
            .register_pending_load_blocker(LoadBlockerKind::Script);
          self.start_module_graph_fetch(script_id, event_loop)?;
        }
        ScriptSchedulerAction::BlockParserUntilExecuted { script_id, .. } => {
          if self.executed.contains(&script_id) {
            continue;
          }
          if self
            .parser_blocked_on
            .is_some_and(|existing| existing != script_id)
          {
            return Err(Error::Other(
              "ScriptScheduler requested multiple simultaneous parser blocks".to_string(),
            ));
          }
          self.parser_blocked_on = Some(script_id);
        }
        ScriptSchedulerAction::ExecuteNow {
          script_id,
          node_id,
          source_text,
          ..
        } => {
          let entry = self.scripts.get(&script_id).cloned();
          let should_checkpoint = entry
            .as_ref()
            .is_some_and(|entry| matches!(entry.spec.script_type, ScriptType::Classic | ScriptType::Module));
          if let Some(csp) = self.csp.as_ref() {
            let is_inline_classic = entry.as_ref().is_some_and(|entry| {
              entry.spec.script_type == ScriptType::Classic && !entry.spec.src_attr_present
            });
            if is_inline_classic {
              fn trim_ascii_whitespace(value: &str) -> &str {
                value.trim_matches(|c: char| {
                  matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' ')
                })
              }
              let nonce_attr = self
                .document
                .dom()
                .get_attribute(node_id, "nonce")
                .ok()
                .flatten()
                .map(trim_ascii_whitespace)
                .filter(|value| !value.is_empty())
                .map(|value| value.to_string());
              if !csp.allows_inline_script(nonce_attr.as_deref(), &source_text) {
                let mut span = self.trace.span("js.script.csp_block", "js");
                span.arg_u64("node_id", node_id.index() as u64);
                span.arg_str("kind", "inline");
                if let Some(nonce) = nonce_attr.as_deref() {
                  span.arg_str("nonce", nonce);
                }

                self.dispatch_script_error_event_in_event_loop(node_id, event_loop)?;
                self
                  .mutate_dom(|dom| (dom.set_script_already_started(node_id, true), false))
                  .map_err(|err| Error::Other(err.to_string()))?;
                self.finish_script_execution(script_id, event_loop)?;
                continue;
              }
            }
          }

          if self.streaming_parse_active
            && self.should_delay_parser_blocking_script(script_id)
            && self.script_blocking_stylesheets.has_blocking_stylesheet()
          {
            // Treat stylesheet-blocking as another reason for a parser-inserted script to stall
            // synchronous execution/parsing.
            if self
              .parser_blocked_on
              .is_some_and(|existing| existing != script_id)
            {
              return Err(Error::Other(
                "Attempted to delay multiple parser-blocking scripts".to_string(),
              ));
            }
            self.parser_blocked_on = Some(script_id);
            self.pending_parser_blocking_script = Some(PendingParserBlockingScript {
              script_id,
              source_text,
            });
            return Ok(());
          }

          let generation_before = self.document.dom_mutation_generation();
          let exec_result = {
            let _guard = JsExecutionGuard::enter(&self.js_execution_depth);
            self.execute_script(script_id, &source_text, event_loop)
          };
          // Ensure a script failure doesn't leave parsing blocked forever.
          self.finish_script_execution(script_id, event_loop)?;

          // HTML: "clean up after running script" performs a microtask checkpoint only when the JS
          // execution context stack is empty. Nested (re-entrant) script execution must not drain
          // microtasks until the outermost script returns.
          //
          // HTML fires `<script>` load/error events *after* `run a classic script` returns, so any
          // microtasks queued by script execution must run before those events are dispatched.
          if matches!(&exec_result, Err(Error::Render(_))) {
            // Script execution timed out/cancelled. Preserve existing behavior: dispatch the script
            // element error event, then abort without attempting a microtask checkpoint.
            if let Some(entry) = entry {
              self.dispatch_script_event_in_event_loop(entry.node_id, "error", event_loop)?;
            }
            return exec_result;
          }

          let microtask_err = if should_checkpoint && self.js_execution_depth.get() == 0 {
            self
              .with_installed_document_write_state(|host| event_loop.perform_microtask_checkpoint(host))
              .err()
          } else {
            None
          };

          match exec_result {
            Ok(()) => {
              if let Some(entry) = entry {
                if entry.spec.src_attr_present && entry.spec.src.as_deref().is_some_and(|s| !s.is_empty()) {
                  self.dispatch_script_event_in_event_loop(entry.node_id, "load", event_loop)?;
                }
              }
            }
            Err(err) => {
              let Some(entry) = entry else {
                return Err(err);
              };
              self.dispatch_script_event_in_event_loop(entry.node_id, "error", event_loop)?;
              // Uncaught exceptions from scripts should not abort parsing/task scheduling (browser
              // behavior). Still propagate host-level render timeouts/cancellation.
              if matches!(err, Error::Render(_)) {
                return Err(err);
              }
            }
          }

          if let Some(err) = microtask_err {
            return Err(err);
          }

          // HTML: scripts inserted by the executed script (or its microtasks) should be prepared
          // once the script finishes running. Avoid O(N) DOM scans when the script did not mutate
          // the DOM.
          if generation_before != self.document.dom_mutation_generation() {
            self.discover_dynamic_scripts(event_loop)?;
          }
        }
        ScriptSchedulerAction::QueueTask {
          script_id,
          node_id,
          source_text,
          ..
        } => {
          if let Some(csp) = self.csp.as_ref() {
            let is_inline_classic = self.scripts.get(&script_id).is_some_and(|entry| {
              entry.spec.script_type == ScriptType::Classic && !entry.spec.src_attr_present
            });
            if is_inline_classic {
              fn trim_ascii_whitespace(value: &str) -> &str {
                value.trim_matches(|c: char| {
                  matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' ')
                })
              }
              let nonce_attr = self
                .document
                .dom()
                .get_attribute(node_id, "nonce")
                .ok()
                .flatten()
                .map(trim_ascii_whitespace)
                .filter(|value| !value.is_empty())
                .map(|value| value.to_string());
              if !csp.allows_inline_script(nonce_attr.as_deref(), &source_text) {
                let mut span = self.trace.span("js.script.csp_block", "js");
                span.arg_u64("node_id", node_id.index() as u64);
                span.arg_str("kind", "inline");
                if let Some(nonce) = nonce_attr.as_deref() {
                  span.arg_str("nonce", nonce);
                }

                self.dispatch_script_error_event_in_event_loop(node_id, event_loop)?;
                self
                  .mutate_dom(|dom| (dom.set_script_already_started(node_id, true), false))
                  .map_err(|err| Error::Other(err.to_string()))?;
                self.finish_script_execution(script_id, event_loop)?;
                continue;
              }
            }
          }
          let task_source = self
            .scripts
            .get(&script_id)
            .map(|entry| match entry.spec.script_type {
              ScriptType::Module => TaskSource::Networking,
              _ => TaskSource::Script,
            })
            .unwrap_or(TaskSource::Script);

          event_loop.queue_task(task_source, move |host, event_loop| {
            let entry = host.scripts.get(&script_id).cloned();
            let result = {
              let _guard = JsExecutionGuard::enter(&host.js_execution_depth);
              host.execute_script(script_id, &source_text, event_loop)
            };
            host.finish_script_execution(script_id, event_loop)?;

            if matches!(&result, Err(Error::Render(_))) {
              // Preserve existing behavior: dispatch the script element error event, then abort
              // without attempting a microtask checkpoint.
              if let Some(entry) = entry {
                host.dispatch_script_event_in_event_loop(entry.node_id, "error", event_loop)?;
              }
              return result;
            }

            let microtask_err = if host.js_execution_depth.get() == 0 {
              host
                .with_installed_document_write_state(|host| event_loop.perform_microtask_checkpoint(host))
                .err()
            } else {
              None
            };

            match result {
              Ok(()) => {
                if let Some(entry) = entry {
                  if entry.spec.src_attr_present && entry.spec.src.as_deref().is_some_and(|s| !s.is_empty()) {
                    host.dispatch_script_event_in_event_loop(entry.node_id, "load", event_loop)?;
                  }
                }
              }
              Err(err) => {
                let Some(entry) = entry else {
                  return Err(err);
                };
                host.dispatch_script_event_in_event_loop(entry.node_id, "error", event_loop)?;
                if matches!(err, Error::Render(_)) {
                  return Err(err);
                }
              }
            }

            if let Some(err) = microtask_err {
              return Err(err);
            }

            Ok(())
          })?;
        }
        ScriptSchedulerAction::QueueScriptEventTask { node_id, event, .. } => {
          let type_str = event.as_type_str();
          event_loop.queue_task(TaskSource::DOMManipulation, move |host, event_loop| {
            let mut ev = Event::new(type_str, EventInit::default());
            ev.is_trusted = true;
            with_event_loop(event_loop, || host.dispatch_lifecycle_event(EventTargetId::Node(node_id), ev))?;
            Ok(())
          })?;
        }
      }
    }
    Ok(())
  }

  fn finish_script_execution(
    &mut self,
    script_id: ScriptId,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<()> {
    let newly_executed = self.executed.insert(script_id);
    if self.parser_blocked_on == Some(script_id) {
      self.parser_blocked_on = None;
    }
    if newly_executed && self.deferred_scripts.contains(&script_id) {
      self.lifecycle.deferred_script_executed(event_loop)?;
    }
    if newly_executed && self.pending_script_load_blockers.remove(&script_id) {
      self
        .lifecycle
        .load_blocker_completed(LoadBlockerKind::Script, event_loop)?;
    }
    Ok(())
  }

  fn execute_script(
    &mut self,
    script_id: ScriptId,
    source_text: &str,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<()> {
    if self.executed.contains(&script_id) {
      return Ok(());
    }

    let Some(entry) = self.scripts.get(&script_id).cloned() else {
      return Err(Error::Other(format!(
        "ScriptScheduler requested execution for unknown script_id={}",
        script_id.as_u64()
      )));
    };

    let node_id = entry.node_id;
    let script_type = entry.spec.script_type;

    struct Adapter<'a> {
      script_id: ScriptId,
      source_text: &'a str,
      spec: &'a ScriptElementSpec,
      event_loop: &'a mut EventLoop<BrowserTabHost>,
    }

    impl ScriptBlockExecutor<BrowserTabHost> for Adapter<'_> {
      fn execute_script(
        &mut self,
        host: &mut BrowserTabHost,
        _orchestrator: &mut ScriptOrchestrator,
        _script: NodeId,
        script_type: ScriptType,
      ) -> Result<()> {
        let mut span = host.trace.span("js.script.execute", "js");
        span.arg_u64("script_id", self.script_id.as_u64());
        span.arg_str(
          "script_type",
          match script_type {
            ScriptType::Classic => "classic",
            ScriptType::Module => "module",
            ScriptType::ImportMap => "importmap",
            ScriptType::Unknown => "unknown",
          },
        );
        if let Some(url) = self.spec.src.as_deref() {
          span.arg_str("url", url);
        }
        span.arg_bool("async_attr", self.spec.async_attr);
        span.arg_bool("defer_attr", self.spec.defer_attr);
        span.arg_bool("parser_inserted", self.spec.parser_inserted);

        let current_script = host.current_script_node();
        // Split the host borrow so we can install a JS-visible `DocumentWriteState` while still
        // calling into the executor.
        let BrowserTabHost {
          executor,
          document,
          pending_navigation,
          pending_navigation_deadline,
          document_write_state,
          ..
        } = host;
        let result = crate::js::with_document_write_state(document_write_state, || match script_type {
          ScriptType::Classic => executor.execute_classic_script(
            self.source_text,
            self.spec,
            current_script,
            document.as_mut(),
            self.event_loop,
          ),
          ScriptType::Module => executor.execute_module_script(
            self.source_text,
            self.spec,
            current_script,
            document.as_mut(),
            self.event_loop,
          ),
          ScriptType::ImportMap => executor.execute_import_map_script(
            self.source_text,
            self.spec,
            current_script,
            document.as_mut(),
            self.event_loop,
          ),
          ScriptType::Unknown => Ok(()),
        });
        if let Some(req) = executor.take_navigation_request() {
          *pending_navigation = Some(req);
          *pending_navigation_deadline =
            crate::render_control::root_deadline().filter(|deadline| deadline.is_enabled());
        }
        result
      }
    }

    let mut adapter = Adapter {
      script_id,
      source_text,
      spec: &entry.spec,
      event_loop,
    };

    // Avoid double-borrowing `self` by temporarily moving the orchestrator out.
    let mut orchestrator = std::mem::take(&mut self.orchestrator);
    let result = orchestrator.execute_script_element(self, node_id, script_type, &mut adapter);
    self.orchestrator = orchestrator;
    result
  }

  fn start_module_graph_fetch(
    &mut self,
    script_id: ScriptId,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<()> {
    let Some(entry) = self.scripts.get(&script_id).cloned() else {
      return Err(Error::Other(format!(
        "StartModuleGraphFetch for unknown script_id={}",
        script_id.as_u64()
      )));
    };
    let mut spec = entry.spec.clone();
    if spec.script_type != ScriptType::Module {
      return Err(Error::Other(format!(
        "StartModuleGraphFetch for non-module script_id={}",
        script_id.as_u64()
      )));
    }
    let script_node_id = entry.node_id;

    use crate::resource::FetchedResource;
    use std::collections::HashMap;
    use std::sync::Mutex;

    struct OverlayFetcher {
      base: Arc<dyn ResourceFetcher>,
      sources: Arc<Mutex<HashMap<String, String>>>,
    }

    impl OverlayFetcher {
      fn override_bytes(&self, url: &str) -> Option<Vec<u8>> {
        let source = {
          let sources = self
            .sources
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
          sources.get(url).cloned()
        };
        source.map(|s| s.into_bytes())
      }

      fn http_origin_header_value(origin: &crate::resource::DocumentOrigin) -> Option<String> {
        // Match the serialization used for request `Origin` headers: omit default ports and use
        // bracketed IPv6 host literals.
        //
        // Note: `DocumentOrigin`'s Display impl always prints an effective port for http(s), which
        // is fine for internal comparisons but can diverge from the browser's header form.
        if !origin.is_http_like() {
          return None;
        }
        let host = origin.host()?;
        let host = match host.parse::<std::net::IpAddr>() {
          Ok(std::net::IpAddr::V6(_)) => format!("[{host}]"),
          _ => host.to_string(),
        };

        let mut origin_str = format!("{}://{}", origin.scheme(), host);
        if let Some(port) = origin.port() {
          let default_port = match origin.scheme() {
            "http" => 80,
            "https" => 443,
            _ => port,
          };
          if port != default_port {
            origin_str.push_str(&format!(":{port}"));
          }
        }
        Some(origin_str)
      }

      fn allow_origin_for_request(req: FetchRequest<'_>) -> String {
        // Synthetic script sources are used by tests/fixtures and do not have real response headers.
        // Mirror permissive CORS headers so module graphs can be fetched in CORS mode without
        // introducing test-only branching in the module loader.
        if req.credentials_mode == crate::resource::FetchCredentialsMode::Include {
          match req.client_origin {
            Some(origin) if origin.is_http_like() => {
              Self::http_origin_header_value(origin).unwrap_or_else(|| origin.to_string())
            }
            // File/non-http origins are represented as "null" for CORS header matching.
            Some(_) => "null".to_string(),
            None => "*".to_string(),
          }
        } else {
          "*".to_string()
        }
      }

      fn override_resource(url: &str, bytes: Vec<u8>, allow_origin: String) -> FetchedResource {
        let mut res = FetchedResource::new(bytes, Some("application/javascript".to_string()));
        // Mirror HTTP fetches so downstream validations (status/CORS) remain deterministic.
        res.status = Some(200);
        res.final_url = Some(url.to_string());
        // Allow CORS-mode scripts/modules to pass enforcement if enabled.
        res.access_control_allow_origin = Some(allow_origin);
        res.access_control_allow_credentials = true;
        res
      }
    }

    impl ResourceFetcher for OverlayFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        if let Some(bytes) = self.override_bytes(url) {
          return Ok(Self::override_resource(url, bytes, "*".to_string()));
        }
        self.base.fetch(url)
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        if let Some(bytes) = self.override_bytes(req.url) {
          let allow_origin = Self::allow_origin_for_request(req);
          return Ok(Self::override_resource(req.url, bytes, allow_origin));
        }
        self.base.fetch_with_request(req)
      }

      fn fetch_partial_with_request(
        &self,
        req: FetchRequest<'_>,
        max_bytes: usize,
      ) -> Result<FetchedResource> {
        if let Some(mut bytes) = self.override_bytes(req.url) {
          if bytes.len() > max_bytes {
            bytes.truncate(max_bytes);
          }
          let allow_origin = Self::allow_origin_for_request(req);
          return Ok(Self::override_resource(req.url, bytes, allow_origin));
        }
        self.base.fetch_partial_with_request(req, max_bytes)
      }

      fn request_header_value(&self, req: FetchRequest<'_>, header_name: &str) -> Option<String> {
        let has_source = self
          .sources
          .lock()
          .unwrap_or_else(|poisoned| poisoned.into_inner())
          .contains_key(req.url);
        if has_source {
          // In-memory script sources are synthetic and do not have a stable header profile. Treat
          // them as unknown so caching wrappers conservatively avoid Vary-dependent caching.
          return None;
        }
        self.base.request_header_value(req, header_name)
      }

      fn cookie_header_value(&self, url: &str) -> Option<String> {
        self.base.cookie_header_value(url)
      }

      fn store_cookie_from_document(&self, url: &str, cookie_string: &str) {
        self.base.store_cookie_from_document(url, cookie_string);
      }
    }

    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(OverlayFetcher {
      base: self.document.fetcher(),
      sources: Arc::clone(&self.external_script_sources),
    });

    let supports_module_graph_fetch = self.executor.supports_module_graph_fetch();

    // HTML queues module graph fetching on the networking task source. This applies even for inline
    // module scripts.
    event_loop.queue_task(TaskSource::Networking, move |host, event_loop| {
      if !supports_module_graph_fetch {
        // `BrowserTabJsExecutor::fetch_module_graph` is optional. Executors that support module
        // scripts but do not implement graph prefetch should still fetch the entry module so
        // `<script type="module" src=...>` participates in the normal resource pipeline (e.g.
        // request destination classification).
        let source_text = if spec.src_attr_present {
          let Some(url) = spec.src.as_deref().filter(|s| !s.is_empty()) else {
            // Mirror the scheduler's invalid-src behavior (error event + no execution).
            host.dispatch_script_event_in_event_loop(script_node_id, "error", event_loop)?;
            host.mutate_dom(|dom| {
              dom.node_mut(script_node_id).script_already_started = true;
              ((), false)
            });
            let actions = host.scheduler.module_graph_failed(script_id)?;
            host.finish_script_execution(script_id, event_loop)?;
            host.apply_scheduler_actions(actions, event_loop)?;
            return Ok(());
          };
          match host.fetch_script_source(script_id, url, FetchDestination::ScriptCors) {
            Ok(source_text) => source_text,
            Err(err) => {
              host.dispatch_script_event_in_event_loop(script_node_id, "error", event_loop)?;
              host.mutate_dom(|dom| {
                dom.node_mut(script_node_id).script_already_started = true;
                ((), false)
              });
              let actions = host.scheduler.module_graph_failed(script_id)?;
              host.finish_script_execution(script_id, event_loop)?;
              host.apply_scheduler_actions(actions, event_loop)?;
              if matches!(err, Error::Render(_)) {
                return Err(err);
              }
              return Ok(());
            }
          }
        } else {
          std::mem::take(&mut spec.inline_text)
        };

        let actions = host.scheduler.module_graph_ready(script_id, source_text)?;
        host.apply_scheduler_actions(actions, event_loop)?;
        return Ok(());
      }

      let result = {
        let BrowserTabHost { executor, document, .. } = host;
        executor.fetch_module_graph(&spec, Arc::clone(&fetcher), document.as_mut(), event_loop)
      };
      match result {
        Ok(()) => {
          let actions = host.scheduler.module_graph_ready(script_id, String::new())?;
          host.apply_scheduler_actions(actions, event_loop)?;
        }
        Err(err) => {
          // Module graph failures dispatch an `error` event and must not execute.
          host.dispatch_script_event_in_event_loop(script_node_id, "error", event_loop)?;
          host.mutate_dom(|dom| {
            dom.node_mut(script_node_id).script_already_started = true;
            ((), false)
          });
 
          let actions = host.scheduler.module_graph_failed(script_id)?;
          // Treat the script as "done" for deferred-script lifecycle gates.
          host.finish_script_execution(script_id, event_loop)?;
          host.apply_scheduler_actions(actions, event_loop)?;
 
          // Uncaught module graph errors should not abort parsing/task scheduling (browser
          // behavior). Still propagate host-level render timeouts/cancellation.
          if matches!(err, Error::Render(_)) {
            return Err(err);
          }
        }
      }
      Ok(())
    })?;
    Ok(())
  }

  fn start_fetch(
    &mut self,
    script_id: ScriptId,
    url: String,
    destination: FetchDestination,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<()> {
    if let Some(csp) = self.csp.as_ref() {
      let parsed = Url::parse(&url).ok();
      let doc_origin = self
        .document_url
        .as_deref()
        .and_then(crate::resource::origin_from_url)
        .or_else(|| {
          self
            .scripts
            .get(&script_id)
            .and_then(|entry| entry.spec.base_url.as_deref())
            .and_then(crate::resource::origin_from_url)
        });
      fn trim_ascii_whitespace(value: &str) -> &str {
        value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
      }
      let nonce_attr = self
        .scripts
        .get(&script_id)
        .map(|entry| entry.node_id)
        .and_then(|node_id| {
          self
            .document
            .dom()
            .get_attribute(node_id, "nonce")
            .ok()
            .flatten()
            .map(trim_ascii_whitespace)
            .filter(|value| !value.is_empty())
            .map(|value| value.to_string())
        });
      let allowed = parsed.as_ref().is_some_and(|parsed| {
        csp.allows_script_url(doc_origin.as_ref(), nonce_attr.as_deref(), parsed)
      });
      if !allowed {
        let script_node = self
          .scripts
          .get(&script_id)
          .map(|entry| entry.node_id)
          .ok_or_else(|| Error::Other("internal error: missing script entry".to_string()))?;
        return self.fail_external_script_fetch(script_id, script_node, event_loop);
      }
    }

    let is_blocking = self
      .scripts
      .get(&script_id)
      .is_some_and(|entry| {
        entry.spec.parser_inserted && entry.spec.script_type == ScriptType::Classic
          && entry.spec.src_attr_present
          && !entry.spec.async_attr
          && !entry.spec.defer_attr
          && !entry.spec.force_async
      });

    if is_blocking {
      let script_node_id = self
        .scripts
        .get(&script_id)
        .map(|entry| entry.node_id)
        .ok_or_else(|| {
          Error::Other(format!(
            "ScriptScheduler requested fetch for unknown script_id={}",
            script_id.as_u64()
          ))
        })?;
      match self.fetch_script_source(script_id, &url, destination) {
        Ok(source) => {
          let actions = self.scheduler.fetch_completed(script_id, source)?;
          self.apply_scheduler_actions(actions, event_loop)?;
        }
        Err(err) => {
          self.fail_external_script_fetch(script_id, script_node_id, event_loop)?;
          if matches!(err, Error::Render(_)) {
            return Err(err);
          }
        }
      }
      return Ok(());
    }

    let destination = destination;
    event_loop.queue_task(TaskSource::Networking, move |host, event_loop| {
      match host.fetch_script_source(script_id, &url, destination) {
        Ok(source) => {
          let actions = host.scheduler.fetch_completed(script_id, source)?;
          host.apply_scheduler_actions(actions, event_loop)?;
        }
        Err(err) => {
          let script_node_id = host
            .scripts
            .get(&script_id)
            .map(|entry| entry.node_id)
            .ok_or_else(|| {
              Error::Other(format!(
                "ScriptScheduler requested fetch for unknown script_id={}",
                script_id.as_u64()
              ))
            })?;
          host.fail_external_script_fetch(script_id, script_node_id, event_loop)?;
          if matches!(err, Error::Render(_)) {
            return Err(err);
          }
        }
      }
      Ok(())
    })?;
    Ok(())
  }

  fn fetch_script_source(
    &self,
    script_id: ScriptId,
    url: &str,
    destination: FetchDestination,
  ) -> Result<String> {
    let mut span = self.trace.span("js.script.fetch", "js");
    span.arg_u64("script_id", script_id.as_u64());
    span.arg_str("url", url);

    let spec = self
      .scripts
      .get(&script_id)
      .map(|entry| &entry.spec)
      .ok_or_else(|| {
        Error::Other(format!(
          "fetch_script_source called for unknown script_id={}",
          script_id.as_u64()
        ))
      })?;

    span.arg_str(
      "script_type",
      match spec.script_type {
        ScriptType::Classic => "classic",
        ScriptType::Module => "module",
        ScriptType::ImportMap => "importmap",
        ScriptType::Unknown => "unknown",
      },
    );

    // HTML: module scripts are fetched in CORS mode by default. The `crossorigin` attribute only
    // controls the *credentials mode* ("anonymous" vs "use-credentials"). For classic scripts, CORS
    // mode is enabled only when the attribute is present.
    let cors_mode = match spec.script_type {
      ScriptType::Module => Some(spec.crossorigin.unwrap_or(crate::resource::CorsMode::Anonymous)),
      _ => spec.crossorigin,
    };

    // Subresource Integrity (SRI) enforcement. When the `integrity` attribute is present, we must
    // reject the script if the metadata is invalid or the fetched bytes do not match.
    //
    // Additionally, HTML requires a CORS-enabled fetch for cross-origin resources when SRI is used.
    // We enforce this by requiring a `crossorigin` attribute for cross-origin URLs.
    let integrity = if spec.integrity_attr_present {
      let integrity = spec.integrity.as_deref().ok_or_else(|| {
        Error::Other(format!(
          "SRI blocked script {url}: integrity attribute exceeded max length of {} bytes",
          crate::js::sri::MAX_INTEGRITY_ATTRIBUTE_BYTES
        ))
      })?;

      if cors_mode.is_none() {
        if let (Some(doc_origin), Some(target_origin)) =
          (self.document_origin.as_ref(), crate::resource::origin_from_url(url))
        {
          if !doc_origin.same_origin(&target_origin) {
            return Err(Error::Other(format!(
              "SRI blocked script {url}: cross-origin integrity requires a CORS-enabled fetch (missing crossorigin attribute)"
            )));
          }
        }
      }

      Some(integrity)
    } else {
      None
    };

    let override_source = self
      .external_script_sources
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .get(url)
      .cloned();
    if let Some(source) = override_source {
      span.arg_u64("bytes", source.as_bytes().len() as u64);
      self.js_execution_options.check_script_source_bytes(
        source.as_bytes().len(),
        &format!("source=external url={url}"),
      )?;
      if let Some(integrity) = integrity {
        crate::js::sri::verify_integrity(source.as_bytes(), integrity).map_err(|message| {
          Error::Other(format!("SRI blocked script {url}: {message}"))
        })?;
      }
      return Ok(source);
    }

    let fetcher = self.document.fetcher();
    let effective_destination = match spec.script_type {
      ScriptType::Module => FetchDestination::ScriptCors,
      _ => destination,
    };
    let mut req = FetchRequest::new(url, effective_destination);
    if let Some(referrer) = self.document_url.as_deref() {
      req = req.with_referrer_url(referrer);
    }
    if let Some(origin) = self.document_origin.as_ref() {
      req = req.with_client_origin(origin);
    }
    let effective_referrer_policy = spec.referrer_policy.unwrap_or(self.document_referrer_policy);
    req = req.with_referrer_policy(effective_referrer_policy);
    if let Some(cors_mode) = cors_mode {
      req = req.with_credentials_mode(cors_mode.credentials_mode());
    }

    let max_fetch = self.js_execution_options.max_script_bytes.saturating_add(1);
    let resource = fetcher.fetch_partial_with_request(req, max_fetch)?;
    span.arg_u64("bytes", resource.bytes.len() as u64);
    self.js_execution_options.check_script_source_bytes(
      resource.bytes.len(),
      &format!("source=external url={url}"),
    )?;

    crate::resource::ensure_http_success(&resource, url)?;
    crate::resource::ensure_script_mime_sane(&resource, url)?;
    if let Some(cors_mode) = cors_mode {
      if crate::resource::cors_enforcement_enabled() {
        crate::resource::ensure_cors_allows_origin(
          self.document_origin.as_ref(),
          &resource,
          url,
          cors_mode,
        )?;
      }
    }
    if let Some(integrity) = integrity {
      crate::js::sri::verify_integrity(&resource.bytes, integrity).map_err(|message| {
        Error::Other(format!("SRI blocked script {url}: {message}"))
      })?;
    }

    let fallback_encoding = self
      .scripts
      .get(&script_id)
      .and_then(|entry| {
        let dom = self.document.dom();
        dom.get_attribute(entry.node_id, "charset").ok().flatten()
      })
      .map(|value| {
        value.trim_matches(|c: char| {
          matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' ')
        })
      })
      .and_then(|label| Encoding::for_label(label.as_bytes()))
      .unwrap_or(UTF_8);

    Ok(decode_classic_script_bytes(
      &resource.bytes,
      resource.content_type.as_deref(),
      fallback_encoding,
    ))
  }
}

fn reset_event_loop_for_navigation(
  event_loop: &mut EventLoop<BrowserTabHost>,
  trace: TraceHandle,
  queue_limits: crate::js::QueueLimits,
) {
  event_loop.reset_for_navigation(trace, queue_limits);
}

impl CurrentScriptHost for BrowserTabHost {
  fn current_script_state(&self) -> &CurrentScriptStateHandle {
    &self.current_script
  }
}

impl DomHost for BrowserTabHost {
  fn with_dom<R, F>(&self, f: F) -> R
  where
    F: FnOnce(&Document) -> R,
  {
    <BrowserDocumentDom2 as DomHost>::with_dom(self.document.as_ref(), f)
  }

  fn mutate_dom<R, F>(&mut self, f: F) -> R
  where
    F: FnOnce(&mut Document) -> (R, bool),
  {
    <BrowserDocumentDom2 as DomHost>::mutate_dom(self.document.as_mut(), f)
  }
}

impl DocumentLifecycleHost for BrowserTabHost {
  fn with_dom_mut<R>(&mut self, f: impl FnOnce(&mut crate::dom2::Document) -> R) -> Result<R> {
    let dom = self.document.dom_mut();
    Ok(f(dom))
  }

  fn notify_parsing_completed(&mut self, event_loop: &mut EventLoop<Self>) -> Result<()>
  where
    Self: Sized + 'static,
  {
    // Mirror the default `DocumentLifecycleHost::notify_parsing_completed` implementation, but gate
    // the synchronous microtask checkpoint on the JS execution context stack being empty.
    //
    // BrowserTab's streaming parser can run re-entrantly (e.g. future `document.write`) outside an
    // event-loop task turn, so `EventLoop::currently_running_task()` alone is not sufficient to
    // decide whether it's safe to drain microtasks.
    let ready_state_changed = self.with_dom_mut(|dom| {
      if dom.ready_state() == DocumentReadyState::Loading {
        dom.set_ready_state(DocumentReadyState::Interactive);
        true
      } else {
        false
      }
    })?;

    if ready_state_changed {
      // Fire `readystatechange` whenever `document.readyState` changes.
      let mut event = Event::new(
        "readystatechange",
        EventInit {
          bubbles: false,
          cancelable: false,
          composed: false,
        },
      );
      event.is_trusted = true;
      with_event_loop(event_loop, || self.dispatch_lifecycle_event(EventTargetId::Document, event))?;
    }

    self.document_lifecycle_mut().parsing_completed(event_loop)?;

    // If parsing completion is signalled from outside an event-loop task turn, perform a microtask
    // checkpoint immediately *only* when we are not currently executing JS.
    if event_loop.currently_running_task().is_none() && self.js_execution_depth.get() == 0 {
      event_loop.perform_microtask_checkpoint(self)?;
    }

    Ok(())
  }

  fn dispatch_lifecycle_event(
    &mut self,
    target: crate::web::events::EventTargetId,
    mut event: crate::web::events::Event,
  ) -> Result<()> {
    let target = target.normalize();
    let result = match target {
      EventTargetId::Document | EventTargetId::Window => {
        let (executor, document) = (&mut self.executor, &mut self.document);
        executor.dispatch_lifecycle_event(target, &event, document.as_mut())
      }
      // Fall back to Rust-side dispatch for non-document/window targets (e.g. `<script>` element
      // `load`/`error` events queued by the script scheduler).
      EventTargetId::Node(_) | EventTargetId::Opaque(_) => {
        return self.dispatch_dom_event(target, event).map(|_| ());
      }
    };
    if let Some(req) = self.executor.take_navigation_request() {
      self.pending_navigation = Some(req);
      self.pending_navigation_deadline =
        crate::render_control::root_deadline().filter(|deadline| deadline.is_enabled());
    }
    match result {
      Ok(()) => {
        if self.pending_navigation.is_some() {
          return Ok(());
        }
      }
      Err(err) => {
        if self.pending_navigation.is_some() {
          return Ok(());
        }
        return Err(err);
      }
    }

    let dom: &crate::dom2::Document = self.document.dom();
    self
      .js_events
      .dispatch_dom_event(dom, target, &mut event)
      .map(|_default_not_prevented| ())
  }

  fn document_lifecycle_mut(&mut self) -> &mut DocumentLifecycle {
    &mut self.lifecycle
  }
}

impl crate::js::window_realm::WindowRealmHost for BrowserTabHost {
  fn vm_host_and_window_realm(&mut self) -> (&mut dyn vm_js::VmHost, &mut crate::js::WindowRealm) {
    let BrowserTabHost { document, executor, .. } = self;
    let Some(realm) = executor.window_realm_mut() else {
      panic!("BrowserTabHost does not have an active vm-js WindowRealm for timer/microtask callbacks");
    };
    (document.as_mut(), realm)
  }

  fn webidl_bindings_host(&mut self) -> Option<&mut dyn webidl_vm_js::WebIdlBindingsHost> {
    Some(self.webidl_bindings_host.as_mut())
  }
}

impl crate::js::html_script_pipeline::ScriptElementEventHost for BrowserTabHost {
  fn dispatch_script_element_event(&mut self, script: NodeId, event_name: &'static str) -> Result<()> {
    // HTML "fire an event" for `<script>` load/error is an element task on the DOM manipulation
    // task source. The task itself performs event dispatch synchronously.
    let mut event = Event::new(
      event_name,
      EventInit {
        bubbles: false,
        cancelable: false,
        composed: false,
      },
    );
    event.is_trusted = true;
    let _default_not_prevented = self.dispatch_dom_event(EventTargetId::Node(script).normalize(), event)?;
    Ok(())
  }
}

pub struct BrowserTab {
  trace: TraceHandle,
  trace_output: Option<PathBuf>,
  diagnostics: Option<super::SharedRenderDiagnostics>,
  host: BrowserTabHost,
  event_loop: EventLoop<BrowserTabHost>,
  pending_frame: Option<Pixmap>,
  history: TabHistory,
}

impl BrowserTab {
  pub fn from_html_with_vmjs_executor(html: &str, options: RenderOptions) -> Result<Self> {
    Self::from_html(html, options, super::VmJsBrowserTabExecutor::default())
  }

  /// Creates a new tab from an HTML string with JavaScript enabled via the production `vm-js`
  /// executor.
  pub fn from_html_with_vmjs(html: &str, options: RenderOptions) -> Result<Self> {
    Self::from_html_with_vmjs_and_js_execution_options(html, options, JsExecutionOptions::default())
  }

  /// Like [`BrowserTab::from_html_with_vmjs`], but supplies a document URL hint for base URL
  /// resolution and referrer/origin semantics.
  pub fn from_html_with_vmjs_and_document_url(
    html: &str,
    document_url: &str,
    options: RenderOptions,
  ) -> Result<Self> {
    Self::from_html_with_vmjs_and_document_url_and_js_execution_options(
      html,
      document_url,
      options,
      JsExecutionOptions::default(),
    )
  }

  /// Like [`BrowserTab::from_html_with_vmjs`], but allows overriding JavaScript execution budgets.
  pub fn from_html_with_vmjs_and_js_execution_options(
    html: &str,
    options: RenderOptions,
    js_execution_options: JsExecutionOptions,
  ) -> Result<Self> {
    Self::from_html_with_js_execution_options(
      html,
      options,
      super::VmJsBrowserTabExecutor::default(),
      js_execution_options,
    )
  }

  /// Like [`BrowserTab::from_html_with_vmjs_and_document_url`], but allows overriding JavaScript
  /// execution budgets.
  pub fn from_html_with_vmjs_and_document_url_and_js_execution_options(
    html: &str,
    document_url: &str,
    options: RenderOptions,
    js_execution_options: JsExecutionOptions,
  ) -> Result<Self> {
    // This mirrors `from_html_with_document_url_and_fetcher_and_js_execution_options`, but wires
    // the production vm-js executor by default.
    let trace_session = super::TraceSession::from_options(Some(&options));
    let trace_handle = trace_session.handle.clone();
    let trace_output = trace_session.output.clone();

    // Parse-time script execution requires a script-aware streaming parser driver. Start the tab
    // with an empty DOM and then stream-parse the provided HTML, pausing at `</script>` boundaries.
    let diagnostics = (!matches!(options.diagnostics_level, super::DiagnosticsLevel::None))
      .then(super::SharedRenderDiagnostics::new);
    let renderer = super::FastRender::builder()
      .dom_scripting_enabled(true)
      .build()?;
    let mut document = BrowserDocumentDom2::new(renderer, "", options.clone())?;
    if let Some(diag) = diagnostics.as_ref() {
      document
        .renderer_mut()
        .set_diagnostics_sink(Some(Arc::clone(&diag.inner)));
    }
    let host = BrowserTabHost::new(
      document,
      Box::new(super::VmJsBrowserTabExecutor::default()),
      trace_handle.clone(),
      js_execution_options,
    )?;
    let mut event_loop = EventLoop::new();
    event_loop.set_trace_handle(trace_handle.clone());
    event_loop.set_queue_limits(js_execution_options.event_loop_queue_limits);

    let mut tab = Self {
      trace: trace_handle,
      trace_output,
      diagnostics,
      host,
      event_loop,
      pending_frame: None,
      history: TabHistory::new(),
    };

    // Configure the renderer's document URL hint up-front so any non-script fetches (stylesheets,
    // images, etc) see consistent referrer/origin context during parsing.
    tab
      .host
      .document
      .renderer_mut()
      .set_document_url(document_url);

    let document_referrer_policy =
      crate::html::referrer_policy::extract_referrer_policy_from_html(html).unwrap_or_default();
    tab
      .host
      .reset_scripting_state(Some(document_url.to_string()), document_referrer_policy)?;
    let base_url =
      tab.parse_html_streaming_and_schedule_scripts(html, Some(document_url), &options)?;
    if let Some(req) = tab.host.pending_navigation.take() {
      tab.navigate_to_url_with_replace(&req.url, options.clone(), req.replace)?;
    } else {
      let renderer = tab.host.document.renderer_mut();
      match base_url {
        Some(url) => renderer.set_base_url(url),
        None => renderer.clear_base_url(),
      }
    }
    Ok(tab)
  }

  /// Like [`BrowserTab::from_html_with_vmjs_and_document_url`], but uses the provided
  /// [`ResourceFetcher`] for subresource/script/fetch() loads.
  pub fn from_html_with_vmjs_and_document_url_and_fetcher(
    html: &str,
    document_url: &str,
    options: RenderOptions,
    fetcher: Arc<dyn ResourceFetcher>,
  ) -> Result<Self> {
    Self::from_html_with_vmjs_and_document_url_and_fetcher_and_js_execution_options(
      html,
      document_url,
      options,
      fetcher,
      JsExecutionOptions::default(),
    )
  }

  /// Like [`BrowserTab::from_html_with_vmjs_and_document_url_and_fetcher`], but allows overriding
  /// JavaScript execution budgets.
  pub fn from_html_with_vmjs_and_document_url_and_fetcher_and_js_execution_options(
    html: &str,
    document_url: &str,
    options: RenderOptions,
    fetcher: Arc<dyn ResourceFetcher>,
    js_execution_options: JsExecutionOptions,
  ) -> Result<Self> {
    Self::from_html_with_document_url_and_fetcher_and_js_execution_options(
      html,
      document_url,
      options,
      super::VmJsBrowserTabExecutor::default(),
      fetcher,
      js_execution_options,
    )
  }

  /// Creates a new vm-js-backed tab and navigates it to `url`.
  pub fn from_url_with_vmjs(url: &str, options: RenderOptions) -> Result<Self> {
    Self::from_url_with_vmjs_and_js_execution_options(url, options, JsExecutionOptions::default())
  }

  /// Like [`BrowserTab::from_url_with_vmjs`], but allows overriding JavaScript execution budgets.
  pub fn from_url_with_vmjs_and_js_execution_options(
    url: &str,
    options: RenderOptions,
    js_execution_options: JsExecutionOptions,
  ) -> Result<Self> {
    let mut tab = Self::from_html_with_vmjs_and_js_execution_options(
      "",
      options.clone(),
      js_execution_options,
    )?;
    tab.navigate_to_url(url, options)?;
    Ok(tab)
  }

  fn parse_html_streaming_and_schedule_scripts(
    &mut self,
    html: &str,
    document_url: Option<&str>,
    options: &RenderOptions,
  ) -> Result<Option<String>> {
    self.host.document_url = document_url.map(|url| url.to_string());
    self.host.update_stylesheet_media_context(options);
    // Seed/extend the CSP policy before any scripts execute. This is a bounded scan of the document
    // head and keeps behavior deterministic for large HTML strings.
    if let Some(meta_csp) = crate::html::content_security_policy::extract_csp_from_html(html) {
      match self.host.csp.as_mut() {
        Some(existing) => {
          existing.extend(meta_csp);
        }
        None => {
          self.host.csp = Some(meta_csp);
        }
      }
    }
    // `StreamingHtmlParser` cooperatively checks any *active* render deadline via
    // `check_active_periodic`, but it does not accept `RenderOptions` directly. Store the deadline
    // in the streaming parse state so it remains active if parsing is resumed via event-loop tasks
    // (e.g. when parser-blocking scripts are delayed by script-blocking stylesheets).
    // Prefer the existing root deadline (if enabled) so resumed parse tasks inherit the same start
    // instant across nested callers.
    let deadline = crate::render_control::root_deadline()
      .filter(|deadline| deadline.is_enabled())
      .or_else(|| {
        (options.timeout.is_some() || options.cancel_callback.is_some()).then(|| {
          RenderDeadline::new(options.timeout, options.cancel_callback.clone())
        })
      });

    self.host.streaming_parse = Some(StreamingParseState {
      parser: StreamingHtmlParser::new(document_url),
      input: html.to_string(),
      input_offset: 0,
      eof_set: false,
      deadline,
      parse_task_scheduled: false,
      resume_task_scheduled: false,
      host_snapshot_committed: false,
      last_synced_host_dom_generation: 0,
    });
    self.host.streaming_parse_active = true;
    self.host.document_write_state.set_parsing_active(true);

    let should_continue = self.host.parse_until_blocked(&mut self.event_loop)?;
    if should_continue {
      self.host.queue_parse_resume_task(&mut self.event_loop)?;
    }
    let base_url = if let Some(state) = self.host.streaming_parse.as_ref() {
      state.parser.current_base_url()
    } else {
      self.host.base_url.clone()
    };
    Ok(base_url)
  }

  pub fn from_html<E>(html: &str, options: RenderOptions, executor: E) -> Result<Self>
  where
    E: BrowserTabJsExecutor + 'static,
  {
    Self::from_html_with_js_execution_options(html, options, executor, JsExecutionOptions::default())
  }

  pub fn from_html_with_event_loop<E>(
    html: &str,
    options: RenderOptions,
    executor: E,
    event_loop: EventLoop<BrowserTabHost>,
  ) -> Result<Self>
  where
    E: BrowserTabJsExecutor + 'static,
  {
    Self::from_html_with_event_loop_and_js_execution_options(
      html,
      options,
      executor,
      event_loop,
      JsExecutionOptions::default(),
    )
  }

  pub fn from_html_with_js_execution_options<E>(
    html: &str,
    options: RenderOptions,
    executor: E,
    js_execution_options: JsExecutionOptions,
  ) -> Result<Self>
  where
    E: BrowserTabJsExecutor + 'static,
  {
    let trace_session = super::TraceSession::from_options(Some(&options));
    let trace_handle = trace_session.handle.clone();
    let trace_output = trace_session.output.clone();

    // Parse-time script execution requires a script-aware streaming parser driver. Start the tab
    // with an empty DOM and then stream-parse the provided HTML, pausing at `</script>` boundaries.
    let diagnostics = (!matches!(options.diagnostics_level, super::DiagnosticsLevel::None))
      .then(super::SharedRenderDiagnostics::new);
    let external_script_sources: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));
    let inner_fetcher: Arc<dyn ResourceFetcher> = Arc::new(crate::resource::HttpFetcher::new());
    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(ScriptSourceOverrideFetcher {
      overrides: Arc::clone(&external_script_sources),
      inner: inner_fetcher,
    });
    let renderer = super::FastRender::builder()
      .dom_scripting_enabled(true)
      .fetcher(fetcher)
      .build()?;
    let mut document = BrowserDocumentDom2::new(renderer, "", options.clone())?;
    if let Some(diag) = diagnostics.as_ref() {
      document
        .renderer_mut()
        .set_diagnostics_sink(Some(Arc::clone(&diag.inner)));
    }
    let mut host = BrowserTabHost::new(
      document,
      Box::new(executor),
      trace_handle.clone(),
      js_execution_options,
    )?;
    host.external_script_sources = Arc::clone(&external_script_sources);
    let mut event_loop = EventLoop::new();
    event_loop.set_trace_handle(trace_handle.clone());
    event_loop.set_queue_limits(js_execution_options.event_loop_queue_limits);

    let mut tab = Self {
      trace: trace_handle,
      trace_output,
      diagnostics,
      host,
      event_loop,
      pending_frame: None,
      history: TabHistory::new(),
    };
    let options_for_parse = options.clone();
    // Install a root deadline so `RenderOptions::{timeout,cancel_callback}` bounds:
    // - HTML parsing,
    // - and any synchronous navigation requests triggered by scripts during parsing.
    let deadline_enabled =
      options_for_parse.timeout.is_some() || options_for_parse.cancel_callback.is_some();
    let root_deadline_is_enabled =
      crate::render_control::root_deadline().is_some_and(|deadline| deadline.is_enabled());
    let deadline = (deadline_enabled && !root_deadline_is_enabled).then(|| {
      RenderDeadline::new(options_for_parse.timeout, options_for_parse.cancel_callback.clone())
    });
    let _deadline_guard = deadline
      .as_ref()
      .map(|deadline| DeadlineGuard::install(Some(deadline)));
    let document_referrer_policy =
      crate::html::referrer_policy::extract_referrer_policy_from_html(html).unwrap_or_default();
    tab.host.reset_scripting_state(None, document_referrer_policy)?;
    let mut parse_options = options_for_parse.clone();
    if root_deadline_is_enabled {
      // Avoid installing a nested deadline: the outer root deadline already enforces the render
      // budget across parsing + any follow-up navigation committed from scripts.
      parse_options.timeout = None;
      parse_options.cancel_callback = None;
    }
    let base_url = tab.parse_html_streaming_and_schedule_scripts(html, None, &parse_options)?;
    if let Some(req) = tab.host.pending_navigation.take() {
      tab.navigate_to_url_with_replace(&req.url, options_for_parse.clone(), req.replace)?;
    } else if tab.host.streaming_parse.is_none() {
      let renderer = tab.host.document.renderer_mut();
      match base_url {
        Some(url) => renderer.set_base_url(url),
        None => renderer.clear_base_url(),
      }
    }
    Ok(tab)
  }

  /// Construct a `BrowserTab` from a pre-built renderer instance with JavaScript enabled via the
  /// production `vm-js` executor.
  ///
  /// This is primarily used by CLI tools and other embeddings that want to configure the renderer
  /// (fetcher, runtime toggles, etc) before enabling JS execution via `BrowserTab`, without having
  /// to manually instantiate a `VmJsBrowserTabExecutor`.
  pub fn with_renderer_and_vmjs(renderer: super::FastRender, options: RenderOptions) -> Result<Self> {
    Self::with_renderer_and_vmjs_and_js_execution_options(renderer, options, JsExecutionOptions::default())
  }

  /// Like [`BrowserTab::with_renderer_and_vmjs`], but allows overriding JavaScript execution budgets.
  pub fn with_renderer_and_vmjs_and_js_execution_options(
    renderer: super::FastRender,
    options: RenderOptions,
    js_execution_options: JsExecutionOptions,
  ) -> Result<Self> {
    Self::with_renderer_and_js_execution_options(
      renderer,
      options,
      super::VmJsBrowserTabExecutor::default(),
      js_execution_options,
    )
  }

  /// Construct a `BrowserTab` from a pre-built renderer instance.
  ///
  /// This is primarily used by CLI tools and other embeddings that want to configure the renderer
  /// (fetcher, runtime toggles, etc) before enabling JS execution via `BrowserTab`.
  pub fn with_renderer_and_js_execution_options<E>(
    renderer: super::FastRender,
    options: RenderOptions,
    executor: E,
    js_execution_options: JsExecutionOptions,
  ) -> Result<Self>
  where
    E: BrowserTabJsExecutor + 'static,
  {
    let trace_session = super::TraceSession::from_options(Some(&options));
    let trace_handle = trace_session.handle.clone();
    let trace_output = trace_session.output.clone();
    let diagnostics = (!matches!(options.diagnostics_level, super::DiagnosticsLevel::None))
      .then(super::SharedRenderDiagnostics::new);

    let mut document = BrowserDocumentDom2::new(renderer, "", options.clone())?;
    if let Some(diag) = diagnostics.as_ref() {
      document
        .renderer_mut()
        .set_diagnostics_sink(Some(Arc::clone(&diag.inner)));
    }
    let host = BrowserTabHost::new(
      document,
      Box::new(executor),
      trace_handle.clone(),
      js_execution_options,
    )?;
    let mut event_loop = EventLoop::new();
    event_loop.set_trace_handle(trace_handle.clone());
    event_loop.set_queue_limits(js_execution_options.event_loop_queue_limits);

    Ok(Self {
      trace: trace_handle,
      trace_output,
      diagnostics,
      host,
      event_loop,
      pending_frame: None,
      history: TabHistory::new(),
    })
  }

  pub fn from_html_with_document_url_and_fetcher<E>(
    html: &str,
    document_url: &str,
    options: RenderOptions,
    executor: E,
    fetcher: Arc<dyn ResourceFetcher>,
  ) -> Result<Self>
  where
    E: BrowserTabJsExecutor + 'static,
  {
    Self::from_html_with_document_url_and_fetcher_and_js_execution_options(
      html,
      document_url,
      options,
      executor,
      fetcher,
      JsExecutionOptions::default(),
    )
  }

  pub fn from_html_with_document_url_and_fetcher_and_js_execution_options<E>(
    html: &str,
    document_url: &str,
    options: RenderOptions,
    executor: E,
    fetcher: Arc<dyn ResourceFetcher>,
    js_execution_options: JsExecutionOptions,
  ) -> Result<Self>
  where
    E: BrowserTabJsExecutor + 'static,
  {
    let trace_session = super::TraceSession::from_options(Some(&options));
    let trace_handle = trace_session.handle.clone();
    let trace_output = trace_session.output.clone();

    // Parse-time script execution requires a script-aware streaming parser driver. Start the tab
    // with an empty DOM and then stream-parse the provided HTML, pausing at `</script>` boundaries.
    let diagnostics = (!matches!(options.diagnostics_level, super::DiagnosticsLevel::None))
      .then(super::SharedRenderDiagnostics::new);
    let external_script_sources: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));
    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(ScriptSourceOverrideFetcher {
      overrides: Arc::clone(&external_script_sources),
      inner: fetcher,
    });
    let renderer = super::FastRender::builder()
      .dom_scripting_enabled(true)
      .fetcher(fetcher)
      .build()?;
    let mut document = BrowserDocumentDom2::new(renderer, "", options.clone())?;
    if let Some(diag) = diagnostics.as_ref() {
      document
        .renderer_mut()
        .set_diagnostics_sink(Some(Arc::clone(&diag.inner)));
    }
    let mut host = BrowserTabHost::new(
      document,
      Box::new(executor),
      trace_handle.clone(),
      js_execution_options,
    )?;
    host.external_script_sources = Arc::clone(&external_script_sources);
    let mut event_loop = EventLoop::new();
    event_loop.set_trace_handle(trace_handle.clone());
    event_loop.set_queue_limits(js_execution_options.event_loop_queue_limits);

    let mut tab = Self {
      trace: trace_handle,
      trace_output,
      diagnostics,
      host,
      event_loop,
      pending_frame: None,
      history: TabHistory::new(),
    };

    // Configure the renderer's document URL hint up-front so any non-script fetches (stylesheets,
    // images, etc) see consistent referrer/origin context during parsing.
    tab.host.document.renderer_mut().set_document_url(document_url);

    let options_for_parse = options.clone();
    // Install a root deadline so `RenderOptions::{timeout,cancel_callback}` bounds:
    // - HTML parsing,
    // - and any synchronous navigation requests triggered by scripts during parsing.
    let deadline_enabled =
      options_for_parse.timeout.is_some() || options_for_parse.cancel_callback.is_some();
    let root_deadline_is_enabled =
      crate::render_control::root_deadline().is_some_and(|deadline| deadline.is_enabled());
    let deadline = (deadline_enabled && !root_deadline_is_enabled).then(|| {
      RenderDeadline::new(options_for_parse.timeout, options_for_parse.cancel_callback.clone())
    });
    let _deadline_guard = deadline
      .as_ref()
      .map(|deadline| DeadlineGuard::install(Some(deadline)));

    let document_referrer_policy =
      crate::html::referrer_policy::extract_referrer_policy_from_html(html).unwrap_or_default();
    tab
      .host
      .reset_scripting_state(Some(document_url.to_string()), document_referrer_policy)?;
    let mut parse_options = options_for_parse.clone();
    if root_deadline_is_enabled {
      // Avoid installing a nested deadline: the outer root deadline already enforces the render
      // budget across parsing + any follow-up navigation committed from scripts.
      parse_options.timeout = None;
      parse_options.cancel_callback = None;
    }
    let base_url = tab.parse_html_streaming_and_schedule_scripts(
      html,
      Some(document_url),
      &parse_options,
    )?;
    if let Some(req) = tab.host.pending_navigation.take() {
      tab.navigate_to_url_with_replace(&req.url, options_for_parse.clone(), req.replace)?;
    } else {
      let renderer = tab.host.document.renderer_mut();
      match base_url {
        Some(url) => renderer.set_base_url(url),
        None => renderer.clear_base_url(),
      }
    }
    Ok(tab)
  }

  pub fn from_html_with_event_loop_and_js_execution_options<E>(
    html: &str,
    options: RenderOptions,
    executor: E,
    mut event_loop: EventLoop<BrowserTabHost>,
    js_execution_options: JsExecutionOptions,
  ) -> Result<Self>
  where
    E: BrowserTabJsExecutor + 'static,
  {
    let trace_session = super::TraceSession::from_options(Some(&options));
    let trace_handle = trace_session.handle.clone();
    let trace_output = trace_session.output.clone();

    let diagnostics = (!matches!(options.diagnostics_level, super::DiagnosticsLevel::None))
      .then(super::SharedRenderDiagnostics::new);

    // Parse-time script execution requires a script-aware streaming parser driver. Start the tab
    // with an empty DOM and then stream-parse the provided HTML, pausing at `</script>` boundaries.
    let external_script_sources: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));
    let inner_fetcher: Arc<dyn ResourceFetcher> = Arc::new(crate::resource::HttpFetcher::new());
    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(ScriptSourceOverrideFetcher {
      overrides: Arc::clone(&external_script_sources),
      inner: inner_fetcher,
    });
    let renderer = super::FastRender::builder()
      .dom_scripting_enabled(true)
      .fetcher(fetcher)
      .build()?;
    let mut document = BrowserDocumentDom2::new(renderer, "", options.clone())?;
    if let Some(diag) = diagnostics.as_ref() {
      document
        .renderer_mut()
        .set_diagnostics_sink(Some(Arc::clone(&diag.inner)));
    }
    let mut host = BrowserTabHost::new(
      document,
      Box::new(executor),
      trace_handle.clone(),
      js_execution_options,
    )?;
    host.external_script_sources = Arc::clone(&external_script_sources);
    event_loop.set_trace_handle(trace_handle.clone());
    event_loop.set_queue_limits(js_execution_options.event_loop_queue_limits);

    let mut tab = Self {
      trace: trace_handle,
      trace_output,
      diagnostics,
      host,
      event_loop,
      pending_frame: None,
      history: TabHistory::new(),
    };
    let options_for_parse = options.clone();
    // Install a root deadline so `RenderOptions::{timeout,cancel_callback}` bounds:
    // - HTML parsing,
    // - and any synchronous navigation requests triggered by scripts during parsing.
    let deadline_enabled =
      options_for_parse.timeout.is_some() || options_for_parse.cancel_callback.is_some();
    let root_deadline_is_enabled =
      crate::render_control::root_deadline().is_some_and(|deadline| deadline.is_enabled());
    let deadline = (deadline_enabled && !root_deadline_is_enabled).then(|| {
      RenderDeadline::new(options_for_parse.timeout, options_for_parse.cancel_callback.clone())
    });
    let _deadline_guard = deadline
      .as_ref()
      .map(|deadline| DeadlineGuard::install(Some(deadline)));
    let document_referrer_policy =
      crate::html::referrer_policy::extract_referrer_policy_from_html(html).unwrap_or_default();
    tab.host.reset_scripting_state(None, document_referrer_policy)?;
    let mut parse_options = options_for_parse.clone();
    if root_deadline_is_enabled {
      // Avoid installing a nested deadline: the outer root deadline already enforces the render
      // budget across parsing + any follow-up navigation committed from scripts.
      parse_options.timeout = None;
      parse_options.cancel_callback = None;
    }
    let base_url = tab.parse_html_streaming_and_schedule_scripts(html, None, &parse_options)?;
    if let Some(req) = tab.host.pending_navigation.take() {
      tab.navigate_to_url_with_replace(&req.url, options_for_parse.clone(), req.replace)?;
    } else {
      let renderer = tab.host.document.renderer_mut();
      match base_url {
        Some(url) => renderer.set_base_url(url),
        None => renderer.clear_base_url(),
      }
    }
    Ok(tab)
  }

  pub fn register_script_source(&mut self, url: impl Into<String>, source: impl Into<String>) {
    self
      .host
      .register_external_script_source(url.into(), source.into());
  }

  /// Register an in-memory HTML payload that can be navigated to by URL (including via
  /// `window.location`-driven navigations).
  pub fn register_html_source(&mut self, url: impl Into<String>, html: impl Into<String>) {
    self
      .host
      .register_html_source(url.into(), html.into());
  }

  pub fn set_event_listener_invoker(
    &mut self,
    invoker: Box<dyn crate::web::events::EventListenerInvoker>,
  ) {
    self.host.set_event_invoker(invoker);
  }

  pub fn write_trace(&self) -> Result<()> {
    let Some(path) = self.trace_output.as_deref() else {
      return Ok(());
    };
    self.trace.write_chrome_trace(path).map_err(Error::Io)
  }

  pub fn diagnostics_snapshot(&self) -> Option<super::RenderDiagnostics> {
    self.diagnostics.as_ref().map(|diag| diag.clone().into_inner())
  }

  pub fn navigate_to_html(&mut self, html: &str, options: RenderOptions) -> Result<()> {
    // Navigations replace the current document; any pending frame is no longer relevant.
    self.pending_frame = None;
    let trace_session = super::TraceSession::from_options(Some(&options));
    self.trace = trace_session.handle.clone();
    self.trace_output = trace_session.output.clone();

    if let Some(diag) = self.diagnostics.as_ref() {
      if let Ok(mut guard) = diag.inner.lock() {
        *guard = super::RenderDiagnostics::default();
      }
    }

    let options_for_parse = options.clone();
    // Install a root deadline so `RenderOptions::{timeout,cancel_callback}` bounds:
    // - HTML parsing,
    // - and any synchronous navigation requests triggered by scripts during parsing.
    let deadline_enabled =
      options_for_parse.timeout.is_some() || options_for_parse.cancel_callback.is_some();
    let root_deadline_is_enabled =
      crate::render_control::root_deadline().is_some_and(|deadline| deadline.is_enabled());
    let deadline = (deadline_enabled && !root_deadline_is_enabled).then(|| {
      RenderDeadline::new(options_for_parse.timeout, options_for_parse.cancel_callback.clone())
    });
    let _deadline_guard = deadline
      .as_ref()
      .map(|deadline| DeadlineGuard::install(Some(deadline)));
    self
      .host
      .document
      .reset_with_dom(Document::new(QuirksMode::NoQuirks), options);
    self.reset_event_loop();
    self.host.trace = self.trace.clone();
    let document_referrer_policy =
      crate::html::referrer_policy::extract_referrer_policy_from_html(html).unwrap_or_default();

    // Clear URL hints so relative resources do not resolve against the previous navigation.
    {
      let renderer = self.host.document.renderer_mut();
      renderer.clear_document_url();
      renderer.clear_base_url();
    }

    self
      .host
      .reset_scripting_state(None, document_referrer_policy)?;
    // Avoid installing a nested deadline: the outer guard already enforces the render budget across
    // parsing + any follow-up navigation committed from scripts.
    let mut parse_options = options_for_parse.clone();
    parse_options.timeout = None;
    parse_options.cancel_callback = None;
    let base_url = self.parse_html_streaming_and_schedule_scripts(html, None, &parse_options)?;
    if let Some(req) = self.host.pending_navigation.take() {
      self.navigate_to_url_with_replace(&req.url, options_for_parse.clone(), req.replace)?;
      return Ok(());
    }

    if self.host.streaming_parse.is_none() {
      // Update the renderer's base URL hint to match the parse-time base URL after processing the
      // full document.
      let renderer = self.host.document.renderer_mut();
      match base_url {
        Some(url) => renderer.set_base_url(url),
        None => renderer.clear_base_url(),
      }
    }

    Ok(())
  }

  pub fn navigate_to_url(&mut self, url: &str, options: RenderOptions) -> Result<()> {
    self.navigate_to_url_with_replace(url, options, /*replace=*/ false)
  }

  fn navigate_to_url_with_replace(
    &mut self,
    url: &str,
    options: RenderOptions,
    mut replace: bool,
  ) -> Result<()> {
    // Navigations replace the current document; any pending frame is no longer relevant.
    self.pending_frame = None;
    let trace_session = super::TraceSession::from_options(Some(&options));
    self.trace = trace_session.handle.clone();
    self.trace_output = trace_session.output.clone();

    if let Some(diag) = self.diagnostics.as_ref() {
      if let Ok(mut guard) = diag.inner.lock() {
        *guard = super::RenderDiagnostics::default();
      }
    }

    // Install a root deadline so `RenderOptions::{timeout,cancel_callback}` bounds:
    // - the document fetch phase,
    // - the subsequent script-aware streaming HTML parse,
    // - and any synchronous navigation requests triggered by scripts (redirect chains).
    //
    // This mirrors `FastRender::prepare_url`'s fetch-time deadline guard, but drives
    // `StreamingHtmlParser` so parser-inserted scripts execute at `</script>` boundaries against a
    // partially-built DOM.
    let deadline_enabled = options.timeout.is_some() || options.cancel_callback.is_some();
    let root_deadline_is_enabled =
      crate::render_control::root_deadline().is_some_and(|deadline| deadline.is_enabled());
    let deadline = (deadline_enabled && !root_deadline_is_enabled)
      .then(|| RenderDeadline::new(options.timeout, options.cancel_callback.clone()));
    let _deadline_guard = deadline
      .as_ref()
      .map(|deadline| DeadlineGuard::install(Some(deadline)));

    let mut target_url = url.to_string();
    // Even when callers pass unbounded `RunLimits`, keep navigations finite so hostile pages cannot
    // loop forever by repeatedly assigning `window.location`.
    const MAX_NAVIGATIONS_PER_CALL: usize = 128;
    let mut navigations_in_call: usize = 0;
    loop {
      navigations_in_call = navigations_in_call.saturating_add(1);
      if navigations_in_call > MAX_NAVIGATIONS_PER_CALL {
        return Err(Error::Other(format!(
          "Navigation loop detected: exceeded {MAX_NAVIGATIONS_PER_CALL} navigations in one navigation chain"
        )));
      }
      // Fetch the document first so a failed request doesn't clobber the existing navigation's
      // committed DOM.
      let (final_url, html, header_csp, document_referrer_policy) =
        if let Some(html) = self.host.html_sources.get(&target_url).cloned() {
          let final_url = target_url.clone();
          let header_csp = None;
          let document_referrer_policy =
            crate::html::referrer_policy::extract_referrer_policy_from_html(&html).unwrap_or_default();
          (final_url, html, header_csp, document_referrer_policy)
        } else {
          let resource = {
            let fetcher = self.host.document.fetcher();
            let mut req = FetchRequest::document(&target_url);
            if let Some(referrer) = self.host.document_url.as_deref() {
              req = req.with_referrer_url(referrer);
            }
            if let Some(origin) = self.host.document_origin.as_ref() {
              req = req.with_client_origin(origin);
            }
            req = req.with_referrer_policy(self.host.document_referrer_policy);
            fetcher.fetch_with_request(req)?
          };
          let header_csp = CspPolicy::from_response_headers(&resource);
          let hint = resource
            .final_url
            .as_deref()
            .unwrap_or_else(|| target_url.as_str());
          let final_url = super::merge_fragment_from_url(hint, target_url.as_str());
          let html = decode_html_bytes(&resource.bytes, resource.content_type.as_deref());

          // The `Referrer-Policy` response header applies as the initial document referrer policy
          // (matching `FastRender::prepare_url`). `<meta name="referrer">` can override it.
          let initial_referrer_policy = resource.response_referrer_policy.unwrap_or_default();
          let document_referrer_policy =
            crate::html::referrer_policy::extract_referrer_policy_from_html(&html)
              .unwrap_or(initial_referrer_policy);
          (final_url, html, header_csp, document_referrer_policy)
        };

      if replace {
        self.history.replace_current_url(final_url.clone());
      } else {
        self.history.push(final_url.clone());
      }

      // Seed navigation URL hints for downstream subresource fetches. Unlike the base URL, the
      // document URL is used for referrer/origin semantics and must remain stable even if `<base
      // href>` changes during parsing.
      {
        let renderer = self.host.document.renderer_mut();
        renderer.set_document_url(final_url.clone());
        renderer.set_base_url(final_url.clone());
      }

      // Replace the document DOM with an empty tree before streaming in the new navigation HTML.
      // This ensures scripts observe a partially-built DOM (not the previous navigation's tree).
      let options_for_parse = options.clone();
      self
        .host
        .document
        .reset_with_dom(Document::new(QuirksMode::NoQuirks), options_for_parse.clone());
      self.reset_event_loop();
      self.host.trace = self.trace.clone();
      self
        .host
        .reset_scripting_state(Some(final_url.clone()), document_referrer_policy)?;
      self.host.csp = header_csp;

      // Avoid installing a nested deadline: the outer guard already enforces the render budget
      // across fetch + parse.
      let mut parse_options = options_for_parse;
      parse_options.timeout = None;
      parse_options.cancel_callback = None;

      let base_url = self.parse_html_streaming_and_schedule_scripts(
        &html,
        Some(final_url.as_str()),
        &parse_options,
      )?;

      if let Some(req) = self.host.pending_navigation.take() {
        target_url = req.url;
        replace = req.replace;
        continue;
      }

      if self.host.streaming_parse.is_none() {
        // Update the renderer's base URL hint to match the parse-time base URL after processing the
        // full document.
        let renderer = self.host.document.renderer_mut();
        match base_url {
          Some(url) => renderer.set_base_url(url),
          None => renderer.clear_base_url(),
        }
      }

      return Ok(());
    }
  }

  fn commit_pending_navigation(&mut self) -> Result<bool> {
    let Some(req) = self.host.pending_navigation.take() else {
      return Ok(false);
    };
    let deadline = self.host.pending_navigation_deadline.take();
    let root_deadline_is_enabled =
      crate::render_control::root_deadline().is_some_and(|deadline| deadline.is_enabled());
    // If the navigation request was produced while a root deadline was active (e.g. during parsing),
    // preserve it across the commit so render timeouts/cancellation cannot be bypassed by resetting
    // the clock at navigation time.
    let _deadline_guard = if root_deadline_is_enabled {
      None
    } else {
      deadline
        .as_ref()
        .filter(|deadline| deadline.is_enabled())
        .map(|deadline| DeadlineGuard::install(Some(deadline)))
    };
    let options = self.host.document.options().clone();
    self.navigate_to_url_with_replace(&req.url, options, req.replace)?;
    Ok(true)
  }

  fn run_event_loop_until_idle_handling_errors_with_pending_navigation_abort(
    &mut self,
    limits: RunLimits,
    render_between_turns: bool,
    mut on_error: impl FnMut(Error),
    mut on_render: impl FnMut(),
  ) -> Result<RunUntilIdleOutcome> {
    {
      // Ensure scripts inserted outside the event loop (e.g. via host DOM mutations) are detected
      // before we decide the loop is idle.
      let (host, event_loop) = (&mut self.host, &mut self.event_loop);
      host.discover_dynamic_scripts(event_loop)?;
    }
    let pending_frame = &mut self.pending_frame;
    self.event_loop.run_until_idle_handling_errors_with_hook(
      &mut self.host,
      limits,
      &mut on_error,
      |host, event_loop| -> Result<()> {
        if host.pending_navigation.is_some() {
          // Abort the current document's task processing immediately; the embedding will commit the
          // navigation synchronously (clearing all outstanding work for the old document).
          event_loop.clear_all_pending_work();
          return Ok(());
        }
        if render_between_turns && host.document.is_dirty() {
          if let Some(frame) = host.document.render_if_needed()? {
            *pending_frame = Some(frame);
            on_render();
          }
        }
        host.discover_dynamic_scripts(event_loop)?;
        Ok(())
      },
    )
  }

  /// Dispatch a trusted `click` DOM event to `node_id`.
  ///
  /// Returns `true` when the event's default was **not** prevented.
  pub fn dispatch_click_event(&mut self, node_id: NodeId) -> Result<bool> {
    let mut event = Event::new(
      "click",
      EventInit {
        bubbles: true,
        cancelable: true,
        composed: false,
      },
    );
    event.is_trusted = true;
    // Install the tab's event loop in TLS so JS Web APIs like `setTimeout` can schedule tasks
    // during event listener invocation.
    let (host, event_loop) = (&mut self.host, &mut self.event_loop);
    with_event_loop(event_loop, || {
      host.dispatch_dom_event(EventTargetId::Node(node_id).normalize(), event)
    })
  }

  /// Dispatch a trusted `submit` DOM event to `node_id`.
  ///
  /// Returns `true` when the event's default was **not** prevented.
  pub fn dispatch_submit_event(&mut self, node_id: NodeId) -> Result<bool> {
    let mut event = Event::new(
      "submit",
      EventInit {
        bubbles: true,
        cancelable: true,
        composed: false,
      },
    );
    event.is_trusted = true;
    // Install the tab's event loop in TLS so JS Web APIs like `setTimeout` can schedule tasks
    // during event listener invocation.
    let (host, event_loop) = (&mut self.host, &mut self.event_loop);
    with_event_loop(event_loop, || {
      host.dispatch_dom_event(EventTargetId::Node(node_id).normalize(), event)
    })
  }

  /// Simulate a user click on `node_id` and return the resolved navigation target URL if the
  /// element's default click action should navigate.
  ///
  /// This:
  /// - dispatches a trusted, bubbling, cancelable `"click"` event at `node_id`, and
  /// - if the click is on (or inside) an `<a href=...>` element, returns the resolved `href` **only
  ///   when** the click event's default was not prevented.
  pub fn resolve_navigation_for_click(&mut self, node_id: NodeId) -> Result<Option<String>> {
    fn trim_ascii_whitespace(value: &str) -> &str {
      value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
    }

    fn is_javascript_url(href: &str) -> bool {
      href
        .as_bytes()
        .get(.."javascript:".len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"javascript:"))
    }

    fn link_href_for_click(dom: &Document, mut current: NodeId) -> Option<String> {
      loop {
        let node = dom.node(current);
        if let NodeKind::Element {
          tag_name,
          namespace,
          ..
        } = &node.kind
        {
          if tag_name.eq_ignore_ascii_case("a") && (namespace.is_empty() || namespace == HTML_NAMESPACE) {
            if let Ok(Some(href)) = dom.get_attribute(current, "href") {
              let href = trim_ascii_whitespace(&href);
              if !href.is_empty() && !is_javascript_url(href) {
                return Some(href.to_string());
              }
            }
          }
        }

        match node.parent {
          Some(parent) => current = parent,
          None => return None,
        }
      }
    }

    fn resolve_href(document_url: Option<&str>, href: &str) -> Option<String> {
      let href = trim_ascii_whitespace(href);
      if href.is_empty() {
        return None;
      }

      if let Ok(url) = url::Url::parse(href) {
        if url.scheme().eq_ignore_ascii_case("javascript") {
          return None;
        }
        return Some(url.to_string());
      }

      let base = document_url.and_then(|u| url::Url::parse(u).ok())?;
      let joined = base.join(href).ok()?;
      if joined.scheme().eq_ignore_ascii_case("javascript") {
        return None;
      }
      Some(joined.to_string())
    }

    let href = link_href_for_click(self.dom(), node_id);
    let default_allowed = self.dispatch_click_event(node_id)?;
    if !default_allowed {
      return Ok(None);
    }
    let Some(href) = href else {
      return Ok(None);
    };
    Ok(resolve_href(self.host.base_url.as_deref(), &href))
  }

  /// Dispatch a user `click` event and, if not canceled, navigate to the clicked link's target.
  ///
  /// Returns `true` if the click triggered a navigation.
  pub fn dispatch_click_and_follow_link(&mut self, node_id: NodeId, options: RenderOptions) -> Result<bool> {
    let Some(url) = self.resolve_navigation_for_click(node_id)? else {
      return Ok(false);
    };
    self.navigate_to_url(&url, options)?;
    Ok(true)
  }

  pub fn run_event_loop_until_idle(&mut self, limits: RunLimits) -> Result<RunUntilIdleOutcome> {
    let trace = self.trace.clone();
    let diagnostics = self.diagnostics.clone();
    let outcome = self.run_event_loop_until_idle_handling_errors_with_pending_navigation_abort(
      limits,
      /*render_between_turns=*/ false,
      move |err| {
        // Match browser behavior: report uncaught task errors but keep the event loop running.
        //
        // NOTE: This method intentionally does *not* render. Callers that want to observe whether
        // rendering is required should follow up with `render_if_needed()` (or use
        // `run_until_stable`).
        let message = err.to_string();
        if let Some(diag) = &diagnostics {
          diag.record_js_exception(message.clone(), None);
        }
        if trace.is_enabled() {
          let mut span = trace.span("js.uncaught_exception", "js");
          span.arg_str("message", &message);
        }
      },
      || {},
    )?;
    if matches!(outcome, RunUntilIdleOutcome::Idle) {
      let _ = self.commit_pending_navigation()?;
    }
    Ok(outcome)
  }

  pub fn js_execution_options(&self) -> JsExecutionOptions {
    self.host.js_execution_options
  }

  pub fn set_js_execution_options(&mut self, options: JsExecutionOptions) {
    self.host.js_execution_options = options;
    self.host.scheduler.set_options(options);
    self.host.document_write_state.update_limits(options);
    self.event_loop.set_queue_limits(options.event_loop_queue_limits);
  }

  pub fn run_until_stable(&mut self, max_frames: usize) -> Result<RunUntilStableOutcome> {
    self.run_until_stable_with_run_limits(self.host.js_execution_options.event_loop_run_limits, max_frames)
  }

  pub fn run_until_stable_with_run_limits(
    &mut self,
    limits: RunLimits,
    max_frames: usize,
  ) -> Result<RunUntilStableOutcome> {
    let mut frames_rendered = 0usize;
    let _ = self.commit_pending_navigation()?;
    if !self.host.document.is_dirty()
      && self.event_loop.is_idle()
      && !self.event_loop.has_pending_animation_frame_callbacks()
    {
      return Ok(RunUntilStableOutcome::Stable { frames_rendered });
    }
    let mut frames_executed = 0usize;
    let trace = self.trace.clone();
    let diagnostics = self.diagnostics.clone();
    let mut report_error = move |err: Error| {
      // Uncaught JS exceptions should not abort event-loop execution (browser behavior). Record
      // them into diagnostics, and into the trace when tracing is enabled.
      let message = err.to_string();
      if let Some(diag) = &diagnostics {
        diag.record_js_exception(message.clone(), None);
      }
      if trace.is_enabled() {
        let mut span = trace.span("js.uncaught_exception", "js");
        span.arg_str("message", &message);
      }
    };

    loop {
      if frames_executed >= max_frames {
        return Ok(RunUntilStableOutcome::Stopped {
          reason: RunUntilStableStopReason::MaxFrames { limit: max_frames },
          frames_rendered,
        });
      }
      frames_executed += 1;

      // Drive event-loop work (tasks/microtasks/timers) first.
      match self.run_event_loop_until_idle_handling_errors_with_pending_navigation_abort(
        limits,
        /*render_between_turns=*/ true,
        &mut report_error,
        || {
          frames_rendered = frames_rendered.saturating_add(1);
        },
      )? {
        RunUntilIdleOutcome::Idle => {}
        RunUntilIdleOutcome::Stopped(RunUntilIdleStopReason::WallTime { .. }) => {
          continue;
        }
        RunUntilIdleOutcome::Stopped(reason) => {
          return Ok(RunUntilStableOutcome::Stopped {
            reason: RunUntilStableStopReason::EventLoop(reason),
            frames_rendered,
          });
        }
      }

      if self.commit_pending_navigation()? {
        // We just replaced the document; restart the stable loop so we drain tasks and render the
        // new document before running rAF callbacks.
        continue;
      }

      let raf_outcome = self
        .event_loop
        .run_animation_frame_handling_errors(&mut self.host, &mut report_error)?;
      if matches!(raf_outcome, RunAnimationFrameOutcome::Ran { .. }) {
        // HTML: microtask checkpoint after rAF callbacks.
        //
        // We run this as a "microtasks only" spin so that:
        // - microtasks queued by rAF are drained immediately,
        // - normal tasks (including timer tasks) are not run until the next loop iteration (after
        //   rendering).
        let microtask_limits = RunLimits {
          max_tasks: 0,
          max_microtasks: limits.max_microtasks,
          max_wall_time: limits.max_wall_time,
        };
        match self.run_event_loop_until_idle_handling_errors_with_pending_navigation_abort(
          microtask_limits,
          /*render_between_turns=*/ false,
          &mut report_error,
          || {
            frames_rendered = frames_rendered.saturating_add(1);
          },
        )? {
          RunUntilIdleOutcome::Idle => {}
          RunUntilIdleOutcome::Stopped(RunUntilIdleStopReason::MaxTasks { .. }) => {
            // Expected: tasks are present, but this checkpoint only drains microtasks.
          }
          RunUntilIdleOutcome::Stopped(reason) => {
            return Ok(RunUntilStableOutcome::Stopped {
              reason: RunUntilStableStopReason::EventLoop(reason),
              frames_rendered,
            });
          }
        }

        // Ensure scripts inserted by rAF callbacks are discovered even when the microtask queue was
        // empty (meaning the microtask-only run stopped at `MaxTasks` without invoking hooks).
        let (host, event_loop) = (&mut self.host, &mut self.event_loop);
        host.discover_dynamic_scripts(event_loop)?;
      }

      if self.commit_pending_navigation()? {
        // Navigation can be requested by rAF callbacks or microtasks drained after the frame.
        // Restart so the new document's tasks run before we attempt to render another frame.
        continue;
      }

      if let Some(frame) = self.host.document.render_if_needed()? {
        self.pending_frame = Some(frame);
        frames_rendered = frames_rendered.saturating_add(1);
      }

      if !self.host.document.is_dirty()
        && self.event_loop.is_idle()
        && !self.event_loop.has_pending_animation_frame_callbacks()
      {
        return Ok(RunUntilStableOutcome::Stable { frames_rendered });
      }
    }
  }

  /// Execute at most one task turn (or a standalone microtask checkpoint) and return a freshly
  /// rendered frame when the document becomes dirty.
  pub fn tick_frame(&mut self) -> Result<Option<Pixmap>> {
    {
      // Ensure dynamically inserted scripts are discovered even if the event loop is currently
      // idle.
      let (host, event_loop) = (&mut self.host, &mut self.event_loop);
      host.discover_dynamic_scripts(event_loop)?;
    }
    let run_limits = self.host.js_execution_options.event_loop_run_limits;
    if self.event_loop.pending_microtask_count() > 0 {
      // Drain microtasks only (HTML microtask checkpoint), but do not run any tasks.
      let microtask_limits = RunLimits {
        max_tasks: 0,
        max_microtasks: run_limits.max_microtasks,
        max_wall_time: run_limits.max_wall_time,
      };
      let trace = self.trace.clone();
      let diagnostics = self.diagnostics.clone();
      match self.event_loop.run_until_idle_handling_errors_with_hook(
        &mut self.host,
        microtask_limits,
        move |err| {
          let message = err.to_string();
          if let Some(diag) = &diagnostics {
            diag.record_js_exception(message.clone(), None);
          }
          if trace.is_enabled() {
            let mut span = trace.span("js.uncaught_exception", "js");
            span.arg_str("message", &message);
          }
        },
        |host, event_loop| host.discover_dynamic_scripts(event_loop),
      )? {
        RunUntilIdleOutcome::Idle
        | RunUntilIdleOutcome::Stopped(RunUntilIdleStopReason::MaxTasks { .. }) => {}
        RunUntilIdleOutcome::Stopped(reason) => {
          return Err(Error::Other(format!(
            "BrowserTab::tick_frame microtask checkpoint stopped: {reason:?}"
          )))
        }
      }
    } else {
      // Run exactly one task turn (a task + its post-task microtask checkpoint).
      let one_task_limits = RunLimits {
        max_tasks: 1,
        max_microtasks: run_limits.max_microtasks,
        max_wall_time: run_limits.max_wall_time,
      };
      let trace = self.trace.clone();
      let diagnostics = self.diagnostics.clone();
      match self.event_loop.run_until_idle_handling_errors_with_hook(
        &mut self.host,
        one_task_limits,
        move |err| {
          let message = err.to_string();
          if let Some(diag) = &diagnostics {
            diag.record_js_exception(message.clone(), None);
          }
          if trace.is_enabled() {
            let mut span = trace.span("js.uncaught_exception", "js");
            span.arg_str("message", &message);
          }
        },
        |host, event_loop| host.discover_dynamic_scripts(event_loop),
      )? {
        RunUntilIdleOutcome::Idle
        | RunUntilIdleOutcome::Stopped(RunUntilIdleStopReason::MaxTasks { .. }) => {}
        RunUntilIdleOutcome::Stopped(reason) => {
          return Err(Error::Other(format!(
            "BrowserTab::tick_frame task turn stopped: {reason:?}"
          )))
        }
      }
    }

    if self.commit_pending_navigation()? {
      // Navigation resets the document/event loop; render the new document if needed.
      return self.render_if_needed();
    }

    self.render_if_needed()
  }

  pub fn render_if_needed(&mut self) -> Result<Option<Pixmap>> {
    // When BrowserTab runs the JS event loop it may have already rendered between tasks to keep the
    // document stable (see `browser_tab_render_interleaving` tests). Those frames are buffered here
    // so callers can still pull the updated pixels via `render_if_needed()`.
    if self.host.document.is_dirty() {
      // Any buffered frame is stale if the document is currently dirty.
      self.pending_frame = None;
      return self.host.document.render_if_needed();
    }

    if let Some(frame) = self.pending_frame.take() {
      return Ok(Some(frame));
    }

    self.host.document.render_if_needed()
  }

  pub fn render_frame(&mut self) -> Result<Pixmap> {
    // A forced render implies the caller will consume the freshest pixels, so drop any queued
    // internal frame.
    self.pending_frame = None;
    self.host.document.render_frame()
  }

  pub fn dom(&self) -> &Document {
    self.host.dom()
  }

  pub fn dom_mut(&mut self) -> &mut Document {
    self.host.dom_mut()
  }

  fn reset_event_loop(&mut self) {
    let queue_limits = self.event_loop.queue_limits();
    reset_event_loop_for_navigation(&mut self.event_loop, self.trace.clone(), queue_limits);
    self
      .event_loop
      .set_queue_limits(self.host.js_execution_options.event_loop_queue_limits);
  }

  /// Notify the tab that the HTML parser discovered a parser-inserted `<script>` element.
  ///
  /// This is the integration point used by the script-aware streaming HTML parser driver
  /// (`StreamingHtmlParser`).
  pub(crate) fn on_parser_discovered_script(
    &mut self,
    node_id: NodeId,
    spec: ScriptElementSpec,
    base_url_at_discovery: Option<String>,
  ) -> Result<ScriptId> {
    self.host.register_and_schedule_script(
      node_id,
      spec,
      base_url_at_discovery,
      &mut self.event_loop,
    )
  }

  /// Notify the tab that HTML parsing completed.
  ///
  /// This allows deferred scripts to be queued once parsing reaches EOF.
  pub(crate) fn on_parsing_completed(&mut self) -> Result<()> {
    let actions = self.host.scheduler.parsing_completed()?;
    self.host.apply_scheduler_actions(actions, &mut self.event_loop)?;
    self.host.notify_parsing_completed(&mut self.event_loop)?;
    Ok(())
  }

  fn discover_and_schedule_scripts(&mut self, document_url: Option<&str>) -> Result<()> {
    let discovered = self.host.discover_scripts_best_effort(document_url);
    for (node_id, spec) in discovered {
      let base_url_at_discovery = spec.base_url.clone();
      self
        .host
        .register_and_schedule_script(node_id, spec, base_url_at_discovery, &mut self.event_loop)?;
      if self.host.pending_navigation.is_some() {
        return Ok(());
      }
    }

    self.on_parsing_completed()
  }
}

fn extract_referrer_policy_from_dom2_document(dom: &Document) -> Option<ReferrerPolicy> {
  fn is_html_namespace(namespace: &str) -> bool {
    namespace.is_empty() || namespace == HTML_NAMESPACE
  }

  let mut stack = vec![dom.root()];
  let mut head: Option<NodeId> = None;
  while let Some(id) = stack.pop() {
    let node = dom.node(id);

    if node.inert_subtree {
      continue;
    }
    if matches!(node.kind, NodeKind::ShadowRoot { .. }) {
      continue;
    }

    if let NodeKind::Element {
      tag_name, namespace, ..
    } = &node.kind
    {
      if tag_name.eq_ignore_ascii_case("head") && is_html_namespace(namespace) {
        head = Some(id);
        break;
      }
    }

    for &child in node.children.iter().rev() {
      stack.push(child);
    }
  }

  let head = head?;
  let mut policy: Option<ReferrerPolicy> = None;

  let mut stack: Vec<(NodeId, bool)> = vec![(head, false)];
  while let Some((id, in_foreign_namespace)) = stack.pop() {
    let node = dom.node(id);

    if node.inert_subtree {
      continue;
    }
    if matches!(node.kind, NodeKind::ShadowRoot { .. }) {
      continue;
    }

    let mut next_in_foreign_namespace = in_foreign_namespace;
    if let NodeKind::Element {
      tag_name, namespace, ..
    } = &node.kind
    {
      next_in_foreign_namespace = in_foreign_namespace || !is_html_namespace(namespace);

      if !in_foreign_namespace && tag_name.eq_ignore_ascii_case("meta") && is_html_namespace(namespace) {
        let name_attr = dom.get_attribute(id, "name").ok().flatten();
        if name_attr
          .map(|name| name.eq_ignore_ascii_case("referrer"))
          .unwrap_or(false)
        {
          let content_attr = dom.get_attribute(id, "content").ok().flatten();
          if let Some(content) = content_attr {
            if let Some(parsed) = ReferrerPolicy::parse_value_list(content) {
              policy = Some(parsed);
            }
          }
        }
      }
    }

    if next_in_foreign_namespace {
      continue;
    }

    for &child in node.children.iter().rev() {
      stack.push((child, next_in_foreign_namespace));
    }
  }

  policy
}

#[cfg(test)]
mod tests {
  use super::*;

  use crate::api::FastRender;
  use crate::js::runtime::with_event_loop;
  use crate::js::window_timers::VmJsEventLoopHooks;
  use crate::js::window_realm::{register_dom_source, unregister_dom_source};
  use crate::js::{WindowRealm, WindowRealmConfig};
  use crate::resource::{FetchedResource, ResourceFetcher};

  use std::cell::{Cell, RefCell};
  use std::collections::HashMap;
  use std::ptr::NonNull;
  use std::rc::Rc;
  use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
  use std::sync::{Arc, Mutex, OnceLock};

  use vm_js::{GcObject, PropertyDescriptor, PropertyKey, PropertyKind, Value, Vm, VmError, VmHost, VmHostHooks};

  use tempfile::tempdir;
  use url::Url;

  use crate::web::events::{AddEventListenerOptions, DomError, EventListenerInvoker, ListenerId};
  use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
  use base64::Engine;
  use sha2::{Digest, Sha256};

  struct RecordingInvoker {
    log: Rc<RefCell<Vec<String>>>,
  }

  impl EventListenerInvoker for RecordingInvoker {
    fn invoke(&mut self, _listener_id: ListenerId, event: &mut Event) -> std::result::Result<(), DomError> {
      assert!(event.is_trusted, "expected host-dispatched events to be trusted");
      self.log.borrow_mut().push(event.type_.clone());
      Ok(())
    }
  }

  struct TestExecutor {
    log: Rc<RefCell<Vec<String>>>,
  }

  impl BrowserTabJsExecutor for TestExecutor {
    fn execute_classic_script(
      &mut self,
      script_text: &str,
      _spec: &ScriptElementSpec,
      _current_script: Option<NodeId>,
      _document: &mut BrowserDocumentDom2,
      event_loop: &mut EventLoop<BrowserTabHost>,
    ) -> Result<()> {
      self
        .log
        .borrow_mut()
        .push(format!("script:{script_text}"));
      let log = Rc::clone(&self.log);
      let name = script_text.to_string();
      event_loop.queue_microtask(move |_host, _event_loop| {
        log.borrow_mut().push(format!("microtask:{name}"));
        Ok(())
      })?;
      Ok(())
    }

    fn execute_module_script(
      &mut self,
      script_text: &str,
      _spec: &ScriptElementSpec,
      _current_script: Option<NodeId>,
      _document: &mut BrowserDocumentDom2,
      event_loop: &mut EventLoop<BrowserTabHost>,
    ) -> Result<()> {
      self
        .log
        .borrow_mut()
        .push(format!("module:{script_text}"));
      let log = Rc::clone(&self.log);
      let name = script_text.to_string();
      event_loop.queue_microtask(move |_host, _event_loop| {
        log.borrow_mut().push(format!("microtask:{name}"));
        Ok(())
      })?;
      Ok(())
    }
  }

  fn build_host_with_options(
    html: &str,
    log: Rc<RefCell<Vec<String>>>,
    js_execution_options: JsExecutionOptions,
  ) -> Result<(BrowserTabHost, EventLoop<BrowserTabHost>)> {
    let document = BrowserDocumentDom2::from_html(html, RenderOptions::default())?;
    let mut host = BrowserTabHost::new(
      document,
      Box::new(TestExecutor { log }),
      TraceHandle::default(),
      js_execution_options,
    )?;
    host.reset_scripting_state(None, ReferrerPolicy::default())?;
    Ok((host, EventLoop::new()))
  }

  fn sri_sha256_token(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let b64 = BASE64_STANDARD.encode(digest);
    format!("sha256-{b64}")
  }

  #[test]
  fn dynamic_script_discovery_propagates_force_async_internal_slot() -> Result<()> {
    let log = Rc::new(RefCell::new(Vec::new()));
    let (mut host, mut event_loop) = build_host("<!doctype html><html><body></body></html>", log)?;
    let script = host.mutate_dom(|dom| {
      let script = dom.create_element("script", "");
      dom
        .set_attribute(script, "src", "https://example.com/dyn.js")
        .expect("set_attribute");
      let body = dom.body().expect("expected a <body> element");
      dom.append_child(body, script).expect("append_child");
      (script, true)
    });

    assert!(host.dom().node(script).script_force_async);

    host.discover_dynamic_scripts(&mut event_loop)?;

    let entry = host
      .scripts
      .values()
      .find(|entry| entry.node_id == script)
      .expect("expected script to be registered");
    assert!(!entry.spec.parser_inserted);
    assert!(entry.spec.force_async);
    Ok(())
  }

  #[test]
  fn external_script_fetch_uses_script_destinations() -> Result<()> {
    #[derive(Default)]
    struct DestinationRecordingFetcher {
      calls: Arc<Mutex<Vec<(String, FetchDestination)>>>,
    }

    impl ResourceFetcher for DestinationRecordingFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        Err(Error::Other("unexpected call to ResourceFetcher::fetch".to_string()))
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        self
          .calls
          .lock()
          .expect("calls lock")
          .push((req.url.to_string(), req.destination));

        let body = match req.url {
          "https://example.com/a.js" => "A",
          "https://example.com/b.js" => "B",
          "https://example.com/m.js" => "M",
          _ => "",
        };
        let mut res =
          FetchedResource::new(body.as_bytes().to_vec(), Some("application/javascript".to_string()));
        // Mirror HTTP fetches so downstream validations (status/CORS) remain deterministic.
        res.status = Some(200);
        res.final_url = Some(req.url.to_string());
        // Allow CORS-mode scripts to pass enforcement if enabled.
        res.access_control_allow_origin = Some("*".to_string());
        res.access_control_allow_credentials = true;
        Ok(res)
      }
    }

    let calls: Arc<Mutex<Vec<(String, FetchDestination)>>> = Arc::new(Mutex::new(Vec::new()));
    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(DestinationRecordingFetcher {
      calls: Arc::clone(&calls),
    });

    let renderer = crate::FastRender::builder().fetcher(fetcher).build()?;
    let document = BrowserDocumentDom2::new(
      renderer,
      r#"<!doctype html>
        <script src="https://example.com/a.js"></script>
        <script src="https://example.com/b.js" crossorigin></script>
        <script type="module" src="https://example.com/m.js"></script>"#,
      RenderOptions::default(),
    )?;
    let mut js_options = JsExecutionOptions::default();
    js_options.supports_module_scripts = true;
    let mut host = BrowserTabHost::new(
      document,
      Box::new(NoopExecutor::default()),
      TraceHandle::default(),
      js_options,
    )?;
    host.reset_scripting_state(
      Some("https://example.com/doc.html".to_string()),
      ReferrerPolicy::default(),
    )?;
    let mut event_loop = EventLoop::new();

    let mut discovered = host.discover_scripts_best_effort(Some("https://example.com/doc.html"));
    assert_eq!(discovered.len(), 3);
    for (node_id, spec) in discovered.drain(..) {
      let base_url_at_discovery = spec.base_url.clone();
      host.register_and_schedule_script(node_id, spec, base_url_at_discovery, &mut event_loop)?;
    }
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    let mut calls = calls.lock().expect("calls lock").clone();
    // Ignore any incidental requests and focus on `<script src>` fetches.
    calls.retain(|(url, _)| url.ends_with(".js"));
    calls.sort_by(|(a, _), (b, _)| a.cmp(b));
    assert_eq!(
      calls,
      vec![
        ("https://example.com/a.js".to_string(), FetchDestination::Script),
        ("https://example.com/b.js".to_string(), FetchDestination::ScriptCors),
        ("https://example.com/m.js".to_string(), FetchDestination::ScriptCors),
      ]
    );
    Ok(())
  }

  #[test]
  fn module_script_error_event_installs_event_loop_for_js_listeners() -> Result<()> {
    #[derive(Default)]
    struct FailingFetcher;

    impl ResourceFetcher for FailingFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        Err(Error::Other(format!("fetch blocked in test for {url}")))
      }

      fn fetch_with_request(&self, req: crate::resource::FetchRequest<'_>) -> Result<FetchedResource> {
        self.fetch(req.url)
      }
    }

    let log: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));

    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(FailingFetcher);
    let renderer = crate::FastRender::builder().fetcher(fetcher).build()?;
    let document = BrowserDocumentDom2::new(
      renderer,
      r#"<!doctype html>
        <script type="module" src="https://example.com/m.js"></script>"#,
      RenderOptions::default(),
    )?;

    let mut js_options = JsExecutionOptions::default();
    js_options.supports_module_scripts = true;

    let mut host = BrowserTabHost::new(
      document,
      Box::new(NoopExecutor::default()),
      TraceHandle::default(),
      js_options,
    )?;
    host.reset_scripting_state(
      Some("https://example.com/doc.html".to_string()),
      ReferrerPolicy::default(),
    )?;
    let mut event_loop = EventLoop::new();

    let mut discovered = host.discover_scripts_best_effort(Some("https://example.com/doc.html"));
    assert_eq!(discovered.len(), 1);
    let (node_id, spec) = discovered.pop().expect("missing discovered module script");
    assert_eq!(spec.script_type, ScriptType::Module);

    struct MicrotaskInvoker {
      log: Rc<RefCell<Vec<String>>>,
    }

    impl EventListenerInvoker for MicrotaskInvoker {
      fn invoke(
        &mut self,
        _listener_id: ListenerId,
        event: &mut Event,
      ) -> std::result::Result<(), DomError> {
        self.log.borrow_mut().push("listener".to_string());
        assert_eq!(event.type_, "error");

        let Some(event_loop) = crate::js::runtime::current_event_loop_mut::<BrowserTabHost>() else {
          self.log.borrow_mut().push("missing_event_loop".to_string());
          return Ok(());
        };

        let log_for_task = Rc::clone(&self.log);
        event_loop
          .queue_microtask(move |_host, _event_loop| {
            log_for_task.borrow_mut().push("microtask".to_string());
            Ok(())
          })
          .map_err(|err| DomError::new(err.to_string()))?;
        Ok(())
      }
    }

    host.set_event_invoker(Box::new(MicrotaskInvoker {
      log: Rc::clone(&log),
    }));
    host.dom_mut().events_mut().add_event_listener(
      EventTargetId::Node(node_id),
      "error",
      ListenerId::new(1),
      AddEventListenerOptions::default(),
    );

    let base_url_at_discovery = spec.base_url.clone();
    host.register_and_schedule_script(node_id, spec, base_url_at_discovery, &mut event_loop)?;
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    assert_eq!(
      &*log.borrow(),
      &["listener".to_string(), "microtask".to_string()],
      "expected error listener to see a current event loop and schedule microtasks"
    );
    Ok(())
  }

  #[test]
  fn dynamic_script_discovery_respects_force_async_internal_slot() -> Result<()> {
    struct LoggingExecutor {
      log: Rc<RefCell<Vec<String>>>,
    }

    impl BrowserTabJsExecutor for LoggingExecutor {
      fn execute_classic_script(
        &mut self,
        script_text: &str,
        _spec: &ScriptElementSpec,
        _current_script: Option<NodeId>,
        _document: &mut BrowserDocumentDom2,
        _event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<()> {
        self.log.borrow_mut().push(script_text.to_string());
        Ok(())
      }

      fn execute_module_script(
        &mut self,
        script_text: &str,
        spec: &ScriptElementSpec,
        current_script: Option<NodeId>,
        document: &mut BrowserDocumentDom2,
        event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<()> {
        self.execute_classic_script(script_text, spec, current_script, document, event_loop)
      }
    }

    let log: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let document =
      BrowserDocumentDom2::from_html("<!doctype html><html><body></body></html>", RenderOptions::default())?;
    let mut host = BrowserTabHost::new(
      document,
      Box::new(LoggingExecutor {
        log: Rc::clone(&log),
      }),
      TraceHandle::default(),
      JsExecutionOptions::default(),
    )?;
    host.reset_scripting_state(None, ReferrerPolicy::default())?;

    let (script_a, script_b) = host.mutate_dom(|dom| {
      let body = dom.body().expect("expected document.body to exist");
      let script_a = dom.create_element("script", "");
      dom
        .set_attribute(script_a, "src", "a.js")
        .expect("set_attribute");
      let script_b = dom.create_element("script", "");
      dom
        .set_attribute(script_b, "src", "b.js")
        .expect("set_attribute");
      dom
        .append_child(body, script_a)
        .expect("append_child should succeed");
      dom
        .append_child(body, script_b)
        .expect("append_child should succeed");
      ((script_a, script_b), false)
    });

    assert!(
      host.dom().node(script_a).script_force_async,
      "expected DOM-created scripts to default script_force_async=true"
    );
    assert!(
      host.dom().node(script_b).script_force_async,
      "expected DOM-created scripts to default script_force_async=true"
    );

    // Discover dynamic scripts (this schedules fetches and queues networking tasks).
    let mut event_loop = EventLoop::new();
    host.discover_dynamic_scripts(&mut event_loop)?;
    // Clear queued networking tasks; we'll drive fetch completion manually to control ordering.
    event_loop.clear_all_pending_work();

    let (id_a, id_b) = {
      let mut id_a = None;
      let mut id_b = None;
      for (id, entry) in &host.scripts {
        if entry.node_id == script_a {
          id_a = Some(*id);
        } else if entry.node_id == script_b {
          id_b = Some(*id);
        }
      }
      (id_a.expect("missing script_id for a.js"), id_b.expect("missing script_id for b.js"))
    };

    // Complete fetch for B first. When `force_async=true` is plumbed through, scripts behave like
    // async scripts and execute in completion order (B then A). If `force_async` is ignored, the
    // scheduler treats them as in-order-asap scripts and would execute A before B.
    let actions_b = host
      .scheduler
      .fetch_completed(id_b, "B".to_string())
      .expect("fetch_completed for B");
    host.apply_scheduler_actions(actions_b, &mut event_loop)?;
    let actions_a = host
      .scheduler
      .fetch_completed(id_a, "A".to_string())
      .expect("fetch_completed for A");
    host.apply_scheduler_actions(actions_a, &mut event_loop)?;

    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;
    assert_eq!(&*log.borrow(), &["B".to_string(), "A".to_string()]);
    Ok(())
  }

  #[derive(Default)]
  struct ScriptSourceFetcher {
    calls: AtomicUsize,
    sources: Mutex<HashMap<String, Vec<u8>>>,
  }

  impl ScriptSourceFetcher {
    fn new(sources: &[(&str, &str)]) -> Self {
      let mut map = HashMap::new();
      for (url, source) in sources {
        map.insert((*url).to_string(), (*source).as_bytes().to_vec());
      }
      Self {
        calls: AtomicUsize::new(0),
        sources: Mutex::new(map),
      }
    }

    fn call_count(&self) -> usize {
      self.calls.load(Ordering::Relaxed)
    }
  }

  impl crate::resource::ResourceFetcher for ScriptSourceFetcher {
    fn fetch(&self, url: &str) -> Result<crate::resource::FetchedResource> {
      self.calls.fetch_add(1, Ordering::Relaxed);
      let map = self.sources.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
      let bytes = map.get(url).cloned().ok_or_else(|| {
        Error::Other(format!("ScriptSourceFetcher has no source registered for url={url}"))
      })?;
      Ok(crate::resource::FetchedResource::new(
        bytes,
        Some("application/javascript".to_string()),
      ))
    }

    fn fetch_with_request(&self, req: crate::resource::FetchRequest<'_>) -> Result<crate::resource::FetchedResource> {
      self.fetch(req.url)
    }
  }

  fn build_host_with_fetcher(
    html: &str,
    log: Rc<RefCell<Vec<String>>>,
    fetcher: Arc<dyn crate::resource::ResourceFetcher>,
  ) -> Result<(BrowserTabHost, EventLoop<BrowserTabHost>)> {
    let renderer = super::super::FastRender::builder()
      .dom_scripting_enabled(true)
      .fetcher(fetcher)
      .build()?;
    let document = BrowserDocumentDom2::new(renderer, html, RenderOptions::default())?;
    let mut host = BrowserTabHost::new(
      document,
      Box::new(TestExecutor { log }),
      TraceHandle::default(),
      JsExecutionOptions::default(),
    )?;
    host.reset_scripting_state(None, ReferrerPolicy::default())?;
    Ok((host, EventLoop::new()))
  }

  fn build_host(html: &str, log: Rc<RefCell<Vec<String>>>) -> Result<(BrowserTabHost, EventLoop<BrowserTabHost>)> {
    build_host_with_options(html, log, JsExecutionOptions::default())
  }

  #[derive(Default)]
  struct NoopExecutor;

  impl BrowserTabJsExecutor for NoopExecutor {
    fn execute_classic_script(
      &mut self,
      _script_text: &str,
      _spec: &ScriptElementSpec,
      _current_script: Option<NodeId>,
      _document: &mut BrowserDocumentDom2,
      _event_loop: &mut EventLoop<BrowserTabHost>,
    ) -> Result<()> {
      Ok(())
    }

    fn execute_module_script(
      &mut self,
      _script_text: &str,
      _spec: &ScriptElementSpec,
      _current_script: Option<NodeId>,
      _document: &mut BrowserDocumentDom2,
      _event_loop: &mut EventLoop<BrowserTabHost>,
    ) -> Result<()> {
      Ok(())
    }

    fn fetch_module_graph(
      &mut self,
      spec: &ScriptElementSpec,
      fetcher: Arc<dyn ResourceFetcher>,
      _document: &mut BrowserDocumentDom2,
      _event_loop: &mut EventLoop<BrowserTabHost>,
    ) -> Result<()> {
      // Module scripts are fetched in CORS mode (ScriptCors destination). This test-only executor
      // does not attempt to build a real module graph; it only triggers the fetch so callers can
      // assert the request destination.
      if !spec.src_attr_present {
        return Ok(());
      }
      let Some(url) = spec.src.as_deref().filter(|s| !s.is_empty()) else {
        return Err(Error::Other(
          "module script src attribute was present but empty/invalid".to_string(),
        ));
      };
      fetcher.fetch_with_request(crate::resource::FetchRequest::new(url, FetchDestination::ScriptCors))?;
      Ok(())
    }
  }

  struct WindowRealmExecutor {
    realm: WindowRealm,
    log: Rc<RefCell<Vec<String>>>,
  }

  impl WindowRealmExecutor {
    fn new(log: Rc<RefCell<Vec<String>>>) -> Result<Self> {
      let realm =
        WindowRealm::new(WindowRealmConfig::new("https://example.com/")).map_err(|err| {
          Error::Other(format!("failed to create WindowRealm: {err}"))
        })?;
      Ok(Self { realm, log })
    }
  }

  impl BrowserTabJsExecutor for WindowRealmExecutor {
    fn execute_classic_script(
      &mut self,
      script_text: &str,
      _spec: &ScriptElementSpec,
      _current_script: Option<NodeId>,
      _document: &mut BrowserDocumentDom2,
      event_loop: &mut EventLoop<BrowserTabHost>,
    ) -> Result<()> {
      self
        .log
        .borrow_mut()
        .push(format!("script:{script_text}"));
      let result = with_event_loop(event_loop, || self.realm.exec_script(script_text));
      result
        .map(|_value| ())
        .map_err(|err| Error::Other(err.to_string()))
    }

    fn execute_module_script(
      &mut self,
      script_text: &str,
      spec: &ScriptElementSpec,
      current_script: Option<NodeId>,
      document: &mut BrowserDocumentDom2,
      event_loop: &mut EventLoop<BrowserTabHost>,
    ) -> Result<()> {
      self.execute_classic_script(script_text, spec, current_script, document, event_loop)
    }
  }

  fn rgba_at(pixmap: &Pixmap, x: u32, y: u32) -> [u8; 4] {
    let width = pixmap.width();
    let height = pixmap.height();
    assert!(
      x < width && y < height,
      "rgba_at out of bounds: requested ({x}, {y}) in {width}x{height} pixmap"
    );
    let idx = (y as usize * width as usize + x as usize) * 4;
    let data = pixmap.data();
    [data[idx], data[idx + 1], data[idx + 2], data[idx + 3]]
  }

  #[test]
  fn dispatches_load_event_for_successful_external_script() -> Result<()> {
    let mut host = BrowserTabHost::new(
      BrowserDocumentDom2::from_html("<script src=a.js></script>", RenderOptions::default())?,
      Box::new(NoopExecutor::default()),
      TraceHandle::default(),
      JsExecutionOptions::default(),
    )?;
    host.reset_scripting_state(None, ReferrerPolicy::default())?;
    host.register_external_script_source("a.js".to_string(), "/* ok */".to_string());

    let event_log: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    host.set_event_invoker(Box::new(RecordingInvoker {
      log: Rc::clone(&event_log),
    }));

    let mut discovered = host.discover_scripts_best_effort(None);
    assert_eq!(discovered.len(), 1);
    let (node_id, spec) = discovered.pop().unwrap();
    let base_url_at_discovery = spec.base_url.clone();

    let load_listener = ListenerId::new(1);
    assert!(
      host.dom().events().add_event_listener(
        EventTargetId::Node(node_id),
        "load",
        load_listener,
        AddEventListenerOptions::default(),
      ),
      "expected load listener to be inserted"
    );
    let error_listener = ListenerId::new(2);
    assert!(
      host.dom().events().add_event_listener(
        EventTargetId::Node(node_id),
        "error",
        error_listener,
        AddEventListenerOptions::default(),
      ),
      "expected error listener to be inserted"
    );

    let mut event_loop = EventLoop::new();
    host.register_and_schedule_script(node_id, spec, base_url_at_discovery, &mut event_loop)?;

    assert_eq!(&*event_log.borrow(), &["load".to_string()]);
    Ok(())
  }

  #[test]
  fn script_load_event_runs_after_microtask_checkpoint_for_blocking_external_script() -> Result<()> {
    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let (mut host, mut event_loop) = build_host("<script src=a.js></script>", Rc::clone(&log))?;
    host.register_external_script_source("a.js".to_string(), "A".to_string());

    host.set_event_invoker(Box::new(RecordingInvoker {
      log: Rc::clone(&log),
    }));

    let mut discovered = host.discover_scripts_best_effort(None);
    assert_eq!(discovered.len(), 1);
    let (node_id, spec) = discovered.pop().unwrap();
    let base_url_at_discovery = spec.base_url.clone();

    assert!(
      host.dom().events().add_event_listener(
        EventTargetId::Node(node_id),
        "load",
        ListenerId::new(1),
        AddEventListenerOptions::default(),
      ),
      "expected load listener to be inserted"
    );

    host.register_and_schedule_script(node_id, spec, base_url_at_discovery, &mut event_loop)?;

    assert_eq!(
      &*log.borrow(),
      &[
        "script:A".to_string(),
        "microtask:A".to_string(),
        "load".to_string()
      ]
    );
    Ok(())
  }

  #[test]
  fn script_load_event_runs_after_microtask_checkpoint_for_async_external_script() -> Result<()> {
    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let (mut host, mut event_loop) = build_host("<script async src=a.js></script>", Rc::clone(&log))?;
    host.register_external_script_source("a.js".to_string(), "A".to_string());

    host.set_event_invoker(Box::new(RecordingInvoker {
      log: Rc::clone(&log),
    }));

    let mut discovered = host.discover_scripts_best_effort(None);
    assert_eq!(discovered.len(), 1);
    let (node_id, spec) = discovered.pop().unwrap();
    let base_url_at_discovery = spec.base_url.clone();

    assert!(
      host.dom().events().add_event_listener(
        EventTargetId::Node(node_id),
        "load",
        ListenerId::new(1),
        AddEventListenerOptions::default(),
      ),
      "expected load listener to be inserted"
    );

    host.register_and_schedule_script(node_id, spec, base_url_at_discovery, &mut event_loop)?;
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    assert_eq!(
      &*log.borrow(),
      &[
        "script:A".to_string(),
        "microtask:A".to_string(),
        "load".to_string()
      ]
    );
    Ok(())
  }

  #[test]
  fn dispatches_error_event_for_external_script_fetch_failure_and_continues() -> Result<()> {
    struct LoggingExecutor {
      log: Rc<RefCell<Vec<String>>>,
    }

    impl BrowserTabJsExecutor for LoggingExecutor {
      fn execute_classic_script(
        &mut self,
        script_text: &str,
        _spec: &ScriptElementSpec,
        _current_script: Option<NodeId>,
        _document: &mut BrowserDocumentDom2,
        _event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<()> {
        self.log.borrow_mut().push(script_text.to_string());
        Ok(())
      }

      fn execute_module_script(
        &mut self,
        script_text: &str,
        _spec: &ScriptElementSpec,
        _current_script: Option<NodeId>,
        _document: &mut BrowserDocumentDom2,
        _event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<()> {
        // Tests in this module primarily cover classic script execution; implement module execution
        // by reusing the same "log the script text" behavior.
        self.log.borrow_mut().push(script_text.to_string());
        Ok(())
      }
    }

    let js_options = JsExecutionOptions {
      max_script_bytes: 1,
      ..JsExecutionOptions::default()
    };
    let script_log: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let mut host = BrowserTabHost::new(
      BrowserDocumentDom2::from_html("<script src=a.js></script><script>B</script>", RenderOptions::default())?,
      Box::new(LoggingExecutor {
        log: Rc::clone(&script_log),
      }),
      TraceHandle::default(),
      js_options,
    )?;
    host.reset_scripting_state(None, ReferrerPolicy::default())?;
    // Trigger a deterministic fetch failure via the max_script_bytes check.
    host.register_external_script_source("a.js".to_string(), "XX".to_string());

    let event_log: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    host.set_event_invoker(Box::new(RecordingInvoker {
      log: Rc::clone(&event_log),
    }));

    let discovered = host.discover_scripts_best_effort(None);
    assert_eq!(discovered.len(), 2);
    // Discovery is in document order; schedule scripts in that order.
    let (first_node_id, first_spec) = discovered[0].clone();
    let (second_node_id, second_spec) = discovered[1].clone();

    let error_listener = ListenerId::new(1);
    host.dom().events().add_event_listener(
      EventTargetId::Node(first_node_id),
      "error",
      error_listener,
      AddEventListenerOptions::default(),
    );

    let mut event_loop = EventLoop::new();
    host.register_and_schedule_script(
      first_node_id,
      first_spec.clone(),
      first_spec.base_url.clone(),
      &mut event_loop,
    )?;
    host.register_and_schedule_script(
      second_node_id,
      second_spec.clone(),
      second_spec.base_url.clone(),
      &mut event_loop,
    )?;

    assert_eq!(&*event_log.borrow(), &["error".to_string()]);
    assert_eq!(&*script_log.borrow(), &["B".to_string()]);
    Ok(())
  }

  #[test]
  fn external_script_integrity_match_executes_and_dispatches_load_event() -> Result<()> {
    let source = "A";
    let integrity = sri_sha256_token(source.as_bytes());
    let html = format!(r#"<script src="a.js" integrity="{integrity}"></script>"#);
    let script_log: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let (mut host, mut event_loop) = build_host(&html, Rc::clone(&script_log))?;
    host.register_external_script_source("a.js".to_string(), source.to_string());

    let event_log: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    host.set_event_invoker(Box::new(RecordingInvoker {
      log: Rc::clone(&event_log),
    }));

    let mut discovered = host.discover_scripts_best_effort(None);
    assert_eq!(discovered.len(), 1);
    let (node_id, spec) = discovered.pop().unwrap();
    let base_url_at_discovery = spec.base_url.clone();

    let load_listener = ListenerId::new(1);
    assert!(
      host.dom().events().add_event_listener(
        EventTargetId::Node(node_id),
        "load",
        load_listener,
        AddEventListenerOptions::default(),
      ),
      "expected load listener to be inserted"
    );
    let error_listener = ListenerId::new(2);
    assert!(
      host.dom().events().add_event_listener(
        EventTargetId::Node(node_id),
        "error",
        error_listener,
        AddEventListenerOptions::default(),
      ),
      "expected error listener to be inserted"
    );

    host.register_and_schedule_script(node_id, spec, base_url_at_discovery, &mut event_loop)?;

    assert_eq!(&*event_log.borrow(), &["load".to_string()]);
    assert_eq!(
      &*script_log.borrow(),
      &["script:A".to_string(), "microtask:A".to_string()]
    );
    Ok(())
  }

  #[test]
  fn external_script_integrity_mismatch_dispatches_error_and_skips_execution() -> Result<()> {
    let source = "A";
    let wrong = sri_sha256_token(b"other");
    let html = format!(
      r#"<script src="a.js" integrity="{wrong}"></script><script>B</script>"#
    );
    let script_log: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let (mut host, mut event_loop) = build_host(&html, Rc::clone(&script_log))?;
    host.register_external_script_source("a.js".to_string(), source.to_string());

    let event_log: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    host.set_event_invoker(Box::new(RecordingInvoker {
      log: Rc::clone(&event_log),
    }));

    let discovered = host.discover_scripts_best_effort(None);
    assert_eq!(discovered.len(), 2);
    let (first_node_id, first_spec) = discovered[0].clone();
    let (second_node_id, second_spec) = discovered[1].clone();

    let error_listener = ListenerId::new(1);
    assert!(
      host.dom().events().add_event_listener(
        EventTargetId::Node(first_node_id),
        "error",
        error_listener,
        AddEventListenerOptions::default(),
      ),
      "expected error listener to be inserted"
    );

    host.register_and_schedule_script(
      first_node_id,
      first_spec.clone(),
      first_spec.base_url.clone(),
      &mut event_loop,
    )?;
    host.register_and_schedule_script(
      second_node_id,
      second_spec.clone(),
      second_spec.base_url.clone(),
      &mut event_loop,
    )?;

    assert_eq!(&*event_log.borrow(), &["error".to_string()]);
    assert_eq!(
      &*script_log.borrow(),
      &["script:B".to_string(), "microtask:B".to_string()]
    );
    Ok(())
  }

  #[test]
  fn external_script_integrity_accepts_multiple_hashes_if_any_matches() -> Result<()> {
    let source = "A";
    let wrong = sri_sha256_token(b"other");
    let correct = sri_sha256_token(source.as_bytes());
    let integrity = format!("{wrong} {correct}");
    let html = format!(r#"<script src="a.js" integrity="{integrity}"></script>"#);
    let script_log: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let (mut host, mut event_loop) = build_host(&html, Rc::clone(&script_log))?;
    host.register_external_script_source("a.js".to_string(), source.to_string());

    let event_log: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    host.set_event_invoker(Box::new(RecordingInvoker {
      log: Rc::clone(&event_log),
    }));

    let mut discovered = host.discover_scripts_best_effort(None);
    assert_eq!(discovered.len(), 1);
    let (node_id, spec) = discovered.pop().unwrap();
    let base_url_at_discovery = spec.base_url.clone();

    let load_listener = ListenerId::new(1);
    assert!(
      host.dom().events().add_event_listener(
        EventTargetId::Node(node_id),
        "load",
        load_listener,
        AddEventListenerOptions::default(),
      ),
      "expected load listener to be inserted"
    );

    host.register_and_schedule_script(node_id, spec, base_url_at_discovery, &mut event_loop)?;

    assert_eq!(&*event_log.borrow(), &["load".to_string()]);
    Ok(())
  }

  #[test]
  fn external_script_integrity_rejects_oversized_integrity_attribute() -> Result<()> {
    let source = "A";
    let integrity = "a".repeat(crate::js::sri::MAX_INTEGRITY_ATTRIBUTE_BYTES + 1);
    let html = format!(
      r#"<script src="a.js" integrity="{integrity}"></script><script>B</script>"#
    );
    let script_log: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let (mut host, mut event_loop) = build_host(&html, Rc::clone(&script_log))?;
    host.register_external_script_source("a.js".to_string(), source.to_string());

    let event_log: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    host.set_event_invoker(Box::new(RecordingInvoker {
      log: Rc::clone(&event_log),
    }));

    let discovered = host.discover_scripts_best_effort(None);
    assert_eq!(discovered.len(), 2);
    let (first_node_id, first_spec) = discovered[0].clone();
    let (second_node_id, second_spec) = discovered[1].clone();

    let error_listener = ListenerId::new(1);
    assert!(
      host.dom().events().add_event_listener(
        EventTargetId::Node(first_node_id),
        "error",
        error_listener,
        AddEventListenerOptions::default(),
      ),
      "expected error listener to be inserted"
    );

    host.register_and_schedule_script(
      first_node_id,
      first_spec.clone(),
      first_spec.base_url.clone(),
      &mut event_loop,
    )?;
    host.register_and_schedule_script(
      second_node_id,
      second_spec.clone(),
      second_spec.base_url.clone(),
      &mut event_loop,
    )?;

    assert_eq!(&*event_log.borrow(), &["error".to_string()]);
    assert_eq!(
      &*script_log.borrow(),
      &["script:B".to_string(), "microtask:B".to_string()]
    );
    Ok(())
  }

  #[test]
  fn external_script_integrity_with_only_unsupported_algorithms_is_rejected() -> Result<()> {
    let source = "A";
    let html =
      r#"<script src="a.js" integrity="sha512-deadbeef"></script><script>B</script>"#;
    let script_log: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let (mut host, mut event_loop) = build_host(html, Rc::clone(&script_log))?;
    host.register_external_script_source("a.js".to_string(), source.to_string());

    let event_log: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    host.set_event_invoker(Box::new(RecordingInvoker {
      log: Rc::clone(&event_log),
    }));

    let discovered = host.discover_scripts_best_effort(None);
    assert_eq!(discovered.len(), 2);
    let (first_node_id, first_spec) = discovered[0].clone();
    let (second_node_id, second_spec) = discovered[1].clone();

    let error_listener = ListenerId::new(1);
    assert!(
      host.dom().events().add_event_listener(
        EventTargetId::Node(first_node_id),
        "error",
        error_listener,
        AddEventListenerOptions::default(),
      ),
      "expected error listener to be inserted"
    );

    host.register_and_schedule_script(
      first_node_id,
      first_spec.clone(),
      first_spec.base_url.clone(),
      &mut event_loop,
    )?;
    host.register_and_schedule_script(
      second_node_id,
      second_spec.clone(),
      second_spec.base_url.clone(),
      &mut event_loop,
    )?;

    assert_eq!(&*event_log.borrow(), &["error".to_string()]);
    assert_eq!(
      &*script_log.borrow(),
      &["script:B".to_string(), "microtask:B".to_string()]
    );
    Ok(())
  }

  #[test]
  fn external_script_integrity_cross_origin_requires_crossorigin_attribute() -> Result<()> {
    let source = "A";
    let integrity = sri_sha256_token(source.as_bytes());
    let html = format!(
      r#"<script src="https://other.com/a.js" integrity="{integrity}"></script><script>B</script>"#
    );
    let script_log: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let (mut host, mut event_loop) = build_host(&html, Rc::clone(&script_log))?;
    host.reset_scripting_state(Some("https://example.com/doc.html".to_string()), ReferrerPolicy::default())?;
    host.register_external_script_source("https://other.com/a.js".to_string(), source.to_string());

    let event_log: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    host.set_event_invoker(Box::new(RecordingInvoker {
      log: Rc::clone(&event_log),
    }));

    let discovered = host.discover_scripts_best_effort(Some("https://example.com/doc.html"));
    assert_eq!(discovered.len(), 2);
    let (first_node_id, first_spec) = discovered[0].clone();
    let (second_node_id, second_spec) = discovered[1].clone();

    let error_listener = ListenerId::new(1);
    assert!(
      host.dom().events().add_event_listener(
        EventTargetId::Node(first_node_id),
        "error",
        error_listener,
        AddEventListenerOptions::default(),
      ),
      "expected error listener to be inserted"
    );

    host.register_and_schedule_script(
      first_node_id,
      first_spec.clone(),
      first_spec.base_url.clone(),
      &mut event_loop,
    )?;
    host.register_and_schedule_script(
      second_node_id,
      second_spec.clone(),
      second_spec.base_url.clone(),
      &mut event_loop,
    )?;

    assert_eq!(&*event_log.borrow(), &["error".to_string()]);
    assert_eq!(
      &*script_log.borrow(),
      &["script:B".to_string(), "microtask:B".to_string()]
    );
    Ok(())
  }

  #[test]
  fn external_script_integrity_cross_origin_with_crossorigin_allows_execution() -> Result<()> {
    let source = "A";
    let integrity = sri_sha256_token(source.as_bytes());
    let html = format!(
      r#"<script src="https://other.com/a.js" crossorigin integrity="{integrity}"></script>"#
    );
    let script_log: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let (mut host, mut event_loop) = build_host(&html, Rc::clone(&script_log))?;
    host.reset_scripting_state(Some("https://example.com/doc.html".to_string()), ReferrerPolicy::default())?;
    host.register_external_script_source("https://other.com/a.js".to_string(), source.to_string());

    let event_log: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    host.set_event_invoker(Box::new(RecordingInvoker {
      log: Rc::clone(&event_log),
    }));

    let mut discovered = host.discover_scripts_best_effort(Some("https://example.com/doc.html"));
    assert_eq!(discovered.len(), 1);
    let (node_id, spec) = discovered.pop().unwrap();
    let base_url_at_discovery = spec.base_url.clone();

    let load_listener = ListenerId::new(1);
    assert!(
      host.dom().events().add_event_listener(
        EventTargetId::Node(node_id),
        "load",
        load_listener,
        AddEventListenerOptions::default(),
      ),
      "expected load listener to be inserted"
    );
    let error_listener = ListenerId::new(2);
    assert!(
      host.dom().events().add_event_listener(
        EventTargetId::Node(node_id),
        "error",
        error_listener,
        AddEventListenerOptions::default(),
      ),
      "expected error listener to be inserted"
    );

    host.register_and_schedule_script(
      node_id,
      spec,
      base_url_at_discovery,
      &mut event_loop,
    )?;

    assert_eq!(&*event_log.borrow(), &["load".to_string()]);
    assert_eq!(
      &*script_log.borrow(),
      &["script:A".to_string(), "microtask:A".to_string()]
    );
    Ok(())
  }

  #[test]
  fn dispatches_error_event_for_script_execution_failure_and_continues() -> Result<()> {
    struct ErroringExecutor {
      log: Rc<RefCell<Vec<String>>>,
    }

    impl BrowserTabJsExecutor for ErroringExecutor {
      fn execute_classic_script(
        &mut self,
        script_text: &str,
        _spec: &ScriptElementSpec,
        _current_script: Option<NodeId>,
        _document: &mut BrowserDocumentDom2,
        _event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<()> {
        self.log.borrow_mut().push(script_text.to_string());
        if script_text == "bad" {
          return Err(Error::Other("boom".to_string()));
        }
        Ok(())
      }

      fn execute_module_script(
        &mut self,
        script_text: &str,
        _spec: &ScriptElementSpec,
        _current_script: Option<NodeId>,
        _document: &mut BrowserDocumentDom2,
        _event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<()> {
        self.log.borrow_mut().push(script_text.to_string());
        if script_text == "bad" {
          return Err(Error::Other("boom".to_string()));
        }
        Ok(())
      }
    }

    let script_log: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let mut host = BrowserTabHost::new(
      BrowserDocumentDom2::from_html("<script>bad</script><script>ok</script>", RenderOptions::default())?,
      Box::new(ErroringExecutor {
        log: Rc::clone(&script_log),
      }),
      TraceHandle::default(),
      JsExecutionOptions::default(),
    )?;
    host.reset_scripting_state(None, ReferrerPolicy::default())?;

    let event_log: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    host.set_event_invoker(Box::new(RecordingInvoker {
      log: Rc::clone(&event_log),
    }));

    let discovered = host.discover_scripts_best_effort(None);
    assert_eq!(discovered.len(), 2);
    let (first_node_id, first_spec) = discovered[0].clone();
    let (second_node_id, second_spec) = discovered[1].clone();

    let error_listener = ListenerId::new(1);
    host.dom().events().add_event_listener(
      EventTargetId::Node(first_node_id),
      "error",
      error_listener,
      AddEventListenerOptions::default(),
    );

    let mut event_loop = EventLoop::new();
    host.register_and_schedule_script(
      first_node_id,
      first_spec.clone(),
      first_spec.base_url.clone(),
      &mut event_loop,
    )?;
    host.register_and_schedule_script(
      second_node_id,
      second_spec.clone(),
      second_spec.base_url.clone(),
      &mut event_loop,
    )?;

    assert_eq!(&*event_log.borrow(), &["error".to_string()]);
    assert_eq!(&*script_log.borrow(), &["bad".to_string(), "ok".to_string()]);
    Ok(())
  }

  #[derive(Clone)]
  struct SingleResourceFetcher {
    url: String,
    bytes: Arc<Vec<u8>>,
  }

  impl ResourceFetcher for SingleResourceFetcher {
    fn fetch(&self, url: &str) -> Result<FetchedResource> {
      if url != self.url {
        return Err(Error::Other(format!(
          "unexpected fetch url={url} (expected {})",
          self.url
        )));
      }
      Ok(FetchedResource::new((*self.bytes).clone(), None))
    }
  }

  #[test]
  fn fetch_script_source_honors_script_charset_attribute() -> Result<()> {
    let url = "https://example.com/test.js";
    let bytes = encoding_rs::SHIFT_JIS.encode("console.log('デ')").0.into_owned();
    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(SingleResourceFetcher {
      url: url.to_string(),
      bytes: Arc::new(bytes),
    });

    let renderer = FastRender::builder()
      .dom_scripting_enabled(true)
      .fetcher(fetcher)
      .build()?;
    let document = BrowserDocumentDom2::new(
      renderer,
      &format!("<script src=\"{url}\" charset=\"shift_jis\"></script>"),
      RenderOptions::default(),
    )?;

    let mut host = BrowserTabHost::new(
      document,
      Box::new(NoopExecutor::default()),
      TraceHandle::default(),
      JsExecutionOptions::default(),
    )?;
    host.reset_scripting_state(None, ReferrerPolicy::default())?;

    let mut discovered = host.discover_scripts_best_effort(None);
    assert_eq!(discovered.len(), 1);
    let (node_id, spec) = discovered.pop().unwrap();
    let base_url_at_discovery = spec.base_url.clone();

    // Insert the script into the host's scheduler tables without triggering fetch/execution, then
    // call `fetch_script_source` directly so this test doesn't depend on JS execution plumbing.
    let spec_for_table = spec.clone();
    let discovered = host
      .scheduler
      .discovered_parser_script(spec, node_id, base_url_at_discovery)?;
    host.scripts.insert(
      discovered.id,
      ScriptEntry {
        node_id,
        spec: spec_for_table.clone(),
      },
    );

    let destination = if spec_for_table.crossorigin.is_some() {
      FetchDestination::ScriptCors
    } else {
      FetchDestination::Script
    };
    let source = host.fetch_script_source(
      discovered.id,
      spec_for_table.src.as_deref().unwrap(),
      destination,
    )?;
    assert!(
      source.contains('デ'),
      "expected decoded source to include kana from shift_jis bytes, got {source:?}"
    );
    Ok(())
  }

  #[test]
  fn dispatches_error_event_for_invalid_external_src_attribute() -> Result<()> {
    let mut host = BrowserTabHost::new(
      BrowserDocumentDom2::from_html("<script src></script>", RenderOptions::default())?,
      Box::new(NoopExecutor::default()),
      TraceHandle::default(),
      JsExecutionOptions::default(),
    )?;
    host.reset_scripting_state(None, ReferrerPolicy::default())?;

    let event_log: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    host.set_event_invoker(Box::new(RecordingInvoker {
      log: Rc::clone(&event_log),
    }));

    let mut discovered = host.discover_scripts_best_effort(None);
    assert_eq!(discovered.len(), 1);
    let (node_id, spec) = discovered.pop().unwrap();

    host.dom().events().add_event_listener(
      EventTargetId::Node(node_id),
      "error",
      ListenerId::new(1),
      AddEventListenerOptions::default(),
    );

    let mut event_loop = EventLoop::new();
    host.register_and_schedule_script(node_id, spec.clone(), spec.base_url.clone(), &mut event_loop)?;

    // `QueueScriptEventTask` dispatches as an element task, so run the event loop.
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    assert_eq!(&*event_log.borrow(), &["error".to_string()]);
    Ok(())
  }

  #[test]
  fn dispatches_error_event_for_invalid_module_src_attribute() -> Result<()> {
    let js_options = JsExecutionOptions {
      supports_module_scripts: true,
      ..JsExecutionOptions::default()
    };
    let mut host = BrowserTabHost::new(
      BrowserDocumentDom2::from_html("<script type=\"module\" src></script>", RenderOptions::default())?,
      Box::new(NoopExecutor::default()),
      TraceHandle::default(),
      js_options,
    )?;
    host.reset_scripting_state(None, ReferrerPolicy::default())?;

    let event_log: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    host.set_event_invoker(Box::new(RecordingInvoker {
      log: Rc::clone(&event_log),
    }));

    let mut discovered = host.discover_scripts_best_effort(None);
    assert_eq!(discovered.len(), 1);
    let (node_id, spec) = discovered.pop().unwrap();

    host.dom().events().add_event_listener(
      EventTargetId::Node(node_id),
      "error",
      ListenerId::new(1),
      AddEventListenerOptions::default(),
    );

    let mut event_loop = EventLoop::new();
    host.register_and_schedule_script(node_id, spec.clone(), spec.base_url.clone(), &mut event_loop)?;

    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    assert_eq!(&*event_log.borrow(), &["error".to_string()]);
    Ok(())
  }

  #[test]
  fn dispatches_error_event_for_invalid_importmap_src_attribute() -> Result<()> {
    let mut host = BrowserTabHost::new(
      BrowserDocumentDom2::from_html("<script type=\"importmap\" src></script>", RenderOptions::default())?,
      Box::new(NoopExecutor::default()),
      TraceHandle::default(),
      JsExecutionOptions::default(),
    )?;
    host.reset_scripting_state(None, ReferrerPolicy::default())?;

    let event_log: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    host.set_event_invoker(Box::new(RecordingInvoker {
      log: Rc::clone(&event_log),
    }));

    let mut discovered = host.discover_scripts_best_effort(None);
    assert_eq!(discovered.len(), 1);
    let (node_id, spec) = discovered.pop().unwrap();

    host.dom().events().add_event_listener(
      EventTargetId::Node(node_id),
      "error",
      ListenerId::new(1),
      AddEventListenerOptions::default(),
    );

    let mut event_loop = EventLoop::new();
    host.register_and_schedule_script(node_id, spec.clone(), spec.base_url.clone(), &mut event_loop)?;

    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    assert_eq!(&*event_log.borrow(), &["error".to_string()]);
    Ok(())
  }

  #[test]
  fn microtasks_run_at_parser_script_boundaries_when_js_stack_empty() -> Result<()> {
    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let (mut host, mut event_loop) = build_host("<script>A</script>", Rc::clone(&log))?;

    event_loop.queue_microtask({
      let log = Rc::clone(&log);
      move |_host, _event_loop| {
        log.borrow_mut().push("pre".to_string());
        Ok(())
      }
    })?;

    let mut discovered = host.discover_scripts_best_effort(None);
    assert_eq!(discovered.len(), 1);
    let (node_id, spec) = discovered.pop().unwrap();
    let base_url_at_discovery = spec.base_url.clone();

    host.register_and_schedule_script(node_id, spec, base_url_at_discovery, &mut event_loop)?;

    assert_eq!(
      &*log.borrow(),
      &[
        "pre".to_string(),
        "script:A".to_string(),
        "microtask:A".to_string()
      ]
    );
    Ok(())
  }

  #[test]
  fn from_html_streaming_parse_honors_renderoptions_cancel_callback() -> Result<()> {
    let dom_parse_checks = Arc::new(AtomicUsize::new(0));
    let dom_parse_checks_for_cb = Arc::clone(&dom_parse_checks);
    let cancel: Arc<crate::render_control::CancelCallback> = Arc::new(move || {
      if crate::render_control::active_stage() != Some(crate::error::RenderStage::DomParse) {
        return false;
      }
      // Cancel after several checks so we don't trip during the initial empty document parse.
      dom_parse_checks_for_cb.fetch_add(1, Ordering::Relaxed) >= 5
    });

    let mut html = "<!doctype html>".to_string();
    // Produce enough parser-inserted script boundaries to force multiple streaming `pump` calls (and
    // therefore multiple deadline checks).
    for _ in 0..10 {
      html.push_str("<script>noop</script>");
    }
    html.push_str("<p>done</p>");

    let options = RenderOptions::new()
      .with_viewport(64, 64)
      .with_cancel_callback(Some(cancel));

    let err = match BrowserTab::from_html(&html, options, NoopExecutor::default()) {
      Ok(_) => panic!("expected streaming HTML parse to be cancelled"),
      Err(err) => err,
    };
    assert!(
      matches!(
        err,
        Error::Render(crate::error::RenderError::Timeout {
          stage: crate::error::RenderStage::DomParse,
          ..
        })
      ),
      "expected dom_parse timeout/cancel error; got {err:?}"
    );
    assert!(
      dom_parse_checks.load(Ordering::Relaxed) >= 6,
      "expected cancel callback to be polled during streaming dom parse"
    );
    Ok(())
  }

  #[test]
  fn async_external_script_can_execute_before_parsing_finishes() -> Result<()> {
    #[derive(Clone)]
    struct AssertNotYetParsedExecutor {
      executed: Rc<Cell<bool>>,
    }

    impl BrowserTabJsExecutor for AssertNotYetParsedExecutor {
      fn execute_classic_script(
        &mut self,
        script_text: &str,
        _spec: &ScriptElementSpec,
        _current_script: Option<NodeId>,
        document: &mut BrowserDocumentDom2,
        _event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<()> {
        assert_eq!(script_text, "A");
        assert!(
          document.dom().get_element_by_id("late").is_none(),
          "expected async script to run before parser reached the late marker"
        );
        self.executed.set(true);
        Ok(())
      }

      fn execute_module_script(
        &mut self,
        script_text: &str,
        spec: &ScriptElementSpec,
        current_script: Option<NodeId>,
        document: &mut BrowserDocumentDom2,
        event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<()> {
        self.execute_classic_script(script_text, spec, current_script, document, event_loop)
      }
    }

    let executed = Rc::new(Cell::new(false));
    // Use a fetcher-backed script source instead of `register_script_source` so this regression test
    // proves async scripts can interleave with parsing via event-loop tasks (rather than relying on
    // the "spin for fast async sources" heuristic).
    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(SingleResourceFetcher {
      url: "https://example.com/a.js".to_string(),
      bytes: Arc::new(b"A".to_vec()),
    });

    // Force parsing to yield across multiple DOMManipulation tasks so async script tasks can
    // interleave mid-parse.
    let mut js_execution_options = JsExecutionOptions::default();
    js_execution_options.dom_parse_budget = crate::js::ParseBudget::new(1);

    let filler = "x".repeat(10_000);
    let html = format!(
      "<!doctype html><html><body><script async src=\"https://example.com/a.js\"></script><!--{filler}--><div id=late></div></body></html>"
    );
    let mut tab = BrowserTab::from_html_with_document_url_and_fetcher_and_js_execution_options(
      &html,
      "https://example.com/",
      RenderOptions::default(),
      AssertNotYetParsedExecutor {
        executed: Rc::clone(&executed),
      },
      fetcher,
      js_execution_options,
    )?;

    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    assert!(executed.get(), "expected async external script to execute");
    assert!(
      tab.host.dom().get_element_by_id("late").is_some(),
      "expected parsing to eventually reach the late marker"
    );
    Ok(())
  }

  #[test]
  fn streaming_parser_sets_force_async_false_for_parser_inserted_scripts() -> Result<()> {
    #[derive(Clone)]
    struct RecordingExecutor {
      seen: Rc<RefCell<Vec<bool>>>,
    }

    impl BrowserTabJsExecutor for RecordingExecutor {
      fn execute_classic_script(
        &mut self,
        _script_text: &str,
        spec: &ScriptElementSpec,
        _current_script: Option<NodeId>,
        _document: &mut BrowserDocumentDom2,
        _event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<()> {
        self.seen.borrow_mut().push(spec.force_async);
        Ok(())
      }

      fn execute_module_script(
        &mut self,
        script_text: &str,
        spec: &ScriptElementSpec,
        current_script: Option<NodeId>,
        document: &mut BrowserDocumentDom2,
        event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<()> {
        self.execute_classic_script(script_text, spec, current_script, document, event_loop)
      }
    }

    let seen = Rc::new(RefCell::new(Vec::<bool>::new()));
    let _tab = BrowserTab::from_html(
      "<!doctype html><script>noop</script>",
      RenderOptions::default(),
      RecordingExecutor { seen: Rc::clone(&seen) },
    )?;

    assert_eq!(
      &*seen.borrow(),
      &[false],
      "expected parser-inserted <script> to have force_async=false"
    );
    Ok(())
  }

  #[test]
  fn microtask_checkpoint_after_execute_now_is_gated_on_js_execution_depth() -> Result<()> {
    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let (mut host, mut event_loop) = build_host("<script>B</script>", Rc::clone(&log))?;

    let _outer_guard = JsExecutionGuard::enter(&host.js_execution_depth);

    let mut discovered = host.discover_scripts_best_effort(None);
    assert_eq!(discovered.len(), 1);
    let (node_id, spec) = discovered.pop().unwrap();
    let base_url_at_discovery = spec.base_url.clone();

    host.register_and_schedule_script(node_id, spec, base_url_at_discovery, &mut event_loop)?;

    assert_eq!(&*log.borrow(), &["script:B".to_string()]);
    assert_eq!(event_loop.pending_microtask_count(), 1);

    drop(_outer_guard);
    event_loop.perform_microtask_checkpoint(&mut host)?;

    assert_eq!(
      &*log.borrow(),
      &["script:B".to_string(), "microtask:B".to_string()]
    );
    Ok(())
  }

  #[test]
  fn streaming_parser_pre_script_checkpoint_is_gated_on_js_execution_depth() -> Result<()> {
    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let executor = TestExecutor {
      log: Rc::clone(&log),
    };
    let mut tab = BrowserTab::from_html("", RenderOptions::default(), executor)?;

    // Reset scripting state so parsing new HTML will schedule/execute scripts.
    tab.host.reset_scripting_state(None, ReferrerPolicy::default())?;

    tab.event_loop.queue_microtask({
      let log = Rc::clone(&log);
      move |_host, _event_loop| {
        log.borrow_mut().push("pre".to_string());
        Ok(())
      }
    })?;

    // Simulate re-entrant parsing while already in JS execution (e.g. future document.write).
    let outer_guard = JsExecutionGuard::enter(&tab.host.js_execution_depth);
    let _ = tab.parse_html_streaming_and_schedule_scripts("<script>A</script>", None, &RenderOptions::default())?;

    // No checkpoint should have run at the script boundary while the JS stack is non-empty.
    assert_eq!(&*log.borrow(), &["script:A".to_string()]);

    drop(outer_guard);
    tab.event_loop.perform_microtask_checkpoint(&mut tab.host)?;

    assert_eq!(
      &*log.borrow(),
      &[
        "script:A".to_string(),
        "pre".to_string(),
        "microtask:A".to_string()
      ]
    );
    Ok(())
  }

  #[test]
  fn empty_parser_inserted_script_can_execute_after_later_text_mutation_via_best_effort_scheduling() -> Result<()> {
    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let (mut host, mut event_loop) =
      build_host("<!doctype html><html><body><script id=s></script></body></html>", Rc::clone(&log))?;

    let mut discovered = host.discover_scripts_best_effort(None);
    assert_eq!(discovered.len(), 1);
    let (script, spec) = discovered.pop().unwrap();

    host.register_and_schedule_script(script, spec.clone(), spec.base_url.clone(), &mut event_loop)?;

    // Empty parser-inserted scripts must not execute during preparation.
    assert!(log.borrow().is_empty());

    let node = host.dom().node(script);
    assert!(
      !node.script_already_started,
      "empty parser-inserted script must not be marked already started"
    );
    assert!(
      !node.script_parser_document,
      "empty parser-inserted script must clear parser document so later mutations can run it"
    );
    assert!(
      node.script_force_async,
      "empty parser-inserted script must set force-async so later src mutation is async-by-default"
    );

    // Simulate a later DOM mutation that provides inline script text.
    host.mutate_dom(|dom| {
      let text = dom.create_text("A");
      dom.append_child(script, text).expect("append_child");
      ((), true)
    });

    host.discover_dynamic_scripts(&mut event_loop)?;
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    assert_eq!(
      &*log.borrow(),
      &["script:A".to_string(), "microtask:A".to_string()]
    );
    Ok(())
  }

  #[test]
  fn empty_parser_inserted_script_can_execute_after_later_text_mutation() -> Result<()> {
    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let executor = TestExecutor { log: Rc::clone(&log) };
    let mut tab = BrowserTab::from_html(
      "<!doctype html><html><body><script id=s></script></body></html>",
      RenderOptions::default(),
      executor,
    )?;

    // Empty parser-inserted scripts must not execute during parsing.
    assert!(log.borrow().is_empty());

    let script = tab
      .host
      .dom()
      .get_element_by_id("s")
      .expect("expected #s script to exist");
    let node = tab.host.dom().node(script);
    assert!(
      !node.script_already_started,
      "empty parser-inserted script must not be marked already started"
    );
    assert!(
      !node.script_parser_document,
      "empty parser-inserted script must clear parser document to allow later mutation"
    );
    assert!(
      node.script_force_async,
      "empty parser-inserted script must set force-async so later src mutation is async-by-default"
    );

    // Simulate a later DOM mutation that provides inline script text.
    tab.host.mutate_dom(|dom| {
      let text = dom.create_text("A");
      dom.append_child(script, text).expect("append_child");
      ((), true)
    });

    tab.host.discover_dynamic_scripts(&mut tab.event_loop)?;
    tab
      .event_loop
      .run_until_idle(&mut tab.host, RunLimits::unbounded())?;

    assert_eq!(
      &*log.borrow(),
      &["script:A".to_string(), "microtask:A".to_string()]
    );
    Ok(())
  }

  #[test]
  fn unsupported_type_parser_inserted_script_can_execute_after_type_and_children_mutation() -> Result<()> {
    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let executor = TestExecutor { log: Rc::clone(&log) };
    let mut tab = BrowserTab::from_html(
      "<!doctype html><html><body><script type=\"text/plain\" id=s>A</script></body></html>",
      RenderOptions::default(),
      executor,
    )?;

    // Unsupported-type parser-inserted scripts must not execute during parsing.
    assert!(log.borrow().is_empty());

    let script = tab
      .host
      .dom()
      .get_element_by_id("s")
      .expect("expected #s script to exist");
    let node = tab.host.dom().node(script);
    assert!(
      !node.script_already_started,
      "unsupported-type parser-inserted script must not be marked already started"
    );
    assert!(
      !node.script_parser_document,
      "unsupported-type parser-inserted script must clear parser document to allow later mutation"
    );

    // Mutate the element to become a classic script and change its children, then discover/execute.
    tab.host.mutate_dom(|dom| {
      dom.remove_attribute(script, "type").expect("remove_attribute");
      let text = dom.create_text("B");
      dom.append_child(script, text).expect("append_child");
      ((), true)
    });

    tab.host.discover_dynamic_scripts(&mut tab.event_loop)?;
    tab
      .event_loop
      .run_until_idle(&mut tab.host, RunLimits::unbounded())?;

    assert_eq!(
      &*log.borrow(),
      &["script:AB".to_string(), "microtask:AB".to_string()]
    );
    Ok(())
  }

  #[test]
  fn empty_parser_inserted_script_sets_force_async_for_later_external_script_execution() -> Result<()> {
    struct LoggingExecutor {
      log: Rc<RefCell<Vec<String>>>,
    }

    impl BrowserTabJsExecutor for LoggingExecutor {
      fn execute_classic_script(
        &mut self,
        script_text: &str,
        _spec: &ScriptElementSpec,
        _current_script: Option<NodeId>,
        _document: &mut BrowserDocumentDom2,
        _event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<()> {
        self.log.borrow_mut().push(script_text.to_string());
        Ok(())
      }

      fn execute_module_script(
        &mut self,
        script_text: &str,
        spec: &ScriptElementSpec,
        current_script: Option<NodeId>,
        document: &mut BrowserDocumentDom2,
        event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<()> {
        self.execute_classic_script(script_text, spec, current_script, document, event_loop)
      }
    }

    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let executor = LoggingExecutor { log: Rc::clone(&log) };

    // Parse an empty parser-inserted script. It should not execute, but it should clear the parser
    // document slot and set force-async so it behaves like a dynamically inserted script if mutated
    // later.
    let mut tab = BrowserTab::from_html(
      "<!doctype html><html><body><script id=a></script></body></html>",
      RenderOptions::default(),
      executor,
    )?;
    assert!(log.borrow().is_empty());

    let script_a = tab
      .host
      .dom()
      .get_element_by_id("a")
      .expect("expected #a script");
    assert!(tab.host.dom().node(script_a).script_force_async);
    assert!(!tab.host.dom().node(script_a).script_parser_document);
    assert!(!tab.host.dom().node(script_a).script_already_started);

    // Create a second script that is *not* async-like (`force_async=false`, no `async` attribute) so
    // it executes using the HTML "in-order-asap" rules. We clear force-async by toggling the `async`
    // attribute (sticky per the HTML spec).
    let script_b = tab.host.mutate_dom(|dom| {
      let body = dom.body().expect("expected document.body to exist");

      dom
        .set_attribute(script_a, "src", "a.js")
        .expect("set_attribute");

      let script_b = dom.create_element("script", "");
      dom.set_attribute(script_b, "async", "").expect("set_attribute");
      dom.remove_attribute(script_b, "async").expect("remove_attribute");
      dom.set_attribute(script_b, "src", "b.js").expect("set_attribute");
      dom.append_child(body, script_b).expect("append_child");
      (script_b, true)
    });
    assert!(
      !tab.host.dom().has_attribute(script_b, "async").unwrap_or(false),
      "expected `async` attribute to be removed"
    );
    assert!(
      !tab.host.dom().node(script_b).script_force_async,
      "expected force-async to be cleared for script_b"
    );

    tab.host.discover_dynamic_scripts(&mut tab.event_loop)?;
    // Clear queued networking tasks; we'll drive fetch completion manually to control ordering.
    tab.event_loop.clear_all_pending_work();

    let (id_a, id_b) = {
      let mut id_a = None;
      let mut id_b = None;
      for (id, entry) in &tab.host.scripts {
        if entry.node_id == script_a {
          id_a = Some(*id);
        } else if entry.node_id == script_b {
          id_b = Some(*id);
        }
      }
      (id_a.expect("missing script_id for a.js"), id_b.expect("missing script_id for b.js"))
    };

    // Verify the discovered specs match the internal-slot state.
    assert!(
      tab.host
        .scripts
        .get(&id_a)
        .expect("missing script entry for a.js")
        .spec
        .force_async,
      "expected force_async to be propagated for previously-failed parser-inserted script"
    );
    assert!(
      !tab.host
        .scripts
        .get(&id_b)
        .expect("missing script entry for b.js")
        .spec
        .force_async,
      "expected force_async to be cleared for in-order-asap script"
    );

    // Complete fetch for B first. Because A is async-like (force_async=true), it is not part of the
    // in-order-asap list; therefore B can execute immediately on fetch completion, before A.
    let actions_b = tab.host.scheduler.fetch_completed(id_b, "B".to_string())?;
    tab.host.apply_scheduler_actions(actions_b, &mut tab.event_loop)?;
    let actions_a = tab.host.scheduler.fetch_completed(id_a, "A".to_string())?;
    tab.host.apply_scheduler_actions(actions_a, &mut tab.event_loop)?;

    tab
      .event_loop
      .run_until_idle(&mut tab.host, RunLimits::unbounded())?;
    assert_eq!(&*log.borrow(), &["B".to_string(), "A".to_string()]);
    Ok(())
  }

  struct DomSourceGuard {
    id: u64,
  }

  impl Drop for DomSourceGuard {
    fn drop(&mut self) {
      unregister_dom_source(self.id);
    }
  }

  fn value_to_string(realm: &WindowRealm, value: Value) -> String {
    let Value::String(s) = value else {
      panic!("expected string, got {value:?}");
    };
    realm.heap().get_string(s).unwrap().to_utf8_lossy()
  }

  static NEXT_QUEUE_MICROTASK_HOOK_ID: AtomicU64 = AtomicU64::new(1);
  static QUEUE_MICROTASK_HOOKS: OnceLock<Mutex<HashMap<u64, Arc<AtomicUsize>>>> = OnceLock::new();

  fn queue_microtask_hooks() -> &'static Mutex<HashMap<u64, Arc<AtomicUsize>>> {
    QUEUE_MICROTASK_HOOKS.get_or_init(|| Mutex::new(HashMap::new()))
  }

  thread_local! {
    static MICROTASK_CHECKPOINT_TEST_COUNTER: RefCell<Option<Arc<AtomicUsize>>> = const { RefCell::new(None) };
  }

  fn microtask_checkpoint_counting_hook(
    _host: &mut BrowserTabHost,
    _event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    MICROTASK_CHECKPOINT_TEST_COUNTER.with(|slot| {
      if let Some(counter) = slot.borrow().as_ref() {
        counter.fetch_add(1, Ordering::SeqCst);
      }
    });
    Ok(())
  }

  struct MicrotaskCheckpointTestCounterGuard {
    prev: Option<Arc<AtomicUsize>>,
  }

  impl MicrotaskCheckpointTestCounterGuard {
    fn install(counter: Arc<AtomicUsize>) -> Self {
      let prev = MICROTASK_CHECKPOINT_TEST_COUNTER.with(|slot| slot.borrow_mut().replace(counter));
      Self { prev }
    }
  }

  impl Drop for MicrotaskCheckpointTestCounterGuard {
    fn drop(&mut self) {
      MICROTASK_CHECKPOINT_TEST_COUNTER.with(|slot| {
        *slot.borrow_mut() = self.prev.take();
      });
    }
  }

  struct QueueMicrotaskHookGuard {
    id: u64,
  }

  impl Drop for QueueMicrotaskHookGuard {
    fn drop(&mut self) {
      queue_microtask_hooks()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(&self.id);
    }
  }

  fn queue_microtask_test_native(
    _vm: &mut Vm,
    scope: &mut vm_js::Scope<'_>,
    _host: &mut dyn VmHost,
    _hooks: &mut dyn VmHostHooks,
    callee: GcObject,
    _this: Value,
    _args: &[Value],
  ) -> std::result::Result<Value, VmError> {
    let slots = scope.heap().get_function_native_slots(callee)?;
    let id = match slots.get(0).copied().unwrap_or(Value::Undefined) {
      Value::Number(n) if n.is_finite() && n >= 0.0 && n <= u64::MAX as f64 => n as u64,
      _ => return Err(VmError::TypeError("__queueMicrotaskTest missing hook id slot")),
    };

    let counter = queue_microtask_hooks()
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .get(&id)
      .cloned()
      .ok_or(VmError::TypeError("__queueMicrotaskTest hook id not registered"))?;

    let Some(event_loop) = crate::js::runtime::current_event_loop_mut::<BrowserTabHost>() else {
      return Err(VmError::TypeError(
        "__queueMicrotaskTest called without an active EventLoop",
      ));
    };

    event_loop
      .queue_microtask(move |_host, _event_loop| {
        counter.fetch_add(1, Ordering::SeqCst);
        Ok(())
      })
      .map_err(|_err| VmError::TypeError("__queueMicrotaskTest failed to queue microtask"))?;
    Ok(Value::Undefined)
  }

  fn alloc_key(scope: &mut vm_js::Scope<'_>, name: &str) -> std::result::Result<PropertyKey, VmError> {
    let s = scope.alloc_string(name)?;
    scope.push_root(Value::String(s))?;
    Ok(PropertyKey::from_string(s))
  }

  #[test]
  fn streaming_parser_does_not_double_run_pre_script_microtask_checkpoint() -> Result<()> {
    // The streaming parser driver performs a microtask checkpoint at `</script>` boundaries before
    // preparing the parser-inserted script, and then performs the post-script checkpoint after
    // executing. Ensure we don't accidentally run the pre-script checkpoint twice (which would be a
    // spec-shaped ordering hazard once additional features start queueing microtasks during script
    // preparation).
    let counter = Arc::new(AtomicUsize::new(0));
    let _guard = MicrotaskCheckpointTestCounterGuard::install(Arc::clone(&counter));

    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let executor = TestExecutor { log };
    let mut tab = BrowserTab::from_html("", RenderOptions::default(), executor)?;
    tab
      .event_loop
      .set_microtask_checkpoint_hook(Some(microtask_checkpoint_counting_hook));
    tab.host.reset_scripting_state(None, ReferrerPolicy::default())?;

    // Use a small budget that still reaches the first `</script>` boundary in the initial parse
    // call (first pump requests input, second pump yields the script boundary).
    tab.host.js_execution_options.dom_parse_budget = crate::js::ParseBudget::new(2);

    let _ = tab.parse_html_streaming_and_schedule_scripts("<script>A</script>", None, &RenderOptions::default())?;

    assert_eq!(
      counter.load(Ordering::SeqCst),
      2,
      "expected exactly one pre-script + one post-script microtask checkpoint during initial streaming parse"
    );
    Ok(())
  }

  fn record_host_native(
    _vm: &mut Vm,
    _scope: &mut vm_js::Scope<'_>,
    host: &mut dyn VmHost,
    _hooks: &mut dyn VmHostHooks,
    _callee: GcObject,
    _this: Value,
    _args: &[Value],
  ) -> std::result::Result<Value, VmError> {
    if host.as_any_mut().downcast_mut::<BrowserDocumentDom2>().is_some() {
      Ok(Value::Bool(true))
    } else {
      Err(VmError::TypeError(
        "recordHost called without the embedder BrowserDocumentDom2 VmHost context",
      ))
    }
  }

  fn install_record_host_global(realm: &mut WindowRealm) -> std::result::Result<(), VmError> {
    let (vm, realm_ref, heap) = realm.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let global = realm_ref.global_object();
    scope.push_root(Value::Object(global))?;

    let call_id = vm.register_native_call(record_host_native)?;
    let name_s = scope.alloc_string("recordHost")?;
    scope.push_root(Value::String(name_s))?;

    let func = scope.alloc_native_function(call_id, None, name_s, 0)?;
    scope
      .heap_mut()
      .object_set_prototype(func, Some(realm_ref.intrinsics().function_prototype()))?;
    scope.push_root(Value::Object(func))?;

    let key = PropertyKey::from_string(name_s);
    scope.define_property(global, key, data_desc(Value::Object(func)))?;
    Ok(())
  }

  fn data_desc(value: Value) -> PropertyDescriptor {
    PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Data {
        value,
        writable: true,
      },
    }
  }

  #[test]
  fn host_dispatched_click_event_listener_runs_with_real_vm_host() -> Result<()> {
    let mut tab = BrowserTab::from_html_with_vmjs_executor(
      "<!doctype html><html><body></body></html>",
      RenderOptions::default(),
    )?;

    // Create a clickable target node.
    let button_id = tab.host.mutate_dom(|dom| {
      let button = dom.create_element("button", "");
      dom.set_attribute(button, "id", "btn").expect("set_attribute");
      let body = dom.body().expect("expected <body>");
      dom.append_child(body, button).expect("append_child");
      (button, true)
    });

    // Install `recordHost()` in the vm-js realm.
    {
      let realm = tab
        .host
        .executor
        .window_realm_mut()
        .expect("vm-js executor should expose a WindowRealm");
      install_record_host_global(realm).map_err(|err| Error::Other(err.to_string()))?;
    }

    // Add a JS click handler that requires a real `BrowserDocumentDom2` VmHost context.
    {
      let (host, event_loop) = (&mut tab.host, &mut tab.event_loop);
      with_event_loop(event_loop, || {
        let (document, executor) = (&mut host.document, &mut host.executor);
        let realm = executor
          .window_realm_mut()
          .expect("vm-js executor should expose a WindowRealm");
        let host_ctx: &mut dyn VmHost = document.as_mut();
        let mut hooks = VmJsEventLoopHooks::<BrowserTabHost>::new(host_ctx);
        realm
          .exec_script_with_host_and_hooks(
            host_ctx,
            &mut hooks,
            r#"
            globalThis.__host_ok = false;
            window.addEventListener('click', () => { globalThis.__host_ok = recordHost(); });
            "#,
          )
          .map_err(|err| Error::Other(err.to_string()))?;
        if let Some(err) = hooks.finish(realm.heap_mut()) {
          return Err(err);
        }
        Ok(())
      })?;
    }

    // Host dispatch: should invoke the JS listener with a real vm-js `VmHost`.
    tab.dispatch_click_event(button_id)?;

    let realm = tab
      .host
      .executor
      .window_realm_mut()
      .expect("vm-js executor should expose a WindowRealm");
    let ok = realm.exec_script("globalThis.__host_ok").map_err(|err| Error::Other(err.to_string()))?;
    assert!(matches!(ok, Value::Bool(true)));
    Ok(())
  }

  fn install_queue_microtask_test_global(
    realm: &mut WindowRealm,
    hook_id: u64,
  ) -> std::result::Result<(), VmError> {
    let (vm, realm_ref, heap) = realm.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let global = realm_ref.global_object();
    scope.push_root(Value::Object(global))?;

    let call_id = vm.register_native_call(queue_microtask_test_native)?;
    let name = scope.alloc_string("__queueMicrotaskTest")?;
    scope.push_root(Value::String(name))?;

    let slots = [Value::Number(hook_id as f64)];
    let func = scope.alloc_native_function_with_slots(call_id, None, name, 0, &slots)?;
    scope
      .heap_mut()
      .object_set_prototype(func, Some(realm_ref.intrinsics().function_prototype()))
      ?;
    scope.push_root(Value::Object(func))?;

    let key = alloc_key(&mut scope, "__queueMicrotaskTest")?;
    scope
      .define_property(global, key, data_desc(Value::Object(func)))
      ?;

    Ok(())
  }

  #[derive(Default)]
  struct VmJsLifecycleExecutor {
    dom_source_id: Rc<Cell<Option<u64>>>,
    microtask_hook_id: u64,
    realm: Option<WindowRealm>,
  }

  impl VmJsLifecycleExecutor {
    fn new(dom_source_id: Rc<Cell<Option<u64>>>, microtask_hook_id: u64) -> Self {
      Self {
        dom_source_id,
        microtask_hook_id,
        realm: None,
      }
    }

    fn ensure_realm(&mut self) -> Result<()> {
      if self.realm.is_some() {
        return Ok(());
      }
      let dom_source_id = self
        .dom_source_id
        .get()
        .expect("dom_source_id should be registered before script execution");
      let mut realm = WindowRealm::new(
        WindowRealmConfig::new("https://example.com/").with_dom_source_id(dom_source_id),
      )
      .map_err(|err| Error::Other(err.to_string()))?;
      install_queue_microtask_test_global(&mut realm, self.microtask_hook_id)
        .map_err(|err| Error::Other(err.to_string()))?;
      self.realm = Some(realm);
      Ok(())
    }
  }

  impl BrowserTabJsExecutor for VmJsLifecycleExecutor {
    fn execute_classic_script(
      &mut self,
      script_text: &str,
      _spec: &ScriptElementSpec,
      _current_script: Option<NodeId>,
      _document: &mut BrowserDocumentDom2,
      event_loop: &mut EventLoop<BrowserTabHost>,
    ) -> Result<()> {
      self.ensure_realm()?;
      let realm = self.realm.as_mut().expect("realm should be initialized");
      with_event_loop(event_loop, || realm.exec_script(script_text))
        .map_err(|err| Error::Other(err.to_string()))?;
      Ok(())
    }

    fn execute_module_script(
      &mut self,
      script_text: &str,
      spec: &ScriptElementSpec,
      current_script: Option<NodeId>,
      document: &mut BrowserDocumentDom2,
      event_loop: &mut EventLoop<BrowserTabHost>,
    ) -> Result<()> {
      // These lifecycle tests exercise event dispatch and microtask checkpoints; module-specific
      // semantics are validated by dedicated tests elsewhere. Treat module scripts like classic
      // scripts here so the executor satisfies the `BrowserTabJsExecutor` contract.
      self.execute_classic_script(script_text, spec, current_script, document, event_loop)
    }

    fn dispatch_lifecycle_event(
      &mut self,
      target: EventTargetId,
      event: &Event,
      _document: &mut BrowserDocumentDom2,
    ) -> Result<()> {
      self.ensure_realm()?;
      let realm = self.realm.as_mut().expect("realm should be initialized");

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

      realm
        .exec_script(&source)
        .map_err(|err| Error::Other(err.to_string()))?;
      Ok(())
    }
  }

  #[test]
  fn vm_js_document_ready_state_tracks_document_lifecycle_transitions() -> Result<()> {
    let document = BrowserDocumentDom2::from_html("<!doctype html><html></html>", RenderOptions::default())?;
    let mut host = BrowserTabHost::new(
      document,
      Box::new(NoopExecutor::default()),
      TraceHandle::default(),
      JsExecutionOptions::default(),
    )?;
    let mut event_loop = EventLoop::<BrowserTabHost>::new();

    let dom_source_id = register_dom_source(NonNull::from(host.dom_mut()));
    let _guard = DomSourceGuard { id: dom_source_id };

    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/").with_dom_source_id(dom_source_id))
      .map_err(|err| Error::Other(err.to_string()))?;

    let ready_state = realm
      .exec_script("document.readyState")
      .map_err(|err| Error::Other(err.to_string()))?;
    assert_eq!(value_to_string(&realm, ready_state), "loading");

    host.notify_parsing_completed(&mut event_loop)?;

    let ready_state = realm
      .exec_script("document.readyState")
      .map_err(|err| Error::Other(err.to_string()))?;
    assert_eq!(value_to_string(&realm, ready_state), "interactive");

    assert!(event_loop.run_next_task(&mut host)?);
    let ready_state = realm
      .exec_script("document.readyState")
      .map_err(|err| Error::Other(err.to_string()))?;
    assert_eq!(value_to_string(&realm, ready_state), "interactive");

    assert!(event_loop.run_next_task(&mut host)?);
    let ready_state = realm
      .exec_script("document.readyState")
      .map_err(|err| Error::Other(err.to_string()))?;
    assert_eq!(value_to_string(&realm, ready_state), "interactive");

    assert!(event_loop.run_next_task(&mut host)?);
    let ready_state = realm
      .exec_script("document.readyState")
      .map_err(|err| Error::Other(err.to_string()))?;
    assert_eq!(value_to_string(&realm, ready_state), "complete");
    Ok(())
  }

  #[test]
  fn browser_tab_lifecycle_dispatch_invokes_vm_js_listeners_and_microtasks() -> Result<()> {
    let microtask_hook_id = NEXT_QUEUE_MICROTASK_HOOK_ID.fetch_add(1, Ordering::Relaxed);
    let microtasks_run: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
    queue_microtask_hooks()
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .insert(microtask_hook_id, Arc::clone(&microtasks_run));
    let _hook_guard = QueueMicrotaskHookGuard {
      id: microtask_hook_id,
    };

    let dom_source_id_cell = Rc::new(Cell::new(None));
    let document = BrowserDocumentDom2::from_html("<!doctype html><html></html>", RenderOptions::default())?;
    let executor = VmJsLifecycleExecutor::new(Rc::clone(&dom_source_id_cell), microtask_hook_id);
    let mut host = BrowserTabHost::new(
      document,
      Box::new(executor),
      TraceHandle::default(),
      JsExecutionOptions::default(),
    )?;
    let mut event_loop = EventLoop::<BrowserTabHost>::new();

    let dom_source_id = register_dom_source(NonNull::from(host.dom_mut()));
    let _guard = DomSourceGuard { id: dom_source_id };
    dom_source_id_cell.set(Some(dom_source_id));

    // Register a JS listener that queues a microtask via the test-only native helper.
    let setup_script = "document.addEventListener('readystatechange', () => { __queueMicrotaskTest(); });";
    let spec = ScriptElementSpec {
      base_url: Some("https://example.com/".to_string()),
      src: None,
      src_attr_present: false,
      inline_text: setup_script.to_string(),
      async_attr: false,
      defer_attr: false,
      nomodule_attr: false,
      crossorigin: None,
      integrity_attr_present: false,
      integrity: None,
      referrer_policy: None,
      parser_inserted: false,
      force_async: false,
      node_id: None,
      script_type: ScriptType::Classic,
    };
    let current_script = host.current_script_node();
    let (executor, document) = (&mut host.executor, &mut host.document);
    executor.execute_classic_script(
      setup_script,
      &spec,
      current_script,
      document,
      &mut event_loop,
    )?;

    host.notify_parsing_completed(&mut event_loop)?;

    assert_eq!(
      microtasks_run.load(Ordering::SeqCst),
      1,
      "expected readystatechange listener to enqueue a microtask that runs during parsing completion"
    );
    Ok(())
  }

  #[test]
  fn browser_tab_rust_dom_event_dispatch_invokes_vm_js_promise_microtasks() -> Result<()> {
    let html = r#"<!doctype html>
      <html>
        <body>
          <div id="target"></div>
          <script>
            globalThis.__ran = false;
            document.getElementById("target").addEventListener("click", () => {
              Promise.resolve().then(() => { globalThis.__ran = true; });
            });
          </script>
        </body>
      </html>"#;
    let mut tab = BrowserTab::from_html_with_vmjs_executor(html, RenderOptions::default())?;

    let target = tab
      .dom()
      .get_element_by_id("target")
      .expect("expected #target element");

    tab.dispatch_click_event(target)?;

    // Promise jobs should not run synchronously as part of Rust-driven event dispatch.
    {
      let realm = tab
        .host
        .executor
        .window_realm_mut()
        .expect("expected vm-js WindowRealm");
      let ran = realm
        .exec_script("globalThis.__ran")
        .map_err(|err| Error::Other(err.to_string()))?;
      assert!(
        matches!(ran, Value::Bool(false)),
        "expected __ran=false before microtask checkpoint, got {ran:?}"
      );
    }

    tab.event_loop.perform_microtask_checkpoint(&mut tab.host)?;

    {
      let realm = tab
        .host
        .executor
        .window_realm_mut()
        .expect("expected vm-js WindowRealm");
      let ran = realm
        .exec_script("globalThis.__ran")
        .map_err(|err| Error::Other(err.to_string()))?;
      assert!(
        matches!(ran, Value::Bool(true)),
        "expected __ran=true after microtask checkpoint, got {ran:?}"
      );
    }

    Ok(())
  }

  #[test]
  fn js_document_write_inserts_html_before_following_markup_and_affects_render() -> Result<()> {
    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let executor = WindowRealmExecutor::new(Rc::clone(&log))?;
    let html = r#"<!doctype html><html><head><style>
html, body { margin: 0; padding: 0; }
#x { width: 64px; height: 64px; background: rgb(255, 0, 0); }
#after { width: 64px; height: 64px; background: rgb(0, 0, 255); }
</style></head><body><script>document.write('<div id="x"></div>')</script><div id="after"></div></body></html>"#;
    let options = RenderOptions::new().with_viewport(64, 64);
    let mut tab = BrowserTab::from_html(html, options, executor)?;

    assert_eq!(log.borrow().len(), 1, "expected exactly one script execution");

    let doc = tab.dom();
    let body = doc.body().expect("missing <body>");
    let injected = doc
      .get_element_by_id("x")
      .expect("expected element inserted by document.write");
    let after = doc
      .get_element_by_id("after")
      .expect("expected #after element");

    let element_children: Vec<NodeId> = doc
      .node(body)
      .children
      .iter()
      .copied()
      .filter(|&id| matches!(doc.node(id).kind, NodeKind::Element { .. }))
      .collect();
    assert_eq!(
      element_children.len(),
      3,
      "expected <body> to have <script>, injected <div>, and following <div>"
    );
    assert_eq!(element_children[1], injected, "expected injected node after <script>");
    assert_eq!(element_children[2], after, "expected #after node after injected markup");

    let pixmap = tab.render_frame()?;
    assert_eq!(rgba_at(&pixmap, 32, 32), [255, 0, 0, 255]);
    Ok(())
  }

  #[test]
  fn js_document_write_preserves_call_order_across_multiple_calls() -> Result<()> {
    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let executor = WindowRealmExecutor::new(Rc::clone(&log))?;
    let html = r#"<!doctype html><html><body><script>
document.write('<div id="a"></div>');
document.write('<div id="b"></div>');
</script><div id="after"></div></body></html>"#;
    let mut tab = BrowserTab::from_html(html, RenderOptions::default(), executor)?;

    assert_eq!(log.borrow().len(), 1, "expected exactly one script execution");

    let doc = tab.dom();
    let body = doc.body().expect("missing <body>");
    let a = doc
      .get_element_by_id("a")
      .expect("expected #a element inserted by document.write");
    let b = doc
      .get_element_by_id("b")
      .expect("expected #b element inserted by document.write");
    let after = doc
      .get_element_by_id("after")
      .expect("expected #after element");

    let element_children: Vec<NodeId> = doc
      .node(body)
      .children
      .iter()
      .copied()
      .filter(|&id| matches!(doc.node(id).kind, NodeKind::Element { .. }))
      .collect();
    assert_eq!(element_children.len(), 4, "expected 4 element children in <body>");
    assert_eq!(element_children[1], a, "expected #a to be first injected element");
    assert_eq!(element_children[2], b, "expected #b to be second injected element");
    assert_eq!(
      element_children[3], after,
      "expected following markup to appear after injected elements"
    );
    Ok(())
  }

  #[test]
  fn js_document_writeln_appends_newline_and_parses_markup() -> Result<()> {
    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let executor = WindowRealmExecutor::new(Rc::clone(&log))?;
    let html = r#"<!doctype html><html><body><script>
document.writeln('<div id="x"></div>');
</script><div id="after"></div></body></html>"#;
    let mut tab = BrowserTab::from_html(html, RenderOptions::default(), executor)?;

    assert_eq!(log.borrow().len(), 1, "expected exactly one script execution");

    let doc = tab.dom();
    let body = doc.body().expect("missing <body>");
    let injected = doc
      .get_element_by_id("x")
      .expect("expected element inserted by document.writeln");
    let after = doc
      .get_element_by_id("after")
      .expect("expected #after element");

    let element_children: Vec<NodeId> = doc
      .node(body)
      .children
      .iter()
      .copied()
      .filter(|&id| matches!(doc.node(id).kind, NodeKind::Element { .. }))
      .collect();
    assert_eq!(element_children.len(), 3, "expected 3 element children in <body>");
    assert_eq!(element_children[1], injected);
    assert_eq!(element_children[2], after);
    Ok(())
  }

  #[test]
  fn js_document_write_injected_scripts_execute_before_later_scripts() -> Result<()> {
    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let executor = WindowRealmExecutor::new(Rc::clone(&log))?;
    let html = r#"<!doctype html><html><body><script>document.write('<script>window.__injected = 1;<\/script>');</script><script>window.__after = window.__injected;</script></body></html>"#;
    let _tab = BrowserTab::from_html(html, RenderOptions::default(), executor)?;

    assert_eq!(
      &*log.borrow(),
      &[
        r"script:document.write('<script>window.__injected = 1;<\/script>');".to_string(),
        "script:window.__injected = 1;".to_string(),
        "script:window.__after = window.__injected;".to_string(),
      ],
      "expected injected <script> to execute before later scripts"
    );
    Ok(())
  }

  #[test]
  fn js_document_write_budget_enforced_and_does_not_mutate_dom() -> Result<()> {
    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let executor = WindowRealmExecutor::new(Rc::clone(&log))?;
    let js_options = JsExecutionOptions {
      max_document_write_bytes_per_call: 4,
      ..JsExecutionOptions::default()
    };
    let html = r#"<!doctype html><html><body><script>
document.write('<div id="x"></div>');
</script><div id="after"></div></body></html>"#;
    let mut tab = BrowserTab::from_html_with_js_execution_options(
      html,
      RenderOptions::default(),
      executor,
      js_options,
    )?;

    assert_eq!(log.borrow().len(), 1, "expected the script to execute once");
    assert!(
      tab.dom().get_element_by_id("x").is_none(),
      "expected injected markup to be rejected when over budget"
    );
    assert!(
      tab.dom().get_element_by_id("after").is_some(),
      "expected parsing to continue after document.write budget error"
    );
    Ok(())
  }

  #[test]
  fn js_document_write_is_noop_when_no_active_parser() -> Result<()> {
    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let executor = WindowRealmExecutor::new(Rc::clone(&log))?;
    let html = r#"<!doctype html><html><head><style>
html, body { margin: 0; padding: 0; }
#after { width: 64px; height: 64px; background: rgb(0, 0, 255); }
</style></head><body><div id="after"></div><script async src="https://example.com/async.js"></script></body></html>"#;
    let options = RenderOptions::new().with_viewport(64, 64);
    let mut tab = BrowserTab::from_html(html, options, executor)?;
    tab.register_script_source(
      "https://example.com/async.js",
      r#"document.write('<div id="x"></div>');"#,
    );

    let pixmap = tab.render_frame()?;
    assert_eq!(rgba_at(&pixmap, 32, 32), [0, 0, 255, 255]);

    assert_eq!(
      tab.run_event_loop_until_idle(RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(log.borrow().len(), 1, "expected async script to execute");

    assert!(
      tab.dom().get_element_by_id("x").is_none(),
      "document.write from an async script should not insert markup without an active parser"
    );
    if let Some(pixmap_after) = tab.render_if_needed()? {
      assert_eq!(
        rgba_at(&pixmap_after, 32, 32),
        [0, 0, 255, 255],
        "unexpected visual change after async script execution"
      );
    }
    Ok(())
  }

  struct VmJsExecutor {
    dom_source_id: Rc<Cell<Option<u64>>>,
    result: Rc<RefCell<Option<Value>>>,
    realm: Option<WindowRealm>,
  }

  impl VmJsExecutor {
    fn new(dom_source_id: Rc<Cell<Option<u64>>>, result: Rc<RefCell<Option<Value>>>) -> Self {
      Self {
        dom_source_id,
        result,
        realm: None,
      }
    }
  }

  impl BrowserTabJsExecutor for VmJsExecutor {
    fn execute_classic_script(
      &mut self,
      script_text: &str,
      _spec: &ScriptElementSpec,
      _current_script: Option<NodeId>,
      _document: &mut BrowserDocumentDom2,
      _event_loop: &mut EventLoop<BrowserTabHost>,
    ) -> Result<()> {
      if self.realm.is_none() {
        let dom_source_id = self
          .dom_source_id
          .get()
          .expect("dom_source_id should be registered before script execution");
        let realm = WindowRealm::new(
          WindowRealmConfig::new("https://example.com/").with_dom_source_id(dom_source_id),
        )
        .map_err(|err| Error::Other(err.to_string()))?;
        self.realm = Some(realm);
      }

      let realm = self.realm.as_mut().expect("initialized realm");
      let value = realm
        .exec_script(script_text)
        .map_err(|err| Error::Other(err.to_string()))?;
      *self.result.borrow_mut() = Some(value);
      Ok(())
    }

    fn execute_module_script(
      &mut self,
      script_text: &str,
      spec: &ScriptElementSpec,
      current_script: Option<NodeId>,
      document: &mut BrowserDocumentDom2,
      event_loop: &mut EventLoop<BrowserTabHost>,
    ) -> Result<()> {
      self.execute_classic_script(script_text, spec, current_script, document, event_loop)
    }
  }

  #[test]
  fn dom2_document_address_is_stable_across_tab_move_for_vm_js_shims() -> Result<()> {
    let dom_source_id_cell = Rc::new(Cell::new(None));
    let script_result = Rc::new(RefCell::new(None));

    let executor = VmJsExecutor::new(Rc::clone(&dom_source_id_cell), Rc::clone(&script_result));

    let html = "<!doctype html><html><body><div id=target></div>\
      <script src=\"https://example.com/app.js\" defer></script>\
      </body></html>";
    let mut tab = BrowserTab::from_html(html, RenderOptions::default(), executor)?;
    tab.register_script_source(
      "https://example.com/app.js",
      "(() => {\n\
        const el = document.getElementById('target');\n\
        return el && el.id === 'target';\n\
      })()",
    );

    let dom_ptr = tab.host.document.dom_non_null();
    let dom_ptr_addr = dom_ptr.as_ptr() as usize;
    let dom_source_id = register_dom_source(dom_ptr);
    let _guard = DomSourceGuard { id: dom_source_id };
    dom_source_id_cell.set(Some(dom_source_id));

    let mut tabs = Vec::new();
    tabs.push(tab);

    // The DOM pointer registered in TLS must remain stable even when the entire `BrowserTab` is
    // moved (e.g. into a `Vec`), because vm-js DOM shims dereference it.
    let ptr_in_vec = tabs[0].host.document.dom() as *const crate::dom2::Document as usize;
    assert_eq!(dom_ptr_addr, ptr_in_vec, "dom2::Document moved in memory");

    tabs[0].run_event_loop_until_idle(RunLimits::unbounded())?;

    assert_eq!(*script_result.borrow(), Some(Value::Bool(true)));
    Ok(())
  }

  fn exec_vm_js_dom_script(tab: &mut BrowserTab, source: &str) {
    // The vm-js DOM shims store a `dom_source_id` that resolves to a pointer in a thread-local
    // registry. Use BrowserDocumentDom2's registration helper so DOM mutations route through its
    // `DomHost::mutate_dom` implementation (which coalesces invalidation and skips no-op renders).
    let dom_source_id = tab.host.document.ensure_dom_source_registered();
    assert!(
      crate::js::window_realm::is_dom_host_source_registered(dom_source_id),
      "expected BrowserDocumentDom2 to register a DomHost source for vm-js DOM shims"
    );

    let mut realm = WindowRealm::new(
      WindowRealmConfig::new("https://example.com/").with_dom_source_id(dom_source_id),
    )
    .expect("WindowRealm");
    realm.exec_script(source).expect("exec_script");
  }

  #[test]
  fn vm_js_dom_shim_mutations_invalidate_browser_document_dom2() -> Result<()> {
    let mut tab = BrowserTab::from_html(
      "<!doctype html><html><body><div id=target class=a data-foo=1 style=\"display: block\">hello</div></body></html>",
      RenderOptions::new().with_viewport(64, 64),
      NoopExecutor::default(),
    )?;

    // Initial render clears dirty flags and records the current DOM mutation generation.
    assert!(tab.render_if_needed()?.is_some());
    assert!(tab.render_if_needed()?.is_none());

    // No-op mutations should not invalidate rendering.
    exec_vm_js_dom_script(
      &mut tab,
      "(() => {\n\
        const el = document.getElementById('target');\n\
        el.classList.add('a');\n\
        el.dataset.foo = '1';\n\
        el.style.display = 'block';\n\
      })();",
    );
    assert!(
      tab.render_if_needed()?.is_none(),
      "expected no-op reflected attribute writes to avoid invalidation"
    );

    exec_vm_js_dom_script(&mut tab, "document.documentElement.className = 'x';");

    // `run_until_stable` must observe that the document is dirty (via the mutation generation) and
    // render a frame before reporting stability.
    let mut limits = tab.js_execution_options().event_loop_run_limits;
    // Default run limits include a conservative wall-time budget; bump it to avoid spurious failures
    // on slow CI machines while keeping the frame cap as a safety net.
    limits.max_wall_time = Some(std::time::Duration::from_secs(2));
    match tab.run_until_stable_with_run_limits(limits, 10)? {
      RunUntilStableOutcome::Stable { frames_rendered } => {
        assert!(
          frames_rendered > 0,
          "expected DOM mutation to trigger a render before reaching Stable"
        );
      }
      other => panic!("expected Stable after rendering, got {other:?}"),
    }
    // Drain any buffered frame produced by `run_until_stable` so later assertions only observe
    // renders triggered by subsequent mutations.
    assert!(tab.render_if_needed()?.is_some());
    assert!(tab.render_if_needed()?.is_none());

    exec_vm_js_dom_script(
      &mut tab,
      "document.body.innerHTML = '<p id=changed>changed</p>';",
    );

    assert!(
      tab.render_if_needed()?.is_some(),
      "expected BrowserTab::render_if_needed to produce a new frame after JS DOM mutation"
    );
    assert!(tab.render_if_needed()?.is_none());

    // Exercise a mutation that removes children without inserting any replacement nodes (empty
    // `textContent`). This previously bypassed generation tracking for raw-pointer shims because it
    // edited `Node.children` directly without calling higher-level mutation APIs.
    exec_vm_js_dom_script(&mut tab, "document.body.textContent = '';");
    assert!(
      tab.render_if_needed()?.is_some(),
      "expected BrowserTab::render_if_needed to produce a new frame after JS DOM mutation (textContent clear)"
    );
    assert!(tab.render_if_needed()?.is_none());

    Ok(())
  }

  #[derive(Clone)]
  struct LoggingFileFetcher {
    log: Arc<Mutex<Vec<String>>>,
  }

  impl LoggingFileFetcher {
    fn read_file_url(&self, url: &str) -> Result<crate::resource::FetchedResource> {
      let parsed =
        Url::parse(url).map_err(|err| Error::Other(format!("invalid test URL {url:?}: {err}")))?;
      if parsed.scheme() != "file" {
        return Err(Error::Other(format!(
          "test fetcher only supports file:// URLs (got {url})"
        )));
      }
      let path = parsed
        .to_file_path()
        .map_err(|()| Error::Other(format!("invalid file:// URL path: {url}")))?;
      let bytes = std::fs::read(&path).map_err(Error::Io)?;
      let content_type = match path.extension().and_then(|ext| ext.to_str()) {
        Some("css") => Some("text/css".to_string()),
        Some("js") => Some("text/javascript".to_string()),
        Some("html") => Some("text/html".to_string()),
        _ => None,
      };
      Ok(crate::resource::FetchedResource::with_final_url(
        bytes,
        content_type,
        Some(url.to_string()),
      ))
    }
  }

  impl crate::resource::ResourceFetcher for LoggingFileFetcher {
    fn fetch(&self, url: &str) -> Result<crate::resource::FetchedResource> {
      self
        .log
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(format!("fetch:{:?}:{url}", FetchDestination::Other));
      self.read_file_url(url)
    }

    fn fetch_with_request(
      &self,
      req: FetchRequest<'_>,
    ) -> Result<crate::resource::FetchedResource> {
      self
        .log
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(format!("fetch:{:?}:{}", req.destination, req.url));
      self.read_file_url(req.url)
    }
  }

  struct LoggingExecutor {
    log: Arc<Mutex<Vec<String>>>,
  }

  impl BrowserTabJsExecutor for LoggingExecutor {
    fn execute_classic_script(
      &mut self,
      script_text: &str,
      _spec: &ScriptElementSpec,
      _current_script: Option<NodeId>,
      _document: &mut BrowserDocumentDom2,
      _event_loop: &mut EventLoop<BrowserTabHost>,
    ) -> Result<()> {
      self
        .log
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(format!("exec:{script_text}"));
      Ok(())
    }

    fn execute_module_script(
      &mut self,
      script_text: &str,
      spec: &ScriptElementSpec,
      current_script: Option<NodeId>,
      document: &mut BrowserDocumentDom2,
      event_loop: &mut EventLoop<BrowserTabHost>,
    ) -> Result<()> {
      self.execute_classic_script(script_text, spec, current_script, document, event_loop)
    }
  }

  fn log_index(log: &[String], needle: &str) -> Option<usize> {
    log.iter().position(|line| line.contains(needle))
  }

  #[test]
  fn parser_inserted_external_script_waits_for_script_blocking_stylesheet_and_imports() -> Result<()> {
    let temp = tempdir().map_err(Error::Io)?;
    std::fs::write(temp.path().join("imported.css"), "body { color: red; }")
      .map_err(Error::Io)?;
    std::fs::write(
      temp.path().join("style.css"),
      r#"@import "imported.css";"#,
    )
    .map_err(Error::Io)?;
    std::fs::write(temp.path().join("script.js"), "SCRIPT").map_err(Error::Io)?;

    let document_url = Url::from_file_path(temp.path().join("index.html"))
      .map_err(|()| Error::Other("failed to build file:// document URL".to_string()))?
      .to_string();

    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let fetcher = Arc::new(LoggingFileFetcher {
      log: Arc::clone(&log),
    });
    let renderer = crate::FastRender::builder()
      .dom_scripting_enabled(true)
      .fetcher(fetcher)
      .build()?;
    let options = RenderOptions::default();
    let document = BrowserDocumentDom2::new(renderer, "", options.clone())?;
    let host = BrowserTabHost::new(
      document,
      Box::new(LoggingExecutor {
        log: Arc::clone(&log),
      }),
      TraceHandle::default(),
      JsExecutionOptions::default(),
    )?;
    let mut tab = BrowserTab {
      trace: TraceHandle::default(),
      trace_output: None,
      diagnostics: None,
      host,
      event_loop: EventLoop::new(),
      pending_frame: None,
      history: TabHistory::new(),
    };
    tab
      .host
      .reset_scripting_state(Some(document_url.clone()), ReferrerPolicy::default())?;

    let html = r#"<!doctype html>
      <link rel="stylesheet" href="style.css">
      <script src="script.js"></script>
    "#;
    tab.parse_html_streaming_and_schedule_scripts(html, Some(&document_url), &options)?;
    let _ = tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    let entries = log
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .clone();
    let exec_idx = log_index(&entries, "exec:SCRIPT").expect("expected script execution");
    let style_idx = log_index(&entries, "style.css").expect("expected stylesheet fetch");
    let imported_idx = log_index(&entries, "imported.css").expect("expected import fetch");

    assert!(
      style_idx < exec_idx,
      "expected style.css fetch before exec; log={entries:?}"
    );
    assert!(
      imported_idx < exec_idx,
      "expected imported.css fetch before exec; log={entries:?}"
    );
    Ok(())
  }

  #[test]
  fn parser_inserted_inline_script_waits_for_script_blocking_stylesheet() -> Result<()> {
    let temp = tempdir().map_err(Error::Io)?;
    std::fs::write(temp.path().join("style.css"), "body { color: red; }").map_err(Error::Io)?;

    let document_url = Url::from_file_path(temp.path().join("index.html"))
      .map_err(|()| Error::Other("failed to build file:// document URL".to_string()))?
      .to_string();

    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let fetcher = Arc::new(LoggingFileFetcher {
      log: Arc::clone(&log),
    });
    let renderer = crate::FastRender::builder()
      .dom_scripting_enabled(true)
      .fetcher(fetcher)
      .build()?;
    let options = RenderOptions::default();
    let document = BrowserDocumentDom2::new(renderer, "", options.clone())?;
    let host = BrowserTabHost::new(
      document,
      Box::new(LoggingExecutor {
        log: Arc::clone(&log),
      }),
      TraceHandle::default(),
      JsExecutionOptions::default(),
    )?;
    let mut tab = BrowserTab {
      trace: TraceHandle::default(),
      trace_output: None,
      diagnostics: None,
      host,
      event_loop: EventLoop::new(),
      pending_frame: None,
      history: TabHistory::new(),
    };
    tab
      .host
      .reset_scripting_state(Some(document_url.clone()), ReferrerPolicy::default())?;

    let html = r#"<!doctype html>
      <link rel="stylesheet" href="style.css">
      <script>INLINE</script>
    "#;
    tab.parse_html_streaming_and_schedule_scripts(html, Some(&document_url), &options)?;

    let entries_before = log
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .clone();
    assert!(
      log_index(&entries_before, "exec:INLINE").is_none(),
      "expected inline script to be delayed until stylesheet load; log={entries_before:?}"
    );

    let _ = tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    let entries = log
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .clone();
    let exec_idx = log_index(&entries, "exec:INLINE").expect("expected script execution");
    let style_idx = log_index(&entries, "style.css").expect("expected stylesheet fetch");
    assert!(
      style_idx < exec_idx,
      "expected style.css fetch before exec; log={entries:?}"
    );
    Ok(())
  }

  #[test]
  fn async_script_executes_even_with_pending_script_blocking_stylesheet() -> Result<()> {
    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let (mut host, mut event_loop) = build_host("<script async src=\"https://example.com/a.js\"></script>", Rc::clone(&log))?;

    // Simulate a streaming parse context so any accidental stylesheet-gating logic that only
    // applies while parsing would be exercised here.
    host.streaming_parse = Some(StreamingParseState {
      parser: StreamingHtmlParser::new(None),
      input: String::new(),
      input_offset: 0,
      eof_set: false,
      deadline: None,
      parse_task_scheduled: false,
      resume_task_scheduled: false,
      host_snapshot_committed: false,
      last_synced_host_dom_generation: 0,
    });
    host.streaming_parse_active = true;

    // Simulate a pending script-blocking stylesheet. Async scripts must not be delayed by this.
    host.script_blocking_stylesheets.register_blocking_stylesheet(0);

    host.register_external_script_source("https://example.com/a.js".to_string(), "ASYNC".to_string());

    let mut discovered = host.discover_scripts_best_effort(None);
    assert_eq!(discovered.len(), 1);
    let (node_id, spec) = discovered.pop().unwrap();
    let base_url_at_discovery = spec.base_url.clone();
    host.register_and_schedule_script(node_id, spec, base_url_at_discovery, &mut event_loop)?;

    let _ = event_loop.run_until_idle_handling_errors(&mut host, RunLimits::unbounded(), |_err| {})?;

    assert!(
      log.borrow().iter().any(|line| line == "script:ASYNC"),
      "expected async script to execute even with pending stylesheet; log={:?}",
      &*log.borrow()
    );
    Ok(())
  }

  #[test]
  fn template_contents_do_not_register_script_blocking_stylesheets_or_scripts() -> Result<()> {
    let temp = tempdir().map_err(Error::Io)?;
    std::fs::write(temp.path().join("style.css"), "body { color: red; }").map_err(Error::Io)?;

    let document_url = Url::from_file_path(temp.path().join("index.html"))
      .map_err(|()| Error::Other("failed to build file:// document URL".to_string()))?
      .to_string();

    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let fetcher = Arc::new(LoggingFileFetcher {
      log: Arc::clone(&log),
    });
    let renderer = crate::FastRender::builder()
      .dom_scripting_enabled(true)
      .fetcher(fetcher)
      .build()?;
    let options = RenderOptions::default();
    let document = BrowserDocumentDom2::new(renderer, "", options.clone())?;
    let host = BrowserTabHost::new(
      document,
      Box::new(LoggingExecutor {
        log: Arc::clone(&log),
      }),
      TraceHandle::default(),
      JsExecutionOptions::default(),
    )?;
    let mut tab = BrowserTab {
      trace: TraceHandle::default(),
      trace_output: None,
      diagnostics: None,
      host,
      event_loop: EventLoop::new(),
      pending_frame: None,
      history: TabHistory::new(),
    };
    tab
      .host
      .reset_scripting_state(Some(document_url.clone()), ReferrerPolicy::default())?;

    let html = r#"<!doctype html>
      <template>
        <link rel="stylesheet" href="style.css">
        <script>TEMPLATE</script>
      </template>
      <script>MAIN</script>
    "#;
    tab.parse_html_streaming_and_schedule_scripts(html, Some(&document_url), &options)?;

    let entries = log
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .clone();
    assert!(
      log_index(&entries, "exec:MAIN").is_some(),
      "expected main script execution; log={entries:?}"
    );
    assert!(
      log_index(&entries, "exec:TEMPLATE").is_none(),
      "expected template-contained script to be inert; log={entries:?}"
    );

    let _ = tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    let entries = log
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .clone();
    assert!(
      log_index(&entries, "style.css").is_none(),
      "expected template-contained stylesheet link to be ignored; log={entries:?}"
    );
    Ok(())
  }

  #[test]
  fn navigate_to_url_executes_parser_inserted_scripts_with_partial_dom() -> Result<()> {
    struct DomSnapshotExecutor {
      log: Rc<RefCell<Vec<String>>>,
    }

    impl BrowserTabJsExecutor for DomSnapshotExecutor {
      fn execute_classic_script(
        &mut self,
        script_text: &str,
        _spec: &ScriptElementSpec,
        _current_script: Option<NodeId>,
        document: &mut BrowserDocumentDom2,
        _event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<()> {
        let dom = document.dom();
        let has_before = dom.get_element_by_id("before").is_some();
        let has_after = dom.get_element_by_id("after").is_some();
        self
          .log
          .borrow_mut()
          .push(format!("{script_text}:before={has_before} after={has_after}"));
        Ok(())
      }

      fn execute_module_script(
        &mut self,
        script_text: &str,
        spec: &ScriptElementSpec,
        current_script: Option<NodeId>,
        document: &mut BrowserDocumentDom2,
        event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<()> {
        self.execute_classic_script(script_text, spec, current_script, document, event_loop)
      }
    }

    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let executor = DomSnapshotExecutor { log: Rc::clone(&log) };
    let mut tab = BrowserTab::from_html("", RenderOptions::default(), executor)?;

    let dir = tempdir().map_err(Error::Io)?;
    let file_path = dir.path().join("index.html");
    std::fs::write(
      &file_path,
      "<!doctype html><html><body><div id=before></div><script>OBSERVE</script><div id=after></div></body></html>",
    )
    .map_err(Error::Io)?;
    let file_url = Url::from_file_path(&file_path)
      .map_err(|()| Error::Other("failed to build file:// document URL".to_string()))?
      .to_string();

    tab.navigate_to_url(&file_url, RenderOptions::default())?;

    assert_eq!(
      &*log.borrow(),
      &["OBSERVE:before=true after=false".to_string()]
    );
    Ok(())
  }

  #[test]
  fn navigate_to_url_base_href_affects_only_later_scripts() -> Result<()> {
    struct SpecLoggingExecutor {
      log: Rc<RefCell<Vec<String>>>,
    }

    impl BrowserTabJsExecutor for SpecLoggingExecutor {
      fn execute_classic_script(
        &mut self,
        script_text: &str,
        spec: &ScriptElementSpec,
        _current_script: Option<NodeId>,
        document: &mut BrowserDocumentDom2,
        _event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<()> {
        let dom = document.dom();
        let mut has_base = false;
        for node_id in dom.subtree_preorder(dom.root()) {
          let NodeKind::Element { tag_name, .. } = &dom.node(node_id).kind else {
            continue;
          };
          if tag_name.eq_ignore_ascii_case("base") {
            has_base = true;
            break;
          }
        }

        self.log.borrow_mut().push(format!(
          "src={};base_present={has_base};text={script_text}",
          spec.src.as_deref().unwrap_or_default()
        ));
        Ok(())
      }

      fn execute_module_script(
        &mut self,
        script_text: &str,
        spec: &ScriptElementSpec,
        current_script: Option<NodeId>,
        document: &mut BrowserDocumentDom2,
        event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<()> {
        self.execute_classic_script(script_text, spec, current_script, document, event_loop)
      }
    }

    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let executor = SpecLoggingExecutor { log: Rc::clone(&log) };
    let mut tab = BrowserTab::from_html("", RenderOptions::default(), executor)?;

    // The `<base>` element appears between two parser-inserted scripts. The first script must run
    // before the `<base>` element is parsed/inserted, while the second script should observe the
    // updated base URL.
    let dir = tempdir().map_err(Error::Io)?;
    let file_path = dir.path().join("index.html");
    std::fs::write(
      &file_path,
      r#"<!doctype html><html><head>
        <script src="a.js"></script>
        <base href="https://example.com/base/">
        <script src="b.js"></script>
      </head><body></body></html>"#,
    )
    .map_err(Error::Io)?;
    let file_url = Url::from_file_path(&file_path)
      .map_err(|()| Error::Other("failed to build file:// document URL".to_string()))?;
    let doc_url = file_url.to_string();

    let a_file_url = file_url.join("a.js").expect("join a.js").to_string();
    let b_file_url = file_url.join("b.js").expect("join b.js").to_string();
    let base = Url::parse("https://example.com/base/").expect("base url");
    let b_base_url = base.join("b.js").expect("join base b.js").to_string();

    tab.register_script_source(a_file_url.clone(), "A_FILE");
    // Register a fallback `file://` b.js source so the test fails via assertions (not I/O) if the
    // base URL logic regresses.
    tab.register_script_source(b_file_url, "B_FILE");
    tab.register_script_source(b_base_url.clone(), "B_BASE");

    tab.navigate_to_url(&doc_url, RenderOptions::default())?;

    assert_eq!(
      &*log.borrow(),
      &[
        format!("src={a_file_url};base_present=false;text=A_FILE"),
        format!("src={b_base_url};base_present=true;text=B_BASE"),
      ]
    );
    Ok(())
  }

  #[test]
  fn navigate_to_url_cancel_callback_aborts_streaming_parse() -> Result<()> {
    // Use a large HTML body so `parse_html_streaming_and_schedule_scripts` performs multiple pump
    // iterations and thus consults the active render deadline more than once.
    let dir = tempdir().map_err(Error::Io)?;
    let file_path = dir.path().join("index.html");
    let big_body = "a".repeat(32 * 1024);
    let html = format!("<!doctype html><html><body>{big_body}</body></html>");
    std::fs::write(&file_path, html).map_err(Error::Io)?;
    let file_url = Url::from_file_path(&file_path)
      .map_err(|()| Error::Other("failed to build file:// document URL".to_string()))?
      .to_string();

    // Install a file:// fetcher that does not perform deadline checks. This ensures the cancel
    // callback is triggered from the streaming HTML parse loop (not the document fetch phase).
    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let fetcher = Arc::new(LoggingFileFetcher {
      log: Arc::clone(&log),
    });
    let renderer = crate::FastRender::builder()
      .dom_scripting_enabled(true)
      .fetcher(fetcher)
      .build()?;
    let base_options = RenderOptions::default();
    let document = BrowserDocumentDom2::new(renderer, "", base_options.clone())?;
    let mut host = BrowserTabHost::new(
      document,
      Box::new(NoopExecutor::default()),
      TraceHandle::default(),
      JsExecutionOptions::default(),
    )?;
    host.reset_scripting_state(None, ReferrerPolicy::default())?;

    let mut tab = BrowserTab {
      trace: TraceHandle::default(),
      trace_output: None,
      diagnostics: None,
      host,
      event_loop: EventLoop::new(),
      pending_frame: None,
      history: TabHistory::new(),
    };

    let calls: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
    let calls_for_cb = Arc::clone(&calls);
    let cancel_cb: Arc<crate::render_control::CancelCallback> = Arc::new(move || {
      assert_eq!(
        crate::render_control::active_stage(),
        Some(crate::error::RenderStage::DomParse)
      );
      let prev = calls_for_cb.fetch_add(1, Ordering::Relaxed);
      // Cancel on the *second* deadline check so the navigation proves that streaming parsing makes
      // multiple periodic deadline checks while consuming the input stream.
      prev >= 1
    });

    let err = tab
      .navigate_to_url(
        &file_url,
        RenderOptions::default().with_cancel_callback(Some(cancel_cb)),
      )
      .expect_err("expected cancellation during streaming parse");

    match err {
      Error::Render(crate::error::RenderError::Timeout {
        stage: crate::error::RenderStage::DomParse,
        ..
      }) => {}
      other => panic!("expected dom_parse timeout, got {other:?}"),
    }

    assert!(
      calls.load(Ordering::Relaxed) >= 2,
      "expected cancel callback to be consulted multiple times during parsing"
    );
    Ok(())
  }

  #[test]
  fn navigate_to_url_fetch_error_does_not_clobber_existing_dom() -> Result<()> {
    let mut tab = BrowserTab::from_html(
      "<!doctype html><html><body><div id=old></div></body></html>",
      RenderOptions::default(),
      NoopExecutor::default(),
    )?;
    assert!(
      tab.dom().get_element_by_id("old").is_some(),
      "expected initial DOM to contain #old"
    );

    let dir = tempdir().map_err(Error::Io)?;
    let missing_path = dir.path().join("missing.html");
    let missing_url = Url::from_file_path(&missing_path)
      .map_err(|()| Error::Other("failed to build file:// document URL".to_string()))?
      .to_string();

    let err = tab
      .navigate_to_url(&missing_url, RenderOptions::default())
      .expect_err("expected navigation to fail for missing file");
    match err {
      Error::Resource(_) => {}
      other => panic!("expected resource error for missing file, got {other:?}"),
    }

    assert!(
      tab.dom().get_element_by_id("old").is_some(),
      "expected failed navigation not to clobber existing DOM"
    );
    Ok(())
  }

  #[test]
  fn navigate_to_url_navigation_request_from_script_commits_new_document() -> Result<()> {
    struct NavigationRequestExecutor {
      target_url: String,
      pending: Option<LocationNavigationRequest>,
    }
 
    impl BrowserTabJsExecutor for NavigationRequestExecutor {
      fn execute_classic_script(
        &mut self,
        script_text: &str,
        _spec: &ScriptElementSpec,
        _current_script: Option<NodeId>,
        _document: &mut BrowserDocumentDom2,
        _event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<()> {
        if script_text == "NAVIGATE" && self.pending.is_none() {
          self.pending = Some(LocationNavigationRequest {
            url: self.target_url.clone(),
            replace: false,
          });
        }
        Ok(())
      }

      fn execute_module_script(
        &mut self,
        script_text: &str,
        _spec: &ScriptElementSpec,
        _current_script: Option<NodeId>,
        _document: &mut BrowserDocumentDom2,
        _event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<()> {
        if script_text == "NAVIGATE" && self.pending.is_none() {
          self.pending = Some(LocationNavigationRequest {
            url: self.target_url.clone(),
            replace: false,
          });
        }
        Ok(())
      }

      fn take_navigation_request(&mut self) -> Option<LocationNavigationRequest> {
        self.pending.take()
      }
    }

    let dir = tempdir().map_err(Error::Io)?;
    let first_path = dir.path().join("first.html");
    let second_path = dir.path().join("second.html");
    std::fs::write(
      &first_path,
      "<!doctype html><html><body><div id=first></div><script>NAVIGATE</script></body></html>",
    )
    .map_err(Error::Io)?;
    std::fs::write(
      &second_path,
      "<!doctype html><html><body><div id=second></div></body></html>",
    )
    .map_err(Error::Io)?;

    let first_url = Url::from_file_path(&first_path)
      .map_err(|()| Error::Other("failed to build file:// document URL".to_string()))?
      .to_string();
    let second_url = Url::from_file_path(&second_path)
      .map_err(|()| Error::Other("failed to build file:// document URL".to_string()))?
      .to_string();

    let executor = NavigationRequestExecutor {
      target_url: second_url.clone(),
      pending: None,
    };
    let mut tab = BrowserTab::from_html("", RenderOptions::default(), executor)?;
    tab.navigate_to_url(&first_url, RenderOptions::default())?;

    assert!(
      tab.dom().get_element_by_id("second").is_some(),
      "expected navigation request to commit second document"
    );
    assert!(
      tab.dom().get_element_by_id("first").is_none(),
      "expected navigation request to replace first document"
    );
    Ok(())
  }

  #[test]
  fn navigate_to_url_location_assign_pushes_history_entry() -> Result<()> {
    struct ScriptNavigationExecutor {
      target_url: String,
      replace: bool,
      pending: Option<LocationNavigationRequest>,
    }

    impl BrowserTabJsExecutor for ScriptNavigationExecutor {
      fn execute_classic_script(
        &mut self,
        script_text: &str,
        _spec: &ScriptElementSpec,
        _current_script: Option<NodeId>,
        _document: &mut BrowserDocumentDom2,
        _event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<()> {
        if script_text == "NAVIGATE" && self.pending.is_none() {
          self.pending = Some(LocationNavigationRequest {
            url: self.target_url.clone(),
            replace: self.replace,
          });
        }
        Ok(())
      }

      fn execute_module_script(
        &mut self,
        script_text: &str,
        spec: &ScriptElementSpec,
        current_script: Option<NodeId>,
        document: &mut BrowserDocumentDom2,
        event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<()> {
        // Tests only exercise classic scripts today; treat module scripts the same way so the
        // executor remains usable as module support is incrementally added.
        self.execute_classic_script(script_text, spec, current_script, document, event_loop)
      }

      fn take_navigation_request(&mut self) -> Option<LocationNavigationRequest> {
        self.pending.take()
      }
    }

    let page1_url = "https://example.com/page1.html";
    let page2_url = "https://example.com/page2.html";
    let page1_html = "<!doctype html><html><body><script>NAVIGATE</script></body></html>";
    let page2_html = "<!doctype html><html><body><div id=done></div></body></html>";

    let executor = ScriptNavigationExecutor {
      target_url: page2_url.to_string(),
      replace: false,
      pending: None,
    };
    let mut tab = BrowserTab::from_html("", RenderOptions::default(), executor)?;
    tab.register_html_source(page1_url, page1_html);
    tab.register_html_source(page2_url, page2_html);

    tab.navigate_to_url(page1_url, RenderOptions::default())?;

    assert_eq!(tab.history.len(), 2);
    assert_eq!(tab.history.current().map(|e| e.url.as_str()), Some(page2_url));
    let mut history = tab.history.clone();
    assert_eq!(
      history.go_back().map(|e| e.url.as_str()),
      Some(page1_url),
      "expected assign navigation to push history entry for the intermediate page"
    );
    Ok(())
  }

  #[test]
  fn navigate_to_url_location_replace_replaces_history_entry() -> Result<()> {
    struct ScriptNavigationExecutor {
      target_url: String,
      replace: bool,
      pending: Option<LocationNavigationRequest>,
    }

    impl BrowserTabJsExecutor for ScriptNavigationExecutor {
      fn execute_classic_script(
        &mut self,
        script_text: &str,
        _spec: &ScriptElementSpec,
        _current_script: Option<NodeId>,
        _document: &mut BrowserDocumentDom2,
        _event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<()> {
        if script_text == "NAVIGATE" && self.pending.is_none() {
          self.pending = Some(LocationNavigationRequest {
            url: self.target_url.clone(),
            replace: self.replace,
          });
        }
        Ok(())
      }

      fn execute_module_script(
        &mut self,
        script_text: &str,
        spec: &ScriptElementSpec,
        current_script: Option<NodeId>,
        document: &mut BrowserDocumentDom2,
        event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<()> {
        // Navigation tests don't distinguish script types; treat module scripts as classic.
        self.execute_classic_script(script_text, spec, current_script, document, event_loop)
      }

      fn take_navigation_request(&mut self) -> Option<LocationNavigationRequest> {
        self.pending.take()
      }
    }

    let page1_url = "https://example.com/page1.html";
    let page2_url = "https://example.com/page2.html";
    let page1_html = "<!doctype html><html><body><script>NAVIGATE</script></body></html>";
    let page2_html = "<!doctype html><html><body><div id=done></div></body></html>";

    let executor = ScriptNavigationExecutor {
      target_url: page2_url.to_string(),
      replace: true,
      pending: None,
    };
    let mut tab = BrowserTab::from_html("", RenderOptions::default(), executor)?;
    tab.register_html_source(page1_url, page1_html);
    tab.register_html_source(page2_url, page2_html);

    tab.navigate_to_url(page1_url, RenderOptions::default())?;

    assert_eq!(tab.history.len(), 1);
    assert_eq!(tab.history.current().map(|e| e.url.as_str()), Some(page2_url));
    assert!(!tab.history.can_go_back(), "expected replace to not push history");
    Ok(())
  }

  #[test]
  fn event_loop_navigation_replace_replaces_history_entry() -> Result<()> {
    let page1_url = "https://example.com/page1.html";
    let page2_url = "https://example.com/page2.html";
    let page1_html = "<!doctype html><html><body><div id=one></div></body></html>";
    let page2_html = "<!doctype html><html><body><div id=two></div></body></html>";

    let mut tab = BrowserTab::from_html("", RenderOptions::default(), NoopExecutor::default())?;
    tab.register_html_source(page1_url, page1_html);
    tab.register_html_source(page2_url, page2_html);
    tab.navigate_to_url(page1_url, RenderOptions::default())?;

    assert_eq!(tab.history.len(), 1);
    assert_eq!(tab.history.current().map(|e| e.url.as_str()), Some(page1_url));

    // Simulate a script-triggered `location.replace(page2_url)` during the event loop.
    tab.host.pending_navigation = Some(LocationNavigationRequest {
      url: page2_url.to_string(),
      replace: true,
    });
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    assert_eq!(tab.history.len(), 1);
    assert_eq!(tab.history.current().map(|e| e.url.as_str()), Some(page2_url));
    Ok(())
  }

  #[test]
  fn navigate_to_url_timeout_spans_script_redirect_chain() -> Result<()> {
    use crate::error::{RenderError, RenderStage};
    use std::time::Duration;
 
    struct SlowMapFetcher {
      pages: HashMap<String, String>,
      delay: Duration,
    }
 
    impl ResourceFetcher for SlowMapFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        self.fetch_with_request(FetchRequest::new(url, FetchDestination::Other))
      }
 
      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        std::thread::sleep(self.delay);
        crate::render_control::check_active(RenderStage::DomParse).map_err(Error::Render)?;
 
        let html = self.pages.get(req.url).ok_or_else(|| {
          Error::Other(format!("no test response registered for URL {}", req.url))
        })?;
        Ok(FetchedResource::with_final_url(
          html.as_bytes().to_vec(),
          Some("text/html".to_string()),
          Some(req.url.to_string()),
        ))
      }
    }
 
    struct RedirectExecutor {
      redirects: Vec<String>,
      next: usize,
      pending: Option<LocationNavigationRequest>,
    }

    impl BrowserTabJsExecutor for RedirectExecutor {
      fn execute_classic_script(
        &mut self,
        script_text: &str,
        _spec: &ScriptElementSpec,
        _current_script: Option<NodeId>,
        _document: &mut BrowserDocumentDom2,
        _event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<()> {
        if script_text == "NAVIGATE" && self.pending.is_none() {
          if let Some(url) = self.redirects.get(self.next) {
            self.pending = Some(LocationNavigationRequest {
              url: url.clone(),
              replace: false,
            });
            self.next += 1;
          }
        }
        Ok(())
      }

      fn execute_module_script(
        &mut self,
        script_text: &str,
        spec: &ScriptElementSpec,
        current_script: Option<NodeId>,
        document: &mut BrowserDocumentDom2,
        event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<()> {
        self.execute_classic_script(script_text, spec, current_script, document, event_loop)
      }
 
      fn take_navigation_request(&mut self) -> Option<LocationNavigationRequest> {
        self.pending.take()
      }
    }
 
    let url1 = "https://example.com/one".to_string();
    let url2 = "https://example.com/two".to_string();
    let url3 = "https://example.com/three".to_string();
    let url4 = "https://example.com/four".to_string();
 
    let mut pages = HashMap::<String, String>::new();
    pages.insert(
      url1.clone(),
      "<!doctype html><html><body><script>NAVIGATE</script></body></html>".to_string(),
    );
    pages.insert(
      url2.clone(),
      "<!doctype html><html><body><script>NAVIGATE</script></body></html>".to_string(),
    );
    pages.insert(
      url3.clone(),
      "<!doctype html><html><body><script>NAVIGATE</script></body></html>".to_string(),
    );
    pages.insert(
      url4.clone(),
      "<!doctype html><html><body><div id=done></div></body></html>".to_string(),
    );
 
    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(SlowMapFetcher {
      pages,
      delay: Duration::from_millis(15),
    });
    let renderer = crate::FastRender::builder()
      .dom_scripting_enabled(true)
      .fetcher(fetcher)
      .build()?;
    let options = RenderOptions::default();
    let document = BrowserDocumentDom2::new(renderer, "", options.clone())?;
    let mut host = BrowserTabHost::new(
      document,
      Box::new(RedirectExecutor {
        redirects: vec![url2.clone(), url3.clone(), url4.clone()],
        next: 0,
        pending: None,
      }),
      TraceHandle::default(),
      JsExecutionOptions::default(),
    )?;
    host.reset_scripting_state(None, ReferrerPolicy::default())?;
    let mut tab = BrowserTab {
      trace: TraceHandle::default(),
      trace_output: None,
      diagnostics: None,
      host,
      event_loop: EventLoop::new(),
      pending_frame: None,
      history: TabHistory::new(),
    };
 
    let err = tab
      .navigate_to_url(
        &url1,
        RenderOptions::default().with_timeout(Some(Duration::from_millis(50))),
      )
      .expect_err("expected deadline to apply across redirect chain");
 
    match err {
      Error::Render(RenderError::Timeout {
        stage: RenderStage::DomParse,
        ..
      }) => {}
      other => panic!("expected dom_parse timeout, got {other:?}"),
    }
 
    Ok(())
  }

  #[test]
  fn navigate_to_html_timeout_spans_script_triggered_navigation() -> Result<()> {
    use crate::error::{RenderError, RenderStage};
    use std::time::Duration;

    struct SlowMapFetcher {
      pages: HashMap<String, String>,
      delay: Duration,
    }

    impl ResourceFetcher for SlowMapFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        self.fetch_with_request(FetchRequest::new(url, FetchDestination::Other))
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        std::thread::sleep(self.delay);
        crate::render_control::check_active(RenderStage::DomParse).map_err(Error::Render)?;

        let html = self.pages.get(req.url).ok_or_else(|| {
          Error::Other(format!("no test response registered for URL {}", req.url))
        })?;
        Ok(FetchedResource::with_final_url(
          html.as_bytes().to_vec(),
          Some("text/html".to_string()),
          Some(req.url.to_string()),
        ))
      }
    }

    struct SleepyNavigateExecutor {
      target_url: String,
      pending: Option<LocationNavigationRequest>,
    }

    impl BrowserTabJsExecutor for SleepyNavigateExecutor {
      fn execute_classic_script(
        &mut self,
        script_text: &str,
        _spec: &ScriptElementSpec,
        _current_script: Option<NodeId>,
        _document: &mut BrowserDocumentDom2,
        _event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<()> {
        if script_text == "NAVIGATE" && self.pending.is_none() {
          // Deterministically consume most of the timeout budget before requesting a navigation.
          std::thread::sleep(Duration::from_millis(40));
          self.pending = Some(LocationNavigationRequest {
            url: self.target_url.clone(),
            replace: false,
          });
        }
        Ok(())
      }

      fn execute_module_script(
        &mut self,
        script_text: &str,
        spec: &ScriptElementSpec,
        current_script: Option<NodeId>,
        document: &mut BrowserDocumentDom2,
        event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<()> {
        self.execute_classic_script(script_text, spec, current_script, document, event_loop)
      }

      fn take_navigation_request(&mut self) -> Option<LocationNavigationRequest> {
        self.pending.take()
      }
    }

    let target_url = "https://example.com/target".to_string();
    let mut pages = HashMap::<String, String>::new();
    pages.insert(
      target_url.clone(),
      "<!doctype html><html><body><div id=done></div></body></html>".to_string(),
    );

    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(SlowMapFetcher {
      pages,
      delay: Duration::from_millis(15),
    });
    let renderer = crate::FastRender::builder()
      .dom_scripting_enabled(true)
      .fetcher(fetcher)
      .build()?;
    let options = RenderOptions::default();
    let document = BrowserDocumentDom2::new(renderer, "", options.clone())?;
    let mut host = BrowserTabHost::new(
      document,
      Box::new(SleepyNavigateExecutor {
        target_url: target_url.clone(),
        pending: None,
      }),
      TraceHandle::default(),
      JsExecutionOptions::default(),
    )?;
    host.reset_scripting_state(None, ReferrerPolicy::default())?;
    let mut tab = BrowserTab {
      trace: TraceHandle::default(),
      trace_output: None,
      diagnostics: None,
      host,
      event_loop: EventLoop::new(),
      pending_frame: None,
      history: TabHistory::new(),
    };

    let err = tab
      .navigate_to_html(
        "<!doctype html><html><body><script>NAVIGATE</script></body></html>",
        RenderOptions::default().with_timeout(Some(Duration::from_millis(40))),
      )
      .expect_err("expected deadline to span script-triggered navigation");

    match err {
      Error::Render(RenderError::Timeout {
        stage: RenderStage::DomParse,
        ..
      }) => {}
      other => panic!("expected dom_parse timeout, got {other:?}"),
    }

    Ok(())
  }

  #[test]
  fn from_html_timeout_spans_script_triggered_navigation() -> Result<()> {
    use crate::error::{RenderError, RenderStage};
    use std::time::Duration;

    struct SleepyNavigateExecutor {
      target_url: String,
      pending: Option<LocationNavigationRequest>,
    }

    impl BrowserTabJsExecutor for SleepyNavigateExecutor {
      fn execute_classic_script(
        &mut self,
        script_text: &str,
        _spec: &ScriptElementSpec,
        _current_script: Option<NodeId>,
        _document: &mut BrowserDocumentDom2,
        _event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<()> {
        if script_text == "NAVIGATE" && self.pending.is_none() {
          std::thread::sleep(Duration::from_millis(60));
          self.pending = Some(LocationNavigationRequest {
            url: self.target_url.clone(),
            replace: false,
          });
        }
        Ok(())
      }

      fn execute_module_script(
        &mut self,
        script_text: &str,
        spec: &ScriptElementSpec,
        current_script: Option<NodeId>,
        document: &mut BrowserDocumentDom2,
        event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<()> {
        self.execute_classic_script(script_text, spec, current_script, document, event_loop)
      }

      fn take_navigation_request(&mut self) -> Option<LocationNavigationRequest> {
        self.pending.take()
      }
    }

    let dir = tempdir().map_err(Error::Io)?;
    let target_path = dir.path().join("target.html");
    std::fs::write(
      &target_path,
      "<!doctype html><html><body><div id=done></div></body></html>",
    )
    .map_err(Error::Io)?;
    let target_url = Url::from_file_path(&target_path)
      .map_err(|()| Error::Other("failed to build file:// document URL".to_string()))?
      .to_string();

    let executor = SleepyNavigateExecutor {
      target_url,
      pending: None,
    };

    let err = match BrowserTab::from_html(
      "<!doctype html><html><body><script>NAVIGATE</script></body></html>",
      RenderOptions::default().with_timeout(Some(Duration::from_millis(40))),
      executor,
    ) {
      Ok(_) => panic!("expected deadline to span script-triggered navigation"),
      Err(err) => err,
    };

    match err {
      Error::Render(RenderError::Timeout {
        stage: RenderStage::DomParse,
        ..
      }) => {}
      other => panic!("expected dom_parse timeout, got {other:?}"),
    }

    Ok(())
  }

  #[test]
  fn from_html_with_document_url_timeout_spans_script_triggered_navigation() -> Result<()> {
    use crate::error::{RenderError, RenderStage};
    use std::time::Duration;

    struct SleepyNavigateExecutor {
      target_url: String,
      pending: Option<LocationNavigationRequest>,
    }

    impl BrowserTabJsExecutor for SleepyNavigateExecutor {
      fn execute_classic_script(
        &mut self,
        script_text: &str,
        _spec: &ScriptElementSpec,
        _current_script: Option<NodeId>,
        _document: &mut BrowserDocumentDom2,
        _event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<()> {
        if script_text == "NAVIGATE" && self.pending.is_none() {
          std::thread::sleep(Duration::from_millis(60));
          self.pending = Some(LocationNavigationRequest {
            url: self.target_url.clone(),
            replace: false,
          });
        }
        Ok(())
      }

      fn execute_module_script(
        &mut self,
        script_text: &str,
        spec: &ScriptElementSpec,
        current_script: Option<NodeId>,
        document: &mut BrowserDocumentDom2,
        event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<()> {
        self.execute_classic_script(script_text, spec, current_script, document, event_loop)
      }

      fn take_navigation_request(&mut self) -> Option<LocationNavigationRequest> {
        self.pending.take()
      }
    }

    let dir = tempdir().map_err(Error::Io)?;
    let document_path = dir.path().join("index.html");
    std::fs::write(&document_path, "<!doctype html><html></html>").map_err(Error::Io)?;
    let document_url = Url::from_file_path(&document_path)
      .map_err(|()| Error::Other("failed to build file:// document URL".to_string()))?
      .to_string();

    let target_path = dir.path().join("target.html");
    std::fs::write(
      &target_path,
      "<!doctype html><html><body><div id=done></div></body></html>",
    )
    .map_err(Error::Io)?;
    let target_url = Url::from_file_path(&target_path)
      .map_err(|()| Error::Other("failed to build file:// document URL".to_string()))?
      .to_string();

    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(LoggingFileFetcher { log });

    let executor = SleepyNavigateExecutor {
      target_url,
      pending: None,
    };

    let err = match BrowserTab::from_html_with_document_url_and_fetcher(
      "<!doctype html><html><body><script>NAVIGATE</script></body></html>",
      &document_url,
      RenderOptions::default().with_timeout(Some(Duration::from_millis(40))),
      executor,
      fetcher,
    ) {
      Ok(_) => panic!("expected deadline to span script-triggered navigation"),
      Err(err) => err,
    };

    match err {
      Error::Render(RenderError::Timeout {
        stage: RenderStage::DomParse,
        ..
      }) => {}
      other => panic!("expected dom_parse timeout, got {other:?}"),
    }

    Ok(())
  }

  #[test]
  fn from_html_with_document_url_timeout_spans_delayed_script_triggered_navigation() -> Result<()> {
    use crate::error::{RenderError, RenderStage};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    let timeout = Duration::from_millis(200);

    struct SlowStyleFetcher {
      url: String,
    }

    impl ResourceFetcher for SlowStyleFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        self.fetch_with_request(FetchRequest::new(url, FetchDestination::Other))
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        if req.url != self.url {
          return Err(Error::Other(format!("unexpected fetch url {}", req.url)));
        }
        Ok(FetchedResource::with_final_url(
          b"body { color: red; }".to_vec(),
          Some("text/css".to_string()),
          Some(req.url.to_string()),
        ))
      }
    }

    struct SleepyNavigateExecutor {
      target_url: String,
      pending: Option<LocationNavigationRequest>,
    }

    impl BrowserTabJsExecutor for SleepyNavigateExecutor {
      fn execute_classic_script(
        &mut self,
        script_text: &str,
        _spec: &ScriptElementSpec,
        _current_script: Option<NodeId>,
        _document: &mut BrowserDocumentDom2,
        _event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<()> {
        if script_text == "NAVIGATE" && self.pending.is_none() {
          self.pending = Some(LocationNavigationRequest {
            url: self.target_url.clone(),
            replace: false,
          });
        }
        Ok(())
      }

      fn execute_module_script(
        &mut self,
        script_text: &str,
        spec: &ScriptElementSpec,
        current_script: Option<NodeId>,
        document: &mut BrowserDocumentDom2,
        event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<()> {
        self.execute_classic_script(script_text, spec, current_script, document, event_loop)
      }

      fn take_navigation_request(&mut self) -> Option<LocationNavigationRequest> {
        self.pending.take()
      }
    }

    let document_url = "https://example.com/index.html";
    // Use a unique URL so any shared resource caches don't make this test flaky when running the
    // full suite (other tests fetch the same `https://example.com/style.css`).
    static NEXT_STYLE_URL_ID: AtomicUsize = AtomicUsize::new(0);
    let style_url = format!(
      "https://example.com/style-{}.css",
      NEXT_STYLE_URL_ID.fetch_add(1, Ordering::Relaxed)
    );
    let target_url = "https://example.com/target.html".to_string();
    let html = format!(
      "<!doctype html><html><head><link rel=\"stylesheet\" href=\"{style_url}\"></head><body><script>NAVIGATE</script></body></html>"
    );

    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(SlowStyleFetcher {
      url: style_url,
    });
    let executor = SleepyNavigateExecutor {
      target_url: target_url.clone(),
      pending: None,
    };

    let mut tab = BrowserTab::from_html_with_document_url_and_fetcher(
      &html,
      document_url,
      RenderOptions::default().with_timeout(Some(timeout)),
      executor,
      fetcher,
    )?;
    tab.register_html_source(
      &target_url,
      "<!doctype html><html><body><div id=done></div></body></html>",
    );

    // Drive the event loop manually without rendering: `BrowserTab::run_event_loop_until_idle`
    // renders between tasks, which can introduce unrelated timeouts in later pipeline stages. We
    // want to specifically assert that the *navigation commit* observes the original from_html
    // deadline (instead of resetting the clock at commit time).
    let _outcome = tab.event_loop.run_until_idle_handling_errors_with_hook(
      &mut tab.host,
      RunLimits::unbounded(),
      &mut |_err| {},
      |host, event_loop| -> Result<()> {
        if host.pending_navigation.is_some() {
          event_loop.clear_all_pending_work();
          return Ok(());
        }
        host.discover_dynamic_scripts(event_loop)?;
        Ok(())
      },
    )?;

    assert!(
      tab.host.pending_navigation.is_some(),
      "expected delayed script to request a navigation"
    );

    // Ensure the original from_html deadline expires *after* the delayed script has requested a
    // navigation, so the timeout is attributed to the navigation commit (and not to parser or task
    // execution timing, which can become nondeterministic when the full unit test suite is heavily
    // loaded).
    std::thread::sleep(timeout + Duration::from_millis(50));

    let err = match tab.commit_pending_navigation() {
      Ok(_) => panic!("expected from_html deadline to span delayed script-triggered navigation"),
      Err(err) => err,
    };

    match err {
      Error::Render(RenderError::Timeout {
        stage: RenderStage::DomParse,
        ..
      }) => {}
      other => panic!("expected dom_parse timeout, got {other:?}"),
    }

    Ok(())
  }

  #[test]
  fn non_matching_media_stylesheet_does_not_block_script() -> Result<()> {
    let temp = tempdir().map_err(Error::Io)?;
    std::fs::write(temp.path().join("print.css"), "body { color: green; }")
      .map_err(Error::Io)?;
    std::fs::write(temp.path().join("script.js"), "SCRIPT").map_err(Error::Io)?;

    let document_url = Url::from_file_path(temp.path().join("index.html"))
      .map_err(|()| Error::Other("failed to build file:// document URL".to_string()))?
      .to_string();

    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let fetcher = Arc::new(LoggingFileFetcher {
      log: Arc::clone(&log),
    });
    let renderer = crate::FastRender::builder()
      .dom_scripting_enabled(true)
      .fetcher(fetcher)
      .build()?;
    let options = RenderOptions::default();
    let document = BrowserDocumentDom2::new(renderer, "", options.clone())?;
    let host = BrowserTabHost::new(
      document,
      Box::new(LoggingExecutor {
        log: Arc::clone(&log),
      }),
      TraceHandle::default(),
      JsExecutionOptions::default(),
    )?;
    let mut tab = BrowserTab {
      trace: TraceHandle::default(),
      trace_output: None,
      diagnostics: None,
      host,
      event_loop: EventLoop::new(),
      pending_frame: None,
      history: TabHistory::new(),
    };
    tab
      .host
      .reset_scripting_state(Some(document_url.clone()), ReferrerPolicy::default())?;

    let html = r#"<!doctype html>
      <link rel="stylesheet" href="print.css" media="print">
      <script src="script.js"></script>
    "#;
    tab.parse_html_streaming_and_schedule_scripts(html, Some(&document_url), &options)?;

    let entries = log
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .clone();
    assert!(
      log_index(&entries, "exec:SCRIPT").is_some(),
      "expected script execution; log={entries:?}"
    );
    assert!(
      log_index(&entries, "print.css").is_none(),
      "did not expect print.css to be fetched for screen media; log={entries:?}"
    );
    Ok(())
  }

  #[test]
  fn csp_blocks_external_script_fetch_and_execution() -> Result<()> {
    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let fetcher = Arc::new(ScriptSourceFetcher::new(&[("https://evil.com/a.js", "EVIL")]));
    let fetcher_for_renderer: Arc<dyn crate::resource::ResourceFetcher> = fetcher.clone();
    let (mut host, mut event_loop) =
      build_host_with_fetcher("<script src=\"https://evil.com/a.js\"></script>", Rc::clone(&log), fetcher_for_renderer)?;

    host.reset_scripting_state(Some("https://example.com/".to_string()), ReferrerPolicy::default())?;
    host.csp = CspPolicy::from_values(["script-src 'self'"]);

    let scripts = host.discover_scripts_best_effort(Some("https://example.com/"));
    assert_eq!(scripts.len(), 1);
    let (node_id, spec) = scripts.into_iter().next().unwrap();
    let base_url_at_discovery = spec.base_url.clone();
    host.register_and_schedule_script(node_id, spec, base_url_at_discovery, &mut event_loop)?;

    assert_eq!(
      fetcher.call_count(),
      0,
      "external script fetch should be blocked by CSP before starting the request"
    );
    assert_eq!(&*log.borrow(), &Vec::<String>::new());
    Ok(())
  }

  #[test]
  fn csp_allows_external_script_fetch_and_execution() -> Result<()> {
    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let fetcher = Arc::new(ScriptSourceFetcher::new(&[("https://evil.com/a.js", "OK")]));
    let fetcher_for_renderer: Arc<dyn crate::resource::ResourceFetcher> = fetcher.clone();
    let (mut host, mut event_loop) =
      build_host_with_fetcher("<script src=\"https://evil.com/a.js\"></script>", Rc::clone(&log), fetcher_for_renderer)?;

    host.reset_scripting_state(Some("https://example.com/".to_string()), ReferrerPolicy::default())?;
    host.csp = CspPolicy::from_values(["script-src https:"]);

    let scripts = host.discover_scripts_best_effort(Some("https://example.com/"));
    assert_eq!(scripts.len(), 1);
    let (node_id, spec) = scripts.into_iter().next().unwrap();
    let base_url_at_discovery = spec.base_url.clone();
    host.register_and_schedule_script(node_id, spec, base_url_at_discovery, &mut event_loop)?;

    assert_eq!(fetcher.call_count(), 1);
    assert_eq!(
      &*log.borrow(),
      &["script:OK".to_string(), "microtask:OK".to_string()]
    );
    Ok(())
  }

  #[test]
  fn nomodule_inline_script_is_skipped_when_module_scripts_supported() -> Result<()> {
    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let mut js_options = JsExecutionOptions::default();
    js_options.supports_module_scripts = true;
    let (mut host, mut event_loop) = build_host_with_options(
      "<script nomodule>SKIP</script><script>RUN</script>",
      Rc::clone(&log),
      js_options,
    )?;

    let discovered = host.discover_scripts_best_effort(None);
    assert_eq!(discovered.len(), 2);
    for (node_id, spec) in discovered {
      let base_url_at_discovery = spec.base_url.clone();
      host.register_and_schedule_script(node_id, spec, base_url_at_discovery, &mut event_loop)?;
    }

    assert_eq!(
      &*log.borrow(),
      &["script:RUN".to_string(), "microtask:RUN".to_string()]
    );
    Ok(())
  }

  fn add_js_event_listener_log(
    host: &mut BrowserTabHost,
    target: EventTargetId,
    type_: &str,
    label: &'static str,
    log: Rc<RefCell<Vec<String>>>,
  ) -> Result<()> {
    let callback = host
      .js_events
      .runtime_mut()
      .alloc_function_value(move |_rt, _this, _args| {
        log.borrow_mut().push(label.to_string());
        Ok(Value::Undefined)
      })
      .map_err(|err: VmError| Error::Other(err.to_string()))?;
    host
      .js_events
      .add_js_event_listener(target, type_, callback, AddEventListenerOptions::default())?;
    Ok(())
  }

  #[test]
  fn lifecycle_events_are_observable_via_js_listeners_and_ordered_with_deferred_scripts() -> Result<()> {
    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let (mut host, mut event_loop) = build_host("<script defer src=\"d.js\"></script>", Rc::clone(&log))?;
    host.register_external_script_source("d.js".to_string(), "D".to_string());

    add_js_event_listener_log(
      &mut host,
      EventTargetId::Document,
      "readystatechange",
      "rs",
      Rc::clone(&log),
    )?;
    add_js_event_listener_log(
      &mut host,
      EventTargetId::Document,
      "DOMContentLoaded",
      "dom",
      Rc::clone(&log),
    )?;
    add_js_event_listener_log(&mut host, EventTargetId::Window, "load", "load", Rc::clone(&log))?;

    assert_eq!(host.dom().ready_state(), DocumentReadyState::Loading);

    let mut discovered = host.discover_scripts_best_effort(None);
    assert_eq!(discovered.len(), 1);
    let (node_id, spec) = discovered.pop().unwrap();
    let base_url_at_discovery = spec.base_url.clone();
    host.register_and_schedule_script(node_id, spec, base_url_at_discovery, &mut event_loop)?;

    let actions = host.scheduler.parsing_completed()?;
    host.apply_scheduler_actions(actions, &mut event_loop)?;
    host.notify_parsing_completed(&mut event_loop)?;

    // `readystatechange` fires synchronously when the `loading` → `interactive` transition occurs.
    assert_eq!(host.dom().ready_state(), DocumentReadyState::Interactive);
    assert_eq!(&*log.borrow(), &["rs".to_string()]);

    let run_limits = RunLimits::unbounded();
    let outcome = event_loop.run_until_idle_with_hook(&mut host, run_limits, {
      let log = Rc::clone(&log);
      move |_host, _event_loop| {
        log.borrow_mut().push("checkpoint".to_string());
        Ok(())
      }
    })?;
    assert!(matches!(outcome, RunUntilIdleOutcome::Idle));

    assert_eq!(host.dom().ready_state(), DocumentReadyState::Complete);

    assert_eq!(
      &*log.borrow(),
      &[
        // Parsing completion: `loading` → `interactive`.
        "rs".to_string(),
        // Networking fetch task turn.
        "checkpoint".to_string(),
        // Deferred script task turn (with post-task microtask checkpoint).
        "script:D".to_string(),
        "microtask:D".to_string(),
        "checkpoint".to_string(),
        // Barrier task inserted before DOMContentLoaded.
        "checkpoint".to_string(),
        // DOMContentLoaded task turn.
        "dom".to_string(),
        "checkpoint".to_string(),
        // Load task turn (`interactive` → `complete` fires another readystatechange immediately
        // before `load`).
        "rs".to_string(),
        "load".to_string(),
        "checkpoint".to_string(),
      ]
    );
    Ok(())
  }

  #[test]
  fn module_scripts_execute_and_can_mutate_dom() -> Result<()> {
    let mut js_options = JsExecutionOptions::default();
    js_options.supports_module_scripts = true;

    let mut tab = BrowserTab::from_html_with_js_execution_options(
      r#"<!doctype html><body>
        <script type="module">
          document.body.setAttribute("data-module-ran", "1");
          document.body.setAttribute(
            "data-current-script-null",
            document.currentScript === null ? "1" : "0",
          );
          document.body.setAttribute(
            "data-top-level-this-undefined",
            this === undefined ? "1" : "0",
          );
        </script>
      </body>"#,
      RenderOptions::default(),
      crate::api::VmJsBrowserTabExecutor::default(),
      js_options,
    )?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    let dom = tab.dom();
    let body = dom.body().expect("body should exist");
    assert_eq!(
      dom.get_attribute(body, "data-module-ran")
        .expect("get_attribute should succeed"),
      Some("1")
    );
    assert_eq!(
      dom.get_attribute(body, "data-current-script-null")
        .expect("get_attribute should succeed"),
      Some("1")
    );
    assert_eq!(
      dom.get_attribute(body, "data-top-level-this-undefined")
        .expect("get_attribute should succeed"),
      Some("1")
    );
    Ok(())
  }

  #[test]
  fn module_import_meta_url_is_exposed_for_entry_and_imported_modules() -> Result<()> {
    let mut js_options = JsExecutionOptions::default();
    js_options.supports_module_scripts = true;

    let dep_source = "globalThis.__dep_url = import.meta.url;";
    let dep_url = {
      let b64 = BASE64_STANDARD.encode(dep_source.as_bytes());
      Url::parse(&format!("data:text/javascript;base64,{b64}"))
        .expect("data: URL should parse")
        .to_string()
    };

    let entry_source = format!(
      "import \"{dep_url}\";\n\
       document.body.setAttribute(\"data-entry-url\", import.meta.url);\n\
       document.body.setAttribute(\"data-dep-url\", globalThis.__dep_url);\n"
    );
    let entry_url = {
      let b64 = BASE64_STANDARD.encode(entry_source.as_bytes());
      Url::parse(&format!("data:text/javascript;base64,{b64}"))
        .expect("data: URL should parse")
        .to_string()
    };

    let html = format!(
      r#"<!doctype html><body>
        <script type="module" src="{entry_url}"></script>
      </body>"#
    );

    let mut tab = BrowserTab::from_html_with_js_execution_options(
      &html,
      RenderOptions::default(),
      crate::api::VmJsBrowserTabExecutor::default(),
      js_options,
    )?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    let dom = tab.dom();
    let body = dom.body().expect("body should exist");
    assert_eq!(
      dom.get_attribute(body, "data-entry-url")
        .expect("get_attribute should succeed"),
      Some(entry_url.as_str())
    );
    assert_eq!(
      dom.get_attribute(body, "data-dep-url")
        .expect("get_attribute should succeed"),
      Some(dep_url.as_str())
    );
    Ok(())
  }

  #[test]
  fn import_meta_url_is_defined_for_inline_module_scripts() -> Result<()> {
    let mut js_options = JsExecutionOptions::default();
    js_options.supports_module_scripts = true;

    let mut tab = BrowserTab::from_html_with_js_execution_options(
      r#"<!doctype html><body>
        <script type="module">
          document.body.setAttribute("data-import-meta-url", import.meta.url);
        </script>
      </body>"#,
      RenderOptions::default(),
      crate::api::VmJsBrowserTabExecutor::default(),
      js_options,
    )?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    let dom = tab.dom();
    let body = dom.body().expect("body should exist");
    let url = dom
      .get_attribute(body, "data-import-meta-url")
      .expect("get_attribute should succeed")
      .expect("import.meta.url should be set");
    let parsed = url::Url::parse(&url).expect("import.meta.url should be a URL");
    assert!(
      parsed
        .fragment()
        .map(|frag| frag.starts_with("inline-module-"))
        .unwrap_or(false),
      "unexpected import.meta.url: {url}",
    );
    Ok(())
  }

  #[test]
  fn dynamic_import_works_in_classic_scripts_when_module_scripts_supported() -> Result<()> {
    let mut js_options = JsExecutionOptions::default();
    js_options.supports_module_scripts = true;

    let module = "data:text/javascript,export%20default%20123%3B";
    let html = format!(
      r#"<!doctype html><body>
        <script>
          import("{module}").then(m => {{
            document.body.setAttribute("data-dynamic-import", String(m.default));
          }});
        </script>
      </body>"#
    );

    let mut tab = BrowserTab::from_html_with_js_execution_options(
      &html,
      RenderOptions::default(),
      crate::api::VmJsBrowserTabExecutor::default(),
      js_options,
    )?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    let dom = tab.dom();
    let body = dom.body().expect("body should exist");
    assert_eq!(
      dom.get_attribute(body, "data-dynamic-import")
        .expect("get_attribute should succeed"),
      Some("123")
    );
    Ok(())
  }

  #[test]
  fn dynamic_import_works_in_promise_microtasks_when_module_scripts_supported() -> Result<()> {
    let mut js_options = JsExecutionOptions::default();
    js_options.supports_module_scripts = true;

    let module = "data:text/javascript,export%20default%20456%3B";
    let html = format!(
      r#"<!doctype html><body>
        <script>
          Promise.resolve()
            .then(() => import("{module}"))
            .then(m => {{
              document.body.setAttribute("data-microtask-dynamic-import", String(m.default));
            }});
        </script>
      </body>"#
    );

    let mut tab = BrowserTab::from_html_with_js_execution_options(
      &html,
      RenderOptions::default(),
      crate::api::VmJsBrowserTabExecutor::default(),
      js_options,
    )?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    let dom = tab.dom();
    let body = dom.body().expect("body should exist");
    assert_eq!(
      dom.get_attribute(body, "data-microtask-dynamic-import")
        .expect("get_attribute should succeed"),
      Some("456")
    );
    Ok(())
  }

  #[test]
  fn import_maps_remap_bare_specifiers_in_module_scripts() -> Result<()> {
    let mut js_options = JsExecutionOptions::default();
    js_options.supports_module_scripts = true;

    // Map a bare specifier to a self-contained `data:` module to avoid network dependencies.
    let mapped = "data:text/javascript,export%20default%20123%3B";

    let mut tab = BrowserTab::from_html_with_js_execution_options(
      &format!(
        r#"<!doctype html><body>
          <script type="importmap">{{"imports":{{"react":"{mapped}"}}}}</script>
          <script type="module">
            import x from "react";
            document.body.setAttribute("data-importmap", String(x));
          </script>
        </body>"#
      ),
      RenderOptions::default(),
      crate::api::VmJsBrowserTabExecutor::default(),
      js_options,
    )?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    let dom = tab.dom();
    let body = dom.body().expect("body should exist");
    assert_eq!(
      dom.get_attribute(body, "data-importmap")
        .expect("get_attribute should succeed"),
      Some("123")
    );
    Ok(())
  }

  #[test]
  fn module_imports_can_load_from_registered_script_sources() -> Result<()> {
    let mut js_options = JsExecutionOptions::default();
    js_options.supports_module_scripts = true;

    let mut tab = BrowserTab::from_html_with_js_execution_options(
      r#"<!doctype html><body>
        <script type="module" src="https://example.com/a.js"></script>
      </body>"#,
      RenderOptions::default(),
      crate::api::VmJsBrowserTabExecutor::default(),
      js_options,
    )?;
    tab.register_script_source(
      "https://example.com/a.js",
      r#"import { value } from "./dep.js";
         document.body.setAttribute("data-value", String(value));"#,
    );
    tab.register_script_source("https://example.com/dep.js", "export const value = 42;");
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    let dom = tab.dom();
    let body = dom.body().expect("body should exist");
    assert_eq!(
      dom.get_attribute(body, "data-value")
        .expect("get_attribute should succeed"),
      Some("42")
    );
    Ok(())
  }

  #[test]
  fn module_imports_enforce_import_map_integrity_metadata() -> Result<()> {
    let mut js_options = JsExecutionOptions::default();
    js_options.supports_module_scripts = true;

    let module_source = "export default 123;";
    let module_url = Url::parse("data:text/javascript,export%20default%20123%3B")
      .expect("data: URL should parse")
      .to_string();

    let digest = Sha256::digest(module_source.as_bytes());
    let integrity = format!("sha256-{}", BASE64_STANDARD.encode(digest));

    let importmap = {
      let mut root = serde_json::Map::new();
      let mut imports = serde_json::Map::new();
      imports.insert(
        "dep".to_string(),
        serde_json::Value::String(module_url.clone()),
      );
      root.insert("imports".to_string(), serde_json::Value::Object(imports));
      let mut integrity_map = serde_json::Map::new();
      integrity_map.insert(
        module_url.clone(),
        serde_json::Value::String(integrity),
      );
      root.insert(
        "integrity".to_string(),
        serde_json::Value::Object(integrity_map),
      );
      serde_json::Value::Object(root).to_string()
    };

    let html = format!(
      r#"<!doctype html><body>
          <script type="importmap">{importmap}</script>
          <script type="module">
            import x from "dep";
            document.body.setAttribute("data-integrity", String(x));
          </script>
        </body>"#
    );

    let mut tab = BrowserTab::from_html_with_js_execution_options(
      &html,
      RenderOptions::default(),
      crate::api::VmJsBrowserTabExecutor::default(),
      js_options,
    )?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    let dom = tab.dom();
    let body = dom.body().expect("body should exist");
    assert_eq!(
      dom.get_attribute(body, "data-integrity")
        .expect("get_attribute should succeed"),
      Some("123")
    );
    Ok(())
  }

  #[test]
  fn module_scripts_resolve_bare_specifiers_via_import_maps() -> Result<()> {
    #[derive(Clone)]
    struct MapFetcher {
      entries: Arc<HashMap<String, FetchedResource>>,
    }

    impl ResourceFetcher for MapFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        self
          .entries
          .get(url)
          .cloned()
          .ok_or_else(|| Error::Other(format!("missing fetcher entry for {url}")))
      }

      fn fetch_with_request(&self, req: crate::resource::FetchRequest<'_>) -> Result<FetchedResource> {
        self.fetch(req.url)
      }
    }

    let mut js_options = JsExecutionOptions::default();
    js_options.supports_module_scripts = true;

    let entry_url = "https://example.invalid/entry.js";
    let dep_url = "https://example.invalid/foo.js";
    let document_url = "https://example.invalid/";

    let mut entries: HashMap<String, FetchedResource> = HashMap::new();
    let mut entry_res = FetchedResource::new(
      br#"import "foo"; document.body.setAttribute("data-importmap", "1");"#.to_vec(),
      Some("text/javascript".to_string()),
    );
    entry_res.status = Some(200);
    entry_res.final_url = Some(entry_url.to_string());
    entry_res.access_control_allow_origin = Some("*".to_string());
    entry_res.access_control_allow_credentials = true;
    entries.insert(entry_url.to_string(), entry_res);

    let mut dep_res = FetchedResource::new(
      br#"export const x = 1;"#.to_vec(),
      Some("text/javascript".to_string()),
    );
    dep_res.status = Some(200);
    dep_res.final_url = Some(dep_url.to_string());
    dep_res.access_control_allow_origin = Some("*".to_string());
    dep_res.access_control_allow_credentials = true;
    entries.insert(dep_url.to_string(), dep_res);

    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(MapFetcher {
      entries: Arc::new(entries),
    });

    let html = format!(
      r#"<!doctype html><body>
        <script type="importmap">{{"imports":{{"foo":"{dep_url}"}}}}</script>
        <script type="module" src="{entry_url}"></script>
      </body>"#
    );

    let mut tab = BrowserTab::from_html_with_document_url_and_fetcher_and_js_execution_options(
      &html,
      document_url,
      RenderOptions::default(),
      crate::api::VmJsBrowserTabExecutor::default(),
      fetcher,
      js_options,
    )?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    let dom = tab.dom();
    let body = dom.body().expect("body should exist");
    assert_eq!(
      dom.get_attribute(body, "data-importmap")
        .expect("get_attribute should succeed"),
      Some("1"),
      "expected module script to run after resolving bare specifier via import map"
    );
    Ok(())
  }

  #[test]
  fn module_imports_reject_mismatched_import_map_integrity_metadata() -> Result<()> {
    let mut js_options = JsExecutionOptions::default();
    js_options.supports_module_scripts = true;

    let module_url = Url::parse("data:text/javascript,export%20default%20123%3B")
      .expect("data: URL should parse")
      .to_string();

    let digest = Sha256::digest(b"other");
    let integrity = format!("sha256-{}", BASE64_STANDARD.encode(digest));

    let importmap = {
      let mut root = serde_json::Map::new();
      let mut imports = serde_json::Map::new();
      imports.insert(
        "dep".to_string(),
        serde_json::Value::String(module_url.clone()),
      );
      root.insert("imports".to_string(), serde_json::Value::Object(imports));
      let mut integrity_map = serde_json::Map::new();
      integrity_map.insert(
        module_url.clone(),
        serde_json::Value::String(integrity),
      );
      root.insert(
        "integrity".to_string(),
        serde_json::Value::Object(integrity_map),
      );
      serde_json::Value::Object(root).to_string()
    };

    let html = format!(
      r#"<!doctype html><body>
          <script type="importmap">{importmap}</script>
          <script type="module">
            import x from "dep";
            document.body.setAttribute("data-integrity", String(x));
          </script>
        </body>"#
    );

    let mut tab = BrowserTab::from_html_with_js_execution_options(
      &html,
      RenderOptions::default(),
      crate::api::VmJsBrowserTabExecutor::default(),
      js_options,
    )?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    let dom = tab.dom();
    let body = dom.body().expect("body should exist");
    assert_eq!(
      dom.get_attribute(body, "data-integrity")
        .expect("get_attribute should succeed"),
      None
    );
    Ok(())
  }

  #[test]
  fn load_waits_for_async_external_script_execution() -> Result<()> {
    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let (mut host, mut event_loop) = build_host(
      "<script async src=\"https://example.com/a.js\"></script>",
      Rc::clone(&log),
    )?;
    host.register_external_script_source("https://example.com/a.js".to_string(), "A".to_string());

    let mut discovered = host.discover_scripts_best_effort(None);
    assert_eq!(discovered.len(), 1);
    let (node_id, spec) = discovered.pop().unwrap();
    let base_url_at_discovery = spec.base_url.clone();
    host.register_and_schedule_script(node_id, spec, base_url_at_discovery, &mut event_loop)?;

    let actions = host.scheduler.parsing_completed()?;
    host.apply_scheduler_actions(actions, &mut event_loop)?;
    host.notify_parsing_completed(&mut event_loop)?;

    // Tasks should run in this order:
    // - networking fetch (queues async execution task)
    // - DOMContentLoaded barrier
    // - DOMContentLoaded
    // - async script execution
    // - load
    assert_eq!(host.dom().ready_state().as_str(), "interactive");

    assert!(event_loop.run_next_task(&mut host)?); // fetch
    assert!(event_loop.run_next_task(&mut host)?); // barrier
    assert!(event_loop.run_next_task(&mut host)?); // DOMContentLoaded

    // `load` must not have fired yet because the async script has not executed.
    assert_eq!(host.dom().ready_state().as_str(), "interactive");
    assert_eq!(&*log.borrow(), &Vec::<String>::new());

    // Async script execution task (and its microtask checkpoint).
    assert!(event_loop.run_next_task(&mut host)?);
    assert_eq!(
      &*log.borrow(),
      &["script:A".to_string(), "microtask:A".to_string()]
    );

    // `load` should now be queued and should advance `document.readyState` to `complete`.
    assert!(event_loop.run_next_task(&mut host)?);
    assert_eq!(host.dom().ready_state().as_str(), "complete");
    Ok(())
  }
}
