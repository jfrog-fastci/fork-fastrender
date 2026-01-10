use crate::dom::HTML_NAMESPACE;
use crate::debug::trace::TraceHandle;
use crate::dom2::{Document, NodeId, NodeKind};
use crate::error::{Error, Result};
use crate::html::base_url_tracker::BaseUrlTracker;
use crate::html::content_security_policy::{CspDirective, CspPolicy};
use crate::html::encoding::decode_html_bytes;
use crate::html::document_write::with_active_streaming_parser;
use crate::html::streaming_parser::{StreamingHtmlParser, StreamingParserYield};
use crate::resource::ResourceFetcher;
use crate::js::{
  CurrentScriptHost, CurrentScriptStateHandle, DocumentLifecycle, DocumentLifecycleHost,
  DocumentReadyState, DomHost, EventLoop, JsExecutionOptions, RunAnimationFrameOutcome, RunLimits,
  RunUntilIdleOutcome, RunUntilIdleStopReason, ScriptBlockExecutor, ScriptElementSpec, ScriptId,
  LocationNavigationRequest, ScriptBlockingStyleSheetSet, ScriptOrchestrator, ScriptScheduler,
  ScriptSchedulerAction, ScriptType, TaskSource,
};
use crate::css::encoding::decode_css_bytes_cow;
use crate::css::loader::resolve_href_with_base;
use crate::css::parser::{parse_stylesheet_with_media, tokenize_rel_list};
use crate::css::types::CssImportLoader;
use crate::render_control::{DeadlineGuard, RenderDeadline};
use crate::resource::{origin_from_url, FetchDestination, FetchRequest, ReferrerPolicy};
use crate::style::media::{MediaContext, MediaQuery, MediaQueryCache, MediaType};
use crate::web::events::{Event, EventInit, EventTargetId};

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use selectors::context::QuirksMode;
use url::Url;

use super::{BrowserDocumentDom2, Pixmap, RenderOptions, RunUntilStableOutcome, RunUntilStableStopReason};

const SCRIPT_BLOCKING_STYLESHEET_SPIN_LIMITS: RunLimits = RunLimits {
  max_tasks: 1024,
  max_microtasks: 4096,
  max_wall_time: None,
};

