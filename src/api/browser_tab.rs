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
  DocumentReadyState, DomHost, EventLoop, JsExecutionOptions, RunAnimationFrameOutcome, RunLimits,
  JsDomEvents, RunUntilIdleOutcome, RunUntilIdleStopReason, ScriptBlockExecutor, ScriptElementSpec,
  ScriptId, LocationNavigationRequest, ScriptBlockingStyleSheetSet, ScriptOrchestrator,
  ScriptScheduler, ScriptSchedulerAction, ScriptType, TaskSource,
};
use crate::js::runtime::with_event_loop;
use crate::js::script_encoding::decode_classic_script_bytes;
use crate::css::encoding::decode_css_bytes_cow;
use crate::css::parser::parse_stylesheet_with_media;
use crate::css::types::CssImportLoader;
use crate::render_control::{DeadlineGuard, RenderDeadline};
use crate::resource::{origin_from_url, FetchDestination, FetchRequest, ReferrerPolicy};
use crate::style::media::{MediaContext, MediaQueryCache, MediaType};
use crate::ui::TabHistory;
use crate::web::events::{Event, EventInit, EventTargetId};

use encoding_rs::{Encoding, UTF_8};

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use selectors::context::QuirksMode;
use url::Url;

use super::{BrowserDocumentDom2, Pixmap, RenderOptions, RunUntilStableOutcome, RunUntilStableStopReason};

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
    script_text: &str,
    spec: &ScriptElementSpec,
    current_script: Option<NodeId>,
    document: &mut BrowserDocumentDom2,
    event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()>;

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

struct StreamingParseState {
  parser: StreamingHtmlParser,
  input: String,
  input_offset: usize,
  eof_set: bool,
  deadline: Option<RenderDeadline>,
  parse_task_scheduled: bool,
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
  html_sources: HashMap<String, String>,
  external_script_sources: HashMap<String, String>,
  script_blocking_stylesheets: ScriptBlockingStyleSheetSet,
  stylesheet_keys_by_node: HashMap<NodeId, usize>,
  next_stylesheet_key: usize,
  stylesheet_media_context: MediaContext,
  stylesheet_media_query_cache: MediaQueryCache,
  js_execution_options: JsExecutionOptions,
  js_execution_depth: Rc<Cell<usize>>,
  lifecycle: DocumentLifecycle,
  last_dynamic_script_discovery_generation: u64,
  /// Whether we are currently running a streaming HTML parse (even if the parser state is
  /// temporarily moved out of `streaming_parse` by `parse_until_blocked`).
  streaming_parse_active: bool,
  streaming_parse: Option<StreamingParseState>,
  pending_parser_blocking_script: Option<PendingParserBlockingScript>,
}

impl BrowserTabHost {
  fn new(
    document: BrowserDocumentDom2,
    executor: Box<dyn BrowserTabJsExecutor>,
    trace: TraceHandle,
    js_execution_options: JsExecutionOptions,
  ) -> Result<Self> {
    let event_invoker = executor
      .event_listener_invoker()
      .unwrap_or_else(|| Box::new(NoopEventInvoker));
    let current_script = CurrentScriptStateHandle::default();
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
      parser_blocked_on: None,
      document_url: None,
      base_url: None,
      document_origin: None,
      document_referrer_policy: ReferrerPolicy::default(),
      csp: None,
      pending_navigation: None,
      html_sources: HashMap::new(),
      external_script_sources: HashMap::new(),
      script_blocking_stylesheets: ScriptBlockingStyleSheetSet::new(),
      stylesheet_keys_by_node: HashMap::new(),
      next_stylesheet_key: 0,
      stylesheet_media_context: MediaContext::default(),
      stylesheet_media_query_cache: MediaQueryCache::default(),
      js_execution_options,
      js_execution_depth: Rc::new(Cell::new(0)),
      lifecycle: DocumentLifecycle::new(),
      last_dynamic_script_discovery_generation: 0,
      streaming_parse_active: false,
      streaming_parse: None,
      pending_parser_blocking_script: None,
    })
  }

  fn register_html_source(&mut self, url: String, html: String) {
    self.html_sources.insert(url, html);
  }

  fn register_external_script_source(&mut self, url: String, source: String) {
    self.external_script_sources.insert(url, source);
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

  fn dispatch_script_error_event(&mut self, script_node_id: NodeId) -> Result<()> {
    self.dispatch_script_event(script_node_id, "error")
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
    self.parser_blocked_on = None;
    self.document_url = document_url.clone();
    self.base_url = document_url;
    self.document_origin = self
      .document_url
      .as_deref()
      .and_then(|url| origin_from_url(url));
    self.document_referrer_policy = document_referrer_policy;
    self.pending_navigation = None;
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
    let Some(state) = self.streaming_parse.as_mut() else {
      return Ok(());
    };
    if state.parse_task_scheduled {
      return Ok(());
    }
    state.parse_task_scheduled = true;

    let queued = event_loop.queue_task(TaskSource::DOMManipulation, |host, event_loop| {
      let result = host.parse_until_blocked(event_loop);
      if let Some(state) = host.streaming_parse.as_mut() {
        state.parse_task_scheduled = false;
      }
      result
    });
    if let Err(err) = queued {
      if let Some(state) = self.streaming_parse.as_mut() {
        state.parse_task_scheduled = false;
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
    self.script_blocking_stylesheets.register_blocking_stylesheet(key);

    event_loop.queue_task(TaskSource::Networking, move |host, event_loop| {
      let load_result = host.load_stylesheet_and_imports(&url);
      host.script_blocking_stylesheets.unregister_blocking_stylesheet(key);
      if !host.script_blocking_stylesheets.has_blocking_stylesheet() {
        // Wake parser-blocking scripts/parsing if this was the last blocking stylesheet.
        if let Err(err) = host.queue_parse_task(event_loop) {
          // Fallback: if we cannot queue a parse task (queue limits), resume immediately to avoid
          // deadlocking parser-blocking scripts.
          let _ = err;
          host.parse_until_blocked(event_loop)?;
        }
      }
      match load_result {
        Ok(()) => Ok(()),
        Err(err @ Error::Render(_)) => Err(err),
        Err(_) => Ok(()),
      }
    })?;

    Ok(())
  }

  fn parse_until_blocked(&mut self, event_loop: &mut EventLoop<Self>) -> Result<()> {
    const INPUT_CHUNK_BYTES: usize = 8 * 1024;

    let Some(mut state) = self.streaming_parse.take() else {
      return Ok(());
    };

    // Ensure any render deadline configured for streaming parsing remains active even when parsing
    // is resumed via event-loop tasks (e.g. after script-blocking stylesheets load).
    let _deadline_guard = state
      .deadline
      .as_ref()
      .map(|deadline| DeadlineGuard::install(Some(deadline)));

    enum Outcome {
      Blocked,
      Finished,
      AbortedForNavigation,
    }

    let outcome = (|| -> Result<Outcome> {
      loop {
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

            match exec_result {
              Ok(()) => {
                if let Some(entry) = entry {
                  if entry.spec.src_attr_present
                    && entry.spec.src.as_deref().is_some_and(|s| !s.is_empty())
                  {
                    self.dispatch_script_event(entry.node_id, "load")?;
                  }
                }
              }
              Err(err) => {
                let Some(entry) = entry else {
                  return Err(err);
                };
                self.dispatch_script_event(entry.node_id, "error")?;
                if matches!(err, Error::Render(_)) {
                  return Err(err);
                }
              }
            }

            // HTML: "clean up after running script" performs a microtask checkpoint only when the JS
            // execution context stack is empty. Nested (re-entrant) script execution must not drain
            // microtasks until the outermost script returns.
            if self.js_execution_depth.get() == 0 {
              event_loop.perform_microtask_checkpoint(self)?;
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
          let updated = self.dom().clone_with_events();
          {
            let Some(mut doc) = state.parser.document_mut() else {
              return Err(Error::Other(
                "StreamingHtmlParser yielded a script without an active document".to_string(),
              ));
            };
            *doc = updated;
          }
          continue;
        }

        let yield_result = state.parser.pump()?;

        // Start fetching any script-blocking stylesheet links discovered during this parse step.
        for (node_id, url) in state.parser.take_pending_stylesheet_links() {
            self.start_script_blocking_stylesheet_load(node_id, url, event_loop)?;
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
            let snapshot = {
              let Some(doc) = state.parser.document() else {
                return Err(Error::Other(
                  "StreamingHtmlParser yielded a script without an active document".to_string(),
                ));
              };
              doc.clone_with_events()
            };

            self.mutate_dom(|dom| {
              *dom = snapshot;
              ((), true)
            });

            // Keep the host's base URL in sync with the parser state so any JS executed at this pause
            // point (including microtasks drained before script execution) resolves relative URLs
            // against the correct base.
            self.base_url = base_url_at_this_point.clone();
            self
              .executor
              .on_document_base_url_updated(self.base_url.as_deref());

            // HTML: before preparing a parser-inserted script at a script end-tag boundary,
            // perform a microtask checkpoint when the JS execution context stack is empty.
            //
            // Microtasks may mutate the document (including removing/detaching this `<script>`
            // element), so this must occur before we check `is_connected_for_scripting` and build
            // the final `ScriptElementSpec`.
            if self.js_execution_depth.get() == 0 {
              with_active_streaming_parser(&state.parser, || {
                event_loop.perform_microtask_checkpoint(self)
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

              let updated = self.dom().clone_with_events();
              {
                let Some(mut doc) = state.parser.document_mut() else {
                  return Err(Error::Other(
                    "StreamingHtmlParser yielded a script without an active document".to_string(),
                  ));
                };
                *doc = updated;
              }
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
              let updated = self.dom().clone_with_events();
              {
                let Some(mut doc) = state.parser.document_mut() else {
                  return Err(Error::Other(
                    "StreamingHtmlParser yielded a script without an active document".to_string(),
                  ));
                };
                *doc = updated;
              }
              continue;
            }

            let base_url_at_discovery = spec.base_url.clone();

            with_active_streaming_parser(&state.parser, || {
              self.register_and_schedule_script(script, spec, base_url_at_discovery, event_loop)
            })?;

            if self.pending_navigation.is_some() {
              // Abort the current parse/execution; the caller will commit the navigation.
              return Ok(Outcome::AbortedForNavigation);
            }

            if self.pending_parser_blocking_script.is_some() || self.parser_blocked_on.is_some() {
              // Parsing is blocked (either on a stylesheet-blocking script, or another parser
              // block). Sync any microtask mutations back into the streaming parser's live DOM so
              // parsing resumes with an up-to-date tree once unblocked.
              let updated = self.dom().clone_with_events();
              {
                let Some(mut doc) = state.parser.document_mut() else {
                  return Err(Error::Other(
                    "StreamingHtmlParser yielded a script without an active document".to_string(),
                  ));
                };
                *doc = updated;
              }
              return Ok(Outcome::Blocked);
            }

            // Sync any DOM mutations from the executed script back into the streaming parser's live
            // DOM before resuming parsing.
            let updated = self.dom().clone_with_events();
            {
              let Some(mut doc) = state.parser.document_mut() else {
                return Err(Error::Other(
                  "StreamingHtmlParser yielded a script without an active document".to_string(),
                ));
              };
              *doc = updated;
            }
          }
          StreamingParserYield::Finished { document } => {
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
    })();

    match outcome {
      Ok(Outcome::Blocked) => {
        self.streaming_parse = Some(state);
        Ok(())
      }
      Ok(Outcome::Finished | Outcome::AbortedForNavigation) => {
        self.streaming_parse_active = false;
        Ok(())
      }
      Err(err) => {
        self.streaming_parse_active = false;
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
    self.dispatch_script_event(script_node, "error")?;
    // Mark the element as already-started so future scheduling attempts short-circuit.
    self.mutate_dom(|dom| {
      dom.node_mut(script_node).script_already_started = true;
      ((), false)
    });

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

    // HTML `</script>` handling performs a microtask checkpoint *before* preparing the script, but
    // only when the JS execution context stack is empty.
    if spec.parser_inserted && self.js_execution_depth.get() == 0 {
      event_loop.perform_microtask_checkpoint(self)?;
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
        // HTML: module scripts are deferred-by-default (unless `async`) and block DOMContentLoaded.
        spec_for_table.parser_inserted
          && !spec_for_table.async_attr
          && (!spec_for_table.src_attr_present
            || spec_for_table.src.as_deref().is_some_and(|src| !src.is_empty()))
      }
      ScriptType::ImportMap | ScriptType::Unknown => false,
    };
    let should_check_inline_source =
      !spec_for_table.src_attr_present
        && matches!(spec_for_table.script_type, ScriptType::Classic | ScriptType::Module)
        && (spec_for_table.script_type == ScriptType::Module || !nomodule_blocked);
    if should_check_inline_source {
      self
        .js_execution_options
        .check_script_source(&spec_for_table.inline_text, "source=inline")?;
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
    if matches!(spec_for_table.script_type, ScriptType::Classic | ScriptType::Module)
      && !spec_for_table.src_attr_present
    {
      self
        .js_execution_options
        .check_script_source(&spec_for_table.inline_text, "source=inline")?;
    }
    let discovered = self
      .scheduler
      .discovered_script(spec, node_id, base_url_at_discovery)?;
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
          self.start_fetch(script_id, url, destination, event_loop)?;
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

                self.dispatch_script_error_event(node_id)?;
                self.mutate_dom(|dom| {
                  dom.node_mut(node_id).script_already_started = true;
                  ((), false)
                });
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
          match exec_result {
            Ok(()) => {
              if let Some(entry) = entry {
                if entry.spec.src_attr_present && entry.spec.src.as_deref().is_some_and(|s| !s.is_empty()) {
                  self.dispatch_script_event(entry.node_id, "load")?;
                }
              }
            }
            Err(err) => {
              let Some(entry) = entry else {
                return Err(err);
              };
              self.dispatch_script_event(entry.node_id, "error")?;
              // Uncaught exceptions from scripts should not abort parsing/task scheduling (browser
              // behavior). Still propagate host-level render timeouts/cancellation.
              if matches!(err, Error::Render(_)) {
                return Err(err);
              }
            }
          }

          // HTML: "clean up after running script" performs a microtask checkpoint only when the JS
          // execution context stack is empty. Nested (re-entrant) script execution must not drain
          // microtasks until the outermost script returns.
          if self.js_execution_depth.get() == 0 {
            event_loop.perform_microtask_checkpoint(self)?;
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

                self.dispatch_script_error_event(node_id)?;
                self.mutate_dom(|dom| {
                  dom.node_mut(node_id).script_already_started = true;
                  ((), false)
                });
                self.finish_script_execution(script_id, event_loop)?;
                continue;
              }
            }
          }
          event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
            let entry = host.scripts.get(&script_id).cloned();
            let _guard = JsExecutionGuard::enter(&host.js_execution_depth);
            let result = host.execute_script(script_id, &source_text, event_loop);
            host.finish_script_execution(script_id, event_loop)?;
            match result {
              Ok(()) => {
                if let Some(entry) = entry {
                  if entry.spec.src_attr_present && entry.spec.src.as_deref().is_some_and(|s| !s.is_empty()) {
                    host.dispatch_script_event(entry.node_id, "load")?;
                  }
                }
                Ok(())
              }
              Err(err) => {
                let Some(entry) = entry else {
                  return Err(err);
                };
                host.dispatch_script_event(entry.node_id, "error")?;
                if matches!(err, Error::Render(_)) {
                  Err(err)
                } else {
                  Ok(())
                }
              }
            }
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
        if let Some(url) = self.spec.src.as_deref() {
          span.arg_str("url", url);
        }
        span.arg_bool("async_attr", self.spec.async_attr);
        span.arg_bool("defer_attr", self.spec.defer_attr);
        span.arg_bool("parser_inserted", self.spec.parser_inserted);

        let current_script = host.current_script_node();
        let result = match script_type {
          ScriptType::Classic => host.executor.execute_classic_script(
            self.source_text,
            self.spec,
            current_script,
            &mut host.document,
            self.event_loop,
          ),
          ScriptType::Module => host.executor.execute_module_script(
            self.source_text,
            self.spec,
            current_script,
            &mut host.document,
            self.event_loop,
          ),
          ScriptType::ImportMap | ScriptType::Unknown => Ok(()),
        };
        if let Some(req) = host.executor.take_navigation_request() {
          host.pending_navigation = Some(req);
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

      if spec.crossorigin.is_none() {
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

    if let Some(source) = self.external_script_sources.get(url) {
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
      return Ok(source.clone());
    }

    let fetcher = self.document.fetcher();
    let mut req = FetchRequest::new(url, destination);
    if let Some(referrer) = self.document_url.as_deref() {
      req = req.with_referrer_url(referrer);
    }
    if let Some(origin) = self.document_origin.as_ref() {
      req = req.with_client_origin(origin);
    }
    let effective_referrer_policy = spec.referrer_policy.unwrap_or(self.document_referrer_policy);
    req = req.with_referrer_policy(effective_referrer_policy);
    if let Some(cors_mode) = spec.crossorigin {
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
    if let Some(cors_mode) = spec.crossorigin {
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
    let deadline = (options.timeout.is_some() || options.cancel_callback.is_some()).then(|| {
      RenderDeadline::new(options.timeout, options.cancel_callback.clone())
    });

    self.host.streaming_parse = Some(StreamingParseState {
      parser: StreamingHtmlParser::new(document_url),
      input: html.to_string(),
      input_offset: 0,
      eof_set: false,
      deadline,
      parse_task_scheduled: false,
    });
    self.host.streaming_parse_active = true;

    self.host.parse_until_blocked(&mut self.event_loop)?;
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
      Box::new(executor),
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
    let document_referrer_policy =
      crate::html::referrer_policy::extract_referrer_policy_from_html(html).unwrap_or_default();
    tab.host.reset_scripting_state(None, document_referrer_policy)?;
    let base_url = tab.parse_html_streaming_and_schedule_scripts(html, None, &options)?;
    if let Some(req) = tab.host.pending_navigation.take() {
      tab.navigate_to_url_with_replace(&req.url, options.clone(), req.replace)?;
    } else if tab.host.streaming_parse.is_none() {
      let renderer = tab.host.document.renderer_mut();
      match base_url {
        Some(url) => renderer.set_base_url(url),
        None => renderer.clear_base_url(),
      }
    }
    Ok(tab)
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
    let host = BrowserTabHost::new(
      document,
      Box::new(executor),
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
    tab.host.document.renderer_mut().set_document_url(document_url);

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
      Box::new(executor),
      trace_handle.clone(),
      js_execution_options,
    )?;
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
    let document_referrer_policy =
      crate::html::referrer_policy::extract_referrer_policy_from_html(html).unwrap_or_default();
    tab.host.reset_scripting_state(None, document_referrer_policy)?;
    let base_url = tab.parse_html_streaming_and_schedule_scripts(html, None, &options)?;
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
    let options = self.host.document.options().clone();
    self.navigate_to_url_with_replace(&req.url, options, req.replace)?;
    Ok(true)
  }

  fn run_event_loop_until_idle_handling_errors_with_pending_navigation_abort(
    &mut self,
    limits: RunLimits,
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
        if host.document.is_dirty() {
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
    self
      .host
      .dispatch_dom_event(EventTargetId::Node(node_id).normalize(), event)
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
      match self.event_loop.run_until_idle_with_hook(&mut self.host, microtask_limits, |host, event_loop| {
        host.discover_dynamic_scripts(event_loop)
      })? {
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
      match self.event_loop.run_until_idle_with_hook(&mut self.host, one_task_limits, |host, event_loop| {
        host.discover_dynamic_scripts(event_loop)
      })? {
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
        <script src="https://example.com/b.js" crossorigin></script>"#,
      RenderOptions::default(),
    )?;
    let mut host = BrowserTabHost::new(
      document,
      Box::new(NoopExecutor::default()),
      TraceHandle::default(),
      JsExecutionOptions::default(),
    )?;
    host.reset_scripting_state(
      Some("https://example.com/doc.html".to_string()),
      ReferrerPolicy::default(),
    )?;
    let mut event_loop = EventLoop::new();

    let mut discovered = host.discover_scripts_best_effort(Some("https://example.com/doc.html"));
    assert_eq!(discovered.len(), 2);
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
      ]
    );
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
      _event_loop: &mut EventLoop<BrowserTabHost>,
    ) -> Result<()> {
      self.ensure_realm()?;
      let realm = self.realm.as_mut().expect("realm should be initialized");
      realm
        .exec_script(script_text)
        .map_err(|err| Error::Other(err.to_string()))?;
      Ok(())
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
    // The vm-js DOM shims store a raw pointer to the host DOM in a thread-local registry keyed by an
    // integer "dom source id". Register the tab's live DOM for the duration of this script call so
    // JS can mutate it directly via raw pointers (bypassing BrowserDocumentDom2's `dom_mut()` /
    // `mutate_dom()` invalidation hooks).
    let dom_source_id = register_dom_source(tab.host.document.dom_ptr());
    let _guard = DomSourceGuard { id: dom_source_id };

    let mut realm = WindowRealm::new(
      WindowRealmConfig::new("https://example.com/").with_dom_source_id(dom_source_id),
    )
    .expect("WindowRealm");
    realm.exec_script(source).expect("exec_script");
  }

  #[test]
  fn vm_js_dom_shim_mutations_invalidate_browser_document_dom2() -> Result<()> {
    let mut tab = BrowserTab::from_html(
      "<!doctype html><html><body><div>hello</div></body></html>",
      RenderOptions::new().with_viewport(64, 64),
      NoopExecutor::default(),
    )?;

    // Initial render clears dirty flags and records the current DOM mutation generation.
    assert!(tab.render_if_needed()?.is_some());
    assert!(tab.render_if_needed()?.is_none());

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
    });

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
}