pub trait BrowserTabJsExecutor {
  fn execute_classic_script(
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
  fn dispatch_lifecycle_event(&mut self, target: EventTargetId, event: &Event) -> Result<()> {
    let _ = (target, event);
    Ok(())
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

pub struct BrowserTabHost {
  trace: TraceHandle,
  document: Box<BrowserDocumentDom2>,
  executor: Box<dyn BrowserTabJsExecutor>,
  current_script: CurrentScriptStateHandle,
  orchestrator: ScriptOrchestrator,
  scheduler: ScriptScheduler<NodeId>,
  scripts: HashMap<ScriptId, ScriptEntry>,
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
  pending_navigation: Option<LocationNavigationRequest>,
  csp: Option<CspPolicy>,
  external_script_sources: HashMap<String, String>,
  script_blocking_stylesheets: ScriptBlockingStyleSheetSet,
  stylesheet_keys_by_node: HashMap<NodeId, usize>,
  next_stylesheet_key: usize,
  stylesheet_media_context: MediaContext,
  stylesheet_media_query_cache: MediaQueryCache,
  js_execution_options: JsExecutionOptions,
  js_execution_depth: Rc<Cell<usize>>,
  lifecycle: DocumentLifecycle,
}

impl BrowserTabHost {
  fn new(
    document: BrowserDocumentDom2,
    executor: Box<dyn BrowserTabJsExecutor>,
    trace: TraceHandle,
    js_execution_options: JsExecutionOptions,
  ) -> Self {
    Self {
      trace,
      document: Box::new(document),
      executor,
      current_script: CurrentScriptStateHandle::default(),
      orchestrator: ScriptOrchestrator::new(),
      scheduler: ScriptScheduler::new(),
      scripts: HashMap::new(),
      deferred_scripts: HashSet::new(),
      executed: HashSet::new(),
      parser_blocked_on: None,
      document_url: None,
      base_url: None,
      document_origin: None,
      document_referrer_policy: ReferrerPolicy::default(),
      pending_navigation: None,
      csp: None,
      external_script_sources: HashMap::new(),
      script_blocking_stylesheets: ScriptBlockingStyleSheetSet::new(),
      stylesheet_keys_by_node: HashMap::new(),
      next_stylesheet_key: 0,
      stylesheet_media_context: MediaContext::default(),
      stylesheet_media_query_cache: MediaQueryCache::default(),
      js_execution_options,
      js_execution_depth: Rc::new(Cell::new(0)),
      lifecycle: DocumentLifecycle::new(),
    }
  }

  fn register_external_script_source(&mut self, url: String, source: String) {
    self.external_script_sources.insert(url, source);
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
    self.scheduler = ScriptScheduler::new();
    self.scripts.clear();
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
    self.js_execution_depth.set(0);
    self.lifecycle = DocumentLifecycle::new();
    self.script_blocking_stylesheets = ScriptBlockingStyleSheetSet::new();
    self.stylesheet_keys_by_node.clear();
    self.next_stylesheet_key = 0;
    self.stylesheet_media_query_cache = MediaQueryCache::default();
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

  fn media_attr_matches_env(
    media_context: &MediaContext,
    cache: &mut MediaQueryCache,
    media: Option<&str>,
  ) -> bool {
    let Some(raw) = media.map(str::trim).filter(|v| !v.is_empty()) else {
      return true;
    };
    let queries = match MediaQuery::parse_list(raw) {
      Ok(queries) => queries,
      Err(_) => vec![MediaQuery::not_all()],
    };
    media_context.evaluate_list_with_cache(&queries, Some(cache))
  }

  fn discover_and_start_script_blocking_stylesheets(
    &mut self,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<()> {
    fn is_html_namespace(namespace: &str) -> bool {
      namespace.is_empty() || namespace == HTML_NAMESPACE
    }

    let dom = self.document.dom();
    let mut base_url_tracker = BaseUrlTracker::new(self.document_url.as_deref());
    let mut stack: Vec<(NodeId, bool, bool, bool)> = Vec::new();
    stack.push((dom.root(), false, false, false));

    while let Some((id, in_head, in_foreign_namespace, in_template)) = stack.pop() {
      let node = dom.node(id);

      // Shadow roots are treated as separate trees for script discovery/execution.
      if matches!(node.kind, NodeKind::ShadowRoot { .. }) {
        continue;
      }

      // Be robust against partially-detached nodes that may still appear in a parent's `children`
      // list.
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

          if tag_name.eq_ignore_ascii_case("link") && is_html_namespace(namespace) {
            // Skip if we already started (or completed) a load for this node.
            if !self.stylesheet_keys_by_node.contains_key(&id) {
              let disabled = dom.has_attribute(id, "disabled").unwrap_or(false);
              if !disabled {
                let rel = dom.get_attribute(id, "rel").ok().flatten();
                let rel_tokens = rel.as_deref().map(tokenize_rel_list).unwrap_or_default();
                let has_stylesheet = rel_tokens.iter().any(|t| t.eq_ignore_ascii_case("stylesheet"));
                let has_alternate = rel_tokens.iter().any(|t| t.eq_ignore_ascii_case("alternate"));
                if has_stylesheet && !has_alternate {
                  let media = dom.get_attribute(id, "media").ok().flatten();
                  if Self::media_attr_matches_env(
                    &self.stylesheet_media_context,
                    &mut self.stylesheet_media_query_cache,
                    media.as_deref(),
                  ) {
                    let href = dom.get_attribute(id, "href").ok().flatten();
                    let base_url = base_url_tracker.current_base_url();
                    let resolved = href
                      .as_deref()
                      .and_then(|href| resolve_href_with_base(base_url.as_deref(), href));
                    if let Some(url) = resolved {
                      let key = self.next_stylesheet_key;
                      self.next_stylesheet_key = self.next_stylesheet_key.saturating_add(1);
                      self.stylesheet_keys_by_node.insert(id, key);
                      self
                        .script_blocking_stylesheets
                        .register_blocking_stylesheet(key);

                      event_loop.queue_task(TaskSource::Networking, move |host, _event_loop| {
                        let load_result = host.load_stylesheet_and_imports(&url);
                        host
                          .script_blocking_stylesheets
                          .unregister_blocking_stylesheet(key);
                        match load_result {
                          Ok(()) => Ok(()),
                          Err(err @ Error::Render(_)) => Err(err),
                          Err(_) => Ok(()),
                        }
                      })?;
                    }
                  }
                }
              }
            }
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

      // Inert subtrees (template contents) should not be traversed.
      if node.inert_subtree {
        continue;
      }

      for &child in node.children.iter().rev() {
        stack.push((child, next_in_head, next_in_foreign_namespace, next_in_template));
      }
    }

    Ok(())
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

  fn script_blocks_on_stylesheets(&self, script_id: ScriptId) -> bool {
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
    // Async external scripts do not participate in stylesheet-blocking semantics.
    if spec.src_attr_present && spec.async_attr {
      return false;
    }
    true
  }

  fn wait_for_stylesheets_if_needed(
    &mut self,
    script_id: ScriptId,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<()> {
    if !self.script_blocks_on_stylesheets(script_id) {
      return Ok(());
    }

    self.discover_and_start_script_blocking_stylesheets(event_loop)?;

    if !self.script_blocking_stylesheets.has_blocking_stylesheet() {
      return Ok(());
    }

    let _ = event_loop.spin_until(
      self,
      SCRIPT_BLOCKING_STYLESHEET_SPIN_LIMITS,
      |host| host.script_blocking_stylesheets.has_blocking_stylesheet(),
    )?;

    // If we hit a run limit or became idle while stylesheets are still pending, give up
    // deterministically so scripts are not blocked forever.
    if self.script_blocking_stylesheets.has_blocking_stylesheet() {
      self.script_blocking_stylesheets = ScriptBlockingStyleSheetSet::new();
    }
    Ok(())
  }

  fn dispatch_script_error_event(&mut self, script: NodeId) -> Result<()> {
    let mut event = Event::new(
      "error",
      EventInit {
        bubbles: false,
        cancelable: false,
        composed: false,
      },
    );
    event.is_trusted = true;
    self.dispatch_lifecycle_event(EventTargetId::Node(script), event)
  }

  fn fail_external_script_fetch(
    &mut self,
    script_id: ScriptId,
    script_node: NodeId,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<()> {
    // HTML: external script fetch failure should dispatch an `error` event and the script should not
    // execute.
    self.dispatch_script_error_event(script_node)?;
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

  fn register_and_schedule_script(
    &mut self,
    node_id: NodeId,
    spec: ScriptElementSpec,
    base_url_at_discovery: Option<String>,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<ScriptId> {
    // HTML `</script>` handling performs a microtask checkpoint *before* preparing the script, but
    // only when the JS execution context stack is empty.
    if spec.parser_inserted && self.js_execution_depth.get() == 0 {
      event_loop.perform_microtask_checkpoint(self)?;
    }

    let spec_for_table = spec.clone();
    let is_deferred = spec_for_table.script_type == ScriptType::Classic
      && spec_for_table.parser_inserted
      && spec_for_table.src_attr_present
      && spec_for_table.src.as_deref().is_some_and(|src| !src.is_empty())
      && spec_for_table.defer_attr
      && !spec_for_table.async_attr;
    if spec_for_table.script_type == ScriptType::Classic && !spec_for_table.src_attr_present {
      self
        .js_execution_options
        .check_script_source(&spec_for_table.inline_text, "source=inline")?;
    }
    let discovered = self
      .scheduler
      .discovered_parser_script(spec, node_id, base_url_at_discovery)?;
    self.scripts.insert(
      discovered.id,
      ScriptEntry {
        node_id,
        spec: spec_for_table,
      },
    );
    if is_deferred {
      self.lifecycle.register_deferred_script();
      self.deferred_scripts.insert(discovered.id);
    }
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
          source_text,
          ..
        } => {
          if self
            .csp
            .as_ref()
            .is_some_and(|csp| csp.blocks_inline_scripts_mvp())
            && self
              .scripts
              .get(&script_id)
              .is_some_and(|entry| entry.spec.script_type == ScriptType::Classic && !entry.spec.src_attr_present)
          {
            let script_node = self
              .scripts
              .get(&script_id)
              .map(|entry| entry.node_id)
              .ok_or_else(|| Error::Other("internal error: missing script entry".to_string()))?;
            self.dispatch_script_error_event(script_node)?;
            self.mutate_dom(|dom| {
              dom.node_mut(script_node).script_already_started = true;
              ((), false)
            });
            self.finish_script_execution(script_id, event_loop)?;
            continue;
          }

          let wait_result = self.wait_for_stylesheets_if_needed(script_id, event_loop);
          let exec_result = match wait_result {
            Ok(()) => {
              let _guard = JsExecutionGuard::enter(&self.js_execution_depth);
              self.execute_script(script_id, &source_text, event_loop)
            }
            Err(err) => Err(err),
          };
          // Ensure a script failure doesn't leave parsing blocked forever.
          self.finish_script_execution(script_id, event_loop)?;
          exec_result?;

          // HTML: "clean up after running script" performs a microtask checkpoint only when the JS
          // execution context stack is empty. Nested (re-entrant) script execution must not drain
          // microtasks until the outermost script returns.
          if self.js_execution_depth.get() == 0 {
            event_loop.perform_microtask_checkpoint(self)?;
          }
        }
        ScriptSchedulerAction::QueueTask {
          script_id,
          source_text,
          ..
        } => {
          event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
            let wait_result = host.wait_for_stylesheets_if_needed(script_id, event_loop);
            let result = match wait_result {
              Ok(()) => {
                let _guard = JsExecutionGuard::enter(&host.js_execution_depth);
                host.execute_script(script_id, &source_text, event_loop)
              }
              Err(err) => Err(err),
            };
            host.finish_script_execution(script_id, event_loop)?;
            result
          })?;
        }
        ScriptSchedulerAction::QueueScriptEventTask { node_id, event, .. } => {
          let type_str = event.as_type_str();
          event_loop.queue_task(TaskSource::DOMManipulation, move |host, _event_loop| {
            let mut ev = Event::new(type_str, EventInit::default());
            ev.is_trusted = true;
            host.dispatch_lifecycle_event(EventTargetId::Node(node_id), ev)?;
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
        if script_type != ScriptType::Classic {
          return Ok(());
        }

        let mut span = host.trace.span("js.script.execute", "js");
        span.arg_u64("script_id", self.script_id.as_u64());
        if let Some(url) = self.spec.src.as_deref() {
          span.arg_str("url", url);
        }
        span.arg_bool("async_attr", self.spec.async_attr);
        span.arg_bool("defer_attr", self.spec.defer_attr);
        span.arg_bool("parser_inserted", self.spec.parser_inserted);

        let current_script = host.current_script_node();
        let result = host.executor.execute_classic_script(
          self.source_text,
          self.spec,
          current_script,
          &mut host.document,
          self.event_loop,
        );
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
      let allowed = parsed
        .as_ref()
        .is_some_and(|parsed| csp.allows_url(CspDirective::ScriptSrc, doc_origin.as_ref(), parsed));
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
      .is_some_and(|entry| entry.spec.src_attr_present && !entry.spec.async_attr && !entry.spec.defer_attr);

    if is_blocking {
      match self.fetch_script_source(script_id, &url, destination) {
        Ok(source) => {
          let actions = self.scheduler.fetch_completed(script_id, source)?;
          self.apply_scheduler_actions(actions, event_loop)?;
        }
        Err(_err) => {
          let actions = self.scheduler.fetch_failed(script_id)?;
          self.apply_scheduler_actions(actions, event_loop)?;
          self.finish_script_execution(script_id, event_loop)?;
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
        Err(_err) => {
          let actions = host.scheduler.fetch_failed(script_id)?;
          host.apply_scheduler_actions(actions, event_loop)?;
          host.finish_script_execution(script_id, event_loop)?;
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

    if let Some(source) = self.external_script_sources.get(url) {
      span.arg_u64("bytes", source.as_bytes().len() as u64);
      self.js_execution_options.check_script_source_bytes(
        source.as_bytes().len(),
        &format!("source=external url={url}"),
      )?;
      if let Some(integrity) = spec.integrity.as_deref() {
        crate::js::sri::verify_integrity_sha256(source.as_bytes(), integrity).map_err(|message| {
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
    if let Some(integrity) = spec.integrity.as_deref() {
      crate::js::sri::verify_integrity_sha256(&resource.bytes, integrity).map_err(|message| {
        Error::Other(format!("SRI blocked script {url}: {message}"))
      })?;
    }
    Ok(String::from_utf8_lossy(&resource.bytes).to_string())
  }
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
      self.dispatch_lifecycle_event(EventTargetId::Document, event)?;
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
    event: crate::web::events::Event,
  ) -> Result<()> {
    let result = self.executor.dispatch_lifecycle_event(target, &event);
    if let Some(req) = self.executor.take_navigation_request() {
      self.pending_navigation = Some(req);
    }
    match result {
      Ok(()) => Ok(()),
      Err(err) if self.pending_navigation.is_some() => Ok(()),
      Err(err) => Err(err),
    }
  }

  fn document_lifecycle_mut(&mut self) -> &mut DocumentLifecycle {
    &mut self.lifecycle
  }
}

impl crate::js::html_script_pipeline::ScriptElementEventHost for BrowserTabHost {
  fn dispatch_script_element_event(&mut self, script: NodeId, event_name: &'static str) -> Result<()> {
    use crate::web::events::{dispatch_event, DomError, EventListenerInvoker, ListenerId};

    struct NoopInvoker;

    impl EventListenerInvoker for NoopInvoker {
      fn invoke(
        &mut self,
        _listener_id: ListenerId,
        _event: &mut crate::web::events::Event,
      ) -> std::result::Result<(), DomError> {
        Ok(())
      }
    }

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

    let dom: &crate::dom2::Document = self.document.dom();
    let mut invoker = NoopInvoker;
    dispatch_event(EventTargetId::Node(script), &mut event, dom, dom.events(), &mut invoker)
      .map(|_default_not_prevented| ())
      .map_err(|err| Error::Other(err.to_string()))
  }
}

pub struct BrowserTab {
  trace: TraceHandle,
  trace_output: Option<PathBuf>,
  host: BrowserTabHost,
  event_loop: EventLoop<BrowserTabHost>,
  pending_frame: Option<Pixmap>,
}

impl BrowserTab {
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
    // `check_active_periodic`, but it does not accept `RenderOptions` directly. Install a scoped
    // deadline so callers can cancel/timeout large HTML parses via `RenderOptions::{timeout,cancel_callback}`.
    let deadline_enabled = options.timeout.is_some() || options.cancel_callback.is_some();
    let _deadline_guard = deadline_enabled.then(|| {
      let deadline = RenderDeadline::new(options.timeout, options.cancel_callback.clone());
      DeadlineGuard::install(Some(&deadline))
    });
    let mut parser = StreamingHtmlParser::new(document_url);

    // Feed HTML into the streaming parser incrementally so URL navigations can reuse this path by
    // chunking network bytes. This keeps parsing cancellable (deadline checks happen during each
    // `pump`) and avoids pushing arbitrarily large strings into html5ever in a single call.
    const INPUT_CHUNK_BYTES: usize = 8 * 1024;
    let mut offset = 0usize;
    while offset < html.len() {
      let mut end = (offset + INPUT_CHUNK_BYTES).min(html.len());
      while end < html.len() && !html.is_char_boundary(end) {
        end += 1;
      }
      debug_assert!(html.is_char_boundary(offset));
      debug_assert!(html.is_char_boundary(end));
      parser.push_str(&html[offset..end]);
      offset = end;

      loop {
        match parser.pump()? {
          StreamingParserYield::Script {
            script,
            base_url_at_this_point,
          } => {
            let snapshot = {
              let Some(doc) = parser.document() else {
                return Err(Error::Other(
                  "StreamingHtmlParser yielded a script without an active document".to_string(),
                ));
              };
              doc.clone_with_events()
            };

            self.host.mutate_dom(|dom| {
              *dom = snapshot;
              ((), true)
            });

            // Keep the host's base URL in sync with the parser state so any JS executed at this pause
            // point (including microtasks drained before script execution) resolves relative URLs
            // against the correct base.
            self.host.base_url = base_url_at_this_point.clone();
            self
              .host
              .executor
              .on_document_base_url_updated(self.host.base_url.as_deref());

            let base = BaseUrlTracker::new(base_url_at_this_point.as_deref());
            let spec = crate::js::streaming_dom2::build_parser_inserted_script_element_spec_dom2(
              self.host.dom(),
              script,
              &base,
            );
            let base_url_at_discovery = spec.base_url.clone();
            with_active_streaming_parser(&parser, || {
              self.on_parser_discovered_script(script, spec, base_url_at_discovery)
            })?;

            if self.host.pending_navigation.is_some() {
              // Abort the current parse/execution; the caller will commit the navigation.
              return Ok(parser.current_base_url());
            }

            // Sync any DOM mutations from the executed script back into the streaming parser's live
            // DOM before resuming parsing.
            let updated = self.host.dom().clone_with_events();
            {
              let Some(mut doc) = parser.document_mut() else {
                return Err(Error::Other(
                  "StreamingHtmlParser yielded a script without an active document".to_string(),
                ));
              };
              *doc = updated;
            }
          }
          StreamingParserYield::NeedMoreInput => break,
          StreamingParserYield::Finished { .. } => {
            return Err(Error::Other(
              "StreamingHtmlParser unexpectedly finished before EOF".to_string(),
            ))
          }
        }
      }
    }

    parser.set_eof();

    loop {
      match parser.pump()? {
        StreamingParserYield::Script {
          script,
          base_url_at_this_point,
        } => {
          let snapshot = {
            let Some(doc) = parser.document() else {
              return Err(Error::Other(
                "StreamingHtmlParser yielded a script without an active document".to_string(),
              ));
            };
            doc.clone_with_events()
          };

          self.host.mutate_dom(|dom| {
            *dom = snapshot;
            ((), true)
          });

          // Keep the host's base URL in sync with the parser state so any JS executed at this pause
          // point (including microtasks drained before script execution) resolves relative URLs
          // against the correct base.
          self.host.base_url = base_url_at_this_point.clone();
          self
            .host
            .executor
            .on_document_base_url_updated(self.host.base_url.as_deref());
          if !self.host.dom().is_connected_for_scripting(script) {
            self.host.mutate_dom(|dom| {
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

            let updated = self.host.dom().clone_with_events();
            {
              let Some(mut doc) = parser.document_mut() else {
                return Err(Error::Other(
                  "StreamingHtmlParser yielded a script without an active document".to_string(),
                ));
              };
              *doc = updated;
            }
            continue;
          }

          // HTML: before executing a parser-inserted script at a script end-tag boundary, perform a
          // microtask checkpoint when the JS execution context stack is empty.
          //
          // Parsing may be driven outside the event loop (e.g. parse-time script execution) or
          // inside event-loop tasks; use explicit JS execution depth tracking rather than
          // `EventLoop::currently_running_task()`.
          if self.host.js_execution_depth.get() == 0 {
            self.event_loop.perform_microtask_checkpoint(&mut self.host)?;
          }

          let base = BaseUrlTracker::new(base_url_at_this_point.as_deref());
          let spec = crate::js::streaming_dom2::build_parser_inserted_script_element_spec_dom2(
            self.host.dom(),
            script,
            &base,
          );
          let base_url_at_discovery = spec.base_url.clone();
          with_active_streaming_parser(&parser, || {
            self.on_parser_discovered_script(script, spec, base_url_at_discovery)
          })?;

          if self.host.pending_navigation.is_some() {
            // Abort the current parse/execution; the caller will commit the navigation.
            return Ok(parser.current_base_url());
          }

          // Sync any DOM mutations from the executed script back into the streaming parser's live
          // DOM before resuming parsing.
          let updated = self.host.dom().clone_with_events();
          {
            let Some(mut doc) = parser.document_mut() else {
              return Err(Error::Other(
                "StreamingHtmlParser yielded a script without an active document".to_string(),
              ));
            };
            *doc = updated;
          }
        }
        StreamingParserYield::NeedMoreInput => {
          return Err(Error::Other(
            "StreamingHtmlParser unexpectedly requested more input after EOF".to_string(),
          ));
        }
        StreamingParserYield::Finished { document } => {
          let final_base_url = parser.current_base_url();
          // Persist the final base URL after parsing completes so any later JS-visible URL
          // resolution uses the post-parse `<base href>` result.
          self.host.base_url = final_base_url.clone();
          self
            .host
            .executor
            .on_document_base_url_updated(self.host.base_url.as_deref());
          self.host.mutate_dom(|dom| {
            *dom = document;
            ((), true)
          });
          self.on_parsing_completed()?;
          return Ok(final_base_url);
        }
      }
    }
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
    let renderer = super::FastRender::builder()
      .dom_scripting_enabled(true)
      .build()?;
    let document = BrowserDocumentDom2::new(renderer, "", options.clone())?;
    let host = BrowserTabHost::new(
      document,
      Box::new(executor),
      trace_handle.clone(),
      js_execution_options,
    );
    let mut event_loop = EventLoop::new();
    event_loop.set_trace_handle(trace_handle.clone());
    event_loop.set_queue_limits(js_execution_options.event_loop_queue_limits);

    let mut tab = Self {
      trace: trace_handle,
      trace_output,
      host,
      event_loop,
      pending_frame: None,
    };
    let document_referrer_policy = crate::html::referrer_policy::extract_referrer_policy_from_html(html)
      .unwrap_or_default();
    tab
      .host
      .reset_scripting_state(None, document_referrer_policy)?;
    let base_url = tab.parse_html_streaming_and_schedule_scripts(html, None, &options)?;
    if let Some(req) = tab.host.pending_navigation.take() {
      tab.navigate_to_url(&req.url, options.clone())?;
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

    // Parse-time script execution requires a script-aware streaming parser driver. Start the tab
    // with an empty DOM and then stream-parse the provided HTML, pausing at `</script>` boundaries.
    let renderer = super::FastRender::builder()
      .dom_scripting_enabled(true)
      .build()?;
    let document = BrowserDocumentDom2::new(renderer, "", options.clone())?;
    let host = BrowserTabHost::new(
      document,
      Box::new(executor),
      trace_handle.clone(),
      js_execution_options,
    );
    event_loop.set_trace_handle(trace_handle.clone());
    event_loop.set_queue_limits(js_execution_options.event_loop_queue_limits);

    let mut tab = Self {
      trace: trace_handle,
      trace_output,
      host,
      event_loop,
      pending_frame: None,
    };
    let document_referrer_policy =
      crate::html::referrer_policy::extract_referrer_policy_from_html(html).unwrap_or_default();
    tab.host.reset_scripting_state(None, document_referrer_policy)?;
    let base_url = tab.parse_html_streaming_and_schedule_scripts(html, None, &options)?;
    if let Some(req) = tab.host.pending_navigation.take() {
      tab.navigate_to_url(&req.url, options.clone())?;
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

  pub fn write_trace(&self) -> Result<()> {
    let Some(path) = self.trace_output.as_deref() else {
      return Ok(());
    };
    self.trace.write_chrome_trace(path).map_err(Error::Io)
  }

  pub fn navigate_to_html(&mut self, html: &str, options: RenderOptions) -> Result<()> {
    // Navigations replace the current document; any pending frame is no longer relevant.
    self.pending_frame = None;
    let trace_session = super::TraceSession::from_options(Some(&options));
    self.trace = trace_session.handle.clone();
    self.trace_output = trace_session.output.clone();

    let options_for_parse = options.clone();
    self
      .host
      .document
      .reset_with_dom(Document::new(QuirksMode::NoQuirks), options);
    self.reset_event_loop();
    self.host.trace = self.trace.clone();
    let document_referrer_policy = crate::html::referrer_policy::extract_referrer_policy_from_html(html)
      .unwrap_or_default();

    // Clear URL hints so relative resources do not resolve against the previous navigation.
    {
      let renderer = self.host.document.renderer_mut();
      renderer.clear_document_url();
      renderer.clear_base_url();
    }

    self
      .host
      .reset_scripting_state(None, document_referrer_policy)?;
    let base_url = self.parse_html_streaming_and_schedule_scripts(html, None, &options_for_parse)?;
    if let Some(req) = self.host.pending_navigation.take() {
      self.navigate_to_url(&req.url, options_for_parse.clone())?;
      return Ok(());
    }

    // Update the renderer's base URL hint to match the parse-time base URL after processing the
    // full document.
    let renderer = self.host.document.renderer_mut();
    match base_url {
      Some(url) => renderer.set_base_url(url),
      None => renderer.clear_base_url(),
    }

    Ok(())
  }

  pub fn navigate_to_url(&mut self, url: &str, options: RenderOptions) -> Result<()> {
    // Navigations replace the current document; any pending frame is no longer relevant.
    self.pending_frame = None;
    let trace_session = super::TraceSession::from_options(Some(&options));
    self.trace = trace_session.handle.clone();
    self.trace_output = trace_session.output.clone();
    let mut target_url = url.to_string();
    loop {
      // Install a root deadline so `RenderOptions::{timeout,cancel_callback}` bounds:
      // - the document fetch phase,
      // - and the subsequent script-aware streaming HTML parse.
      //
      // This mirrors `FastRender::prepare_url`'s fetch-time deadline guard, but drives
      // `StreamingHtmlParser` so parser-inserted scripts execute at `</script>` boundaries against a
      // partially-built DOM.
      let deadline_enabled = options.timeout.is_some() || options.cancel_callback.is_some();
      let _deadline_guard = deadline_enabled.then(|| {
        let deadline = RenderDeadline::new(options.timeout, options.cancel_callback.clone());
        DeadlineGuard::install(Some(&deadline))
      });

      // Fetch the document first so a failed request doesn't clobber the existing navigation's
      // committed DOM.
      let fetcher = self.host.document.fetcher();
      let resource = fetcher.fetch_with_request(FetchRequest::document(&target_url))?;
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
        continue;
      }

      // Update the renderer's base URL hint to match the parse-time base URL after processing the
      // full document.
      let renderer = self.host.document.renderer_mut();
      match base_url {
        Some(url) => renderer.set_base_url(url),
        None => renderer.clear_base_url(),
      }

      return Ok(());
    }
  }

  fn commit_pending_navigation(&mut self) -> Result<bool> {
    let Some(req) = self.host.pending_navigation.take() else {
      return Ok(false);
    };
    let options = self.host.document.options().clone();
    self.navigate_to_url(&req.url, options)?;
    Ok(true)
  }

  fn run_event_loop_until_idle_handling_errors_with_pending_navigation_abort(
    &mut self,
    limits: RunLimits,
    mut on_error: impl FnMut(Error),
    mut on_render: impl FnMut(),
  ) -> Result<RunUntilIdleOutcome> {
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
        Ok(())
      },
    )
  }

  pub fn run_event_loop_until_idle(&mut self, limits: RunLimits) -> Result<RunUntilIdleOutcome> {
    let trace = self.trace.clone();
    let outcome = self.run_event_loop_until_idle_handling_errors_with_pending_navigation_abort(
      limits,
      move |err| {
        // Match browser behavior: report uncaught task errors but keep the event loop running.
        if trace.is_enabled() {
          let mut span = trace.span("js.uncaught_exception", "js");
          span.arg_str("message", &err.to_string());
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
    let mut report_error = move |err: Error| {
      // Uncaught JS exceptions should not abort event-loop execution (browser behavior). For now,
      // record them into the trace when tracing is enabled.
      if trace.is_enabled() {
        let mut span = trace.span("js.uncaught_exception", "js");
        span.arg_str("message", &err.to_string());
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
    let run_limits = self.host.js_execution_options.event_loop_run_limits;
    if self.event_loop.pending_microtask_count() > 0 {
      // Drain microtasks only (HTML microtask checkpoint), but do not run any tasks.
      let microtask_limits = RunLimits {
        max_tasks: 0,
        max_microtasks: run_limits.max_microtasks,
        max_wall_time: run_limits.max_wall_time,
      };
      match self.event_loop.run_until_idle(&mut self.host, microtask_limits)? {
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
      match self.event_loop.run_until_idle(&mut self.host, one_task_limits)? {
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
    let mut event_loop = EventLoop::new();
    event_loop.set_trace_handle(self.trace.clone());
    event_loop.set_queue_limits(self.host.js_execution_options.event_loop_queue_limits);
    self.event_loop = event_loop;
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

  use crate::js::runtime::with_event_loop;
  use crate::js::window_realm::{register_dom_source, unregister_dom_source};
  use crate::js::{WindowRealm, WindowRealmConfig};
  use std::cell::{Cell, RefCell};
  use std::collections::HashMap;
  use std::ptr::NonNull;
  use std::rc::Rc;
  use std::sync::atomic::{AtomicUsize, Ordering};
  use std::sync::{Arc, Mutex};

  use crate::resource::{FetchedResource, ResourceFetcher};
  use vm_js::Value;

  use tempfile::tempdir;
  use url::Url;

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
  }

  fn build_host(html: &str, log: Rc<RefCell<Vec<String>>>) -> Result<(BrowserTabHost, EventLoop<BrowserTabHost>)> {
    let document = BrowserDocumentDom2::from_html(html, RenderOptions::default())?;
    let mut host = BrowserTabHost::new(
      document,
      Box::new(TestExecutor { log }),
      TraceHandle::default(),
      JsExecutionOptions::default(),
    );
    host.reset_scripting_state(None, ReferrerPolicy::default())?;
    Ok((host, EventLoop::new()))
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
    );
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
    );
    host.reset_scripting_state(None, ReferrerPolicy::default())?;
    Ok((host, EventLoop::new()))
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

  #[test]
  fn vm_js_document_ready_state_tracks_document_lifecycle_transitions() -> Result<()> {
    let document = BrowserDocumentDom2::from_html("<!doctype html><html></html>", RenderOptions::default())?;
    let mut host = BrowserTabHost::new(
      document,
      Box::new(NoopExecutor::default()),
      TraceHandle::default(),
      JsExecutionOptions::default(),
    );
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
    );
    let mut tab = BrowserTab {
      trace: TraceHandle::default(),
      trace_output: None,
      host,
      event_loop: EventLoop::new(),
      pending_frame: None,
    };
    tab
      .host
      .reset_scripting_state(Some(document_url.clone()), ReferrerPolicy::default())?;

    let html = r#"<!doctype html>
      <link rel="stylesheet" href="style.css">
      <script src="script.js"></script>
    "#;
    tab.parse_html_streaming_and_schedule_scripts(html, Some(&document_url), &options)?;

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
    );
    let mut tab = BrowserTab {
      trace: TraceHandle::default(),
      trace_output: None,
      host,
      event_loop: EventLoop::new(),
      pending_frame: None,
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
}
