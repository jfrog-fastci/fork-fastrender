use crate::css::encoding::decode_css_bytes_cow;
use crate::css::loader::resolve_href_with_base;
use crate::css::parser::parse_stylesheet_with_media;
use crate::css::types::CssImportLoader;
use crate::debug::runtime::RuntimeToggles;
use crate::debug::trace::TraceHandle;
use crate::dom::HTML_NAMESPACE;
use crate::dom2::{Document, NodeId, NodeKind};
use crate::error::{Error, Result};
use crate::html::base_url_tracker::BaseUrlTracker;
use crate::html::content_security_policy::CspPolicy;
use crate::html::document_write::with_active_streaming_parser;
use crate::html::encoding::decode_html_bytes;
use crate::html::streaming_parser::{StreamingHtmlParser, StreamingParserYield};
use crate::js::html_script_scheduler::ScriptEventKind;
use crate::js::script_encoding::decode_classic_script_bytes;
use crate::js::webidl::VmJsWebIdlBindingsHostDispatch;
use crate::js::{
  Clock, CurrentScriptHost, CurrentScriptStateHandle, DocumentLifecycle, DocumentLifecycleHost,
  DocumentReadyState, DocumentWriteState, DomHost, EventLoop, HtmlScriptId, HtmlScriptScheduler,
  HtmlScriptSchedulerAction, HtmlScriptWork, JsDomEvents, JsExecutionOptions, LoadBlockerKind,
  LocationNavigationRequest, RunAnimationFrameOutcome, RunLimits, RunUntilIdleOutcome,
  RunUntilIdleStopReason, ScriptBlockExecutor, ScriptBlockingStyleSheetSet, ScriptElementSpec,
  ScriptOrchestrator, ScriptType, TaskSource, WindowRealmHost,
};
use crate::render_control::{DeadlineGuard, RenderDeadline};
use crate::resource::ResourceFetcher;
use crate::resource::{origin_from_url, FetchDestination, FetchRequest, ReferrerPolicy};
use crate::scroll::ScrollState;
use crate::style::media::{MediaContext, MediaQuery, MediaQueryCache, MediaType};
use crate::ui::TabHistory;
use crate::web::dom::DocumentVisibilityState;
use crate::web::events::{Event, EventInit, EventTargetId, MouseEvent};
use crate::ui::{PointerButton, PointerModifiers};

use encoding_rs::{Encoding, UTF_8};

#[cfg(feature = "a11y_accesskit")]
use accesskit::{
  Action as AccessKitAction, ActionData as AccessKitActionData,
  ActionRequest as AccessKitActionRequest, NodeId as AccessKitNodeId,
};
#[cfg(feature = "a11y_accesskit")]
use std::num::NonZeroU128;
use std::cell::Cell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use selectors::context::QuirksMode;
use url::Url;

use super::{
  BrowserDocumentDom2, Dom2HitTestResult, Pixmap, RenderOptions, RunUntilStableOutcome,
  RunUntilStableStopReason,
};

const MODULE_GRAPH_FETCH_UNSUPPORTED_MESSAGE: &str =
  "module graph fetching is not supported by this BrowserTabJsExecutor";

const RAF_TICK_CADENCE: Duration = Duration::from_millis(16);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleScriptExecutionStatus {
  /// The module completed evaluation within the current call.
  ///
  /// This includes both:
  /// - modules without top-level await, and
  /// - top-level await that resolves synchronously via microtasks before returning control.
  Completed,
  /// The module has started evaluation but has not completed yet (e.g. top-level await).
  ///
  /// The executor must later notify the host (via the event loop) when the evaluation promise
  /// settles so the host can finalize script execution and unblock ordered module queues.
  Pending,
}

/// Selection actions used by accessibility integrations (e.g. AccessKit) for option/listbox widgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionAction {
  /// Replace the current selection with the target item (clearing other selected items when the
  /// widget supports multiple selection).
  SetSelection,
  /// Add the target item to the current selection (no-op when the widget is not multi-selectable).
  AddToSelection,
  /// Remove the target item from the current selection (no-op when it is already unselected).
  RemoveFromSelection,
}

#[cfg(all(test, feature = "a11y_accesskit"))]
mod accesskit_expand_collapse_tests {
  use super::*;

  fn limits() -> RunLimits {
    RunLimits {
      max_tasks: 1024,
      max_microtasks: 1024,
      max_wall_time: None,
    }
  }

  fn options() -> RenderOptions {
    RenderOptions::new()
      .with_viewport(320, 200)
      .with_layout_parallelism(crate::LayoutParallelism::disabled())
      .with_paint_parallelism(crate::PaintParallelism::disabled())
  }

  #[test]
  fn accesskit_expand_collapse_toggles_details_open_and_fires_toggle_event() -> Result<()> {
    let html = r#"
      <details id=d>
        <summary id=s>More</summary>
        <div>Body</div>
      </details>
      <script>
        document.body.setAttribute('data-toggle-count', '0');
        document.getElementById('d').addEventListener('toggle', () => {
          const current = Number(document.body.getAttribute('data-toggle-count') || '0');
          document.body.setAttribute('data-toggle-count', String(current + 1));
        });
      </script>
    "#;

    let mut tab = BrowserTab::from_html_with_vmjs(html, options())?;
    // Ensure the inline script has executed so the event listener is installed.
    let _ = tab.run_event_loop_until_idle(limits())?;

    // Ensure we have a renderer↔dom2 mapping so AccessKit NodeIds can be decoded.
    let _ = tab.render_frame()?;

    let summary = tab
      .dom()
      .get_element_by_id("s")
      .expect("summary element should exist");
    let details = tab
      .dom()
      .get_element_by_id("d")
      .expect("details element should exist");
    let body = tab.dom().body().expect("body element should exist");

    let summary_accesskit = tab
      .accesskit_node_id_for_dom2_node(summary)
      .expect("summary should map to an AccessKit node");

    tab.dispatch_accesskit_action(summary_accesskit, accesskit::Action::Expand)?;
    assert!(
      tab.dom().has_attribute(details, "open").unwrap_or(false),
      "expected <details> to be opened via AccessKit expand"
    );
    assert_eq!(
      tab.dom()
        .get_attribute(body, "data-toggle-count")
        .unwrap()
        .unwrap_or(""),
      "1",
      "expected toggle event to fire on expand"
    );

    tab.dispatch_accesskit_action(summary_accesskit, accesskit::Action::Collapse)?;
    assert!(
      !tab.dom().has_attribute(details, "open").unwrap_or(true),
      "expected <details> open attribute to be removed via AccessKit collapse"
    );
    assert_eq!(
      tab.dom()
        .get_attribute(body, "data-toggle-count")
        .unwrap()
        .unwrap_or(""),
      "2",
      "expected toggle event to fire on collapse"
    );

    Ok(())
  }

  #[test]
  fn accesskit_expand_updates_aria_expanded_when_present() -> Result<()> {
    let html = r#"<div id=x role=button aria-expanded=false></div>"#;
    let mut tab = BrowserTab::from_html_with_vmjs(html, options())?;
    let _ = tab.run_event_loop_until_idle(limits())?;
    let _ = tab.render_frame()?;

    let x = tab
      .dom()
      .get_element_by_id("x")
      .expect("aria-expanded element should exist");
    let x_accesskit = tab
      .accesskit_node_id_for_dom2_node(x)
      .expect("aria-expanded element should map to an AccessKit node");

    tab.dispatch_accesskit_action(x_accesskit, accesskit::Action::Expand)?;
    assert_eq!(
      tab.dom()
        .get_attribute(x, "aria-expanded")
        .unwrap()
        .unwrap_or(""),
      "true",
      "expected aria-expanded to update to true"
    );

    Ok(())
  }

  #[test]
  fn accesskit_set_value_action_request_updates_dom_state_and_dispatches_input_event() -> Result<()> {
    let html = r#"
      <input id=x value=old>
      <script>
        globalThis.__seen = '';
        const el = document.getElementById('x');
        el.addEventListener('input', (e) => { globalThis.__seen = e.target.value; });
      </script>
    "#;
    let mut tab = BrowserTab::from_html_with_vmjs(html, options())?;
    // Ensure the inline script has executed so the input listener is installed.
    let _ = tab.run_event_loop_until_idle(limits())?;
    // Ensure we have a renderer↔dom2 mapping so AccessKit NodeIds can be decoded.
    let _ = tab.render_frame()?;

    let input = tab
      .dom()
      .get_element_by_id("x")
      .expect("input element should exist");
    let input_accesskit = tab
      .accesskit_node_id_for_dom2_node(input)
      .expect("input should map to an AccessKit node");

    let handled = tab.dispatch_accesskit_action_request(accesskit::ActionRequest {
      action: accesskit::Action::SetValue,
      target: input_accesskit,
      data: Some(accesskit::ActionData::Value("new".into())),
    })?;
    assert!(handled);

    assert_eq!(tab.dom().input_value(input).expect("input_value"), "new");

    // Verify the JS event handler ran and observed the updated value via `event.target.value`.
    let realm = tab
      .host
      .executor
      .window_realm_mut()
      .expect("expected vm-js WindowRealm");
    let seen = realm
      .exec_script("globalThis.__seen")
      .map_err(|err| Error::Other(err.to_string()))?;
    let vm_js::Value::String(seen_s) = seen else {
      return Err(Error::Other(format!(
        "expected globalThis.__seen to be a string, got {seen:?}"
      )));
    };
    assert_eq!(realm.heap().get_string(seen_s).unwrap().to_utf8_lossy(), "new");

    Ok(())
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModuleScriptEvaluationOutcome {
  Fulfilled,
  Rejected,
}

pub trait BrowserTabJsExecutor {
  /// Returns `true` when this executor provides DOM bindings that already prepare dynamic `<script>`
  /// elements via insertion/attribute mutation steps.
  ///
  /// When enabled, [`BrowserTabHost`] can avoid redundant full-DOM scans for dynamically inserted
  /// scripts and instead rely on incremental notifications (for example: vm-js DOM shims calling
  /// [`BrowserTabHost::register_and_schedule_dynamic_script`]).
  fn supports_incremental_dynamic_script_discovery(&self) -> bool {
    false
  }

  /// Notify the executor that the document referrer policy has been set/updated for the current
  /// navigation.
  ///
  /// This is used by module graph loading so module fetch requests can honor:
  /// - `<script referrerpolicy>` overrides, and
  /// - the document's default referrer policy (e.g. `<meta name="referrer">` / `Referrer-Policy` header).
  fn on_document_referrer_policy_updated(&mut self, _policy: ReferrerPolicy) {}

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
    _script_id: HtmlScriptId,
    _script_text: &str,
    _spec: &ScriptElementSpec,
    _current_script: Option<NodeId>,
    _document: &mut BrowserDocumentDom2,
    _event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<ModuleScriptExecutionStatus> {
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

  /// Called by the host after it drains the microtask queue ("perform a microtask checkpoint").
  ///
  /// This lets JS executors finalize work that is expected to settle via Promise jobs/microtasks
  /// (for example, module top-level await that resumes via `Promise.then` reactions).
  fn after_microtask_checkpoint(
    &mut self,
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

  /// Dispatch a `beforeunload` event on the current document and return whether navigation should
  /// proceed.
  ///
  /// The default implementation always allows navigation. JS-capable executors (e.g. the `vm-js`
  /// executor) should override this so scripts can cancel navigations via `beforeunload`.
  fn dispatch_beforeunload_event(
    &mut self,
    _document: &mut BrowserDocumentDom2,
    _event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<bool> {
    Ok(true)
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
    _event_loop: &mut EventLoop<BrowserTabHost>,
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
  fn event_listener_invoker(&self) -> Option<Box<dyn crate::web::events::EventListenerInvoker>> {
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
  script_id: HtmlScriptId,
  source_text: String,
}

#[derive(Debug, Clone)]
struct ImageLoadState {
  url: String,
  request_id: u64,
}

#[derive(Clone, Copy, Debug)]
struct ActiveDataTransfer {
  obj: vm_js::GcObject,
  root_id: vm_js::RootId,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParseUntilBlockedContinueReason {
  /// Parsing yielded because the per-task HTML parse budget was exhausted.
  BudgetExhausted,
  /// Parsing yielded because an "as soon as possible" (`async` / ordered-asap) script is ready to
  /// execute and should run before parsing continues.
  PendingAsapScriptExecution,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParseUntilBlockedResult {
  /// Parsing should not continue immediately:
  /// - the parser is blocked (e.g. on stylesheets),
  /// - parsing finished,
  /// - parsing aborted for navigation,
  /// - or parsing was re-entered and the call was ignored.
  Done,
  /// Parsing should continue on a future event-loop turn.
  Continue(ParseUntilBlockedContinueReason),
}

impl ParseUntilBlockedResult {
  fn should_continue(self) -> bool {
    matches!(self, ParseUntilBlockedResult::Continue(_))
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScriptExecutionCompletion {
  Completed,
  PendingModuleEvaluation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OrderedScriptQueueKind {
  OrderedAsap,
  PostParse,
}

/// Host state for [`BrowserTab`]'s HTML event loop.
///
/// The [`crate::js::EventLoop`] is parameterized by a host type, and tasks are executed with
/// `&mut BrowserTabHost`. Most embedders should use [`BrowserTab`] directly instead of manipulating
/// the host state.
pub struct BrowserTabHost {
  trace: TraceHandle,
  document: Box<BrowserDocumentDom2>,
  executor: Box<dyn BrowserTabJsExecutor>,
  /// Lazily-created vm-js [`crate::js::WindowRealm`] used when the active executor does not expose a
  /// realm.
  ///
  /// Most production paths use [`super::VmJsBrowserTabExecutor`], which provides its own realm. This slot
  /// exists to keep `WindowRealmHost::vm_host_and_window_realm` non-panicking for executors that do
  /// not support vm-js, and for defensive handling when a realm is unexpectedly absent.
  vmjs_fallback_realm: Option<crate::js::WindowRealm>,
  event_invoker: Box<dyn crate::web::events::EventListenerInvoker>,
  js_events: JsDomEvents,
  current_script: CurrentScriptStateHandle,
  script_execution_log: Option<crate::js::ScriptExecutionLog>,
  script_execution_log_capacity: Option<usize>,
  orchestrator: ScriptOrchestrator,
  scheduler: HtmlScriptScheduler<NodeId>,
  scripts: HashMap<HtmlScriptId, ScriptEntry>,
  scheduled_script_nodes: HashSet<NodeId>,
  deferred_scripts: HashSet<HtmlScriptId>,
  executed: HashSet<HtmlScriptId>,
  /// Module scripts whose evaluation has started but not yet completed (e.g. due to top-level
  /// await).
  pending_module_executions: HashSet<HtmlScriptId>,
  pending_script_load_blockers: HashSet<HtmlScriptId>,
  pending_asap_script_executions: HashSet<HtmlScriptId>,
  pending_image_load_blockers: HashSet<(NodeId, u64)>,
  image_load_state: HashMap<NodeId, ImageLoadState>,
  next_image_load_request_id: u64,
  /// The currently in-flight ordered-asap module script (dynamic `type=module` with
  /// `async=false`/`force_async=false`).
  ///
  /// Ordered module scripts must not start until the prior one finishes evaluation (including
  /// top-level await).
  in_flight_ordered_asap_module: Option<HtmlScriptId>,
  queued_ordered_asap_scripts: VecDeque<HtmlScriptSchedulerAction<NodeId>>,
  /// The currently in-flight post-parse module script (parser-inserted, non-async `type=module`).
  in_flight_post_parse_module: Option<HtmlScriptId>,
  queued_post_parse_scripts: VecDeque<HtmlScriptSchedulerAction<NodeId>>,
  /// Script execution tasks (`HtmlScriptSchedulerAction::QueueTask`) that were deferred because the
  /// document still has script-blocking stylesheets.
  ///
  /// HTML requires scripts in the "list of scripts that will execute when the document has
  /// finished parsing" (classic `defer` and parser-inserted non-async module scripts) to wait until
  /// there is no style sheet blocking scripts.
  queued_stylesheet_blocked_script_tasks: VecDeque<HtmlScriptSchedulerAction<NodeId>>,
  parser_blocked_on: Option<HtmlScriptId>,
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
  webidl_bindings_host: Box<VmJsWebIdlBindingsHostDispatch<BrowserTabHost>>,
  pending_dynamic_script_candidates: VecDeque<NodeId>,
  last_dynamic_script_discovery_generation: u64,
  #[cfg(test)]
  dynamic_script_full_scan_count: u64,
  last_image_load_discovery_generation: Option<u64>,
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
  next_data_transfer_id: u64,
  active_data_transfers: HashMap<u64, ActiveDataTransfer>,
}

impl BrowserTabHost {
  fn new(
    mut document: BrowserDocumentDom2,
    mut executor: Box<dyn BrowserTabJsExecutor>,
    trace: TraceHandle,
    js_execution_options: JsExecutionOptions,
  ) -> Result<Self> {
    let mut webidl_bindings_host =
      Box::new(VmJsWebIdlBindingsHostDispatch::<BrowserTabHost>::new_without_global());
    executor.set_webidl_bindings_host(webidl_bindings_host.as_mut());
    let event_invoker = executor
      .event_listener_invoker()
      .unwrap_or_else(|| Box::new(NoopEventInvoker));
    let current_script = CurrentScriptStateHandle::default();
    let mut document_write_state = DocumentWriteState::default();
    document_write_state.update_limits(js_execution_options);
    let mut scheduler = HtmlScriptScheduler::new();
    scheduler.set_options(js_execution_options);
    Ok(Self {
      trace,
      document: Box::new(document),
      executor,
      vmjs_fallback_realm: None,
      event_invoker,
      js_events: JsDomEvents::new()?,
      current_script,
      script_execution_log: None,
      script_execution_log_capacity: None,
      orchestrator: ScriptOrchestrator::new(),
      scheduler,
      scripts: HashMap::new(),
      scheduled_script_nodes: HashSet::new(),
      deferred_scripts: HashSet::new(),
      executed: HashSet::new(),
      pending_module_executions: HashSet::new(),
      pending_script_load_blockers: HashSet::new(),
      pending_asap_script_executions: HashSet::new(),
      pending_image_load_blockers: HashSet::new(),
      image_load_state: HashMap::new(),
      next_image_load_request_id: 1,
      in_flight_ordered_asap_module: None,
      queued_ordered_asap_scripts: VecDeque::new(),
      in_flight_post_parse_module: None,
      queued_post_parse_scripts: VecDeque::new(),
      queued_stylesheet_blocked_script_tasks: VecDeque::new(),
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
      pending_dynamic_script_candidates: VecDeque::new(),
      last_dynamic_script_discovery_generation: 0,
      #[cfg(test)]
      dynamic_script_full_scan_count: 0,
      last_image_load_discovery_generation: None,
      streaming_parse_active: false,
      streaming_parse_in_progress: false,
      streaming_parse: None,
      pending_parser_blocking_script: None,
      next_data_transfer_id: 1,
      active_data_transfers: HashMap::new(),
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

  /// Poll the JS executor for any pending `window.location` navigation request and, if one exists,
  /// decide whether it should proceed by dispatching `beforeunload`.
  ///
  /// Returns `true` if a navigation request was produced (even if it was later canceled by
  /// `beforeunload`).
  fn poll_navigation_request(&mut self, event_loop: &mut EventLoop<Self>) -> Result<bool> {
    // If a navigation is already pending, we are about to abandon the current document. Still drain
    // any additional requests from the executor so it doesn't retain stale state, but do not
    // override the first request.
    if self.pending_navigation.is_some() {
      while self.executor.take_navigation_request().is_some() {}
      return Ok(false);
    }

    // Drain in case the executor queues multiple requests before the host polls (unlikely, but keep
    // this helper robust).
    let mut req: Option<LocationNavigationRequest> = None;
    while let Some(next) = self.executor.take_navigation_request() {
      req = Some(next);
    }
    let Some(req) = req else {
      return Ok(false);
    };

    // `beforeunload` can cancel navigations; only proceed if not canceled.
    let should_navigate = {
      let (executor, document) = (&mut self.executor, &mut self.document);
      executor.dispatch_beforeunload_event(document.as_mut(), event_loop)?
    };

    if should_navigate {
      self.pending_navigation = Some(req);
      self.pending_navigation_deadline =
        crate::render_control::root_deadline().filter(|deadline| deadline.is_enabled());
    } else {
      // Ensure the host does not retain any pending navigation state after cancelation.
      self.pending_navigation = None;
      self.pending_navigation_deadline = None;
    }

    Ok(true)
  }

  fn executor_microtask_checkpoint_hook(
    host: &mut BrowserTabHost,
    event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    {
      let (executor, document) = (&mut host.executor, &mut host.document);
      executor.after_microtask_checkpoint(document.as_mut(), event_loop)?;
    }
    let _ = host.poll_navigation_request(event_loop)?;
    Ok(())
  }

  fn perform_microtask_checkpoint_and_notify_executor(
    &mut self,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<()> {
    let checkpoint_result =
      self.with_installed_document_write_state(|host| event_loop.perform_microtask_checkpoint(host));
    // If the executor was already notified via the event loop's multiplexed microtask checkpoint
    // hooks, avoid double notifications.
    let executor_hook: fn(&mut BrowserTabHost, &mut EventLoop<BrowserTabHost>) -> Result<()> =
      BrowserTabHost::executor_microtask_checkpoint_hook;
    if event_loop
      .microtask_checkpoint_hooks()
      .iter()
      .any(|&hook| std::ptr::fn_addr_eq(hook, executor_hook))
    {
      // Navigation requests triggered from microtasks terminate the currently running microtask with
      // a VM termination error. `BrowserTabHost` polls navigation requests from the executor in the
      // checkpoint hooks, so treat such termination errors as non-fatal: the caller will observe
      // `pending_navigation` and commit/abort parsing accordingly.
      if checkpoint_result.is_err() && self.pending_navigation.is_some() {
        return Ok(());
      }
      checkpoint_result?;
      return Ok(());
    }

    // Fall back to the legacy behavior for embeddings/tests that directly call this helper without
    // having installed the executor hook.
    let hook_result = BrowserTabHost::executor_microtask_checkpoint_hook(self, event_loop);
    if let Err(err) = hook_result {
      return Err(err);
    }

    if checkpoint_result.is_err() && self.pending_navigation.is_some() {
      return Ok(());
    }
    checkpoint_result?;
    Ok(())
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

  fn clear_active_data_transfers(&mut self) {
    if self.active_data_transfers.is_empty() {
      return;
    }
    let roots: Vec<vm_js::RootId> = self
      .active_data_transfers
      .drain()
      .map(|(_, handle)| handle.root_id)
      .collect();
    // Best-effort: if the executor has no WindowRealm, we cannot remove the persistent roots.
    // In that case, the heap is either not present or will be torn down by the executor.
    if let Some(realm) = self.executor.window_realm_mut() {
      for root in roots {
        realm.heap_mut().remove_root(root);
      }
    }
  }

  fn dispatch_dom_event(&mut self, target: EventTargetId, mut event: Event) -> Result<bool> {
    let dom: &crate::dom2::Document = self.document.dom();
    let event_invoker = &mut self.event_invoker;
    if let Some(invoker) = event_invoker
      .as_any_mut()
      .and_then(|any| any.downcast_mut::<crate::js::window_realm::WindowRealmDomEventListenerInvoker<Self>>())
    {
      let mut event_for_event_obj = Event::new(
        event.type_.clone(),
        EventInit {
          bubbles: event.bubbles,
          cancelable: event.cancelable,
          composed: event.composed,
        },
      );
      // `WindowRealmDomEventListenerInvoker` synthesizes a single JS `Event` object per dispatch and
      // reuses it across listeners. That object captures immutable event fields at allocation time
      // (e.g. `isTrusted` and `MouseEvent` properties like `clientX` and modifier keys).
      //
      // Mirror those immutable fields onto the snapshot passed into `with_dispatch_event_object`,
      // even though `web_events::dispatch_event` itself operates on the mutable `event` instance
      // below.
      event_for_event_obj.time_stamp = event.time_stamp;
      event_for_event_obj.is_trusted = event.is_trusted;
      event_for_event_obj.detail = event.detail.clone();
      event_for_event_obj.storage = event.storage.clone();
      event_for_event_obj.mouse = event.mouse;
      event_for_event_obj.drag_data_transfer = event.drag_data_transfer;
      return invoker
        .with_dispatch_event_object(&event_for_event_obj, |invoker| {
          crate::web::events::dispatch_event(target, &mut event, dom, dom.events(), invoker)
        })
        .map_err(|err| Error::Other(err.to_string()));
    }

    crate::web::events::dispatch_event(target, &mut event, dom, dom.events(), event_invoker.as_mut())
      .map_err(|err| Error::Other(err.to_string()))
  }

  fn dispatch_dom_event_in_event_loop(
    &mut self,
    target: EventTargetId,
    mut event: Event,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<bool> {
    let target = target.normalize();
    let dom: &crate::dom2::Document = self.document.dom();
    let event_invoker = &mut self.event_invoker;
    let dispatch_result: Result<bool> = if let Some(invoker) = event_invoker.as_any_mut().and_then(|any| {
      any.downcast_mut::<crate::js::window_realm::WindowRealmDomEventListenerInvoker<Self>>()
    }) {
      invoker.with_event_loop(event_loop, |invoker| {
        let mut event_for_event_obj = Event::new(
          event.type_.clone(),
          EventInit {
            bubbles: event.bubbles,
            cancelable: event.cancelable,
            composed: event.composed,
          },
        );
        // Mirror immutable, JS-observable fields onto the snapshot used to allocate the JS
        // `Event`/`MouseEvent` object for this dispatch.
        event_for_event_obj.time_stamp = event.time_stamp;
        event_for_event_obj.is_trusted = event.is_trusted;
        event_for_event_obj.detail = event.detail.clone();
        event_for_event_obj.storage = event.storage.clone();
        event_for_event_obj.mouse = event.mouse;
        event_for_event_obj.drag_data_transfer = event.drag_data_transfer;
        invoker.with_dispatch_event_object(&event_for_event_obj, |invoker| {
          crate::web::events::dispatch_event(target, &mut event, dom, dom.events(), invoker)
        })
          .map_err(|err| Error::Other(err.to_string()))
      })
    } else {
      crate::web::events::dispatch_event(
        target,
        &mut event,
        dom,
        dom.events(),
        event_invoker.as_mut(),
      )
      .map_err(|err| Error::Other(err.to_string()))
    };

    let default_not_prevented = !event.default_prevented;
    let navigation_request_seen = self.poll_navigation_request(event_loop)?;

    match dispatch_result {
      Ok(default_not_prevented_from_dispatch) => Ok(default_not_prevented_from_dispatch),
      Err(err) => {
        // Navigation requests interrupt the vm-js VM with a termination error so the host can commit
        // navigation synchronously. If a request was produced, treat this termination as non-fatal
        // and let the embedding commit (or cancel) navigation.
        if navigation_request_seen || self.pending_navigation.is_some() {
          return Ok(default_not_prevented);
        }
        Err(err)
      }
    }
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
    let _default_not_prevented =
      self.dispatch_dom_event(EventTargetId::Node(script_node_id), event)?;
    Ok(())
  }

  fn dispatch_script_event_in_event_loop(
    &mut self,
    script_node_id: NodeId,
    type_: &str,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<()> {
    let mut event = Event::new(
      type_,
      EventInit {
        bubbles: false,
        cancelable: false,
        composed: false,
      },
    );
    event.is_trusted = true;
    let _default_not_prevented = self.dispatch_dom_event_in_event_loop(
      EventTargetId::Node(script_node_id),
      event,
      event_loop,
    )?;
    Ok(())
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

  /// Mutate the live form-control state for a text-like control.
  ///
  /// This updates the DOM's **internal** value state (e.g. `HTMLInputElement.value`) rather than
  /// only the serialized content attributes.
  ///
  /// This helper is intended for trusted UI integrations (e.g. accessibility action routing for the
  /// renderer-chrome browser UI). Callers are expected to dispatch appropriate DOM events (`input`,
  /// `change`) after the mutation so JS observers see the new value.
  ///
  /// Currently supported:
  /// - `<input>` (HTML namespace): updates via `dom2::Document::set_input_value`.
  /// - `<textarea>` (HTML namespace): updates via `dom2::Document::set_textarea_value`.
  ///
  /// `<select>` is not supported yet; callers should implement selection changes via
  /// `dom2::Document::set_option_selected`.
  fn set_text_control_value(&mut self, node_id: NodeId, value: &str) -> Result<bool> {
    let value = value.to_string();
    self.mutate_dom(|dom| {
      if node_id.index() >= dom.nodes_len() {
        return (
          Err(Error::Other(format!(
            "set_text_control_value: invalid node id {node_id:?}"
          ))),
          false,
        );
      }

      let kind = &dom.node(node_id).kind;
      let NodeKind::Element {
        tag_name,
        namespace,
        ..
      } = kind
      else {
        return (
          Err(Error::Other(format!(
            "set_text_control_value: node {node_id:?} is not an element"
          ))),
          false,
        );
      };

      // Only support HTML form controls for now. This matches the dom2 form-control state model.
      if !dom.is_html_case_insensitive_namespace(namespace) {
        return (
          Err(Error::Other(format!(
            "set_text_control_value: unsupported namespace {namespace:?} for element <{tag_name}>"
          ))),
          false,
        );
      }

      if tag_name.eq_ignore_ascii_case("input") {
        match dom.set_input_value(node_id, &value) {
          Ok(changed) => return (Ok(changed), changed),
          Err(err) => {
            return (
              Err(Error::Other(format!(
                "set_text_control_value: failed to set <input> value: {err:?}"
              ))),
              false,
            );
          }
        }
      }

      if tag_name.eq_ignore_ascii_case("textarea") {
        match dom.set_textarea_value(node_id, &value) {
          Ok(changed) => return (Ok(changed), changed),
          Err(err) => {
            return (
              Err(Error::Other(format!(
                "set_text_control_value: failed to set <textarea> value: {err:?}"
              ))),
              false,
            );
          }
        }
      }

      if tag_name.eq_ignore_ascii_case("select") {
        return (
          Err(Error::Other(
            "set_text_control_value: <select> SetValue not supported yet".to_string(),
          )),
          false,
        );
      }

      (
        Err(Error::Other(format!(
          "set_text_control_value: unsupported element <{tag_name}>"
        ))),
        false,
      )
    })
  }

  pub(crate) fn document_write_state_mut(&mut self) -> &mut DocumentWriteState {
    &mut self.document_write_state
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
    match self.script_execution_log_capacity {
      Some(capacity) => {
        self.script_execution_log = Some(crate::js::ScriptExecutionLog::new(capacity));
      }
      None => {
        self.script_execution_log = None;
      }
    }
    self.orchestrator = ScriptOrchestrator::new();
    self.scheduler = {
      let mut scheduler = HtmlScriptScheduler::new();
      scheduler.set_options(self.js_execution_options);
      scheduler
    };
    self.scripts.clear();
    self.scheduled_script_nodes.clear();
    self.deferred_scripts.clear();
    self.executed.clear();
    self.pending_module_executions.clear();
    self.pending_script_load_blockers.clear();
    self.pending_asap_script_executions.clear();
    self.pending_image_load_blockers.clear();
    self.image_load_state.clear();
    self.next_image_load_request_id = 1;
    self.in_flight_ordered_asap_module = None;
    self.queued_ordered_asap_scripts.clear();
    self.in_flight_post_parse_module = None;
    self.queued_post_parse_scripts.clear();
    self.queued_stylesheet_blocked_script_tasks.clear();
    self.parser_blocked_on = None;
    self.document_url = document_url.clone();
    self.base_url = document_url;
    self.document_origin = self
      .document_url
      .as_deref()
      .and_then(|url| origin_from_url(url));
    self.document_referrer_policy = document_referrer_policy;
    self.executor.on_document_referrer_policy_updated(document_referrer_policy);
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
    // Keep the renderer-level CSP mirror in sync so JS module loaders (which may only see the
    // `BrowserDocumentDom2` fetcher + metadata) can enforce CSP for module dependency loads.
    self.document.renderer_mut().document_csp = self.csp.clone();
    self.js_events = JsDomEvents::new()?;
    self.js_execution_depth.set(0);
    self.lifecycle = DocumentLifecycle::new();
    self.pending_dynamic_script_candidates.clear();
    self.last_dynamic_script_discovery_generation = 0;
    #[cfg(test)]
    {
      self.dynamic_script_full_scan_count = 0;
    }
    self.last_image_load_discovery_generation = None;
    self.document_write_state.reset_for_navigation();
    self
      .document_write_state
      .update_limits(self.js_execution_options);
    self.script_blocking_stylesheets = ScriptBlockingStyleSheetSet::new();
    self.stylesheet_keys_by_node.clear();
    self.next_stylesheet_key = 0;
    self.stylesheet_media_query_cache = MediaQueryCache::default();
    self.streaming_parse_active = false;
    self.streaming_parse = None;
    self.pending_parser_blocking_script = None;
    // DataTransfer objects are rooted in the current realm's heap so they can be reused across
    // multiple drag event dispatches. Navigations recreate the JS realm, so clear any outstanding
    // roots before the executor tears down the realm.
    self.clear_active_data_transfers();
    self.executor.reset_for_navigation(
      self.document_url.as_deref(),
      &mut self.document,
      &self.current_script,
      self.js_execution_options,
    )?;
    if let Some(realm) = self.executor.window_realm_mut() {
      self
        .webidl_bindings_host
        .reset_for_new_realm(realm.global_object());
    }
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
      _ => super::headless_chrome_screen_media_context(width, height),
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
      .find(|attr| attr.namespace == crate::dom2::NULL_NAMESPACE && attr.local_name.eq_ignore_ascii_case("media"))
      .map(|attr| attr.value.as_str())
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
    let final_url = resource
      .final_url
      .clone()
      .unwrap_or_else(|| url.to_string());

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

  fn should_delay_parser_blocking_script(&self, script_id: HtmlScriptId) -> bool {
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

  fn should_delay_post_parse_script_for_stylesheets(&self, script_id: HtmlScriptId) -> bool {
    // HTML "stop parsing" executes scripts from the "list of scripts that will execute when the
    // document has finished parsing" only once the document has no style sheet blocking scripts.
    //
    // `BrowserTabHost` models this list as `deferred_scripts` (classic `defer` scripts and
    // parser-inserted module scripts without `async`).
    self.deferred_scripts.contains(&script_id)
  }

  fn flush_stylesheet_blocked_script_tasks(
    &mut self,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<()> {
    if self.script_blocking_stylesheets.has_blocking_stylesheet() {
      return Ok(());
    }

    while let Some(action) = self.queued_stylesheet_blocked_script_tasks.pop_front() {
      if self.pending_navigation.is_some() {
        break;
      }
      self.apply_scheduler_actions(vec![action], event_loop)?;
    }
    Ok(())
  }

  fn has_pending_asap_script_execution(&self) -> bool {
    self.streaming_parse_active && !self.pending_asap_script_executions.is_empty()
  }

  fn script_should_preempt_streaming_parse(&self, script_id: HtmlScriptId) -> bool {
    let Some(entry) = self.scripts.get(&script_id) else {
      return false;
    };
    let spec = &entry.spec;
    match spec.script_type {
      ScriptType::Classic => {
        // Inline classic scripts execute synchronously during parsing and do not participate in
        // ASAP task scheduling.
        if !spec.src_attr_present {
          return false;
        }
        if spec.src.as_deref().is_none_or(|src| src.is_empty()) {
          return false;
        }
        let is_async = spec.async_attr || spec.force_async;
        if spec.parser_inserted {
          // Parser-inserted classic scripts are parser-blocking unless `async`-like, and `defer`
          // scripts run only after parsing completes.
          is_async
        } else {
          // Dynamically inserted external classic scripts are either async or ordered-asap; both can
          // execute before parsing completes.
          true
        }
      }
      ScriptType::Module => {
        let is_async = spec.async_attr || spec.force_async;
        if is_async {
          return true;
        }
        if spec.parser_inserted {
          // Parser-inserted module scripts are deferred-by-default when `async` is absent.
          return false;
        }
        // Non-parser-inserted module scripts without `async` execute in insertion order as soon as
        // possible.
        true
      }
      ScriptType::ImportMap | ScriptType::Unknown => false,
    }
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
      if host.has_pending_asap_script_execution()
        && host.pending_parser_blocking_script.is_none()
        && host.parser_blocked_on.is_none()
      {
        if let Some(state) = host.streaming_parse.as_mut() {
          state.parse_task_scheduled = false;
        }
        host.queue_parse_resume_task(event_loop)?;
        return Ok(());
      }
      let task_result = host.parse_until_blocked(event_loop);
      if let Some(state) = host.streaming_parse.as_mut() {
        state.parse_task_scheduled = false;
      }
      let outcome = task_result?;
      if outcome.should_continue() {
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

    if self.script_blocking_stylesheets.len()
      >= self.js_execution_options.max_pending_blocking_stylesheets
    {
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
      let removed = host
        .script_blocking_stylesheets
        .unregister_blocking_stylesheet(key);
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
          while host.parse_until_blocked(event_loop)?.should_continue() {}
        }
        host.flush_stylesheet_blocked_script_tasks(event_loop)?;
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
      self
        .script_blocking_stylesheets
        .unregister_blocking_stylesheet(key);
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

  fn parse_until_blocked(
    &mut self,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<ParseUntilBlockedResult> {
    const INPUT_CHUNK_BYTES: usize = 8 * 1024;

    if self.streaming_parse_in_progress {
      // Re-entrancy guard: when parsing is already active, callers should rely on the outer parse
      // loop to continue. This can happen if a parse-resume hook tries to parse synchronously while
      // we are already parsing on the stack.
      return Ok(ParseUntilBlockedResult::Done);
    }

    let Some(mut state) = self.streaming_parse.take() else {
      return Ok(ParseUntilBlockedResult::Done);
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
    let _parse_guard =
      StreamingParseInProgressGuard(&mut self.streaming_parse_in_progress as *mut bool);

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

    // HTML: async/ordered-asap scripts can become ready while parsing is paused between tasks (e.g.
    // a networking task completes and queues script execution). When such a script is ready to
    // execute, parsing must not continue until the script's execution task has run ("still blocks at
    // its execution point"). Yield back to the event loop so the pending script task can run first.
    if self.pending_parser_blocking_script.is_none()
      && self.parser_blocked_on.is_none()
      && self.has_pending_asap_script_execution()
    {
      self.streaming_parse = Some(state);
      return Ok(ParseUntilBlockedResult::Continue(
        ParseUntilBlockedContinueReason::PendingAsapScriptExecution,
      ));
    }

    enum Outcome {
      Blocked,
      Finished,
      AbortedForNavigation,
      BudgetExhausted,
      YieldForAsapScriptExecution,
    }

    let outcome = (|| -> Result<Outcome> {
      let dom_parse_budget = self.js_execution_options.dom_parse_budget;
      let mut remaining = dom_parse_budget.max_pump_iterations;
      let input_bytes_budget_enabled = dom_parse_budget.max_input_bytes_per_task.is_some();
      let mut remaining_input_bytes = dom_parse_budget
        .max_input_bytes_per_task
        .map(|v| v.max(1))
        .unwrap_or(usize::MAX);
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
              self
                .execute_script(script_id, &source_text, event_loop)
                .map(|_| ())
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
                .perform_microtask_checkpoint_and_notify_executor(event_loop)
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
              if input_bytes_budget_enabled && remaining_input_bytes == 0 {
                // Input-byte budget exhausted: yield back to the event loop so other queued tasks
                // (notably async script fetch/execution) can run before we consume more input.
                return Ok(Outcome::BudgetExhausted);
              }

              let mut end = (state.input_offset
                + INPUT_CHUNK_BYTES.min(remaining_input_bytes))
              .min(state.input.len());
              while end < state.input.len() && !state.input.is_char_boundary(end) {
                end += 1;
              }
              debug_assert!(state.input.is_char_boundary(state.input_offset));
              debug_assert!(state.input.is_char_boundary(end));
              let slice = &state.input[state.input_offset..end];
              state.parser.push_str(slice);
              if input_bytes_budget_enabled {
                remaining_input_bytes = remaining_input_bytes.saturating_sub(slice.len());
              }
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
                self.perform_microtask_checkpoint_and_notify_executor(event_loop)
              })?;
            }
            if self.pending_navigation.is_some() {
              return Ok(Outcome::AbortedForNavigation);
            }

            if !self.dom().is_connected_for_scripting(script) {
              // The streaming parser yielded a `</script>` boundary, but the element is no longer
              // connected for scripting (e.g. it lived inside an inert `<template>`, or it was
              // removed by a microtask checkpoint above).
              //
              // In WHATWG HTML, the parser would still run "prepare the script element" here, which
              // clears the element's "parser document" slot and may set the "force async" flag even
              // though the algorithm then returns early without executing:
              // https://html.spec.whatwg.org/multipage/scripting.html#prepare-a-script
              //
              // We won't call `prepare_script_element_dom2` for disconnected scripts, so mirror those
              // internal-slot side effects explicitly to ensure later DOM mutations/reinsertion treat
              // the element as a dynamic (async) script rather than still parser-inserted.
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
                    dom
                      .set_script_parser_document(script, false)
                      .expect("set_script_parser_document should succeed for <script>"); // fastrender-allow-unwrap
                    if parser_document && !dom.has_attribute(script, "async").unwrap_or(false) {
                      dom
                        .set_script_force_async(script, true)
                        .expect("set_script_force_async should succeed for <script>"); // fastrender-allow-unwrap
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

            // WHATWG HTML "prepare the script element" can return early without executing the script
            // (unsupported `type`, empty inline script, etc), *after* clearing the "parser document"
            // internal slot and (when appropriate) setting the "force async" flag:
            // - https://html.spec.whatwg.org/multipage/scripting.html#prepare-a-script
            // - https://html.spec.whatwg.org/multipage/scripting.html#script-processing-empty
            //
            // This is subtle but important: we must apply those internal-slot updates even for
            // non-executing scripts so later DOM mutation can re-trigger script preparation, and so
            // the element behaves like a dynamic/async script rather than incorrectly remaining
            // parser-blocking.
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
            // background network concurrency, so we yield back to the event loop so the async
            // fetch + execution tasks can run before we resume parsing.
            //
            // Avoid yielding for scripts whose sources are not immediately available. Many tests
            // register in-memory script sources *after* construction (before running the event
            // loop), so yielding here would not help interleaving and would instead stall parsing
            // on a script fetch that is guaranteed to fail.
            let should_yield_for_async = {
              let eligible_script_type = match spec.script_type {
                // Classic async scripts can execute as soon as they load, so yield to the event loop
                // so the fetch/execution tasks can run before parsing continues.
                ScriptType::Classic => true,
                // Module scripts behave similarly: async module scripts execute ASAP once their
                // module graph is ready. Only yield when module scripts are enabled.
                ScriptType::Module => self.js_execution_options.supports_module_scripts,
                _ => false,
              };
              eligible_script_type
                && spec.src_attr_present
                && spec.async_attr
                && spec
                  .src
                  .as_deref()
                  .filter(|src| !src.is_empty())
                  .is_some_and(|src| {
                    // Avoid eager network fetches during parsing when the script source is not
                    // immediately available. Many tests register in-memory script sources *after*
                    // construction (before running the event loop), so only yield for "fast" sources.
                    self
                      .external_script_sources
                      .lock()
                      .unwrap_or_else(|poisoned| poisoned.into_inner())
                      .contains_key(src)
                      || Url::parse(src)
                        .ok()
                        .is_some_and(|parsed| parsed.scheme() == "file")
                  })
            };
            let base_url_at_discovery = spec.base_url.clone();

            let script_id = with_active_streaming_parser(&state.parser, || {
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
            if should_yield_for_async {
              // Yield so the script's fetch/execution tasks can run before we parse further HTML (and
              // potentially discover later parser-inserted scripts).
              return Ok(Outcome::BudgetExhausted);
            }
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
        Ok(ParseUntilBlockedResult::Done)
      }
      Ok(Outcome::BudgetExhausted) => {
        // Budget exhausted: snapshot parser DOM into the host so other tasks observe the most recent
        // parsed DOM state, then yield back to the event loop.
        self.commit_streaming_parser_dom_snapshot_to_host(&mut state)?;
        self.streaming_parse = Some(state);
        Ok(ParseUntilBlockedResult::Continue(
          ParseUntilBlockedContinueReason::BudgetExhausted,
        ))
      }
      Ok(Outcome::YieldForAsapScriptExecution) => {
        self.streaming_parse = Some(state);
        Ok(ParseUntilBlockedResult::Continue(
          ParseUntilBlockedContinueReason::PendingAsapScriptExecution,
        ))
      }
      Ok(Outcome::Finished | Outcome::AbortedForNavigation) => {
        self.streaming_parse_active = false;
        self.document_write_state.set_parsing_active(false);
        Ok(ParseUntilBlockedResult::Done)
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
    script_id: HtmlScriptId,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<()> {
    let actions = self.scheduler.classic_fetch_failed(script_id)?;
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
          ..
        } => {
          if tag_name.eq_ignore_ascii_case("base") {
            let attrs_for_tracker: Vec<(String, String)> = attributes
              .iter()
              .map(|attr| (attr.qualified_name().into_owned(), attr.value.clone()))
              .collect();
            base_url_tracker.on_element_inserted(
              tag_name,
              namespace,
              attrs_for_tracker.as_slice(),
              in_head,
              in_foreign_namespace,
              in_template,
            );
          }

          if tag_name.eq_ignore_ascii_case("script") && is_html_namespace(namespace) {
            // Reuse the shared dom2 parse-time `<script>` normalization logic so best-effort DOM
            // scans observe the same attribute parsing rules as the streaming parser.
            let spec = crate::js::streaming_dom2::build_parser_inserted_script_element_spec_dom2(
              dom,
              id,
              &base_url_tracker,
            );
            out.push((id, spec));
            // Best-effort script discovery runs over a fully-built DOM (unlike the streaming parser
            // which discovers scripts incrementally). While the first discovered scripts execute,
            // internal DOM mutations (e.g. the "already started" flag) can trigger dynamic script
            // discovery. Pre-mark discovered nodes so dynamic discovery doesn't mistakenly schedule
            // later static `<script>` elements as "dynamic" and fetch them twice.
            self.scheduled_script_nodes.insert(id);
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
          ..
        } => {
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
        stack.push((
          child,
          next_in_head,
          next_in_foreign_namespace,
          next_in_template,
        ));
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
    if self.executor.supports_incremental_dynamic_script_discovery() {
      // vm-js DOM bindings already implement the dynamic-script insertion steps and forward
      // discovered scripts to `register_and_schedule_dynamic_script`. Avoid redundant O(N) scans of
      // the connected DOM after every JS-driven mutation.
      self.pending_dynamic_script_candidates.clear();
      // Dynamic script discovery is also used as the between-turn hook for image load discovery.
      self.discover_and_start_image_loads(event_loop)?;
      return Ok(());
    }

    self.discover_dynamic_scripts_full_scan(event_loop)
  }

  fn discover_dynamic_scripts_full_scan(&mut self, event_loop: &mut EventLoop<Self>) -> Result<()> {
    // Avoid O(N) scans on every hook call by gating discovery on the document's mutation counter.
    let generation = self.document.dom_mutation_generation();
    if generation == self.last_dynamic_script_discovery_generation {
      return Ok(());
    }
    self.last_dynamic_script_discovery_generation = generation;
    #[cfg(test)]
    {
      self.dynamic_script_full_scan_count =
        self.dynamic_script_full_scan_count.saturating_add(1);
    }

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
            ..
          } => {
            if tag_name.eq_ignore_ascii_case("base") {
              // `BaseUrlTracker` operates on renderer-style `(name, value)` attributes. Convert on
              // demand so `discover_scripts_best_effort` remains cheap for non-`<base>` elements.
              let attrs_for_tracker: Vec<(String, String)> = attributes
                .iter()
                .map(|attr| (attr.qualified_name().into_owned(), attr.value.clone()))
                .collect();
              base_url_tracker.on_element_inserted(
                tag_name,
                namespace,
                attrs_for_tracker.as_slice(),
                in_head,
                in_foreign_namespace,
                in_template,
              );
            }

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
              // HTML: dynamic scripts with no `src` attribute and empty inline text do nothing and
              // must remain eligible for later `src`/text mutations (do not mark started).
              if !spec.src_attr_present && spec.inline_text.is_empty() {
                // Still update base URL tracking and traversal; just skip scheduling.
              } else {
                discovered.push((id, spec));
              }
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
            ..
          } => {
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
          stack.push((
            child,
            next_in_head,
            next_in_foreign_namespace,
            next_in_template,
          ));
        }
      }

      discovered
    };

    for (node_id, spec) in discovered {
      let base_url_at_discovery = spec.base_url.clone();
      self.register_and_schedule_dynamic_script(
        node_id,
        spec,
        base_url_at_discovery,
        event_loop,
      )?;
    }

    // After DOMContentLoaded, images should behave like load blockers: trigger their fetches in the
    // background and delay `window.load` until they complete.
    self.discover_and_start_image_loads(event_loop)?;

    Ok(())
  }


  fn discover_and_start_image_loads(&mut self, event_loop: &mut EventLoop<Self>) -> Result<()> {
    // The `load` event waits for images, but `DOMContentLoaded` must not. To avoid making
    // DOMContentLoaded observers wait on synchronous fetch work (since `ResourceFetcher` is
    // currently synchronous), we only start image fetch tasks after DOMContentLoaded has fired.
    if !self.lifecycle.dom_content_loaded_fired() || self.lifecycle.load_fired() {
      return Ok(());
    }

    let generation = self.document.dom_mutation_generation();
    if self
      .last_image_load_discovery_generation
      .is_some_and(|last| last == generation)
    {
      return Ok(());
    }
    self.last_image_load_discovery_generation = Some(generation);

    fn is_html_namespace(namespace: &str) -> bool {
      namespace.is_empty() || namespace == HTML_NAMESPACE
    }

    let base_url = self.base_url.clone();
    let (discovered, clear_state): (
      Vec<(
        NodeId,
        String,
        Option<crate::resource::CorsMode>,
        ReferrerPolicy,
      )>,
      Vec<NodeId>,
    ) = {
      let dom = self.document.dom();
      let mut out = Vec::new();
      let mut clear_state = Vec::new();
      for node_id in dom.dom_connected_preorder() {
        let node = dom.node(node_id);
        let NodeKind::Element {
          tag_name,
          namespace,
          ..
        } = &node.kind
        else {
          continue;
        };
        if !is_html_namespace(namespace) {
          continue;
        }

        // Determine which element attribute represents the URL of an image-like subresource.
        //
        // Note: we always consider these element types so we can clear any existing per-node state
        // when attributes change (e.g. `type` mutation on `<input>` or `rel` mutation on `<link>`).
        let maybe_src: Option<&str> = if tag_name.eq_ignore_ascii_case("img") {
          dom.get_attribute(node_id, "src").ok().flatten()
        } else if tag_name.eq_ignore_ascii_case("input") {
          // `<input type=image src=...>` loads an image resource (used for form submission
          // buttons).
          let is_input_image = dom
            .get_attribute(node_id, "type")
            .ok()
            .flatten()
            .map(super::trim_ascii_whitespace)
            .is_some_and(|t| t.eq_ignore_ascii_case("image"));
          if !is_input_image {
            None
          } else {
            dom.get_attribute(node_id, "src").ok().flatten()
          }
        } else if tag_name.eq_ignore_ascii_case("link") {
          // `<link rel=icon href=...>` loads an image-like subresource.
          let is_icon = dom
            .get_attribute(node_id, "rel")
            .ok()
            .flatten()
            .map(super::trim_ascii_whitespace)
            .is_some_and(|rel| {
              rel
                .split_ascii_whitespace()
                .any(|t| t.eq_ignore_ascii_case("icon"))
            });
          if !is_icon {
            None
          } else {
            dom.get_attribute(node_id, "href").ok().flatten()
          }
        } else if tag_name.eq_ignore_ascii_case("video") {
          // `<video poster=...>` loads an image resource for the poster frame.
          dom.get_attribute(node_id, "poster").ok().flatten()
        } else {
          continue;
        };

        let maybe_url = maybe_src
          .map(super::trim_ascii_whitespace)
          .filter(|src| !src.is_empty())
          .and_then(|src| resolve_href_with_base(base_url.as_deref(), src))
          .filter(|url| !crate::resource::is_data_url(url));

        let Some(url) = maybe_url else {
          if self.image_load_state.contains_key(&node_id) {
            clear_state.push(node_id);
          }
          continue;
        };

        // Respect document CSP image restrictions, if present. When a policy blocks an image-like
        // subresource, treat it as if the load immediately errored (i.e. do not start a fetch and
        // do not register a load blocker).
        if let Some(csp) = self.csp.as_ref() {
          let document_origin = self.document_origin.as_ref();
          match Url::parse(&url) {
            Ok(parsed) => {
              if !csp.allows_url(
                crate::html::content_security_policy::CspDirective::ImgSrc,
                document_origin,
                &parsed,
              ) {
                if self.image_load_state.contains_key(&node_id) {
                  clear_state.push(node_id);
                }
                continue;
              }
            }
            Err(_) => {
              // Be conservative: if we can't parse the URL for CSP matching, treat it as blocked
              // when a CSP policy is present.
              if self.image_load_state.contains_key(&node_id) {
                clear_state.push(node_id);
              }
              continue;
            }
          }
        }

        if self
          .image_load_state
          .get(&node_id)
          .is_some_and(|state| state.url == url)
        {
          continue;
        }

        let cors_mode = dom
          .get_attribute(node_id, "crossorigin")
          .ok()
          .flatten()
          .map(|value| {
            let value = super::trim_ascii_whitespace(value);
            if value.eq_ignore_ascii_case("use-credentials") {
              crate::resource::CorsMode::UseCredentials
            } else {
              // Empty, `anonymous`, and unknown tokens are treated as `anonymous`.
              crate::resource::CorsMode::Anonymous
            }
          });

        let effective_referrer_policy = dom
          .get_attribute(node_id, "referrerpolicy")
          .ok()
          .flatten()
          .and_then(ReferrerPolicy::from_attribute)
          .unwrap_or(self.document_referrer_policy);

        out.push((node_id, url, cors_mode, effective_referrer_policy));
      }
      (out, clear_state)
    };

    for node_id in clear_state {
      self.image_load_state.remove(&node_id);
    }

    // Avoid retaining per-node state for images that are no longer connected to the document.
    //
    // `dom2` node ids are stable indices, so pages that create/remove many images could otherwise
    // accumulate unbounded bookkeeping. Removing state also ensures queued loads for disconnected
    // images become deterministic no-ops (they will still clear their registered load blocker).
    {
      let dom = self.document.dom();
      fn is_html_namespace(namespace: &str) -> bool {
        namespace.is_empty() || namespace == HTML_NAMESPACE
      }
      let mut to_remove = Vec::new();
      for (&node_id, _state) in &self.image_load_state {
        if !dom.is_connected_for_scripting(node_id) {
          to_remove.push(node_id);
          continue;
        }
        match &dom.node(node_id).kind {
          NodeKind::Element {
            tag_name,
            namespace,
            ..
          } if is_html_namespace(namespace) => {
            if tag_name.eq_ignore_ascii_case("img") {
              // keep
            } else if tag_name.eq_ignore_ascii_case("input") {
              let is_input_image = dom
                .get_attribute(node_id, "type")
                .ok()
                .flatten()
                .map(super::trim_ascii_whitespace)
                .is_some_and(|t| t.eq_ignore_ascii_case("image"));
              if !is_input_image {
                to_remove.push(node_id);
              }
            } else if tag_name.eq_ignore_ascii_case("link") {
              let is_icon = dom
                .get_attribute(node_id, "rel")
                .ok()
                .flatten()
                .map(super::trim_ascii_whitespace)
                .is_some_and(|rel| {
                  rel
                    .split_ascii_whitespace()
                    .any(|t| t.eq_ignore_ascii_case("icon"))
                });
              if !is_icon {
                to_remove.push(node_id);
              }
            } else if tag_name.eq_ignore_ascii_case("video") {
              // keep
            } else {
              to_remove.push(node_id);
            }
          }
          _ => {
            to_remove.push(node_id);
          }
        }
      }
      for node_id in to_remove {
        self.image_load_state.remove(&node_id);
      }
    }

    for (node_id, url, cors_mode, referrer_policy) in discovered {
      self.start_image_load(node_id, url, cors_mode, referrer_policy, event_loop)?;
    }

    Ok(())
  }

  fn start_image_load(
    &mut self,
    node_id: NodeId,
    url: String,
    cors_mode: Option<crate::resource::CorsMode>,
    referrer_policy: ReferrerPolicy,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<()> {
    if self
      .image_load_state
      .get(&node_id)
      .is_some_and(|state| state.url == url)
    {
      return Ok(());
    }

    let destination = if cors_mode.is_some() {
      FetchDestination::ImageCors
    } else {
      FetchDestination::Image
    };

    // Track as a load blocker before the fetch runs so `load` cannot be queued prematurely.
    let request_id = {
      let id = self.next_image_load_request_id;
      self.next_image_load_request_id = self.next_image_load_request_id.wrapping_add(1);
      if self.next_image_load_request_id == 0 {
        self.next_image_load_request_id = 1;
      }
      id
    };
    let prev_state = self.image_load_state.insert(
      node_id,
      ImageLoadState {
        url: url.clone(),
        request_id,
      },
    );
    let pending_key = (node_id, request_id);
    self.pending_image_load_blockers.insert(pending_key);
    self
      .lifecycle
      .register_pending_load_blocker(LoadBlockerKind::Other);

    let queued = event_loop.queue_task(TaskSource::Networking, move |host, event_loop| {
      let is_current = host
        .image_load_state
        .get(&node_id)
        .is_some_and(|state| state.request_id == request_id);

      let mut fetch_err: Option<Error> = None;
      if is_current {
        let fetcher = host.document.fetcher();

        let mut req = FetchRequest::new(&url, destination);
        if let Some(referrer) = host.document_url.as_deref() {
          req = req.with_referrer_url(referrer);
        }
        if let Some(origin) = host.document_origin.as_ref() {
          req = req.with_client_origin(origin);
        }
        req = req.with_referrer_policy(referrer_policy);
        if let Some(cors_mode) = cors_mode {
          req = req.with_credentials_mode(cors_mode.credentials_mode());
        }

        match fetcher.fetch_with_request(req) {
          Ok(_) => {}
          Err(err) => {
            // Treat ordinary network/image decode failures as non-fatal (they should not abort
            // `window.load`), but still propagate cooperative deadline/cancellation errors.
            if matches!(err, Error::Render(_)) {
              fetch_err = Some(err);
            }
          }
        }
      }

      if host.pending_image_load_blockers.remove(&pending_key) {
        host
          .lifecycle
          .load_blocker_completed(LoadBlockerKind::Other, event_loop)?;
      }

      fetch_err.map_or(Ok(()), Err)
    });

    if let Err(err) = queued {
      // Avoid wedging `load`: if we can't queue the fetch task, treat the image as completed (as if
      // it immediately errored) and unwind our bookkeeping.
      self.pending_image_load_blockers.remove(&pending_key);
      match prev_state {
        Some(prev) => {
          self.image_load_state.insert(node_id, prev);
        }
        None => {
          self.image_load_state.remove(&node_id);
        }
      }
      self
        .lifecycle
        .load_blocker_completed(LoadBlockerKind::Other, event_loop)?;
      // Queueing failures are treated as a non-fatal image load error: `window.load` must still be
      // able to fire and the document should keep running.
      //
      // Only cooperative render deadline/cancellation errors should abort the run; `queue_task`
      // reports allocation/queue-limit failures as `Error::Other`.
      if matches!(&err, Error::Render(_)) {
        return Err(err);
      }
      return Ok(());
    }

    Ok(())
  }

  fn register_and_schedule_script(
    &mut self,
    node_id: NodeId,
    mut spec: ScriptElementSpec,
    base_url_at_discovery: Option<String>,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<HtmlScriptId> {
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
      self.perform_microtask_checkpoint_and_notify_executor(event_loop)?;
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
      // `discover_scripts_best_effort` pre-marks all discovered `<script>` elements as scheduled to
      // prevent dynamic-script discovery from accidentally re-scheduling later static scripts while
      // earlier scripts execute.
      //
      // When "prepare a script" returns early (e.g. empty inline script or unknown type), HTML does
      // *not* mark the element as started. Instead, the element's internal slots are adjusted so
      // later mutations (setting `src`, changing `type`, appending text children) can make it
      // runnable again. Ensure such scripts remain eligible for dynamic discovery by removing the
      // pre-mark.
      self.scheduled_script_nodes.remove(&node_id);
      let discovered =
        self
          .scheduler
          .discovered_parser_script(spec, node_id, base_url_at_discovery)?;
      return Ok(discovered.id);
    }

    // HTML sets the per-element "already started" flag during preparation ("prepare a script"),
    // *before* fetch/execution occurs. This must only run for scripts that successfully pass the
    // early-out conditions above (e.g. non-empty inline scripts, valid types, etc), so that empty
    // scripts remain eligible for later `src`/text/type mutations.
    self
      .mutate_dom(|dom| (dom.set_script_already_started(node_id, true), false))
      .map_err(|err| Error::Other(err.to_string()))?;

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
    let discovered =
      self
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
          && spec_for_table
            .src
            .as_deref()
            .is_some_and(|src| !src.is_empty())
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

  pub(crate) fn register_and_schedule_dynamic_script(
    &mut self,
    node_id: NodeId,
    spec: ScriptElementSpec,
    base_url_at_discovery: Option<String>,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<HtmlScriptId> {
    if self.executor.supports_incremental_dynamic_script_discovery() {
      self.pending_dynamic_script_candidates.push_back(node_id);
    }
    let mut spec = spec;
    let spec_for_table = spec.clone();
    let integrity_invalid =
      spec_for_table.integrity_attr_present && spec_for_table.integrity.is_none();
    let failed_to_run = (!spec_for_table.src_attr_present && spec_for_table.inline_text.is_empty())
      || spec_for_table.script_type == ScriptType::Unknown
      || (integrity_invalid && !spec_for_table.src_attr_present);

    // When integrity metadata is invalid due to clamping, the script must not execute. Inline
    // scripts do not have a fetch pipeline to surface errors, so keep them eligible for later
    // mutation by forcing the scheduler to see an empty inline script (yielding no actions).
    if integrity_invalid && !spec_for_table.src_attr_present {
      spec.inline_text.clear();
    }

    if matches!(
      spec_for_table.script_type,
      ScriptType::Classic | ScriptType::Module | ScriptType::ImportMap
    ) && !spec_for_table.src_attr_present
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
    // Dynamic scripts are marked "already started" at preparation time (DOM insertion steps /
    // attribute mutation steps) so subsequent insertion attempts short-circuit.
    self
      .mutate_dom(|dom| (dom.set_script_already_started(node_id, true), false))
      .map_err(|err| Error::Other(err.to_string()))?;
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
    actions: Vec<HtmlScriptSchedulerAction<NodeId>>,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<()> {
    for action in actions {
      if self.pending_navigation.is_some() {
        break;
      }
      match action {
        HtmlScriptSchedulerAction::StartClassicFetch {
          script_id,
          url,
          destination,
          ..
        } => {
          if !self.pending_script_load_blockers.insert(script_id) {
            return Err(Error::Other(format!(
              "HtmlScriptScheduler requested StartClassicFetch more than once for script_id={}",
              script_id.as_u64()
            )));
          }
          self
            .lifecycle
            .register_pending_load_blocker(LoadBlockerKind::Script);
          self.start_fetch(script_id, url, destination, event_loop)?;
        }
        HtmlScriptSchedulerAction::StartModuleGraphFetch { script_id, .. }
        | HtmlScriptSchedulerAction::StartInlineModuleGraphFetch { script_id, .. } => {
          if !self.pending_script_load_blockers.insert(script_id) {
            return Err(Error::Other(format!(
              "HtmlScriptScheduler requested StartModuleGraphFetch more than once for script_id={}",
              script_id.as_u64()
            )));
          }
          self
            .lifecycle
            .register_pending_load_blocker(LoadBlockerKind::Script);
          self.start_module_graph_fetch(script_id, event_loop)?;
        }
        HtmlScriptSchedulerAction::BlockParserUntilExecuted { script_id, .. } => {
          if self.executed.contains(&script_id) {
            continue;
          }
          if self
            .parser_blocked_on
            .is_some_and(|existing| existing != script_id)
          {
            return Err(Error::Other(
              "HtmlScriptScheduler requested multiple simultaneous parser blocks".to_string(),
            ));
          }
          self.parser_blocked_on = Some(script_id);
        }
        HtmlScriptSchedulerAction::ExecuteNow {
          script_id,
          node_id,
          work,
          ..
        } => {
          let entry = self.scripts.get(&script_id).cloned();

          let should_checkpoint = matches!(
            &work,
            HtmlScriptWork::Classic { .. } | HtmlScriptWork::Module { .. }
          );

          let source_text = match work {
            HtmlScriptWork::Classic { source_text } => {
              let Some(source_text) = source_text else {
                self.dispatch_script_error_event_in_event_loop(node_id, event_loop)?;
                self
                  .mutate_dom(|dom| (dom.set_script_already_started(node_id, true), false))
                  .map_err(|err| Error::Other(err.to_string()))?;
                self.finish_script_execution(script_id, event_loop)?;
                continue;
              };
              source_text
            }
            HtmlScriptWork::Module { source_text } => {
              let Some(source_text) = source_text else {
                self.dispatch_script_error_event_in_event_loop(node_id, event_loop)?;
                self
                  .mutate_dom(|dom| (dom.set_script_already_started(node_id, true), false))
                  .map_err(|err| Error::Other(err.to_string()))?;
                self.finish_script_execution(script_id, event_loop)?;
                continue;
              };
              source_text
            }
            HtmlScriptWork::ImportMap { source_text, .. } => source_text,
          };

          if let Some(csp) = self.csp.as_ref() {
            let is_inline_script = entry.as_ref().is_some_and(|entry| {
              !entry.spec.src_attr_present
                && matches!(
                  entry.spec.script_type,
                  ScriptType::Classic | ScriptType::ImportMap
                )
            });
            if is_inline_script {
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
          //
          // For module scripts with top-level await, execution may still be pending after the call
          // returns. In that case we defer `finish_script_execution` until the executor notifies us
          // that evaluation has settled.
          if matches!(
            exec_result,
            Ok(ScriptExecutionCompletion::PendingModuleEvaluation)
          ) {
            self.pending_module_executions.insert(script_id);
          } else {
            self.finish_script_execution(script_id, event_loop)?;
          }

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
            return exec_result.map(|_| ());
          }
 
          let microtask_err = if should_checkpoint && self.js_execution_depth.get() == 0 {
            self
              .perform_microtask_checkpoint_and_notify_executor(event_loop)
              .err()
          } else {
            None
          };

          match exec_result {
            Ok(ScriptExecutionCompletion::Completed) => {
              if let Some(entry) = entry {
                if entry.spec.src_attr_present
                  && entry.spec.src.as_deref().is_some_and(|s| !s.is_empty())
                {
                  self.dispatch_script_event_in_event_loop(entry.node_id, "load", event_loop)?;
                }
              }
            }
            Ok(ScriptExecutionCompletion::PendingModuleEvaluation) => {
              // Module script evaluation is still pending (top-level await). Dispatch `load`/`error`
              // once the evaluation promise settles.
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
        HtmlScriptSchedulerAction::QueueTask {
          script_id,
          node_id,
          work,
          ..
        } => {
          if self.script_blocking_stylesheets.has_blocking_stylesheet()
            && self.should_delay_post_parse_script_for_stylesheets(script_id)
          {
            // HTML: scripts in the "list of scripts that will execute when the document has finished
            // parsing" (classic `defer` and parser-inserted non-async module scripts) wait until the
            // document has no style sheet blocking scripts.
            self
              .queued_stylesheet_blocked_script_tasks
              .push_back(HtmlScriptSchedulerAction::QueueTask {
                script_id,
                node_id,
                work,
              });
            continue;
          }

          let source_text = match work {
            HtmlScriptWork::Classic { source_text } => {
              let Some(source_text) = source_text else {
                self.dispatch_script_error_event_in_event_loop(node_id, event_loop)?;
                self
                  .mutate_dom(|dom| (dom.set_script_already_started(node_id, true), false))
                  .map_err(|err| Error::Other(err.to_string()))?;
                self.finish_script_execution(script_id, event_loop)?;
                continue;
              };
              source_text
            }
            HtmlScriptWork::Module { source_text } => {
              let Some(source_text) = source_text else {
                self.dispatch_script_error_event_in_event_loop(node_id, event_loop)?;
                self
                  .mutate_dom(|dom| (dom.set_script_already_started(node_id, true), false))
                  .map_err(|err| Error::Other(err.to_string()))?;
                self.finish_script_execution(script_id, event_loop)?;
                continue;
              };
              source_text
            }
            HtmlScriptWork::ImportMap { source_text, .. } => source_text,
          };

          if let Some(csp) = self.csp.as_ref() {
            let is_inline_script = self.scripts.get(&script_id).is_some_and(|entry| {
              !entry.spec.src_attr_present
                && matches!(
                  entry.spec.script_type,
                  ScriptType::Classic | ScriptType::ImportMap
                )
            });
            if is_inline_script {
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
              ScriptType::Module => TaskSource::Script,
              _ => TaskSource::Script,
            })
            .unwrap_or(TaskSource::Script);

          // HTML's ordered script execution lists (`ordered_asap` / `post_parse`) run scripts
          // sequentially:
          // - "list of scripts that will execute in order as soon as possible"
          //   (`specs/whatwg-html/source` `#script-processing-src-sync`)
          // - "list of scripts that will execute when the document has finished parsing"
          //   (`specs/whatwg-html/source` heading "The end")
          //
          // For module scripts with top-level await, `run a module script` returns a Promise that
          // can settle on a future turn. Until that promise settles, later scripts in the same
          // ordered list must not start (even if they are classic scripts), otherwise their
          // execution could interleave between the module's pre- and post-`await` portions.
          if let Some(kind) = self.ordered_script_queue_kind(script_id) {
            let script_type = self
              .scripts
              .get(&script_id)
              .map(|entry| entry.spec.script_type)
              .unwrap_or(ScriptType::Unknown);
            if self.ordered_script_queue_is_blocked(kind) {
              let work = match script_type {
                ScriptType::Classic => HtmlScriptWork::Classic {
                  source_text: Some(source_text),
                },
                ScriptType::Module => HtmlScriptWork::Module {
                  source_text: Some(source_text),
                },
                _ => {
                  return Err(Error::Other(format!(
                    "attempted to enqueue ordered script action for unsupported type: {script_type:?}"
                  )));
                }
              };
              self.enqueue_ordered_script_action(
                kind,
                HtmlScriptSchedulerAction::QueueTask {
                  script_id,
                  node_id,
                  work,
                },
              );
              continue;
            }
            if script_type == ScriptType::Module {
              self.mark_ordered_module_in_flight(kind, script_id);
            }
          }

          event_loop.queue_task(task_source, move |host, event_loop| {
            let entry = host.scripts.get(&script_id).cloned();
            let result = {
              let _guard = JsExecutionGuard::enter(&host.js_execution_depth);
              host.execute_script(script_id, &source_text, event_loop)
            };
            let is_pending_module_eval = matches!(
              result,
              Ok(ScriptExecutionCompletion::PendingModuleEvaluation)
            );
            if is_pending_module_eval {
              host.pending_module_executions.insert(script_id);
            } else {
              host.finish_script_execution(script_id, event_loop)?;
            }

            if matches!(&result, Err(Error::Render(_))) {
              // Preserve existing behavior: dispatch the script element error event, then abort
              // without attempting a microtask checkpoint.
              if let Some(entry) = entry {
                host.dispatch_script_event_in_event_loop(entry.node_id, "error", event_loop)?;
              }
              return result.map(|_| ());
            }
 
            let microtask_err = if host.js_execution_depth.get() == 0 {
              host
                .perform_microtask_checkpoint_and_notify_executor(event_loop)
                .err()
            } else {
              None
            };

            match result {
              Ok(ScriptExecutionCompletion::Completed) => {
                if let Some(entry) = entry {
                  if entry.spec.src_attr_present
                    && entry.spec.src.as_deref().is_some_and(|s| !s.is_empty())
                  {
                    host.dispatch_script_event_in_event_loop(entry.node_id, "load", event_loop)?;
                  }
                }
              }
              Ok(ScriptExecutionCompletion::PendingModuleEvaluation) => {
                // Module script evaluation is still pending (top-level await). Dispatch `load` once
                // the evaluation promise settles.
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

            if !is_pending_module_eval {
              host.finish_ordered_module_and_maybe_start_next(script_id, event_loop)?;
            }

            Ok(())
          })?;

          if self.streaming_parse_active && self.script_should_preempt_streaming_parse(script_id) {
            self.pending_asap_script_executions.insert(script_id);
          }
        }
        HtmlScriptSchedulerAction::QueueScriptEventTask { node_id, event, .. } => {
          let type_str = match event {
            ScriptEventKind::Load => "load",
            ScriptEventKind::Error => "error",
          };
          event_loop.queue_task(TaskSource::DOMManipulation, move |host, event_loop| {
            let mut ev = Event::new(type_str, EventInit::default());
            ev.is_trusted = true;
            host.dispatch_lifecycle_event(event_loop, EventTargetId::Node(node_id), ev)?;
            Ok(())
          })?;
        }
      }
    }
    Ok(())
  }

  fn ordered_script_queue_kind(&self, script_id: HtmlScriptId) -> Option<OrderedScriptQueueKind> {
    let entry = self.scripts.get(&script_id)?;
    match entry.spec.script_type {
      ScriptType::Module => {
        if entry.spec.async_attr || entry.spec.force_async {
          // Async module scripts execute as soon as possible once ready; they are not part of the
          // ordered script queues.
          return None;
        }
        if entry.spec.parser_inserted {
          Some(OrderedScriptQueueKind::PostParse)
        } else {
          Some(OrderedScriptQueueKind::OrderedAsap)
        }
      }
      ScriptType::Classic => {
        // Classic scripts participate in ordered execution via two distinct lists:
        // - Parser-inserted `defer` scripts (`post_parse`)
        // - DOM-inserted (`parser_inserted=false`) external scripts with `async=false`
        //   (`ordered_asap`)
        //
        // These correspond to:
        // - "The end" loop that executes the "list of scripts that will execute when the document
        //   has finished parsing" (spec `#the-end`)
        // - The "list of scripts that will execute in order as soon as possible" loop (spec
        //   `#script-processing-src-sync`)
        if entry.spec.parser_inserted {
          let is_post_parse = entry.spec.src_attr_present
            && entry
              .spec
              .src
              .as_deref()
              .is_some_and(|src| !src.is_empty())
            && entry.spec.defer_attr
            && !entry.spec.async_attr;
          is_post_parse.then_some(OrderedScriptQueueKind::PostParse)
        } else {
          let is_ordered_asap = entry.spec.src_attr_present
            && entry
              .spec
              .src
              .as_deref()
              .is_some_and(|src| !src.is_empty())
            && !entry.spec.async_attr
            && !entry.spec.force_async;
          is_ordered_asap.then_some(OrderedScriptQueueKind::OrderedAsap)
        }
      }
      ScriptType::ImportMap | ScriptType::Unknown => None,
    }
  }

  fn ordered_script_queue_is_blocked(&self, kind: OrderedScriptQueueKind) -> bool {
    match kind {
      OrderedScriptQueueKind::OrderedAsap => self.in_flight_ordered_asap_module.is_some(),
      OrderedScriptQueueKind::PostParse => self.in_flight_post_parse_module.is_some(),
    }
  }

  fn enqueue_ordered_script_action(
    &mut self,
    kind: OrderedScriptQueueKind,
    action: HtmlScriptSchedulerAction<NodeId>,
  ) {
    match kind {
      OrderedScriptQueueKind::OrderedAsap => self.queued_ordered_asap_scripts.push_back(action),
      OrderedScriptQueueKind::PostParse => self.queued_post_parse_scripts.push_back(action),
    }
  }

  fn mark_ordered_module_in_flight(
    &mut self,
    kind: OrderedScriptQueueKind,
    script_id: HtmlScriptId,
  ) {
    match kind {
      OrderedScriptQueueKind::OrderedAsap => self.in_flight_ordered_asap_module = Some(script_id),
      OrderedScriptQueueKind::PostParse => self.in_flight_post_parse_module = Some(script_id),
    }
  }

  fn resume_ordered_script_queue(
    &mut self,
    kind: OrderedScriptQueueKind,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<()> {
    while !self.ordered_script_queue_is_blocked(kind) {
      let next = match kind {
        OrderedScriptQueueKind::OrderedAsap => self.queued_ordered_asap_scripts.pop_front(),
        OrderedScriptQueueKind::PostParse => self.queued_post_parse_scripts.pop_front(),
      };
      let Some(action) = next else {
        break;
      };
      self.apply_scheduler_actions(vec![action], event_loop)?;
    }
    Ok(())
  }

  fn finish_ordered_module_and_maybe_start_next(
    &mut self,
    script_id: HtmlScriptId,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<()> {
    let Some(kind) = self.ordered_script_queue_kind(script_id) else {
      return Ok(());
    };
    let Some(entry) = self.scripts.get(&script_id) else {
      return Ok(());
    };
    if entry.spec.script_type != ScriptType::Module {
      return Ok(());
    }

    let in_flight_matches = match kind {
      OrderedScriptQueueKind::OrderedAsap => self.in_flight_ordered_asap_module == Some(script_id),
      OrderedScriptQueueKind::PostParse => self.in_flight_post_parse_module == Some(script_id),
    };
    if !in_flight_matches {
      return Ok(());
    }

    match kind {
      OrderedScriptQueueKind::OrderedAsap => self.in_flight_ordered_asap_module = None,
      OrderedScriptQueueKind::PostParse => self.in_flight_post_parse_module = None,
    }

    self.resume_ordered_script_queue(kind, event_loop)?;
    Ok(())
  }

  pub(crate) fn on_module_script_evaluation_complete(
    &mut self,
    script_id: HtmlScriptId,
    outcome: ModuleScriptEvaluationOutcome,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<()> {
    if !self.pending_module_executions.remove(&script_id) {
      // Either:
      // - the script was already finalized, or
      // - it was never marked pending (e.g. completed synchronously).
      //
      // In either case, be idempotent.
    }
    if self.executed.contains(&script_id) {
      return Ok(());
    }

    self.finish_script_execution(script_id, event_loop)?;

    if let Some(entry) = self.scripts.get(&script_id) {
      match outcome {
        ModuleScriptEvaluationOutcome::Fulfilled => {
          if entry.spec.src_attr_present
            && entry.spec.src.as_deref().is_some_and(|s| !s.is_empty())
          {
            self.dispatch_script_event_in_event_loop(entry.node_id, "load", event_loop)?;
          }
        }
        ModuleScriptEvaluationOutcome::Rejected => {
          self.dispatch_script_event_in_event_loop(entry.node_id, "error", event_loop)?;
        }
      }
    }

    self.finish_ordered_module_and_maybe_start_next(script_id, event_loop)?;
    Ok(())
  }

  fn finish_script_execution(
    &mut self,
    script_id: HtmlScriptId,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<()> {
    let newly_executed = self.executed.insert(script_id);
    self.pending_asap_script_executions.remove(&script_id);
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
    script_id: HtmlScriptId,
    source_text: &str,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<ScriptExecutionCompletion> {
    if self.executed.contains(&script_id) {
      return Ok(ScriptExecutionCompletion::Completed);
    }

    let Some(entry) = self.scripts.get(&script_id).cloned() else {
      return Err(Error::Other(format!(
        "HtmlScriptScheduler requested execution for unknown script_id={}",
        script_id.as_u64()
      )));
    };

    let node_id = entry.node_id;
    let script_type = entry.spec.script_type;

    struct Adapter<'a> {
      script_id: HtmlScriptId,
      source_text: &'a str,
      spec: &'a ScriptElementSpec,
      event_loop: &'a mut EventLoop<BrowserTabHost>,
      completion: ScriptExecutionCompletion,
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
        let result = {
          // Split the host borrow so we can install a JS-visible `DocumentWriteState` while still
          // calling into the executor.
          let BrowserTabHost {
            executor,
            document,
            document_write_state,
            ..
          } = host;
          crate::js::with_document_write_state(document_write_state, || match script_type {
            ScriptType::Classic => executor.execute_classic_script(
              self.source_text,
              self.spec,
              current_script,
              document.as_mut(),
              self.event_loop,
            ),
            ScriptType::Module => {
              let status = executor.execute_module_script(
                self.script_id,
                self.source_text,
                self.spec,
                current_script,
                document.as_mut(),
                self.event_loop,
              )?;
              self.completion = match status {
                ModuleScriptExecutionStatus::Completed => ScriptExecutionCompletion::Completed,
                ModuleScriptExecutionStatus::Pending => {
                  ScriptExecutionCompletion::PendingModuleEvaluation
                }
              };
              Ok(())
            }
            ScriptType::ImportMap => executor.execute_import_map_script(
              self.source_text,
              self.spec,
              current_script,
              document.as_mut(),
              self.event_loop,
            ),
            ScriptType::Unknown => Ok(()),
          })
        };
        let _ = host.poll_navigation_request(self.event_loop)?;
        result
      }
    }

    let mut adapter = Adapter {
      script_id,
      source_text,
      spec: &entry.spec,
      event_loop,
      completion: ScriptExecutionCompletion::Completed,
    };

    // Avoid double-borrowing `self` by temporarily moving the orchestrator out.
    let mut orchestrator = std::mem::take(&mut self.orchestrator);
    // Scripts are marked as "already started" during preparation (HTML "prepare a script"), not
    // during execution. Use the "prepared script" entrypoint so scripts that were prepared earlier
    // (and therefore already-started) still execute once their fetch completes.
    let result =
      orchestrator.execute_prepared_script_element(self, node_id, script_type, &mut adapter);
    self.orchestrator = orchestrator;
    result.map(|_| adapter.completion)
  }

  fn start_module_graph_fetch(
    &mut self,
    script_id: HtmlScriptId,
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

    if let Some(csp) = self.csp.as_ref() {
      fn trim_ascii_whitespace(value: &str) -> &str {
        value.trim_matches(|c: char| {
          matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' ')
        })
      }
      let nonce_attr = self
        .document
        .dom()
        .get_attribute(entry.node_id, "nonce")
        .ok()
        .flatten()
        .map(trim_ascii_whitespace)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string());
      let doc_origin = self
        .document_url
        .as_deref()
        .and_then(origin_from_url)
        .or_else(|| spec.base_url.as_deref().and_then(origin_from_url));

      let mut blocked: bool = false;
      let mut blocked_kind: &str = "";
      let mut blocked_url: Option<String> = None;
      let mut blocked_reason: Option<&str> = None;

      if spec.src_attr_present {
        if let Some(src) = spec.src.as_deref().filter(|s| !s.is_empty()) {
          match Url::parse(src) {
            Ok(url) => {
              if !csp.allows_script_url(doc_origin.as_ref(), nonce_attr.as_deref(), &url) {
                blocked = true;
                blocked_kind = "external";
                blocked_url = Some(src.to_string());
              }
            }
            Err(_) => {
              blocked = true;
              blocked_kind = "external";
              blocked_url = Some(src.to_string());
              blocked_reason = Some("invalid_url");
            }
          }
        }
      } else if !csp.allows_inline_script(nonce_attr.as_deref(), &spec.inline_text) {
        blocked = true;
        blocked_kind = "inline";
      }

      if blocked {
        let mut span = self.trace.span("js.script.csp_block", "js");
        span.arg_u64("node_id", entry.node_id.index() as u64);
        span.arg_str("kind", blocked_kind);
        if let Some(url) = blocked_url.as_deref() {
          span.arg_str("url", url);
        }
        if let Some(reason) = blocked_reason {
          span.arg_str("reason", reason);
        }
        if let Some(nonce) = nonce_attr.as_deref() {
          span.arg_str("nonce", nonce);
        }

        // Mark the script element as "already started" so later mutations/insertion do not attempt
        // to execute it again (matches browser behavior for CSP-blocked scripts).
        self
          .mutate_dom(|dom| (dom.set_script_already_started(entry.node_id, true), false))
          .map_err(|err| Error::Other(err.to_string()))?;

        let actions = self.scheduler.module_graph_failed(script_id)?;
        let needs_manual_error = actions.is_empty();
        self.apply_scheduler_actions(actions, event_loop)?;
        if needs_manual_error {
          self.dispatch_script_event_in_event_loop(entry.node_id, "error", event_loop)?;
        }
        self.finish_script_execution(script_id, event_loop)?;
        return Ok(());
      }
    }

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
            let actions = host.scheduler.module_graph_failed(script_id)?;
            let needs_manual_error = actions.is_empty();
            host.apply_scheduler_actions(actions, event_loop)?;
            if needs_manual_error {
              let node_id = host
                .scripts
                .get(&script_id)
                .map(|entry| entry.node_id)
                .ok_or_else(|| Error::Other("internal error: missing script entry".to_string()))?;
              host.dispatch_script_event_in_event_loop(node_id, "error", event_loop)?;
            }
            host.finish_script_execution(script_id, event_loop)?;
            return Ok(());
          };
          match host.fetch_script_source(script_id, url, FetchDestination::ScriptCors) {
            Ok(source_text) => source_text,
            Err(err) => {
              let actions = host.scheduler.module_graph_failed(script_id)?;
              let needs_manual_error = actions.is_empty();
              host.apply_scheduler_actions(actions, event_loop)?;
              if needs_manual_error {
                let node_id = host
                  .scripts
                  .get(&script_id)
                  .map(|entry| entry.node_id)
                  .ok_or_else(|| {
                    Error::Other("internal error: missing script entry".to_string())
                  })?;
                host.dispatch_script_event_in_event_loop(node_id, "error", event_loop)?;
              }
              host.finish_script_execution(script_id, event_loop)?;
              if matches!(err, Error::Render(_)) {
                return Err(err);
              }
              return Ok(());
            }
          }
        } else {
          std::mem::take(&mut spec.inline_text)
        };

        let actions = host
          .scheduler
          .module_graph_completed(script_id, source_text)?;
        host.apply_scheduler_actions(actions, event_loop)?;
        return Ok(());
      }

      let result = {
        let BrowserTabHost {
          executor, document, ..
        } = host;
        executor.fetch_module_graph(&spec, Arc::clone(&fetcher), document.as_mut(), event_loop)
      };
      match result {
        Ok(()) => {
          let actions = host
            .scheduler
            .module_graph_completed(script_id, String::new())?;
          host.apply_scheduler_actions(actions, event_loop)?;
        }
        Err(err) => {
          let actions = host.scheduler.module_graph_failed(script_id)?;
          let needs_manual_error = actions.is_empty();
          host.apply_scheduler_actions(actions, event_loop)?;
          if needs_manual_error {
            let node_id = host
              .scripts
              .get(&script_id)
              .map(|entry| entry.node_id)
              .ok_or_else(|| Error::Other("internal error: missing script entry".to_string()))?;
            host.dispatch_script_event_in_event_loop(node_id, "error", event_loop)?;
          }
          // Module graph fetch failures must be treated as script completion for lifecycle/load
          // blocker purposes (matching classic script fetch failures).
          host.finish_script_execution(script_id, event_loop)?;

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
    script_id: HtmlScriptId,
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
        value.trim_matches(|c: char| {
          matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' ')
        })
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
        return self.fail_external_script_fetch(script_id, event_loop);
      }
    }

    let is_blocking = self.scripts.get(&script_id).is_some_and(|entry| {
      entry.spec.parser_inserted
        && entry.spec.script_type == ScriptType::Classic
        && entry.spec.src_attr_present
        && !entry.spec.async_attr
        && !entry.spec.defer_attr
        && !entry.spec.force_async
    });

    if is_blocking {
      let _script_node_id = self
        .scripts
        .get(&script_id)
        .map(|entry| entry.node_id)
        .ok_or_else(|| {
          Error::Other(format!(
            "HtmlScriptScheduler requested fetch for unknown script_id={}",
            script_id.as_u64()
          ))
        })?;
      match self.fetch_script_source(script_id, &url, destination) {
        Ok(source) => {
          let actions = self.scheduler.classic_fetch_completed(script_id, source)?;
          self.apply_scheduler_actions(actions, event_loop)?;
        }
        Err(err) => {
          self.fail_external_script_fetch(script_id, event_loop)?;
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
          let actions = host.scheduler.classic_fetch_completed(script_id, source)?;
          host.apply_scheduler_actions(actions, event_loop)?;
        }
        Err(err) => {
          host.fail_external_script_fetch(script_id, event_loop)?;
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
    script_id: HtmlScriptId,
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

    span.arg_bool("async_attr", spec.async_attr);
    span.arg_bool("defer_attr", spec.defer_attr);
    span.arg_bool("parser_inserted", spec.parser_inserted);

    span.arg_str(
      "script_type",
      match spec.script_type {
        ScriptType::Classic => "classic",
        ScriptType::Module => "module",
        ScriptType::ImportMap => "importmap",
        ScriptType::Unknown => "unknown",
      },
    );
    span.arg_bool("integrity_attr_present", spec.integrity_attr_present);

    // HTML: module scripts are fetched in CORS mode by default. The `crossorigin` attribute only
    // controls the *credentials mode* ("anonymous" vs "use-credentials"). For classic scripts, CORS
    // mode is enabled only when the attribute is present.
    let cors_mode = match spec.script_type {
      ScriptType::Module => Some(
        spec
          .crossorigin
          .unwrap_or(crate::resource::CorsMode::Anonymous),
      ),
      _ => spec.crossorigin,
    };
    span.arg_str(
      "cors_mode",
      match cors_mode {
        None => "none",
        Some(crate::resource::CorsMode::Anonymous) => "anonymous",
        Some(crate::resource::CorsMode::UseCredentials) => "use-credentials",
      },
    );

    let effective_destination = match spec.script_type {
      ScriptType::Module => FetchDestination::ScriptCors,
      _ => destination,
    };
    span.arg_str(
      "destination",
      match effective_destination {
        FetchDestination::Document => "document",
        FetchDestination::DocumentNoUser => "document_no_user",
        FetchDestination::Iframe => "iframe",
        FetchDestination::Style => "style",
        FetchDestination::StyleCors => "style_cors",
        FetchDestination::Script => "script",
        FetchDestination::ScriptCors => "script_cors",
        FetchDestination::Image => "image",
        FetchDestination::ImageCors => "image_cors",
        FetchDestination::Video => "video",
        FetchDestination::VideoCors => "video_cors",
        FetchDestination::Audio => "audio",
        FetchDestination::AudioCors => "audio_cors",
        FetchDestination::Font => "font",
        FetchDestination::Other => "other",
        FetchDestination::Fetch => "fetch",
      },
    );

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
        if let (Some(doc_origin), Some(target_origin)) = (
          self.document_origin.as_ref(),
          crate::resource::origin_from_url(url),
        ) {
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

    // Populate a FetchRequest for trace metadata and for real network fetches.
    let mut req = FetchRequest::new(url, effective_destination);
    if let Some(referrer) = self.document_url.as_deref() {
      req = req.with_referrer_url(referrer);
    }
    if let Some(origin) = self.document_origin.as_ref() {
      req = req.with_client_origin(origin);
    }
    let effective_referrer_policy = spec
      .referrer_policy
      .unwrap_or(self.document_referrer_policy);
    req = req.with_referrer_policy(effective_referrer_policy);
    if let Some(cors_mode) = cors_mode {
      req = req.with_credentials_mode(cors_mode.credentials_mode());
    }
    span.arg_str(
      "credentials_mode",
      match req.credentials_mode {
        crate::resource::FetchCredentialsMode::Omit => "omit",
        crate::resource::FetchCredentialsMode::SameOrigin => "same-origin",
        crate::resource::FetchCredentialsMode::Include => "include",
      },
    );

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
        crate::js::sri::verify_integrity(source.as_bytes(), integrity)
          .map_err(|message| Error::Other(format!("SRI blocked script {url}: {message}")))?;
      }
      return Ok(source);
    }

    let fetcher = self.document.fetcher();

    let max_fetch = self.js_execution_options.max_script_bytes.saturating_add(1);
    let resource = fetcher.fetch_partial_with_request(req, max_fetch)?;
    span.arg_u64("bytes", resource.bytes.len() as u64);
    self
      .js_execution_options
      .check_script_source_bytes(resource.bytes.len(), &format!("source=external url={url}"))?;

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
      crate::js::sri::verify_integrity(&resource.bytes, integrity)
        .map_err(|message| Error::Other(format!("SRI blocked script {url}: {message}")))?;
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

  #[cfg(test)]
  fn dynamic_script_full_scan_count(&self) -> u64 {
    self.dynamic_script_full_scan_count
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

  fn script_execution_log(&self) -> Option<&crate::js::ScriptExecutionLog> {
    self.script_execution_log.as_ref()
  }

  fn script_execution_log_mut(&mut self) -> Option<&mut crate::js::ScriptExecutionLog> {
    self.script_execution_log.as_mut()
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
      self.dispatch_lifecycle_event(event_loop, EventTargetId::Document, event)?;
    }

    self
      .document_lifecycle_mut()
      .parsing_completed(event_loop)?;

    // If parsing completion is signalled from outside an event-loop task turn, perform a microtask
    // checkpoint immediately *only* when we are not currently executing JS.
    if event_loop.currently_running_task().is_none() && self.js_execution_depth.get() == 0 {
      self.perform_microtask_checkpoint_and_notify_executor(event_loop)?;
    }

    Ok(())
  }

  fn dispatch_lifecycle_event(
    &mut self,
    event_loop: &mut EventLoop<Self>,
    target: crate::web::events::EventTargetId,
    mut event: crate::web::events::Event,
  ) -> Result<()> {
    let target = target.normalize();
    let result = match target {
      EventTargetId::Document | EventTargetId::Window => {
        let (executor, document) = (&mut self.executor, &mut self.document);
        executor.dispatch_lifecycle_event(target, &event, document.as_mut(), event_loop)
      }
      // Fall back to Rust-side dispatch for non-document/window targets (e.g. `<script>` element
      // `load`/`error` events queued by the script scheduler).
      EventTargetId::Node(_) | EventTargetId::Opaque(_) => {
        return self
          .dispatch_dom_event_in_event_loop(target, event, event_loop)
          .map(|_| ());
      }
    };
    let navigation_request_seen = self.poll_navigation_request(event_loop)?;
    match result {
      Ok(()) => {
        if self.pending_navigation.is_some() {
          return Ok(());
        }
      }
      Err(err) => {
        if navigation_request_seen || self.pending_navigation.is_some() {
          return Ok(());
        }
        return Err(err);
      }
    }

    let dom: &crate::dom2::Document = self.document.dom();
    self.js_events.dispatch_dom_event(dom, target, &mut event)?;

    // Start loading image-like subresources after DOMContentLoaded has been delivered to listeners.
    // This keeps `DOMContentLoaded` independent of image fetch completion (matching browser
    // behavior), while still ensuring `load` waits on these resources via `LoadBlockerKind::Other`.
    if target == EventTargetId::Document && event.type_ == "DOMContentLoaded" {
      self.discover_and_start_image_loads(event_loop)?;
    }

    Ok(())
  }

  fn before_load_event(&mut self, event_loop: &mut EventLoop<Self>) -> Result<()>
  where
    Self: Sized + 'static,
  {
    // `BrowserTabHost` defers dynamic `<script>` discovery to "between turn" scans. When a load task
    // is already queued, ensure we perform one last discovery pass before deciding whether `load`
    // can be dispatched so late-inserted scripts participate as load blockers.
    self.discover_dynamic_scripts(event_loop)
  }

  fn document_lifecycle_mut(&mut self) -> &mut DocumentLifecycle {
    &mut self.lifecycle
  }
}

impl crate::js::window_realm::WindowRealmHost for BrowserTabHost {
  fn vm_host_and_window_realm(
    &mut self,
  ) -> crate::error::Result<(&mut dyn vm_js::VmHost, &mut crate::js::WindowRealm)> {
    let BrowserTabHost {
      document,
      executor,
      vmjs_fallback_realm,
      js_execution_options,
      current_script,
      ..
    } = self;
    let realm = match executor.window_realm_mut() {
      Some(realm) => realm,
      None => {
        if vmjs_fallback_realm.is_none() {
          let config = crate::js::WindowRealmConfig::new("about:blank")
            .with_current_script_state(current_script.clone());
          let created =
            crate::js::WindowRealm::new_with_js_execution_options(config, *js_execution_options)
              .map_err(|err| {
                crate::error::Error::Other(format!(
                  "failed to create fallback vm-js WindowRealm for callbacks: {err}"
                ))
              })?;
          *vmjs_fallback_realm = Some(created);
        }
        vmjs_fallback_realm.as_mut().ok_or_else(|| {
          crate::error::Error::Other(
            "missing fallback vm-js WindowRealm after initialization".to_string(),
          )
        })?
      }
    };
    Ok((document.as_mut(), realm))
  }

  fn webidl_bindings_host(&mut self) -> Option<&mut dyn webidl_vm_js::WebIdlBindingsHost> {
    Some(self.webidl_bindings_host.as_mut())
  }
}

impl crate::js::html_script_pipeline::ScriptElementEventHost for BrowserTabHost {
  fn dispatch_script_element_event(
    &mut self,
    event_loop: &mut EventLoop<Self>,
    script: NodeId,
    event_name: &'static str,
  ) -> Result<()> {
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
    let _default_not_prevented = self.dispatch_dom_event_in_event_loop(
      EventTargetId::Node(script).normalize(),
      event,
      event_loop,
    )?;
    Ok(())
  }
}

#[derive(Debug, Clone)]
struct RendererDomMappingCache {
  generation: u64,
  mapping: crate::dom2::RendererDomMapping,
}

// Test hook: count how many times `BrowserTab` had to (re)build a renderer preorder mapping for UI
// event dispatch. This should stay low even under high-frequency pointer move events.
//
// Keep this counter thread-local so unit tests can assert against it without flaking under the
// default parallel test runner.
#[cfg(test)]
thread_local! {
  static BROWSER_TAB_RENDERER_DOM_MAPPING_BUILD_COUNT: Cell<usize> = Cell::new(0);
}

/// JS-capable "tab" runtime (DOM + event loop + script scheduling + rendering).
///
/// `BrowserTab` couples:
/// - a live `dom2` document + render caching ([`BrowserDocumentDom2`]),
/// - an HTML-shaped [`EventLoop`] (tasks + microtasks + timers + `requestAnimationFrame` + `requestIdleCallback`),
/// - HTML `<script>` scheduling integrated with streaming parsing,
/// - navigation + history state.
///
/// Scripts are executed through a pluggable [`BrowserTabJsExecutor`] (for example
/// [`super::VmJsBrowserTabExecutor`]).
///
/// For a map of which public containers include JavaScript + an event loop, see
/// `docs/runtime_stacks.md`.
pub struct BrowserTab {
  trace: TraceHandle,
  trace_output: Option<PathBuf>,
  diagnostics: Option<super::SharedRenderDiagnostics>,
  host: BrowserTabHost,
  event_loop: EventLoop<BrowserTabHost>,
  /// The earliest event-loop time at which the next `requestAnimationFrame` callbacks are eligible
  /// to run.
  ///
  /// This is used by step-wise APIs like [`BrowserTab::tick_frame`] so embedders can call into the
  /// tab in a tight loop without accidentally running rAF callbacks as fast as possible.
  next_animation_frame_due: Duration,
  pending_frame: Option<Pixmap>,
  history: TabHistory,
  renderer_dom_mapping_cache: Option<RendererDomMappingCache>,
}

impl BrowserTab {
  fn sync_document_animation_time_to_event_loop(&mut self) {
    let ms = crate::js::time::duration_to_ms_f64(self.event_loop.now()) as f32;
    self.host.document.set_animation_time_ms(ms);
  }

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
    let next_animation_frame_due = event_loop.now();

    let mut tab = Self {
      trace: trace_handle,
      trace_output,
      diagnostics,
      host,
      event_loop,
      next_animation_frame_due,
      pending_frame: None,
      history: TabHistory::new(),
      renderer_dom_mapping_cache: None,
    };
    tab
      .host
      .document
      .set_animation_clock(tab.event_loop.clock());
    tab
      .event_loop
      .register_microtask_checkpoint_hook(BrowserTabHost::executor_microtask_checkpoint_hook)?;

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
      tab.navigate_to_url_with_replace_after_beforeunload(&req.url, options.clone(), req.replace)?;
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
      // Keep renderer metadata aligned with the host-visible CSP policy so JS module loaders can
      // enforce `script-src` for module dependencies/dynamic import.
      self.host.document.renderer_mut().document_csp = self.host.csp.clone();
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
        (options.timeout.is_some() || options.cancel_callback.is_some())
          .then(|| RenderDeadline::new(options.timeout, options.cancel_callback.clone()))
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

    let outcome = self.host.parse_until_blocked(&mut self.event_loop)?;
    if outcome.should_continue() {
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
    Self::from_html_with_js_execution_options(
      html,
      options,
      executor,
      JsExecutionOptions::default(),
    )
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

  /// Like [`BrowserTab::from_html`], but uses the provided [`ResourceFetcher`] for
  /// subresource/script/fetch() loads.
  pub fn from_html_with_fetcher<E>(
    html: &str,
    options: RenderOptions,
    executor: E,
    fetcher: Arc<dyn ResourceFetcher>,
  ) -> Result<Self>
  where
    E: BrowserTabJsExecutor + 'static,
  {
    Self::from_html_with_fetcher_and_js_execution_options(
      html,
      options,
      executor,
      fetcher,
      JsExecutionOptions::default(),
    )
  }

  /// Like [`BrowserTab::from_html_with_fetcher`], but allows overriding JavaScript execution budgets.
  pub fn from_html_with_fetcher_and_js_execution_options<E>(
    html: &str,
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
    let external_script_sources: Arc<Mutex<HashMap<String, String>>> =
      Arc::new(Mutex::new(HashMap::new()));
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
    let next_animation_frame_due = event_loop.now();

    let mut tab = Self {
      trace: trace_handle,
      trace_output,
      diagnostics,
      host,
      event_loop,
      next_animation_frame_due,
      pending_frame: None,
      history: TabHistory::new(),
      renderer_dom_mapping_cache: None,
    };
    tab
      .host
      .document
      .set_animation_clock(tab.event_loop.clock());
    tab
      .event_loop
      .register_microtask_checkpoint_hook(BrowserTabHost::executor_microtask_checkpoint_hook)?;
    let options_for_parse = options.clone();
    // Install a root deadline so `RenderOptions::{timeout,cancel_callback}` bounds:
    // - HTML parsing,
    // - and any synchronous navigation requests triggered by scripts during parsing.
    let deadline_enabled =
      options_for_parse.timeout.is_some() || options_for_parse.cancel_callback.is_some();
    let root_deadline_is_enabled =
      crate::render_control::root_deadline().is_some_and(|deadline| deadline.is_enabled());
    let deadline = (deadline_enabled && !root_deadline_is_enabled).then(|| {
      RenderDeadline::new(
        options_for_parse.timeout,
        options_for_parse.cancel_callback.clone(),
      )
    });
    let _deadline_guard = deadline
      .as_ref()
      .map(|deadline| DeadlineGuard::install(Some(deadline)));
    let document_referrer_policy =
      crate::html::referrer_policy::extract_referrer_policy_from_html(html).unwrap_or_default();
    tab
      .host
      .reset_scripting_state(None, document_referrer_policy)?;
    let mut parse_options = options_for_parse.clone();
    if root_deadline_is_enabled {
      // Avoid installing a nested deadline: the outer root deadline already enforces the render
      // budget across parsing + any follow-up navigation committed from scripts.
      parse_options.timeout = None;
      parse_options.cancel_callback = None;
    }
    let base_url = tab.parse_html_streaming_and_schedule_scripts(html, None, &parse_options)?;
    if let Some(req) = tab.host.pending_navigation.take() {
      tab.navigate_to_url_with_replace_after_beforeunload(&req.url, options_for_parse.clone(), req.replace)?;
    } else if tab.host.streaming_parse.is_none() {
      let renderer = tab.host.document.renderer_mut();
      match base_url {
        Some(url) => renderer.set_base_url(url),
        None => renderer.clear_base_url(),
      }
    }
    Ok(tab)
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
    #[cfg(feature = "direct_network")]
    {
      Self::from_html_with_fetcher_and_js_execution_options(
        html,
        options,
        executor,
        Arc::new(crate::resource::HttpFetcher::new()),
        js_execution_options,
      )
    }
    #[cfg(not(feature = "direct_network"))]
    {
      let _ = (html, options, executor, js_execution_options);
      Err(Error::Other(
        "direct network fetching is disabled; use BrowserTab::from_html_with_fetcher{_and_js_execution_options} and provide an explicit ResourceFetcher".to_string(),
      ))
    }
  }

  /// Construct a `BrowserTab` from a pre-built renderer instance with JavaScript enabled via the
  /// production `vm-js` executor.
  ///
  /// This is primarily used by CLI tools and other embeddings that want to configure the renderer
  /// (fetcher, runtime toggles, etc) before enabling JS execution via `BrowserTab`, without having
  /// to manually instantiate a `VmJsBrowserTabExecutor`.
  pub fn with_renderer_and_vmjs(
    renderer: super::FastRender,
    options: RenderOptions,
  ) -> Result<Self> {
    Self::with_renderer_and_vmjs_and_js_execution_options(
      renderer,
      options,
      JsExecutionOptions::default(),
    )
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
    let next_animation_frame_due = event_loop.now();

    let mut tab = Self {
      trace: trace_handle,
      trace_output,
      diagnostics,
      host,
      event_loop,
      next_animation_frame_due,
      pending_frame: None,
      history: TabHistory::new(),
      renderer_dom_mapping_cache: None,
    };
    tab
      .host
      .document
      .set_animation_clock(tab.event_loop.clock());
    tab
      .event_loop
      .register_microtask_checkpoint_hook(BrowserTabHost::executor_microtask_checkpoint_hook)?;
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
    let external_script_sources: Arc<Mutex<HashMap<String, String>>> =
      Arc::new(Mutex::new(HashMap::new()));
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
    let next_animation_frame_due = event_loop.now();

    let mut tab = Self {
      trace: trace_handle,
      trace_output,
      diagnostics,
      host,
      event_loop,
      next_animation_frame_due,
      pending_frame: None,
      history: TabHistory::new(),
      renderer_dom_mapping_cache: None,
    };
    tab
      .host
      .document
      .set_animation_clock(tab.event_loop.clock());
    tab
      .event_loop
      .register_microtask_checkpoint_hook(BrowserTabHost::executor_microtask_checkpoint_hook)?;

    // Configure the renderer's document URL hint up-front so any non-script fetches (stylesheets,
    // images, etc) see consistent referrer/origin context during parsing.
    tab
      .host
      .document
      .renderer_mut()
      .set_document_url(document_url);

    let options_for_parse = options.clone();
    // Install a root deadline so `RenderOptions::{timeout,cancel_callback}` bounds:
    // - HTML parsing,
    // - and any synchronous navigation requests triggered by scripts during parsing.
    let deadline_enabled =
      options_for_parse.timeout.is_some() || options_for_parse.cancel_callback.is_some();
    let root_deadline_is_enabled =
      crate::render_control::root_deadline().is_some_and(|deadline| deadline.is_enabled());
    let deadline = (deadline_enabled && !root_deadline_is_enabled).then(|| {
      RenderDeadline::new(
        options_for_parse.timeout,
        options_for_parse.cancel_callback.clone(),
      )
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
    let base_url =
      tab.parse_html_streaming_and_schedule_scripts(html, Some(document_url), &parse_options)?;
    if let Some(req) = tab.host.pending_navigation.take() {
      tab.navigate_to_url_with_replace_after_beforeunload(&req.url, options_for_parse.clone(), req.replace)?;
    } else {
      let renderer = tab.host.document.renderer_mut();
      match base_url {
        Some(url) => renderer.set_base_url(url),
        None => renderer.clear_base_url(),
      }
    }
    Ok(tab)
  }

  /// Like [`BrowserTab::from_html_with_document_url_and_fetcher`], but uses the provided
  /// [`EventLoop`] for tasks/timers/`requestAnimationFrame` callbacks.
  pub fn from_html_with_document_url_and_fetcher_and_event_loop<E>(
    html: &str,
    document_url: &str,
    options: RenderOptions,
    executor: E,
    fetcher: Arc<dyn ResourceFetcher>,
    event_loop: EventLoop<BrowserTabHost>,
  ) -> Result<Self>
  where
    E: BrowserTabJsExecutor + 'static,
  {
    Self::from_html_with_document_url_and_fetcher_and_event_loop_and_js_execution_options(
      html,
      document_url,
      options,
      executor,
      fetcher,
      event_loop,
      JsExecutionOptions::default(),
    )
  }

  /// Like [`BrowserTab::from_html_with_document_url_and_fetcher_and_event_loop`], but allows
  /// overriding JavaScript execution budgets.
  pub fn from_html_with_document_url_and_fetcher_and_event_loop_and_js_execution_options<E>(
    html: &str,
    document_url: &str,
    options: RenderOptions,
    executor: E,
    fetcher: Arc<dyn ResourceFetcher>,
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
    let diagnostics = (!matches!(options.diagnostics_level, super::DiagnosticsLevel::None))
      .then(super::SharedRenderDiagnostics::new);
    let external_script_sources: Arc<Mutex<HashMap<String, String>>> =
      Arc::new(Mutex::new(HashMap::new()));
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
    event_loop.set_trace_handle(trace_handle.clone());
    event_loop.set_queue_limits(js_execution_options.event_loop_queue_limits);
    let next_animation_frame_due = event_loop.now();

    let mut tab = Self {
      trace: trace_handle,
      trace_output,
      diagnostics,
      host,
      event_loop,
      next_animation_frame_due,
      pending_frame: None,
      history: TabHistory::new(),
      renderer_dom_mapping_cache: None,
    };
    tab
      .host
      .document
      .set_animation_clock(tab.event_loop.clock());
    tab
      .event_loop
      .register_microtask_checkpoint_hook(BrowserTabHost::executor_microtask_checkpoint_hook)?;

    // Configure the renderer's document URL hint up-front so any non-script fetches (stylesheets,
    // images, etc) see consistent referrer/origin context during parsing.
    tab
      .host
      .document
      .renderer_mut()
      .set_document_url(document_url);

    let options_for_parse = options.clone();
    // Install a root deadline so `RenderOptions::{timeout,cancel_callback}` bounds:
    // - HTML parsing,
    // - and any synchronous navigation requests triggered by scripts during parsing.
    let deadline_enabled =
      options_for_parse.timeout.is_some() || options_for_parse.cancel_callback.is_some();
    let root_deadline_is_enabled =
      crate::render_control::root_deadline().is_some_and(|deadline| deadline.is_enabled());
    let deadline = (deadline_enabled && !root_deadline_is_enabled).then(|| {
      RenderDeadline::new(
        options_for_parse.timeout,
        options_for_parse.cancel_callback.clone(),
      )
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
    let base_url =
      tab.parse_html_streaming_and_schedule_scripts(html, Some(document_url), &parse_options)?;
    if let Some(req) = tab.host.pending_navigation.take() {
      tab.navigate_to_url_with_replace_after_beforeunload(&req.url, options_for_parse.clone(), req.replace)?;
    } else {
      let renderer = tab.host.document.renderer_mut();
      match base_url {
        Some(url) => renderer.set_base_url(url),
        None => renderer.clear_base_url(),
      }
    }
    Ok(tab)
  }

  /// Like [`BrowserTab::from_html_with_event_loop`], but uses the provided [`ResourceFetcher`] for
  /// subresource/script/fetch() loads.
  pub fn from_html_with_event_loop_and_fetcher<E>(
    html: &str,
    options: RenderOptions,
    executor: E,
    event_loop: EventLoop<BrowserTabHost>,
    fetcher: Arc<dyn ResourceFetcher>,
  ) -> Result<Self>
  where
    E: BrowserTabJsExecutor + 'static,
  {
    Self::from_html_with_event_loop_and_fetcher_and_js_execution_options(
      html,
      options,
      executor,
      event_loop,
      fetcher,
      JsExecutionOptions::default(),
    )
  }

  /// Like [`BrowserTab::from_html_with_event_loop_and_fetcher`], but allows overriding JavaScript
  /// execution budgets.
  pub fn from_html_with_event_loop_and_fetcher_and_js_execution_options<E>(
    html: &str,
    options: RenderOptions,
    executor: E,
    mut event_loop: EventLoop<BrowserTabHost>,
    fetcher: Arc<dyn ResourceFetcher>,
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
    let external_script_sources: Arc<Mutex<HashMap<String, String>>> =
      Arc::new(Mutex::new(HashMap::new()));
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
    event_loop.set_trace_handle(trace_handle.clone());
    event_loop.set_queue_limits(js_execution_options.event_loop_queue_limits);
    let next_animation_frame_due = event_loop.now();

    let mut tab = Self {
      trace: trace_handle,
      trace_output,
      diagnostics,
      host,
      event_loop,
      next_animation_frame_due,
      pending_frame: None,
      history: TabHistory::new(),
      renderer_dom_mapping_cache: None,
    };
    tab
      .host
      .document
      .set_animation_clock(tab.event_loop.clock());
    tab
      .event_loop
      .register_microtask_checkpoint_hook(BrowserTabHost::executor_microtask_checkpoint_hook)?;
    let options_for_parse = options.clone();
    // Install a root deadline so `RenderOptions::{timeout,cancel_callback}` bounds:
    // - HTML parsing,
    // - and any synchronous navigation requests triggered by scripts during parsing.
    let deadline_enabled =
      options_for_parse.timeout.is_some() || options_for_parse.cancel_callback.is_some();
    let root_deadline_is_enabled =
      crate::render_control::root_deadline().is_some_and(|deadline| deadline.is_enabled());
    let deadline = (deadline_enabled && !root_deadline_is_enabled).then(|| {
      RenderDeadline::new(
        options_for_parse.timeout,
        options_for_parse.cancel_callback.clone(),
      )
    });
    let _deadline_guard = deadline
      .as_ref()
      .map(|deadline| DeadlineGuard::install(Some(deadline)));
    let document_referrer_policy =
      crate::html::referrer_policy::extract_referrer_policy_from_html(html).unwrap_or_default();
    tab
      .host
      .reset_scripting_state(None, document_referrer_policy)?;
    let mut parse_options = options_for_parse.clone();
    if root_deadline_is_enabled {
      // Avoid installing a nested deadline: the outer root deadline already enforces the render
      // budget across parsing + any follow-up navigation committed from scripts.
      parse_options.timeout = None;
      parse_options.cancel_callback = None;
    }
    let base_url = tab.parse_html_streaming_and_schedule_scripts(html, None, &parse_options)?;
    if let Some(req) = tab.host.pending_navigation.take() {
      tab.navigate_to_url_with_replace_after_beforeunload(&req.url, options_for_parse.clone(), req.replace)?;
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
    #[cfg(feature = "direct_network")]
    {
      Self::from_html_with_event_loop_and_fetcher_and_js_execution_options(
        html,
        options,
        executor,
        event_loop,
        Arc::new(crate::resource::HttpFetcher::new()),
        js_execution_options,
      )
    }
    #[cfg(not(feature = "direct_network"))]
    {
      let _ = (html, options, executor, event_loop, js_execution_options);
      Err(Error::Other(
        "direct network fetching is disabled; use BrowserTab::from_html_with_event_loop_and_fetcher{_and_js_execution_options} and provide an explicit ResourceFetcher".to_string(),
      ))
    }
  }

  pub fn register_script_source(&mut self, url: impl Into<String>, source: impl Into<String>) {
    self
      .host
      .register_external_script_source(url.into(), source.into());
  }

  /// Enable a bounded FIFO log of executed scripts for debugging script ordering and
  /// `document.currentScript`.
  pub fn enable_script_execution_log(&mut self, capacity: usize) {
    self.host.script_execution_log_capacity = Some(capacity);
    self.host.script_execution_log = Some(crate::js::ScriptExecutionLog::new(capacity));
  }

  /// Returns the script execution log if enabled.
  pub fn script_execution_log(&self) -> Option<&crate::js::ScriptExecutionLog> {
    self.host.script_execution_log.as_ref()
  }

  /// Register an in-memory HTML payload that can be navigated to by URL (including via
  /// `window.location`-driven navigations).
  pub fn register_html_source(&mut self, url: impl Into<String>, html: impl Into<String>) {
    self.host.register_html_source(url.into(), html.into());
  }

  /// Returns and clears a pending navigation request emitted by JavaScript (for example via
  /// `window.location.href = ...`).
  ///
  /// This only exposes the request; it does **not** commit the navigation or reset the tab.
  ///
  /// Live embeddings that drive a dedicated JS tab can use this to observe JS-triggered navigations
  /// and synchronize them with a separate render tab.
  pub fn take_pending_navigation_request(&mut self) -> Option<LocationNavigationRequest> {
    self
      .take_pending_navigation_request_with_deadline()
      .map(|(req, _deadline)| req)
  }

  /// Like [`BrowserTab::take_pending_navigation_request`], but also returns any associated render
  /// deadline captured when the request was promoted to `BrowserTabHost.pending_navigation`.
  pub fn take_pending_navigation_request_with_deadline(
    &mut self,
  ) -> Option<(LocationNavigationRequest, Option<RenderDeadline>)> {
    if let Some(req) = self.host.pending_navigation.take() {
      let deadline = self.host.pending_navigation_deadline.take();
      return Some((req, deadline));
    }

    // Requests can be stored in the JS executor (e.g. vm-js `WindowRealm`) until the host polls
    // `take_navigation_request()`. Expose them here so embedders can coordinate navigation without
    // forcing an immediate commit.
    self
      .host
      .executor
      .take_navigation_request()
      .map(|req| (req, None))
  }

  pub fn set_event_listener_invoker(
    &mut self,
    invoker: Box<dyn crate::web::events::EventListenerInvoker>,
  ) {
    self.host.set_event_invoker(invoker);
  }

  pub fn set_visibility(&mut self, state: DocumentVisibilityState) -> Result<()> {
    if self.host.document.visibility_state() == state {
      return Ok(());
    }

    self.host.document.set_visibility_state(state);
    // Page visibility changes are observed via `visibilitychange`, dispatched as a DOM manipulation
    // task (not synchronously), matching browser task ordering.
    self.event_loop.queue_task(TaskSource::DOMManipulation, |host, event_loop| {
      let mut event = Event::new(
        "visibilitychange",
        EventInit {
          bubbles: false,
          cancelable: false,
          composed: false,
        },
      );
      event.is_trusted = true;
      let _default_not_prevented =
        host.dispatch_dom_event_in_event_loop(EventTargetId::Document, event, event_loop)?;
      Ok(())
    })?;
    Ok(())
  }

  pub fn set_hidden(&mut self, hidden: bool) -> Result<()> {
    self.set_visibility(if hidden {
      DocumentVisibilityState::Hidden
    } else {
      DocumentVisibilityState::Visible
    })
  }

  /// Overrides the clock used to derive the document timeline for real-time CSS animation sampling.
  ///
  /// This is forwarded to the underlying [`BrowserDocumentDom2`]. It does **not** change the event
  /// loop's clock (construct the tab with [`EventLoop::with_clock`] to control timer/rAF time).
  pub fn set_animation_clock(&mut self, clock: Arc<dyn Clock>) {
    self.host.document.set_animation_clock(clock);
  }

  /// Enables/disables real-time CSS animation sampling for this tab's document timeline.
  ///
  /// When enabled and `RenderOptions.animation_time` is `None`, each paint call samples CSS
  /// animations/transitions at the time elapsed since the first rendered frame after enabling.
  pub fn set_realtime_animations_enabled(&mut self, enabled: bool) {
    self.host.document.set_realtime_animations_enabled(enabled);
  }

  /// Updates the animation/transition sampling timestamp in milliseconds since document load.
  ///
  /// This is a convenience forwarder to [`BrowserDocumentDom2::set_animation_time`].
  pub fn set_animation_time(&mut self, time_ms: Option<f32>) {
    self.host.document.set_animation_time(time_ms);
  }

  /// Convenience wrapper for [`BrowserTab::set_animation_time`] with a concrete timestamp.
  pub fn set_animation_time_ms(&mut self, time_ms: f32) {
    self.set_animation_time(Some(time_ms));
  }

  pub fn write_trace(&self) -> Result<()> {
    let Some(path) = self.trace_output.as_deref() else {
      return Ok(());
    };
    self.trace.write_chrome_trace(path).map_err(Error::Io)
  }

  pub fn diagnostics_snapshot(&self) -> Option<super::RenderDiagnostics> {
    self
      .diagnostics
      .as_ref()
      .map(|diag| diag.clone().into_inner())
  }

  pub fn navigate_to_html(&mut self, html: &str, options: RenderOptions) -> Result<()> {
    // Navigations replace the current document; any pending frame is no longer relevant.
    self.pending_frame = None;
    self.renderer_dom_mapping_cache = None;
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
      RenderDeadline::new(
        options_for_parse.timeout,
        options_for_parse.cancel_callback.clone(),
      )
    });
    let _deadline_guard = deadline
      .as_ref()
      .map(|deadline| DeadlineGuard::install(Some(deadline)));

    // `beforeunload` can cancel navigations; only proceed if not canceled.
    let should_navigate = {
      let (executor, document) = (&mut self.host.executor, &mut self.host.document);
      executor.dispatch_beforeunload_event(document.as_mut(), &mut self.event_loop)?
    };
    if !should_navigate {
      return Ok(());
    }

    // Navigation proceeds: fire the old document's teardown events before replacing the realm/DOM.
    {
      let mut event = Event::new(
        "pagehide",
        EventInit {
          bubbles: false,
          cancelable: false,
          composed: false,
        },
      );
      event.is_trusted = true;
      self
        .host
        .dispatch_lifecycle_event(&mut self.event_loop, EventTargetId::Window, event)?;
    }
    {
      let mut event = Event::new(
        "unload",
        EventInit {
          bubbles: false,
          cancelable: false,
          composed: false,
        },
      );
      event.is_trusted = true;
      self
        .host
        .dispatch_lifecycle_event(&mut self.event_loop, EventTargetId::Window, event)?;
    }

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
      self.navigate_to_url_with_replace_after_beforeunload(&req.url, options_for_parse.clone(), req.replace)?;
      return Ok(());
    }

    // Navigation committed: fire `pageshow` before DOMContentLoaded/load tasks run.
    {
      let mut event = Event::new(
        "pageshow",
        EventInit {
          bubbles: false,
          cancelable: false,
          composed: false,
        },
      );
      event.is_trusted = true;
      self
        .host
        .dispatch_lifecycle_event(&mut self.event_loop, EventTargetId::Window, event)?;
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
    replace: bool,
  ) -> Result<()> {
    self.navigate_to_url_with_replace_internal(url, options, replace, /*run_beforeunload=*/ true)
  }

  fn navigate_to_url_with_replace_after_beforeunload(
    &mut self,
    url: &str,
    options: RenderOptions,
    replace: bool,
  ) -> Result<()> {
    self.navigate_to_url_with_replace_internal(url, options, replace, /*run_beforeunload=*/ false)
  }

  fn navigate_to_url_with_replace_internal(
    &mut self,
    url: &str,
    options: RenderOptions,
    mut replace: bool,
    mut run_beforeunload: bool,
  ) -> Result<()> {
    // Navigations replace the current document; any pending frame is no longer relevant.
    self.pending_frame = None;
    self.renderer_dom_mapping_cache = None;
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
            crate::html::referrer_policy::extract_referrer_policy_from_html(&html)
              .unwrap_or_default();
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

      if run_beforeunload {
        let should_navigate = {
          let (executor, document) = (&mut self.host.executor, &mut self.host.document);
          executor.dispatch_beforeunload_event(document.as_mut(), &mut self.event_loop)?
        };
        if !should_navigate {
          return Ok(());
        }
      }

      // Navigation proceeds: fire the old document's teardown events before we replace its DOM/realm.
      {
        let mut event = Event::new(
          "pagehide",
          EventInit {
            bubbles: false,
            cancelable: false,
            composed: false,
          },
        );
        event.is_trusted = true;
        self
          .host
          .dispatch_lifecycle_event(&mut self.event_loop, EventTargetId::Window, event)?;
      }
      {
        let mut event = Event::new(
          "unload",
          EventInit {
            bubbles: false,
            cancelable: false,
            composed: false,
          },
        );
        event.is_trusted = true;
        self
          .host
          .dispatch_lifecycle_event(&mut self.event_loop, EventTargetId::Window, event)?;
      }

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
      self.host.document.reset_with_dom(
        Document::new(QuirksMode::NoQuirks),
        options_for_parse.clone(),
      );
      self.reset_event_loop();
      self.host.trace = self.trace.clone();
      self
        .host
        .reset_scripting_state(Some(final_url.clone()), document_referrer_policy)?;
      self.host.csp = header_csp;
      self.host.document.renderer_mut().document_csp = self.host.csp.clone();

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
        // `beforeunload` was dispatched when the navigation request was produced; do not re-dispatch
        // it for the follow-up navigation.
        run_beforeunload = false;
        continue;
      }

      // Navigation committed: fire `pageshow` before DOMContentLoaded/load tasks run.
      {
        let mut event = Event::new(
          "pageshow",
          EventInit {
            bubbles: false,
            cancelable: false,
            composed: false,
          },
        );
        event.is_trusted = true;
        self
          .host
          .dispatch_lifecycle_event(&mut self.event_loop, EventTargetId::Window, event)?;
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
    self.navigate_to_url_with_replace_after_beforeunload(&req.url, options, req.replace)?;
    Ok(true)
  }

  fn run_event_loop_until_idle_handling_errors_with_pending_navigation_abort(
    &mut self,
    limits: RunLimits,
    render_between_turns: bool,
    mut on_error: impl FnMut(Error),
    mut on_render: impl FnMut(),
  ) -> Result<RunUntilIdleOutcome> {
    // If a navigation is already pending before we start driving the event loop (for example, a
    // navigation request produced by a host-dispatched DOM event), abandon all pending work for the
    // current document immediately so we don't run tasks/microtasks that should be discarded once
    // the new document commits.
    if self.host.pending_navigation.is_some() {
      self.event_loop.clear_all_pending_work();
      return Ok(RunUntilIdleOutcome::Idle);
    }
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
        let executor_hook: fn(&mut BrowserTabHost, &mut EventLoop<BrowserTabHost>) -> Result<()> =
          BrowserTabHost::executor_microtask_checkpoint_hook;
        if !event_loop
          .microtask_checkpoint_hooks()
          .iter()
          .any(|&hook| std::ptr::fn_addr_eq(hook, executor_hook))
        {
          BrowserTabHost::executor_microtask_checkpoint_hook(host, event_loop)?;
        }
        if render_between_turns && host.document.is_dirty() {
          // If the document is already dirty, we're about to render a frame. Update the renderer's
          // CSS animation sampling time to match the JS event-loop clock so any time-dependent
          // effects (animations/transitions) stay coherent with `requestAnimationFrame`.
          let ms = crate::js::time::duration_to_ms_f64(event_loop.now()) as f32;
          host.document.set_animation_time_ms(ms);
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
        composed: true,
      },
    );
    event.is_trusted = true;
    // In browsers, `click` is a `MouseEvent`. We may not always have real pointer coordinates (e.g.
    // programmatic "simulate click" helpers or keyboard activation), but surfacing a MouseEvent
    // shape is important for real-world scripts that check `instanceof MouseEvent`.
    let mut mouse = MouseEvent::default();
    mouse.detail = 1;
    event.mouse = Some(mouse);
    let (host, event_loop) = (&mut self.host, &mut self.event_loop);
    host.dispatch_dom_event_in_event_loop(
      EventTargetId::Node(node_id).normalize(),
      event,
      event_loop,
    )
  }

  /// Dispatch a trusted `click` DOM event to `node_id`, including pointer coordinates/buttons.
  ///
  /// Returns `true` when the event's default was **not** prevented.
  pub fn dispatch_click_event_with_pointer(
    &mut self,
    node_id: NodeId,
    pos_css: (f32, f32),
    button: PointerButton,
    modifiers: PointerModifiers,
  ) -> Result<bool> {
    let (dom_button, dom_buttons) = match button {
      PointerButton::Primary => (0i16, 1u16),
      PointerButton::Secondary => (2i16, 2u16),
      PointerButton::Middle => (1i16, 4u16),
      PointerButton::Back => (3i16, 8u16),
      PointerButton::Forward => (4i16, 16u16),
      PointerButton::None | PointerButton::Other(_) => (-1i16, 0u16),
    };
    let mouse = MouseEvent {
      detail: 1,
      client_x: pos_css.0 as f64,
      client_y: pos_css.1 as f64,
      button: dom_button,
      buttons: dom_buttons,
      ctrl_key: modifiers.ctrl(),
      shift_key: modifiers.shift(),
      alt_key: modifiers.alt(),
      meta_key: modifiers.meta(),
      related_target: None,
    };

    self.dispatch_mouse_event(
      node_id,
      "click",
      EventInit {
        bubbles: true,
        cancelable: true,
        composed: true,
      },
      mouse,
    )
  }

  /// Dispatch a trusted `contextmenu` DOM event to `node_id`, including pointer coordinates/buttons.
  ///
  /// This is intended for input modalities that request a context menu without a physical right
  /// click (e.g. assistive technology actions).
  ///
  /// When no real pointer state exists, we still shape the event like a secondary mouse click:
  /// `button=2` and `buttons=2`.
  ///
  /// Returns `true` when the event's default was **not** prevented.
  pub fn dispatch_contextmenu_event_with_pointer(
    &mut self,
    node_id: NodeId,
    pos_css: (f32, f32),
    modifiers: PointerModifiers,
  ) -> Result<bool> {
    let mouse = MouseEvent {
      detail: 0,
      client_x: pos_css.0 as f64,
      client_y: pos_css.1 as f64,
      button: 2,
      buttons: 2,
      ctrl_key: modifiers.ctrl(),
      shift_key: modifiers.shift(),
      alt_key: modifiers.alt(),
      meta_key: modifiers.meta(),
      related_target: None,
    };

    self.dispatch_mouse_event(
      node_id,
      "contextmenu",
      EventInit {
        bubbles: true,
        cancelable: true,
        composed: false,
      },
      mouse,
    )
  }

  /// Dispatch a trusted mouse DOM event to `node_id`.
  ///
  /// Returns `true` when the event's default was **not** prevented.
  pub fn dispatch_mouse_event(
    &mut self,
    node_id: NodeId,
    type_: &str,
    init: EventInit,
    mouse: MouseEvent,
  ) -> Result<bool> {
    let mut event = Event::new(type_, init);
    event.is_trusted = true;
    event.mouse = Some(mouse);
    let (host, event_loop) = (&mut self.host, &mut self.event_loop);
    host.dispatch_dom_event_in_event_loop(EventTargetId::Node(node_id).normalize(), event, event_loop)
  }

  fn create_data_transfer_for_files(
    &mut self,
    paths: &[PathBuf],
  ) -> Result<Option<(vm_js::Value, vm_js::RootId)>> {
    use vm_js::{PropertyDescriptor, PropertyKey, PropertyKind, Value, VmError};

    // `vm-js` integrations may run with JS disabled (no realm). Treat DataTransfer creation as
    // best-effort so callers can still dispatch a `drop` event (for `preventDefault()` semantics)
    // even when the embedding cannot provide `dataTransfer.files` yet.
    let Ok((_host_ctx, realm)) = self.host.vm_host_and_window_realm() else {
      return Ok(None);
    };

    // Reset any prior termination state: host-driven event dispatch should be able to allocate even
    // if the previous turn ran out of budget.
    realm.reset_interrupt();

    let (_vm, realm_ref, heap) = realm.vm_realm_and_heap_mut();
    let mut scope = heap.scope();

    let alloc_key = |scope: &mut vm_js::Scope<'_>, name: &str| -> std::result::Result<PropertyKey, VmError> {
      let s = scope.alloc_string(name)?;
      scope.push_root(Value::String(s))?;
      Ok(PropertyKey::from_string(s))
    };

    let intr = realm_ref.intrinsics();

    // DataTransfer object placeholder.
    let data_transfer_obj = scope.alloc_object().map_err(|e| Error::Other(e.to_string()))?;
    scope
      .push_root(Value::Object(data_transfer_obj))
      .map_err(|e| Error::Other(e.to_string()))?;
    scope
      .heap_mut()
      .object_set_prototype(data_transfer_obj, Some(intr.object_prototype()))
      .map_err(|e| Error::Other(e.to_string()))?;

    // MVP `dataTransfer.files` surface:
    // - implemented as a plain JS `Array` of file name strings
    // - NOT a real `FileList` (and entries are not `File` objects)
    //
    // This is sufficient for basic `drop` handlers that only need to observe that files exist, and
    // keeps the API deterministic while the full File/FileList plumbing is implemented.
    let files_arr = scope
      .alloc_array(paths.len())
      .map_err(|e| Error::Other(e.to_string()))?;
    scope
      .push_root(Value::Object(files_arr))
      .map_err(|e| Error::Other(e.to_string()))?;
    scope
      .heap_mut()
      .object_set_prototype(files_arr, Some(intr.array_prototype()))
      .map_err(|e| Error::Other(e.to_string()))?;

    for (idx, path) in paths.iter().enumerate() {
      let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string());
      let name_s = scope.alloc_string(&name).map_err(|e| Error::Other(e.to_string()))?;
      scope
        .push_root(Value::String(name_s))
        .map_err(|e| Error::Other(e.to_string()))?;
      let key = alloc_key(&mut scope, &idx.to_string()).map_err(|e| Error::Other(e.to_string()))?;
      scope
        .define_property(
          files_arr,
          key,
          PropertyDescriptor {
            enumerable: true,
            configurable: true,
            kind: PropertyKind::Data {
              value: Value::String(name_s),
              writable: true,
            },
          },
        )
        .map_err(|e| Error::Other(e.to_string()))?;
    }

    let files_key = alloc_key(&mut scope, "files").map_err(|e| Error::Other(e.to_string()))?;
    scope
      .define_property(
        data_transfer_obj,
        files_key,
        PropertyDescriptor {
          enumerable: true,
          configurable: true,
          kind: PropertyKind::Data {
            value: Value::Object(files_arr),
            writable: false,
          },
        },
      )
      .map_err(|e| Error::Other(e.to_string()))?;

    let value = Value::Object(data_transfer_obj);
    let root_id = scope
      .heap_mut()
      .add_root(value)
      .map_err(|e| Error::Other(e.to_string()))?;
    Ok(Some((value, root_id)))
  }

  fn release_vm_js_root(&mut self, root_id: vm_js::RootId) {
    let Ok((_host_ctx, realm)) = self.host.vm_host_and_window_realm() else {
      return;
    };
    let (_vm, _realm_ref, heap) = realm.vm_realm_and_heap_mut();
    heap.remove_root(root_id);
  }

  /// Dispatch a trusted, cancelable `"drop"` DOM event with an embedding-created `dataTransfer`
  /// payload.
  ///
  /// Returns `true` when the event's default was **not** prevented.
  pub(crate) fn dispatch_drop_event_with_files(
    &mut self,
    node_id: NodeId,
    pos_css: (f32, f32),
    paths: &[PathBuf],
  ) -> Result<bool> {
    // Best-effort: if DataTransfer allocation fails, still dispatch the `drop` event so JS can
    // cancel it via `preventDefault()`.
    let data_transfer = self.create_data_transfer_for_files(paths).ok().flatten();

    let mut event = Event::new(
      "drop",
      EventInit {
        bubbles: true,
        cancelable: true,
        composed: false,
      },
    );
    event.is_trusted = true;
    // Treat drop as a UIEvent/MouseEvent-like dispatch so JS can observe pointer coordinates.
    event.mouse = Some(MouseEvent {
      client_x: pos_css.0 as f64,
      client_y: pos_css.1 as f64,
      button: 0,
      buttons: 0,
      detail: 0,
      ctrl_key: false,
      shift_key: false,
      alt_key: false,
      meta_key: false,
      related_target: None,
    });
    event.drag_data_transfer = data_transfer.as_ref().map(|(value, _root_id)| *value);

    let dispatch_result = {
      let (host, event_loop) = (&mut self.host, &mut self.event_loop);
      host.dispatch_dom_event_in_event_loop(
        EventTargetId::Node(node_id).normalize(),
        event,
        event_loop,
      )
    };

    if let Some((_value, root_id)) = data_transfer {
      self.release_vm_js_root(root_id);
    }

    dispatch_result
  }

  /// Create and root a `DataTransfer`-like object in the tab's vm-js WindowRealm.
  ///
  /// Returns a stable handle ID that can later be passed to [`BrowserTab::dispatch_drag_event`].
  pub fn create_data_transfer_for_text(&mut self, text: &str) -> Result<u64> {
    let id = self.host.next_data_transfer_id;
    self.host.next_data_transfer_id = self.host.next_data_transfer_id.wrapping_add(1);

    let (obj, root_id) = {
      let Some(realm) = self.host.executor.window_realm_mut() else {
        return Err(Error::Other(
          "create_data_transfer_for_text requires a vm-js WindowRealm".to_string(),
        ));
      };

      realm.reset_interrupt();

      let (vm, realm_ref, heap) = realm.vm_realm_and_heap_mut();
      let obj = crate::js::window_data_transfer::create_data_transfer_with_text_plain(
        vm,
        realm_ref,
        heap,
        text,
      )
      .map_err(|err| Error::Other(err.to_string()))?;

      let root_id = {
        let mut scope = heap.scope();
        scope
          .push_root(vm_js::Value::Object(obj))
          .map_err(|err| Error::Other(err.to_string()))?;
        scope
          .heap_mut()
          .add_root(vm_js::Value::Object(obj))
          .map_err(|err| Error::Other(err.to_string()))?
      };

      (obj, root_id)
    };

    self
      .host
      .active_data_transfers
      .insert(id, ActiveDataTransfer { obj, root_id });
    Ok(id)
  }

  /// Release a previously created DataTransfer handle.
  ///
  /// Best-effort: safe to call multiple times or with an unknown ID.
  pub fn release_data_transfer(&mut self, id: u64) {
    let Some(handle) = self.host.active_data_transfers.remove(&id) else {
      return;
    };

    if let Some(realm) = self.host.executor.window_realm_mut() {
      realm.heap_mut().remove_root(handle.root_id);
    }
  }

  /// Dispatch a trusted drag-and-drop DOM event to `node_id`.
  ///
  /// Returns `true` when the event's default was **not** prevented.
  pub fn dispatch_drag_event(
    &mut self,
    node_id: NodeId,
    type_: &str,
    init: EventInit,
    mouse: MouseEvent,
    data_transfer_id: Option<u64>,
  ) -> Result<bool> {
    let mut event = Event::new(type_, init);
    event.is_trusted = true;
    event.mouse = Some(mouse);
    if let Some(id) = data_transfer_id {
      if let Some(handle) = self.host.active_data_transfers.get(&id) {
        event.drag_data_transfer = Some(vm_js::Value::Object(handle.obj));
      }
    }
    let (host, event_loop) = (&mut self.host, &mut self.event_loop);
    host.dispatch_dom_event_in_event_loop(EventTargetId::Node(node_id).normalize(), event, event_loop)
  }

  /// Returns `true` if `target` has an own, callable `on{type_}` event handler property.
  ///
  /// This mirrors the `vm-js` host-driven dispatch behavior implemented by
  /// [`crate::web::events::EventListenerInvoker::invoke_event_handler_property`], and is intended as
  /// a lightweight query for high-frequency event gating (`mousemove`, `mouseover`, ...).
  pub fn has_event_handler_property(&mut self, target: EventTargetId, type_: &str) -> Result<bool> {
    // Only the vm-js invoker supports handler property checks today; other executors treat handler
    // properties as absent.
    let Some(any) = self.host.event_invoker.as_any_mut() else {
      return Ok(false);
    };
    let Some(invoker) =
      any.downcast_mut::<crate::js::window_realm::WindowRealmDomEventListenerInvoker<BrowserTabHost>>()
    else {
      return Ok(false);
    };
    invoker
      .has_event_handler_property(target, type_)
      .map_err(|err| Error::Other(err.to_string()))
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
        composed: true,
      },
    );
    event.is_trusted = true;
    let (host, event_loop) = (&mut self.host, &mut self.event_loop);
    host.dispatch_dom_event_in_event_loop(
      EventTargetId::Node(node_id).normalize(),
      event,
      event_loop,
    )
  }

  /// Dispatch a trusted `toggle` DOM event to `node_id` (used by `<details>`).
  ///
  /// HTML fires `toggle` when a `<details>` element is opened/closed. This helper allows host-driven
  /// UI actions (e.g. accessibility expand/collapse) to mirror that behavior.
  ///
  /// Returns `true` when the event's default was **not** prevented.
  pub fn dispatch_toggle_event(&mut self, node_id: NodeId) -> Result<bool> {
    let mut event = Event::new(
      "toggle",
      EventInit {
        bubbles: false,
        cancelable: false,
        composed: false,
      },
    );
    event.is_trusted = true;
    let (host, event_loop) = (&mut self.host, &mut self.event_loop);
    host.dispatch_dom_event_in_event_loop(
      EventTargetId::Node(node_id).normalize(),
      event,
      event_loop,
    )
  }

  /// Dispatch a trusted bubbling `input` DOM event to `node_id`.
  ///
  /// Returns `true` when the event's default was **not** prevented.
  pub fn dispatch_input_event(&mut self, node_id: NodeId) -> Result<bool> {
    let mut event = Event::new(
      "input",
      EventInit {
        bubbles: true,
        cancelable: false,
        composed: false,
      },
    );
    event.is_trusted = true;
    let (host, event_loop) = (&mut self.host, &mut self.event_loop);
    host.dispatch_dom_event_in_event_loop(
      EventTargetId::Node(node_id).normalize(),
      event,
      event_loop,
    )
  }

  /// Dispatch a trusted bubbling `change` DOM event to `node_id`.
  ///
  /// Returns `true` when the event's default was **not** prevented.
  pub fn dispatch_change_event(&mut self, node_id: NodeId) -> Result<bool> {
    let mut event = Event::new(
      "change",
      EventInit {
        bubbles: true,
        cancelable: false,
        composed: false,
      },
    );
    event.is_trusted = true;
    let (host, event_loop) = (&mut self.host, &mut self.event_loop);
    host.dispatch_dom_event_in_event_loop(
      EventTargetId::Node(node_id).normalize(),
      event,
      event_loop,
    )
  }

  /// Dispatch a trusted `focus` DOM event to `node_id`.
  ///
  /// Returns `true` when the event's default was **not** prevented.
  pub fn dispatch_focus_event(&mut self, node_id: NodeId) -> Result<bool> {
    let mut event = Event::new(
      "focus",
      EventInit {
        bubbles: false,
        cancelable: false,
        composed: false,
      },
    );
    event.is_trusted = true;
    let (host, event_loop) = (&mut self.host, &mut self.event_loop);
    host.dispatch_dom_event_in_event_loop(
      EventTargetId::Node(node_id).normalize(),
      event,
      event_loop,
    )
  }

  /// Dispatch a trusted `blur` DOM event to `node_id`.
  ///
  /// Returns `true` when the event's default was **not** prevented.
  pub fn dispatch_blur_event(&mut self, node_id: NodeId) -> Result<bool> {
    let mut event = Event::new(
      "blur",
      EventInit {
        bubbles: false,
        cancelable: false,
        composed: false,
      },
    );
    event.is_trusted = true;
    let (host, event_loop) = (&mut self.host, &mut self.event_loop);
    host.dispatch_dom_event_in_event_loop(
      EventTargetId::Node(node_id).normalize(),
      event,
      event_loop,
    )
  }

  /// Dispatch a trusted bubbling `focusin` DOM event to `node_id`.
  ///
  /// Returns `true` when the event's default was **not** prevented.
  pub fn dispatch_focusin_event(&mut self, node_id: NodeId) -> Result<bool> {
    let mut event = Event::new(
      "focusin",
      EventInit {
        bubbles: true,
        cancelable: false,
        composed: false,
      },
    );
    event.is_trusted = true;
    let (host, event_loop) = (&mut self.host, &mut self.event_loop);
    host.dispatch_dom_event_in_event_loop(
      EventTargetId::Node(node_id).normalize(),
      event,
      event_loop,
    )
  }

  /// Dispatch a trusted bubbling `focusout` DOM event to `node_id`.
  ///
  /// Returns `true` when the event's default was **not** prevented.
  pub fn dispatch_focusout_event(&mut self, node_id: NodeId) -> Result<bool> {
    let mut event = Event::new(
      "focusout",
      EventInit {
        bubbles: true,
        cancelable: false,
        composed: false,
      },
    );
    event.is_trusted = true;
    let (host, event_loop) = (&mut self.host, &mut self.event_loop);
    host.dispatch_dom_event_in_event_loop(
      EventTargetId::Node(node_id).normalize(),
      event,
      event_loop,
    )
  }

  /// Handle a selection-related accessibility action targeting `target`.
  ///
  /// This supports:
  /// - native HTML `<select>/<option>` controls (`<option>` targets), and
  /// - ARIA listbox widgets (`role="listbox"` with `role="option"` descendants).
  ///
  /// For native `<select>` controls, selection changes dispatch trusted bubbling `input` and
  /// `change` events on the owning `<select>` element (not the `<option>`).
  ///
  /// Returns `true` when `target` was recognized as a selectable item, even if the selection state
  /// did not change.
  pub fn perform_selection_action(&mut self, target: NodeId, action: SelectionAction) -> Result<bool> {
    let outcome: std::result::Result<(bool, Option<NodeId>, bool), crate::dom2::DomError> =
      self.host.mutate_dom(|dom| {
        let out = (|| -> std::result::Result<(bool, Option<NodeId>, bool), crate::dom2::DomError> {
          // --- Native HTML <option> -------------------------------------------------------------
          if let NodeKind::Element {
            tag_name,
            namespace,
            ..
          } = &dom.node(target).kind
          {
            if dom.is_html_case_insensitive_namespace(namespace) && tag_name.eq_ignore_ascii_case("option") {
              let select = dom
                .ancestors(target)
                .skip(1)
                .find(|&ancestor| match &dom.node(ancestor).kind {
                  NodeKind::Element {
                    tag_name,
                    namespace,
                    ..
                  } => dom.is_html_case_insensitive_namespace(namespace) && tag_name.eq_ignore_ascii_case("select"),
                  _ => false,
                });

              let desired_selected = !matches!(action, SelectionAction::RemoveFromSelection);

              if let Some(select) = select {
                let multiple = dom.has_attribute(select, "multiple")?;
                let options = dom.select_options(select);
                if let Some(target_idx) = options.iter().position(|&id| id == target) {
                  let before: Vec<bool> = options
                    .iter()
                    .map(|&opt| dom.option_selected(opt))
                    .collect::<std::result::Result<_, _>>()?;

                  let mut after = before.clone();
                  if multiple {
                    match action {
                      SelectionAction::SetSelection => {
                        after.fill(false);
                        after[target_idx] = true;
                      }
                      SelectionAction::AddToSelection => after[target_idx] = true,
                      SelectionAction::RemoveFromSelection => after[target_idx] = false,
                    }
                  } else {
                    match action {
                      SelectionAction::SetSelection | SelectionAction::AddToSelection => {
                        after.fill(false);
                        after[target_idx] = true;
                      }
                      SelectionAction::RemoveFromSelection => {
                        after[target_idx] = false;
                      }
                    }
                  }

                  let selection_changed = before != after;
                  if selection_changed {
                    if multiple {
                      match action {
                        SelectionAction::SetSelection => {
                          dom.set_option_selected(target, true)?;
                          // Clear any other selected options.
                          for (idx, &opt) in options.iter().enumerate() {
                            if idx == target_idx {
                              continue;
                            }
                            if before.get(idx).copied().unwrap_or(false) {
                              dom.set_option_selected(opt, false)?;
                            }
                          }
                        }
                        SelectionAction::AddToSelection => {
                          dom.set_option_selected(target, true)?;
                        }
                        SelectionAction::RemoveFromSelection => {
                          dom.set_option_selected(target, false)?;
                        }
                      }
                    } else {
                      // Single-select: `set_option_selected` enforces exclusive selection when an
                      // option becomes selected. Deselecting can leave no option selected; DOM
                      // select accessors normalize this state on read when needed.
                      dom.set_option_selected(target, desired_selected)?;
                    }
                  }

                  return Ok((true, Some(select), selection_changed));
                }
              }

              // Detached option (or a malformed tree): update selectedness without dispatching
              // events.
              let before = dom.option_selected(target)?;
              let selection_changed = before != desired_selected;
              if selection_changed {
                dom.set_option_selected(target, desired_selected)?;
              }
              return Ok((true, None, selection_changed));
            }
          }

          // --- ARIA listbox (role=listbox/option) ---------------------------------------------
          let role_value = match &dom.node(target).kind {
            NodeKind::Element { .. } | NodeKind::Slot { .. } => dom.get_attribute(target, "role")?.unwrap_or(""),
            _ => return Ok((false, None, false)),
          };
          let is_aria_option = role_value
            .split_ascii_whitespace()
            .any(|token| token.eq_ignore_ascii_case("option"));
          if !is_aria_option {
            return Ok((false, None, false));
          }

          let listbox = dom.ancestors(target).find(|&ancestor| {
            dom
              .get_attribute(ancestor, "role")
              .ok()
              .flatten()
              .is_some_and(|role| {
                role
                  .split_ascii_whitespace()
                  .any(|token| token.eq_ignore_ascii_case("listbox"))
              })
          });

          let multiselectable = listbox
            .and_then(|listbox| dom.get_attribute(listbox, "aria-multiselectable").ok().flatten())
            .is_some_and(|value| value.eq_ignore_ascii_case("true"));

          let desired_selected = !matches!(action, SelectionAction::RemoveFromSelection);
          let clear_others = desired_selected
            && (matches!(action, SelectionAction::SetSelection) || !multiselectable);

          let mut selection_changed = false;
          let currently_selected = dom
            .get_attribute(target, "aria-selected")?
            .is_some_and(|value| value.eq_ignore_ascii_case("true"));
          if desired_selected != currently_selected {
            dom.set_attribute(target, "aria-selected", if desired_selected { "true" } else { "false" })?;
            selection_changed = true;
          }

          if clear_others {
            if let Some(listbox) = listbox {
              // Collect first so we can mutate attributes while iterating.
              let nodes: Vec<NodeId> = dom.subtree_preorder(listbox).collect();
              for node in nodes {
                if node == target {
                  continue;
                }
                let Some(role) = dom.get_attribute(node, "role").ok().flatten() else {
                  continue;
                };
                let is_option = role
                  .split_ascii_whitespace()
                  .any(|token| token.eq_ignore_ascii_case("option"));
                if !is_option {
                  continue;
                }
                let other_selected = dom
                  .get_attribute(node, "aria-selected")
                  .ok()
                  .flatten()
                  .is_some_and(|value| value.eq_ignore_ascii_case("true"));
                if other_selected {
                  if dom.set_attribute(node, "aria-selected", "false")? {
                    selection_changed = true;
                  }
                }
              }
            }
          }

          Ok((true, None, selection_changed))
        })();

        let dom_changed = out.as_ref().is_ok_and(|(_, _, selection_changed)| *selection_changed);
        (out, dom_changed)
      });

    let (handled, select_for_events, selection_changed) = outcome
      .map_err(|err| Error::Other(err.to_string()))?;

    if selection_changed {
      if let Some(select) = select_for_events {
        // Mirror browser behavior: native select changes fire `input` then `change`.
        let _ = self.dispatch_input_event(select)?;
        let _ = self.dispatch_change_event(select)?;
      }
    }

    Ok(handled)
  }

  /// Route an AccessKit accessibility action to a DOM mutation/event.
  ///
  /// Currently this supports:
  /// - `Action::ShowContextMenu` → dispatch a trusted, cancelable `contextmenu` MouseEvent, and
  /// - disclosure-style `Expand`/`Collapse` semantics for:
  ///   - HTML `<details>` / `<summary>`, and
  ///   - elements with `aria-expanded`.
  ///
  /// Node IDs are expected to be renderer preorder IDs (`crate::dom::enumerate_dom_ids`) encoded as
  /// AccessKit `NodeId`s (see `BrowserDocumentDom2::dom2_node_for_renderer_preorder`).
  #[cfg(feature = "a11y_accesskit")]
  pub fn dispatch_accesskit_action(
    &mut self,
    target: AccessKitNodeId,
    action: AccessKitAction,
  ) -> Result<()> {
    let Some(target_node_id) = self.dom2_node_for_accesskit_node_id(target) else {
      return Ok(());
    };

    match action {
      // Assistive technologies can request a context menu without pointer input. Surface that as a
      // trusted DOM `contextmenu` event so JS (including renderer-chrome) can react exactly like a
      // right click.
      //
      // Coordinate selection:
      // - Prefer the center of the target node's border box in viewport CSS pixels.
      // - Fall back to (0,0) when bounds are unavailable (e.g. display:none / no layout yet).
      AccessKitAction::ShowContextMenu => {
        let bounds = self.host.document.border_box_rect_viewport(target_node_id)?;
        let pos = bounds
          .map(|rect| rect.center())
          .unwrap_or(crate::geometry::Point::ZERO);
        let _default_allowed = self.dispatch_contextmenu_event_with_pointer(
          target_node_id,
          (pos.x, pos.y),
          PointerModifiers::NONE,
        )?;
        return Ok(());
      }
      AccessKitAction::Expand | AccessKitAction::Collapse => {}
      _ => return Ok(()),
    }

    let expand = matches!(action, AccessKitAction::Expand);

    let (details_target, aria_expanded_target) = {
      let dom = self.dom();

      let is_html_element_tag = |id: NodeId, tag: &str| -> bool {
        let node = dom.node(id);
        match &node.kind {
          NodeKind::Element {
            tag_name,
            namespace,
            ..
          } => {
            tag_name.eq_ignore_ascii_case(tag)
              && (namespace.is_empty() || namespace == HTML_NAMESPACE)
          }
          _ => false,
        }
      };

      let details_target = if is_html_element_tag(target_node_id, "details") {
        Some(target_node_id)
      } else if is_html_element_tag(target_node_id, "summary") {
        match dom.parent_node(target_node_id) {
          Some(parent) if is_html_element_tag(parent, "details") => {
            // Only the first `<summary>` child participates in `<details>` toggling.
            let details_node = dom.node(parent);
            let mut first_summary: Option<NodeId> = None;
            for &child in &details_node.children {
              if is_html_element_tag(child, "summary") {
                first_summary = Some(child);
                break;
              }
            }
            if first_summary == Some(target_node_id) {
              Some(parent)
            } else {
              None
            }
          }
          _ => None,
        }
      } else {
        None
      };

      let aria_expanded_target = if details_target.is_some() {
        None
      } else if dom
        .get_attribute(target_node_id, "aria-expanded")
        .ok()
        .flatten()
        .is_some()
      {
        Some(target_node_id)
      } else {
        None
      };

      (details_target, aria_expanded_target)
    };

    if let Some(details_id) = details_target {
      let changed = self.host.mutate_dom(|dom| {
        let changed = dom
          .set_bool_attribute(details_id, "open", expand)
          .unwrap_or(false);
        (changed, changed)
      });

      // Optional but recommended: fire `toggle` when the open state changes.
      if changed {
        self.dispatch_toggle_event(details_id)?;
      }

      return Ok(());
    }

    if let Some(aria_id) = aria_expanded_target {
      let value = if expand { "true" } else { "false" };
      let _changed = self.host.mutate_dom(|dom| {
        // Do not create `aria-expanded` when absent.
        if dom.get_attribute(aria_id, "aria-expanded").ok().flatten().is_none() {
          return (false, false);
        }
        let changed = dom
          .set_attribute(aria_id, "aria-expanded", value)
          .unwrap_or(false);
        (changed, changed)
      });
    }

    Ok(())
  }

  /// Route an AccessKit [`ActionRequest`](accesskit::ActionRequest) into a DOM mutation/event.
  ///
  /// This is a convenience wrapper around the lower-level action helpers:
  /// - [`dispatch_accesskit_action`](Self::dispatch_accesskit_action) for actions without payloads
  ///   (e.g. `ShowContextMenu`, `Expand`, `Collapse`).
  /// - [`dispatch_set_value_action`](Self::dispatch_set_value_action) for `SetValue`.
  ///
  /// Returns `true` when the request was recognized and dispatched.
  #[cfg(feature = "a11y_accesskit")]
  pub fn dispatch_accesskit_action_request(
    &mut self,
    request: AccessKitActionRequest,
  ) -> Result<bool> {
    match request.action {
      AccessKitAction::SetValue => {
        let Some(AccessKitActionData::Value(value)) = request.data else {
          return Ok(false);
        };
        let Some(node_id) = self.dom2_node_for_accesskit_node_id(request.target) else {
          return Ok(false);
        };
        self.dispatch_set_value_action(node_id, &value)?;
        Ok(true)
      }
      AccessKitAction::ShowContextMenu | AccessKitAction::Expand | AccessKitAction::Collapse => {
        self.dispatch_accesskit_action(request.target, request.action)?;
        Ok(true)
      }
      _ => Ok(false),
    }
  }

  #[cfg(feature = "a11y_accesskit")]
  pub fn dom2_node_for_accesskit_node_id(&self, node_id: AccessKitNodeId) -> Option<NodeId> {
    let preorder_id = usize::try_from(node_id.0.get()).ok()?;
    self
      .host
      .document
      .dom2_node_for_renderer_preorder(preorder_id)
  }

  #[cfg(feature = "a11y_accesskit")]
  pub fn accesskit_node_id_for_dom2_node(&self, node_id: NodeId) -> Option<AccessKitNodeId> {
    let mapping = self.host.document.last_dom_mapping()?;
    let preorder = mapping.preorder_for_node_id(node_id)?;
    let raw = NonZeroU128::new(preorder as u128)?;
    Some(AccessKitNodeId(raw))
  }

  /// Decode an AccessKit node id and perform a `SetValue` action on the corresponding DOM node.
  #[cfg(feature = "a11y_accesskit")]
  pub fn dispatch_accesskit_set_value_action(
    &mut self,
    target: AccessKitNodeId,
    value: &str,
  ) -> Result<()> {
    let Some(node_id) = self.dom2_node_for_accesskit_node_id(target) else {
      return Ok(());
    };
    self.dispatch_set_value_action(node_id, value)
  }

  /// Perform an accessibility-driven "set value" action on a form control.
  ///
  /// This is intended for AccessKit-style integrations (e.g. screen readers setting the value of
  /// the renderer-chrome address bar).
  ///
  /// Behavior:
  /// 1. Mutates the DOM's internal form-control state (`HTMLInputElement.value`,
  ///    `HTMLTextAreaElement.value`).
  /// 2. Dispatches a trusted bubbling `input` event targeted at the control so JS observers can
  ///    react and read the new value from `event.target.value`.
  ///
  /// `change` is *not* dispatched here because for text controls browsers typically fire it on
  /// commit/blur rather than on every value update. Embeddings can choose to dispatch `change`
  /// separately when they implement those higher-level semantics.
  pub fn dispatch_set_value_action(&mut self, node_id: NodeId, value: &str) -> Result<()> {
    let changed = self.host.set_text_control_value(node_id, value)?;
    if changed {
      let _ = self.dispatch_input_event(node_id)?;
    }
    Ok(())
  }
  /// Dispatch a trusted DOM event at `target`.
  ///
  /// Returns `true` when the event's default was **not** prevented.
  pub fn dispatch_event(&mut self, target: EventTargetId, mut event: Event) -> Result<bool> {
    event.is_trusted = true;
    let (host, event_loop) = (&mut self.host, &mut self.event_loop);
    host.dispatch_dom_event_in_event_loop(target.normalize(), event, event_loop)
  }

  /// Dispatch a trusted DOM event on the window (`EventTargetId::Window`).
  ///
  /// Returns `true` when the event's default was **not** prevented.
  pub fn dispatch_window_event(&mut self, type_: &str, init: EventInit) -> Result<bool> {
    self.dispatch_event(EventTargetId::Window, Event::new(type_, init))
  }

  /// Dispatch a trusted DOM event on the document (`EventTargetId::Document`).
  ///
  /// Returns `true` when the event's default was **not** prevented.
  pub fn dispatch_document_event(&mut self, type_: &str, init: EventInit) -> Result<bool> {
    self.dispatch_event(EventTargetId::Document, Event::new(type_, init))
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
      value.trim_matches(|c: char| {
        matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' ')
      })
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
          if tag_name.eq_ignore_ascii_case("a")
            && (namespace.is_empty() || namespace == HTML_NAMESPACE)
          {
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
  pub fn dispatch_click_and_follow_link(
    &mut self,
    node_id: NodeId,
    options: RenderOptions,
  ) -> Result<bool> {
    let Some(url) = self.resolve_navigation_for_click(node_id)? else {
      return Ok(false);
    };
    self.navigate_to_url(&url, options)?;
    Ok(true)
  }

  /// Whether the JS event loop currently has no runnable work queued.
  ///
  /// This includes:
  /// - normal tasks,
  /// - microtasks,
  /// - externally queued tasks (via [`BrowserTab::external_task_queue_handle`]),
  /// - pending `requestIdleCallback` callbacks (which are dispatched as tasks when the loop is idle).
  ///
  /// Note: this does *not* consider:
  /// - future timers that are not yet due, or
  /// - pending `requestAnimationFrame` callbacks (rAF runs on the frame schedule, not as tasks).
  pub fn event_loop_is_idle(&self) -> bool {
    self.event_loop.is_idle()
  }

  pub fn has_pending_animation_frame_callbacks(&self) -> bool {
    self.event_loop.has_pending_animation_frame_callbacks()
  }

  pub fn has_pending_timers(&self) -> bool {
    self.event_loop.has_pending_timers()
  }

  pub fn next_timer_due_time(&mut self) -> Option<std::time::Duration> {
    self.event_loop.next_timer_due_time()
  }

  pub fn next_timer_due_in(&mut self) -> Option<std::time::Duration> {
    self.event_loop.next_timer_due_in()
  }

  pub fn now(&self) -> std::time::Duration {
    self.event_loop.now()
  }

  /// Drive the tab's HTML-like event loop until it becomes idle (or a run limit is hit).
  ///
  /// This drains:
  /// - queued tasks and their post-task microtask checkpoints,
  /// - queued microtasks,
  /// - timers that are already due at the current event-loop time (time is *not* advanced).
  /// - `requestIdleCallback` callbacks (dispatched as tasks when the event loop is otherwise idle).
  ///
  /// This intentionally does **not**:
  /// - render (call [`BrowserTab::render_if_needed`] / [`BrowserTab::render_frame`] yourself, or use
  ///   [`BrowserTab::tick_frame`] / [`BrowserTab::run_until_stable`]),
  /// - run `requestAnimationFrame` callbacks (rAF runs on the frame schedule, not as normal tasks).
  ///
  /// For the intended long-lived interactive embedding loop (`tick_frame` + `next_wake_time`), see
  /// [`docs/live_rendering_loop.md`](../../docs/live_rendering_loop.md).
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

  /// Returns a thread-safe handle for queueing tasks onto this tab's event loop from other threads.
  ///
  /// This is intended for integrations where external I/O (network callbacks, UI threads, etc.)
  /// needs to schedule work to run on the tab's live loop without holding `&mut BrowserTab`.
  ///
  /// If the embedding may sleep when the tab is idle, also install a wake callback via
  /// [`BrowserTab::set_external_wake_callback`] so externally queued tasks can wake the host.
  ///
  /// ## Lifetime / invalidation
  ///
  /// The returned handle is tied to the **currently active** event loop. Navigations reset the
  /// event loop (dropping the old one), so any previously acquired handles will become invalid and
  /// `queue_task` will start returning an error ("external task queue is closed").
  pub fn external_task_queue_handle(
    &self,
  ) -> crate::js::ExternalTaskQueueHandle<BrowserTabHost> {
    self.event_loop.external_task_queue_handle()
  }

  /// Install (or clear) the wake callback invoked when external tasks are queued onto this tab's
  /// event loop from other threads.
  ///
  /// This is a convenience wrapper around [`crate::js::EventLoop::set_external_wake_callback`].
  ///
  /// Embeddings that drive [`BrowserTab::tick_frame`] opportunistically (sleeping when
  /// [`BrowserTab::next_wake_time`] returns `None`) should install a wake callback so background
  /// threads (WebSocket, message channels, etc) can wake the host when new work is queued via
  /// [`BrowserTab::external_task_queue_handle`].
  pub fn set_external_wake_callback(&self, cb: Option<Arc<dyn Fn() + Send + Sync>>) {
    self.event_loop.set_external_wake_callback(cb);
  }

  /// Compatibility alias for [`Self::set_external_wake_callback`].
  pub fn set_external_task_waker(&self, waker: Option<Arc<dyn Fn() + Send + Sync>>) {
    self.set_external_wake_callback(waker);
  }

  /// Returns the clock used by this tab's event loop.
  ///
  /// This can be useful for deterministic embeddings (e.g. virtual-clock driven testing) that need
  /// to coordinate timer scheduling/driving with the tab's event loop.
  pub fn clock(&self) -> Arc<dyn crate::js::Clock> {
    self.event_loop.clock()
  }

  pub fn set_js_execution_options(&mut self, options: JsExecutionOptions) {
    let old_animation_frame_interval = self.host.js_execution_options.animation_frame_interval;
    self.host.js_execution_options = options;
    self.host.scheduler.set_options(options);
    self.host.document_write_state.update_limits(options);
    self
      .event_loop
      .set_queue_limits(options.event_loop_queue_limits);

    if old_animation_frame_interval != options.animation_frame_interval {
      let now = self.event_loop.now();
      // `next_animation_frame_due` is tracked as "last frame time + interval" when frames are being
      // driven via `tick_frame()`. When the embedder updates the interval, adjust the due time so
      // `next_wake_time()` reflects the new pacing immediately.
      //
      // If the stored due time is already in the past, schedule from "now" to avoid underflow and
      // to preserve the invariant that wake times are not in the past.
      self.next_animation_frame_due = if self.next_animation_frame_due > now {
        self
          .next_animation_frame_due
          .saturating_sub(old_animation_frame_interval)
          .saturating_add(options.animation_frame_interval)
      } else {
        now.saturating_add(options.animation_frame_interval)
      };
    }
  }

  /// Drive tasks + `requestAnimationFrame` + rendering until the tab reaches a quiescent state.
  ///
  /// This is a deterministic convergence helper intended for "load then screenshot" workflows and
  /// for tests. It does **not** sleep in real time; instead it repeats a bounded "frame" loop until
  /// no further work remains.
  ///
  /// Each iteration:
  ///
  /// 1. drains tasks/microtasks/timers/idle callbacks until idle (bounded by `RunLimits`),
  /// 2. runs one `requestAnimationFrame` turn (if callbacks are queued),
  /// 3. runs the microtask checkpoint after rAF callbacks,
  /// 4. renders if needed.
  ///
  /// The outer loop is bounded by `max_frames`; if callbacks keep re-queueing work the method will
  /// stop with [`RunUntilStableStopReason::MaxFrames`].
  ///
  /// For interactive/live use, prefer driving [`BrowserTab::tick_frame`] repeatedly and sleeping
  /// until the next wake-up time; see [`docs/live_rendering_loop.md`](../../docs/live_rendering_loop.md).
  pub fn run_until_stable(&mut self, max_frames: usize) -> Result<RunUntilStableOutcome> {
    self.run_until_stable_with_run_limits(
      self.host.js_execution_options.event_loop_run_limits,
      max_frames,
    )
  }

  pub fn run_until_stable_with_run_limits(
    &mut self,
    limits: RunLimits,
    max_frames: usize,
  ) -> Result<RunUntilStableOutcome> {
    let mut frames_rendered = 0usize;
    let _ = self.commit_pending_navigation()?;
    let raf_allowed = self.host.document.visibility_state() == DocumentVisibilityState::Visible;
    if !self.host.document.is_dirty()
      && !self.host.document.needs_animation_frame()
      && self.event_loop.is_idle()
      && (!raf_allowed || !self.event_loop.has_pending_animation_frame_callbacks())
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

      let raf_outcome = if self.host.document.visibility_state() == DocumentVisibilityState::Visible
      {
        self
          .event_loop
          .run_animation_frame_handling_errors(&mut self.host, &mut report_error)?
      } else {
        RunAnimationFrameOutcome::Idle
      };
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

      if self.host.document.is_dirty() || matches!(raf_outcome, RunAnimationFrameOutcome::Ran { .. })
      {
        self.sync_document_animation_time_to_event_loop();
      }
      if let Some(frame) = self.host.document.render_if_needed()? {
        self.pending_frame = Some(frame);
        frames_rendered = frames_rendered.saturating_add(1);
      }

      let raf_allowed = self.host.document.visibility_state() == DocumentVisibilityState::Visible;
      if !self.host.document.is_dirty()
        && self.event_loop.is_idle()
        && (!raf_allowed || !self.event_loop.has_pending_animation_frame_callbacks())
      {
        return Ok(RunUntilStableOutcome::Stable { frames_rendered });
      }
    }
  }

  /// Returns whether this tab should continue to be driven by a periodic "tick" loop.
  ///
  /// This is the single source of truth for embedders that want to stop driving a tab once it has
  /// no more work. A tab wants ticks when *any* of these are true:
  ///
  /// - The document has active CSS animations/transitions (so repainting over time is meaningful).
  /// - The document is dirty and needs a new frame rendered.
  /// - The JavaScript event loop has runnable work (tasks/microtasks/external tasks/idle callbacks).
  /// - The JavaScript event loop has any scheduled timers (even if none are due yet).
  /// - The JavaScript event loop has pending `requestAnimationFrame` callbacks **and the document is visible**.
  pub fn wants_ticks(&self) -> bool {
    let document_wants_ticks = self.host.document.prepared().is_some_and(|prepared| {
      let tree = prepared.fragment_tree();
      !tree.keyframes.is_empty() || tree.transition_state.is_some()
    });
    let raf_allowed = self.host.document.visibility_state() == DocumentVisibilityState::Visible;

    let raf_wants_ticks = raf_allowed && self.event_loop.has_pending_animation_frame_callbacks();

    document_wants_ticks
      || self.host.document.is_dirty()
      || !self.event_loop.is_idle()
      || self.event_loop.has_pending_timers()
      || raf_wants_ticks
  }

  /// Returns a scheduler hint for when the tab should be "ticked" next.
  ///
  /// This is intended for interactive embeddings that want to avoid ticking at a fixed cadence
  /// (e.g. ~60Hz) when the tab only has long-delay timers pending.
  ///
  /// This method is purely a query: it must **not** run any tasks.
  pub fn next_tick_due_in(&mut self) -> Option<Duration> {
    // If there is runnable work immediately (tasks/microtasks/external tasks/idle callbacks), or if
    // rendering is needed, request an immediate tick.
    if self.host.document.is_dirty() || !self.event_loop.is_idle() {
      return Some(Duration::ZERO);
    }

    let timer_due = self.event_loop.duration_until_next_timer();

    // Only request ~60Hz ticks when `requestAnimationFrame` callbacks are pending *and* the
    // document is visible. rAF callbacks are throttled/suppressed in hidden documents.
    let raf_due = (self.event_loop.has_pending_animation_frame_callbacks()
      && self.host.document.visibility_state() == DocumentVisibilityState::Visible)
      .then_some(RAF_TICK_CADENCE);

    match (timer_due, raf_due) {
      (Some(a), Some(b)) => Some(a.min(b)),
      (Some(a), None) => Some(a),
      (None, Some(b)) => Some(b),
      (None, None) => None,
    }
  }

  /// Execute at most one task turn (or a standalone microtask checkpoint) and return a freshly
  /// rendered frame when the document becomes dirty.
  ///
  /// This is the intended primitive for interactive/live embedding: call it repeatedly, present
  /// any returned `Pixmap`, and then sleep until the next wake-up time.
  ///
  /// - If microtasks are pending, this performs a "microtask checkpoint" only (no tasks).
  /// - Otherwise it runs exactly one task turn (one task + post-task microtask checkpoint).
  /// - It then commits any pending navigation and renders if needed.
  ///
  /// Returns `Some(Pixmap)` when a new frame was produced, or `None` when no rendering invalidation
  /// occurred.
  ///
  /// Note: `requestAnimationFrame` callbacks are queued separately from tasks/microtasks.
  /// `run_event_loop_until_idle` will never run rAF callbacks; `tick_frame` will run at most one rAF
  /// turn when callbacks are pending and the next animation frame is due (paced by
  /// [`JsExecutionOptions::animation_frame_interval`]). It drains the microtask checkpoint after rAF
  /// before rendering.
  ///
  /// `tick_frame` does not enforce a wall-clock frame cadence by itself; interactive embedders are
  /// expected to call it on their chosen frame schedule. See
  /// [`docs/live_rendering_loop.md`](../../docs/live_rendering_loop.md).
  pub fn tick_frame(&mut self) -> Result<Option<Pixmap>> {
    {
      // Ensure dynamically inserted scripts are discovered even if the event loop is currently
      // idle.
      let (host, event_loop) = (&mut self.host, &mut self.event_loop);
      host.discover_dynamic_scripts(event_loop)?;
    }
    let run_limits = self.host.js_execution_options.event_loop_run_limits;
    let trace = self.trace.clone();
    let diagnostics = self.diagnostics.clone();
    let mut report_error = move |err: Error| {
      let message = err.to_string();
      if let Some(diag) = &diagnostics {
        diag.record_js_exception(message.clone(), None);
      }
      if trace.is_enabled() {
        let mut span = trace.span("js.uncaught_exception", "js");
        span.arg_str("message", &message);
      }
    };
    if self.event_loop.pending_microtask_count() > 0 {
      // Drain microtasks only (HTML microtask checkpoint), but do not run any tasks.
      let microtask_limits = RunLimits {
        max_tasks: 0,
        max_microtasks: run_limits.max_microtasks,
        max_wall_time: run_limits.max_wall_time,
      };
      match self.event_loop.run_until_idle_handling_errors_with_hook(
        &mut self.host,
        microtask_limits,
        &mut report_error,
        |host, event_loop| {
          let executor_hook: fn(&mut BrowserTabHost, &mut EventLoop<BrowserTabHost>) -> Result<()> =
            BrowserTabHost::executor_microtask_checkpoint_hook;
          if !event_loop
            .microtask_checkpoint_hooks()
            .iter()
            .any(|&hook| std::ptr::fn_addr_eq(hook, executor_hook))
          {
            BrowserTabHost::executor_microtask_checkpoint_hook(host, event_loop)?;
          }
          host.discover_dynamic_scripts(event_loop)
        },
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
      match self.event_loop.run_until_idle_handling_errors_with_hook(
        &mut self.host,
        one_task_limits,
        &mut report_error,
        |host, event_loop| {
          let executor_hook: fn(&mut BrowserTabHost, &mut EventLoop<BrowserTabHost>) -> Result<()> =
            BrowserTabHost::executor_microtask_checkpoint_hook;
          if !event_loop
            .microtask_checkpoint_hooks()
            .iter()
            .any(|&hook| std::ptr::fn_addr_eq(hook, executor_hook))
          {
            BrowserTabHost::executor_microtask_checkpoint_hook(host, event_loop)?;
          }
          host.discover_dynamic_scripts(event_loop)
        },
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

    // `run_event_loop_until_idle` does not run requestAnimationFrame callbacks. When embeddings
    // drive a tab via `tick_frame()`, rAF callbacks would otherwise starve forever once the event
    // loop is idle.
    //
    // To avoid running rAF callbacks as fast as the embedder can call `tick_frame()`, gate rAF
    // execution to a per-tab "next frame due" time.
    let mut ran_animation_frame = false;
    if self.event_loop.has_pending_animation_frame_callbacks()
      && self.host.document.visibility_state() == DocumentVisibilityState::Visible
    {
      let now = self.event_loop.now();
      if now >= self.next_animation_frame_due {
        // Capture the frame time *before* running callbacks so long-running rAF work does not add
        // extra delay to the next frame (stable pacing).
        let frame_time = now;
        let raf_outcome =
          self
            .event_loop
            .run_animation_frame_handling_errors(&mut self.host, &mut report_error)?;
        ran_animation_frame = matches!(raf_outcome, RunAnimationFrameOutcome::Ran { .. });

        if ran_animation_frame {
          self.next_animation_frame_due = frame_time
            .saturating_add(self.host.js_execution_options.animation_frame_interval);

          // HTML: microtask checkpoint after rAF callbacks.
          //
          // Drain microtasks only: tasks/timers must wait for a future tick so rendering can happen
          // first.
          let microtask_limits = RunLimits {
            max_tasks: 0,
            max_microtasks: run_limits.max_microtasks,
            max_wall_time: run_limits.max_wall_time,
          };

          match self.event_loop.run_until_idle_handling_errors_with_hook(
            &mut self.host,
            microtask_limits,
            &mut report_error,
            |host, event_loop| {
              let executor_hook: fn(
                &mut BrowserTabHost,
                &mut EventLoop<BrowserTabHost>,
              ) -> Result<()> = BrowserTabHost::executor_microtask_checkpoint_hook;
              if !event_loop
                .microtask_checkpoint_hooks()
                .iter()
                .any(|&hook| std::ptr::fn_addr_eq(hook, executor_hook))
              {
                BrowserTabHost::executor_microtask_checkpoint_hook(host, event_loop)?;
              }
              host.discover_dynamic_scripts(event_loop)
            },
          )? {
            RunUntilIdleOutcome::Idle
            | RunUntilIdleOutcome::Stopped(RunUntilIdleStopReason::MaxTasks { .. }) => {
              // Expected: tasks may exist, but this checkpoint only drains microtasks.
            }
            RunUntilIdleOutcome::Stopped(reason) => {
              return Err(Error::Other(format!(
                "BrowserTab::tick_frame post-rAF microtask checkpoint stopped: {reason:?}"
              )))
            }
          }

          // Ensure scripts inserted by rAF callbacks are discovered even if there were no microtasks
          // to drain (meaning the microtask-only run can stop at `MaxTasks` without invoking hooks).
          let (host, event_loop) = (&mut self.host, &mut self.event_loop);
          host.discover_dynamic_scripts(event_loop)?;
        }

        if self.commit_pending_navigation()? {
          // Navigation can be requested by rAF callbacks or microtasks drained after the frame.
          // Render the new document if needed.
          return self.render_if_needed();
        }
      }
    }

    // Tie CSS animation sampling to the same event-loop clock used for `requestAnimationFrame`
    // timestamps.
    //
    // We only sync time when we're going to render anyway:
    // - the document is already dirty (DOM/style/layout changes),
    // - or we just ran an animation frame turn.
    //
    // This avoids making `tick_frame()` produce spurious frames when no work remains.
    if self.host.document.is_dirty() || ran_animation_frame {
      self.sync_document_animation_time_to_event_loop();
    }
    self.render_if_needed()
  }

  /// Run one animation frame turn (draining `requestAnimationFrame` callbacks queued before the
  /// frame starts) and render if needed.
  ///
  /// This ties the CSS animation sampling time to the same event-loop clock used for the rAF
  /// timestamp argument so JS-driven frame ticks advance visuals coherently.
  ///
  /// Like browsers, `requestAnimationFrame` callbacks are paused while `document.visibilityState` is
  /// `"hidden"`. Pending callbacks remain queued and will run once the document becomes visible
  /// again.
  pub fn tick_animation_frame(&mut self) -> Result<Option<Pixmap>> {
    let run_limits = self.host.js_execution_options.event_loop_run_limits;
    let trace = self.trace.clone();
    let diagnostics = self.diagnostics.clone();
    let mut report_error = move |err: Error| {
      let message = err.to_string();
      if let Some(diag) = &diagnostics {
        diag.record_js_exception(message.clone(), None);
      }
      if trace.is_enabled() {
        let mut span = trace.span("js.uncaught_exception", "js");
        span.arg_str("message", &message);
      }
    };

    if self.host.document.visibility_state() == DocumentVisibilityState::Visible {
      let raf_outcome = self
        .event_loop
        .run_animation_frame_handling_errors(&mut self.host, &mut report_error)?;
      if matches!(raf_outcome, RunAnimationFrameOutcome::Ran { .. }) {
        // HTML: microtask checkpoint after rAF callbacks.
        //
        // Drain microtasks only: tasks/timers must wait for a future tick so rendering can happen
        // first.
        let microtask_limits = RunLimits {
          max_tasks: 0,
          max_microtasks: run_limits.max_microtasks,
          max_wall_time: run_limits.max_wall_time,
        };
        match self.event_loop.run_until_idle_handling_errors_with_hook(
          &mut self.host,
          microtask_limits,
          &mut report_error,
          |host, event_loop| {
            let executor_hook: fn(
              &mut BrowserTabHost,
              &mut EventLoop<BrowserTabHost>,
            ) -> Result<()> = BrowserTabHost::executor_microtask_checkpoint_hook;
            if !event_loop
              .microtask_checkpoint_hooks()
              .iter()
              .any(|&hook| std::ptr::fn_addr_eq(hook, executor_hook))
            {
              BrowserTabHost::executor_microtask_checkpoint_hook(host, event_loop)?;
            }
            host.discover_dynamic_scripts(event_loop)
          },
        )? {
          RunUntilIdleOutcome::Idle
          | RunUntilIdleOutcome::Stopped(RunUntilIdleStopReason::MaxTasks { .. }) => {
            // Expected: tasks may exist, but this checkpoint only drains microtasks.
          }
          RunUntilIdleOutcome::Stopped(reason) => {
            return Err(Error::Other(format!(
              "BrowserTab::tick_animation_frame microtask checkpoint stopped: {reason:?}"
            )))
          }
        }

        // Ensure scripts inserted by rAF callbacks are discovered even if there were no microtasks to
        // drain (meaning the microtask-only run can stop at `MaxTasks` without invoking hooks).
        let (host, event_loop) = (&mut self.host, &mut self.event_loop);
        host.discover_dynamic_scripts(event_loop)?;
      }
    }

    if self.commit_pending_navigation()? {
      // Navigation resets the document/event loop; render the new document if needed.
      return self.render_if_needed();
    }

    self.sync_document_animation_time_to_event_loop();
    self.render_if_needed()
  }

  /// Returns the next time (in the event loop's clock domain) at which calling [`BrowserTab::tick_frame`]
  /// would make progress.
  ///
  /// - If rendering is needed (`render_if_needed()` would return `Some(_)`), this returns `Some(now)`.
  /// - If tasks/microtasks/idle callbacks are runnable now, this returns `Some(now)`.
  /// - If only timers are pending, this returns their next due time.
  /// - If `requestAnimationFrame` callbacks are pending and nothing else is runnable, this returns
  ///   `Some(max(now, next_animation_frame_due))` so embedders can sleep until the next frame is
  ///   eligible without introducing scheduling drift.
  /// - If the tab is fully idle, this returns `None`.
  pub fn next_wake_time(&mut self) -> Option<Duration> {
    let now = self.event_loop.now();

    // Rendering/navigation can make progress even when the JS event loop is otherwise idle.
    if self.pending_frame.is_some()
      || self.host.document.is_dirty()
      || self.host.document.needs_animation_frame()
      || self.host.pending_navigation.is_some()
    {
      return Some(now);
    }

    // Runnable work (tasks/microtasks/idle callbacks) should be processed immediately.
    if self.event_loop.pending_microtask_count() > 0 || !self.event_loop.is_idle() {
      return Some(now);
    }

    // Timers are not part of `EventLoop::is_idle()` until they become due and enqueue a task.
    let mut next = self.event_loop.next_timer_due_time().map(|due| due.max(now));

    if self.event_loop.has_pending_animation_frame_callbacks()
      && self.host.document.visibility_state() == DocumentVisibilityState::Visible
    {
      let raf_due = self.next_animation_frame_due.max(now);
      next = Some(match next {
        Some(existing) => existing.min(raf_due),
        None => raf_due,
      });
    }

    next
  }

  pub fn render_if_needed(&mut self) -> Result<Option<Pixmap>> {
    // When BrowserTab runs the JS event loop it may have already rendered between tasks to keep the
    // document stable (see `browser_tab_render_interleaving` tests). Those frames are buffered here
    // so callers can still pull the updated pixels via `render_if_needed()`.
    if self.host.document.is_dirty() || self.host.document.needs_animation_frame() {
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

  /// Updates (or clears) the runtime toggle override used by the underlying document.
  ///
  /// This mirrors [`super::BrowserDocument::set_runtime_toggles`]. In addition to invalidating the
  /// renderer pipeline, this updates the JS realm's `matchMedia()` environment so media queries like
  /// `(prefers-color-scheme)` observe the new values immediately.
  pub fn set_runtime_toggles(&mut self, toggles: Option<Arc<RuntimeToggles>>) {
    self.host.document.set_runtime_toggles(toggles);

    // Keep the JS `matchMedia()` surface consistent with the renderer's media context. The vm-js
    // matchMedia implementation evaluates queries against a host-owned `MediaContext`, so update it
    // when preferences change.
    let options = self.host.document.options().clone();
    let toggles = self
      .host
      .document
      .renderer_mut()
      .resolve_runtime_toggles(&options);

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

    let media = crate::debug::runtime::with_thread_runtime_toggles(toggles, || {
      media.with_env_overrides()
    });

    if let Some(realm) = self.host.executor.window_realm_mut() {
      let _ = realm.set_media_context(media);
    }
  }

  /// Applies a scroll wheel delta at a point in viewport coordinates.
  ///
  /// This mirrors [`BrowserDocumentDom2::wheel_scroll_at_viewport_point`] while exposing the helper
  /// on the higher-level tab API.
  pub fn wheel_scroll_at_viewport_point(
    &mut self,
    viewport_point_css: crate::geometry::Point,
    delta_css: (f32, f32),
  ) -> Result<bool> {
    // A scroll mutation invalidates any buffered frame.
    self.pending_frame = None;
    self
      .host
      .document
      .wheel_scroll_at_viewport_point(viewport_point_css, delta_css)
  }

  pub fn dom(&self) -> &Document {
    self.host.dom()
  }

  /// Translate a renderer 1-based pre-order id (as produced by [`crate::dom::enumerate_dom_ids`])
  /// back into a stable `dom2` [`NodeId`].
  ///
  /// This is intended for UI event dispatch: hit testing operates over the renderer DOM snapshot and
  /// returns renderer preorder ids. Those ids are **not** stable across DOM mutations and they do
  /// not correspond to `dom2` node indices (e.g. comment nodes are not rendered and `<wbr>` can
  /// synthesize extra nodes in the renderer snapshot).
  ///
  /// The returned `NodeId` is stable across `dom2` insertions/removals (unlike raw preorder/index
  /// ids), so it can be used to target DOM events in the JS worker.
  ///
  /// Returns `None` if the tab has not produced a renderer snapshot yet (call
  /// [`BrowserTab::render_frame`] / [`BrowserTab::render_if_needed`]) or if `preorder_id` is out of
  /// range.
  pub fn dom_node_for_renderer_preorder(&self, preorder_id: usize) -> Option<NodeId> {
    self.host.document.dom2_node_for_renderer_preorder(preorder_id)
  }

  /// Returns the mapping produced for the most recently prepared renderer DOM snapshot, if
  /// available.
  ///
  /// This is primarily used by UI integrations to translate renderer pre-order ids (from hit
  /// testing) back into stable `dom2::NodeId`s, even when the live DOM has since been mutated.
  pub fn last_dom_mapping(&self) -> Option<&crate::dom2::RendererDomMapping> {
    self.host.document.last_dom_mapping()
  }

  pub fn dom_mut(&mut self) -> &mut Document {
    self.host.dom_mut()
  }

  /// Perform a viewport-coordinate hit test against the current document layout.
  ///
  /// This is a convenience wrapper around [`BrowserDocumentDom2::hit_test_viewport_point`] that
  /// keeps the document animation timestamp synchronized with the tab's event loop clock.
  pub fn hit_test_viewport_point(&mut self, x: f32, y: f32) -> Result<Option<Dom2HitTestResult>> {
    self.sync_document_animation_time_to_event_loop();
    self.host.document.hit_test_viewport_point(x, y)
  }

  /// Like [`BrowserTab::hit_test_viewport_point`], but returns all hits (topmost first).
  pub fn hit_test_viewport_point_all(&mut self, x: f32, y: f32) -> Result<Vec<Dom2HitTestResult>> {
    self.sync_document_animation_time_to_event_loop();
    self.host.document.hit_test_viewport_point_all(x, y)
  }

  fn cached_renderer_dom_mapping(&mut self) -> &crate::dom2::RendererDomMapping {
    let generation = self.dom().mutation_generation();
    let rebuild = match self.renderer_dom_mapping_cache.as_ref() {
      Some(cache) => cache.generation != generation,
      None => true,
    };

    if rebuild {
      let mapping = self.dom().build_renderer_preorder_mapping();
      self.renderer_dom_mapping_cache = Some(RendererDomMappingCache { generation, mapping });
      #[cfg(test)]
      {
        BROWSER_TAB_RENDERER_DOM_MAPPING_BUILD_COUNT.with(|count| {
          count.set(count.get().saturating_add(1));
        });
      }
    }

    // If the cache was empty and mapping construction panicked, we'd have crashed already; keep the
    // unwrap for a compact return path.
    &self
      .renderer_dom_mapping_cache
      .as_ref()
      .expect("renderer dom mapping cache missing after rebuild") // fastrender-allow-unwrap
      .mapping
  }

  /// Translate a renderer/cascade 1-based preorder id (see `crate::dom::enumerate_dom_ids`) back to
  /// a stable `dom2` node id.
  ///
  /// This builds (and caches) a `dom2::RendererDomMapping` on first use, and reuses it until the
  /// underlying dom2 document's [`Document::mutation_generation`] changes.
  pub fn dom2_node_for_renderer_preorder(&mut self, preorder_id: usize) -> Option<crate::dom2::NodeId> {
    self
      .cached_renderer_dom_mapping()
      .node_id_for_preorder(preorder_id)
  }

  #[cfg(test)]
  pub fn renderer_dom_mapping_build_count_for_test() -> usize {
    BROWSER_TAB_RENDERER_DOM_MAPPING_BUILD_COUNT.with(|count| count.get())
  }

  #[cfg(test)]
  pub fn reset_renderer_dom_mapping_build_count_for_test() {
    BROWSER_TAB_RENDERER_DOM_MAPPING_BUILD_COUNT.with(|count| count.set(0));
  }

  /// Updates the viewport size in CSS px for the tab's live `dom2` document.
  ///
  /// This affects layout/geometry queries (`elementFromPoint`, `getBoundingClientRect`, etc) and
  /// media query evaluation, and marks layout+paint dirty.
  ///
  /// This is a lightweight state update used by UI integrations when the embedding window is
  /// resized; it does **not** trigger navigation or reload the document.
  pub fn set_viewport(&mut self, width: u32, height: u32) {
    // Any buffered frame is stale after a viewport change.
    self.pending_frame = None;
    self.host.document.set_viewport(width, height);
    self.sync_window_media_context_and_geometry();
  }
  /// Returns the current viewport size in CSS px, if explicitly set.
  pub fn viewport_size_css(&self) -> Option<(u32, u32)> {
    self.host.document.options().viewport
  }

  /// Updates the device pixel ratio used for media queries and resolution-dependent resources.
  ///
  /// Non-finite or non-positive values clear the override (falling back to the renderer default).
  /// Changing DPR invalidates layout+paint.
  ///
  /// This forwards to [`BrowserDocumentDom2::set_device_pixel_ratio`].
  ///
  /// This is a lightweight state update used by UI integrations when the system scale factor
  /// changes; it does **not** trigger navigation or reload the document.
  pub fn set_device_pixel_ratio(&mut self, dpr: f32) {
    self.pending_frame = None;
    self.host.document.set_device_pixel_ratio(dpr);
    self.sync_window_media_context_and_geometry();
  }

  fn sync_window_media_context_and_geometry(&mut self) {
    // Keep the stylesheet-evaluation media context aligned with the document options (viewport/DPR).
    //
    // This is used by streaming-parse `<link rel=stylesheet media=...>` logic, and is also a good
    // approximation of the window environment media context used by JS shims.
    let options_snapshot = self.host.document.options().clone();
    self.host.update_stylesheet_media_context(&options_snapshot);
    let media = self.host.stylesheet_media_context.clone();

    // Sync the vm-js window shims (matchMedia registry + viewport geometry values) when a realm is
    // present. Avoid panicking on VM errors: viewport updates should be best-effort.
    let env_id = match self.host.vm_host_and_window_realm() {
      Ok((_vm_host, window)) => {
        let _ = update_window_geometry_vm_js(window, &media);
        window.set_media_context(media)
      }
      Err(_) => None,
    };

    let Some(env_id) = env_id else {
      return;
    };

    if !crate::js::window_env::queue_match_media_mql_update(env_id) {
      return;
    }

    // Schedule `MediaQueryList` updates asynchronously so listeners run on the event loop (avoids
    // re-entrancy hazards).
    let _ = self.event_loop.queue_task(TaskSource::MediaQueryList, move |host_state, event_loop| {
      use crate::js::window_timers::VmJsEventLoopHooks;

      let mut hooks = VmJsEventLoopHooks::<BrowserTabHost>::new_with_host(host_state)?;
      hooks.set_event_loop(event_loop);

      let (vm_host, window) = host_state.vm_host_and_window_realm()?;
      let (vm, _realm, heap) = window.vm_realm_and_heap_mut();
      let vm_result = {
        let mut scope = heap.scope();
        crate::js::window_env::process_match_media_mql_update_for_env(
          vm,
          &mut scope,
          vm_host,
          &mut hooks,
          env_id,
        )
      };
      let result = vm_result.map_err(|err| crate::js::vm_error_format::vm_error_to_error(heap, err));

      // Ensure any queued Promise jobs are properly discarded even if dispatch fails.
      if let Some(err) = hooks.finish(heap) {
        return Err(err);
      }

      result
    });
  }

  /// Updates the full scroll state (viewport + element scroll offsets) used for hit testing.
  ///
  /// This is intended for UI integrations that maintain their own scroll offsets and need to keep
  /// `Document.elementFromPoint` and DOM geometry queries consistent with the embedding UI.
  pub fn set_scroll_state(&mut self, state: ScrollState) {
    // Scroll changes affect paint output; any buffered frame is now stale.
    self.pending_frame = None;
    self.host.document.set_scroll_state(state);
  }

  /// Returns the current scroll state used by this tab.
  pub fn scroll_state(&self) -> ScrollState {
    self.host.document.scroll_state()
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
  ) -> Result<HtmlScriptId> {
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
    self
      .host
      .apply_scheduler_actions(actions, &mut self.event_loop)?;
    self.host.notify_parsing_completed(&mut self.event_loop)?;
    Ok(())
  }

  fn discover_and_schedule_scripts(&mut self, document_url: Option<&str>) -> Result<()> {
    let discovered = self.host.discover_scripts_best_effort(document_url);
    for (node_id, spec) in discovered {
      let base_url_at_discovery = spec.base_url.clone();
      self.host.register_and_schedule_script(
        node_id,
        spec,
        base_url_at_discovery,
        &mut self.event_loop,
      )?;
      if self.host.pending_navigation.is_some() {
        return Ok(());
      }
    }

    self.on_parsing_completed()
  }
}

fn update_window_geometry_vm_js(
  window: &mut crate::js::WindowRealm,
  media: &MediaContext,
) -> std::result::Result<(), vm_js::VmError> {
  use vm_js::{PropertyDescriptor, PropertyKey, PropertyKind, Value, VmError};

  fn sanitize_f32_as_f64(value: f32, fallback: f64) -> f64 {
    if value.is_finite() {
      value as f64
    } else {
      fallback
    }
  }

  fn define_read_only_number(
    scope: &mut vm_js::Scope<'_>,
    obj: vm_js::GcObject,
    name: &str,
    value: f64,
  ) -> std::result::Result<(), VmError> {
    // Root `obj` while allocating the property key: `alloc_string` can trigger GC.
    let mut scope = scope.reborrow();
    scope.push_root(Value::Object(obj))?;
    let key_s = scope.alloc_string(name)?;
    scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);
    scope.define_property(
      obj,
      key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Data {
          value: Value::Number(value),
          writable: false,
        },
      },
    )
  }

  let viewport_width = sanitize_f32_as_f64(media.viewport_width, 0.0);
  let viewport_height = sanitize_f32_as_f64(media.viewport_height, 0.0);
  let dpr = sanitize_f32_as_f64(media.device_pixel_ratio, 1.0);

  let (_vm, realm, heap) = window.vm_realm_and_heap_mut();
  let global = realm.global_object();
  let mut scope = heap.scope();
  define_read_only_number(&mut scope, global, "devicePixelRatio", dpr)?;
  define_read_only_number(&mut scope, global, "innerWidth", viewport_width)?;
  define_read_only_number(&mut scope, global, "innerHeight", viewport_height)?;
  define_read_only_number(&mut scope, global, "outerWidth", viewport_width)?;
  define_read_only_number(&mut scope, global, "outerHeight", viewport_height)?;
  Ok(())
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
      tag_name,
      namespace,
      ..
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
      tag_name,
      namespace,
      ..
    } = &node.kind
    {
      next_in_foreign_namespace = in_foreign_namespace || !is_html_namespace(namespace);

      if !in_foreign_namespace
        && tag_name.eq_ignore_ascii_case("meta")
        && is_html_namespace(namespace)
      {
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
  use crate::api::VmJsBrowserTabExecutor;
  use crate::js::window_timers::{event_loop_mut_from_hooks, VmJsEventLoopHooks};
  use crate::js::{Clock, VirtualClock, WindowRealm, WindowRealmConfig, WindowRealmHost};
  use crate::VmJsBrowserTabExecutor;
  use crate::resource::{FetchedResource, ResourceFetcher};

  use std::cell::RefCell;
  use std::collections::HashMap;
  use std::rc::Rc;
  use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
  use std::sync::{Arc, Mutex, OnceLock};
  use std::time::Duration;

  use vm_js::{
    GcObject, PropertyDescriptor, PropertyKey, PropertyKind, Value, Vm, VmError, VmHost,
    VmHostHooks,
  };

  use tempfile::tempdir;
  use url::Url;

  use crate::web::events::{
    AddEventListenerOptions, DomError, Event, EventInit, EventListenerInvoker, EventTargetId,
    ListenerId,
  };
  use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
  use base64::Engine;
  use sha2::{Digest, Sha256};

  #[test]
  fn renderer_dom_mapping_cache_reuses_mapping_per_mutation_generation() -> Result<()> {
    let mut tab = BrowserTab::from_html_with_vmjs_executor(
      "<!doctype html><html><body><div></div></body></html>",
      RenderOptions::default(),
    )?;
    BrowserTab::reset_renderer_dom_mapping_build_count_for_test();

    assert!(
      tab.dom2_node_for_renderer_preorder(1).is_some(),
      "expected renderer preorder id 1 to map to the dom2 document root"
    );
    assert_eq!(
      BrowserTab::renderer_dom_mapping_build_count_for_test(),
      1,
      "expected the first mapping lookup to build the mapping once"
    );

    assert!(
      tab.dom2_node_for_renderer_preorder(1).is_some(),
      "expected repeated lookups to keep working"
    );
    assert_eq!(
      BrowserTab::renderer_dom_mapping_build_count_for_test(),
      1,
      "expected repeated lookups to reuse the cached mapping"
    );

    // Bump the mutation generation (conservative invalidation signal).
    let root = tab.dom().root();
    let _ = tab.dom_mut().node_mut(root);

    assert!(
      tab.dom2_node_for_renderer_preorder(1).is_some(),
      "expected mapping lookups to succeed after mutations"
    );
    assert_eq!(
      BrowserTab::renderer_dom_mapping_build_count_for_test(),
      2,
      "expected exactly one rebuild after mutation_generation changed"
    );

    assert!(tab.dom2_node_for_renderer_preorder(1).is_some());
    assert_eq!(
      BrowserTab::renderer_dom_mapping_build_count_for_test(),
      2,
      "expected repeated lookups in the new generation to reuse the rebuilt mapping"
    );

    Ok(())
  }

  struct RecordingInvoker {
    log: Rc<RefCell<Vec<String>>>,
  }

  impl EventListenerInvoker for RecordingInvoker {
    fn invoke(
      &mut self,
      _listener_id: ListenerId,
      event: &mut Event,
    ) -> std::result::Result<(), DomError> {
      assert!(
        event.is_trusted,
        "expected host-dispatched events to be trusted"
      );
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
      self.log.borrow_mut().push(format!("script:{script_text}"));
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
      _script_id: HtmlScriptId,
      script_text: &str,
      _spec: &ScriptElementSpec,
      _current_script: Option<NodeId>,
      _document: &mut BrowserDocumentDom2,
      event_loop: &mut EventLoop<BrowserTabHost>,
    ) -> Result<ModuleScriptExecutionStatus> {
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
      Ok(ModuleScriptExecutionStatus::Completed)
    }
  }

  #[derive(Clone)]
  struct AfterMicrotaskCheckpointCountingExecutor {
    calls: Rc<Cell<usize>>,
  }

  impl BrowserTabJsExecutor for AfterMicrotaskCheckpointCountingExecutor {
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
      _script_id: HtmlScriptId,
      _script_text: &str,
      _spec: &ScriptElementSpec,
      _current_script: Option<NodeId>,
      _document: &mut BrowserDocumentDom2,
      _event_loop: &mut EventLoop<BrowserTabHost>,
    ) -> Result<ModuleScriptExecutionStatus> {
      Ok(ModuleScriptExecutionStatus::Completed)
    }
    fn after_microtask_checkpoint(
      &mut self,
      _document: &mut BrowserDocumentDom2,
      _event_loop: &mut EventLoop<BrowserTabHost>,
    ) -> Result<()> {
      self.calls.set(self.calls.get().saturating_add(1));
      Ok(())
    }
  }

  struct CountingVmJsExecutor {
    inner: crate::api::VmJsBrowserTabExecutor,
    after_microtask_checkpoint_calls: Arc<AtomicUsize>,
  }

  impl CountingVmJsExecutor {
    fn new(after_microtask_checkpoint_calls: Arc<AtomicUsize>) -> Self {
      Self {
        inner: crate::api::VmJsBrowserTabExecutor::default(),
        after_microtask_checkpoint_calls,
      }
    }
  }

  impl BrowserTabJsExecutor for CountingVmJsExecutor {
    fn set_webidl_bindings_host(&mut self, host: &mut dyn webidl_vm_js::WebIdlBindingsHost) {
      self.inner.set_webidl_bindings_host(host);
    }

    fn event_listener_invoker(
      &self,
    ) -> Option<Box<dyn crate::web::events::EventListenerInvoker>> {
      self.inner.event_listener_invoker()
    }

    fn on_document_base_url_updated(&mut self, base_url: Option<&str>) {
      self.inner.on_document_base_url_updated(base_url);
    }

    fn on_navigation_committed(&mut self, document_url: Option<&str>) {
      self.inner.on_navigation_committed(document_url);
    }

    fn reset_for_navigation(
      &mut self,
      document_url: Option<&str>,
      document: &mut BrowserDocumentDom2,
      current_script: &CurrentScriptStateHandle,
      js_execution_options: JsExecutionOptions,
    ) -> Result<()> {
      self
        .inner
        .reset_for_navigation(document_url, document, current_script, js_execution_options)
    }

    fn execute_classic_script(
      &mut self,
      script_text: &str,
      spec: &ScriptElementSpec,
      current_script: Option<NodeId>,
      document: &mut BrowserDocumentDom2,
      event_loop: &mut EventLoop<BrowserTabHost>,
    ) -> Result<()> {
      self
        .inner
        .execute_classic_script(script_text, spec, current_script, document, event_loop)
    }

    fn execute_module_script(
      &mut self,
      script_id: HtmlScriptId,
      script_text: &str,
      spec: &ScriptElementSpec,
      current_script: Option<NodeId>,
      document: &mut BrowserDocumentDom2,
      event_loop: &mut EventLoop<BrowserTabHost>,
    ) -> Result<ModuleScriptExecutionStatus> {
      self
        .inner
        .execute_module_script(script_id, script_text, spec, current_script, document, event_loop)
    }

    fn supports_module_graph_fetch(&self) -> bool {
      self.inner.supports_module_graph_fetch()
    }

    fn fetch_module_graph(
      &mut self,
      spec: &ScriptElementSpec,
      fetcher: Arc<dyn ResourceFetcher>,
      document: &mut BrowserDocumentDom2,
      event_loop: &mut EventLoop<BrowserTabHost>,
    ) -> Result<()> {
      self
        .inner
        .fetch_module_graph(spec, fetcher, document, event_loop)
    }

    fn execute_import_map_script(
      &mut self,
      script_text: &str,
      spec: &ScriptElementSpec,
      current_script: Option<NodeId>,
      document: &mut BrowserDocumentDom2,
      event_loop: &mut EventLoop<BrowserTabHost>,
    ) -> Result<()> {
      self
        .inner
        .execute_import_map_script(script_text, spec, current_script, document, event_loop)
    }

    fn after_microtask_checkpoint(
      &mut self,
      document: &mut BrowserDocumentDom2,
      event_loop: &mut EventLoop<BrowserTabHost>,
    ) -> Result<()> {
      self
        .after_microtask_checkpoint_calls
        .fetch_add(1, Ordering::SeqCst);
      self.inner.after_microtask_checkpoint(document, event_loop)
    }

    fn take_navigation_request(&mut self) -> Option<LocationNavigationRequest> {
      self.inner.take_navigation_request()
    }

    fn dispatch_lifecycle_event(
      &mut self,
      target: EventTargetId,
      event: &Event,
      document: &mut BrowserDocumentDom2,
      event_loop: &mut EventLoop<BrowserTabHost>,
    ) -> Result<()> {
      self
        .inner
        .dispatch_lifecycle_event(target, event, document, event_loop)
    }

    fn window_realm_mut(&mut self) -> Option<&mut crate::js::WindowRealm> {
      self.inner.window_realm_mut()
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
    let mut event_loop = EventLoop::new();
    event_loop
      .register_microtask_checkpoint_hook(BrowserTabHost::executor_microtask_checkpoint_hook)?;
    Ok((host, event_loop))
  }

  fn sri_sha256_token(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let b64 = BASE64_STANDARD.encode(digest);
    format!("sha256-{b64}")
  }

  #[test]
  fn browser_tab_external_task_queue_handle_allows_cross_thread_dom_mutations() -> Result<()> {
    let html = "<!doctype html><html><body><div id=box></div></body></html>";
    let mut tab = BrowserTab::from_html_with_js_execution_options(
      html,
      RenderOptions::new().with_viewport(64, 64),
      TestExecutor {
        log: Rc::new(RefCell::new(Vec::new())),
      },
      JsExecutionOptions::default(),
    )?;

    // Drain any initial lifecycle work queued during parsing, then render a baseline frame so that
    // subsequent renders are attributable to the externally queued task.
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;
    let _ = tab.render_if_needed()?;
    assert!(
      tab.render_if_needed()?.is_none(),
      "expected tab to be clean after baseline render"
    );

    let box_id = tab
      .dom_mut()
      .query_selector("#box", None)
      .expect("query_selector")
      .expect("expected #box element");

    let handle = tab.external_task_queue_handle();
    let thread = std::thread::spawn(move || {
      handle
        .queue_task(TaskSource::DOMManipulation, move |host, _event_loop| {
          host.mutate_dom(|dom| {
            let changed = dom
              .set_attribute(
                box_id,
                "style",
                "width: 10px; height: 10px; background: #f00;",
              )
              .expect("set_attribute(style)");
            ((), changed)
          });
          Ok(())
        })
        .expect("queue_task");
    });
    thread.join().expect("external task thread join");

    // The task is queued externally but has not yet run.
    assert_eq!(
      tab.dom().get_attribute(box_id, "style").unwrap(),
      None,
      "expected style attribute to be unset before driving the event loop"
    );

    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    let style = tab.dom().get_attribute(box_id, "style").unwrap();
    assert_eq!(
      style,
      Some("width: 10px; height: 10px; background: #f00;")
    );

    // Style mutation should invalidate paint.
    assert!(
      tab.render_if_needed()?.is_some(),
      "expected external DOM mutation to produce a new frame"
    );
    Ok(())
  }

  #[test]
  fn browser_tab_external_task_queue_handle_is_invalidated_after_navigation() -> Result<()> {
    let html = "<!doctype html><html><body><div id=box></div></body></html>";
    let mut tab = BrowserTab::from_html_with_js_execution_options(
      html,
      RenderOptions::new().with_viewport(64, 64),
      TestExecutor {
        log: Rc::new(RefCell::new(Vec::new())),
      },
      JsExecutionOptions::default(),
    )?;

    let handle = tab.external_task_queue_handle();
    tab.navigate_to_html(
      "<!doctype html><html><body><div id=next></div></body></html>",
      RenderOptions::default(),
    )?;

    match handle.queue_task(TaskSource::DOMManipulation, |_host, _event_loop| Ok(())) {
      Ok(()) => {
        return Err(Error::Other(
          "expected external task handle to be closed after navigation".to_string(),
        ));
      }
      Err(Error::Other(msg)) => {
        assert!(
          msg.contains("closed"),
          "expected close error message; got {msg:?}"
        );
      }
      Err(err) => {
        return Err(Error::Other(format!(
          "expected close error after navigation, got {err}"
        )));
      }
    }

    Ok(())
  }

  #[test]
  fn vmjs_executor_supports_webidl_url_search_params() -> Result<()> {
    let mut tab = BrowserTab::from_html_with_vmjs_executor("", RenderOptions::default())?;

    // Install generated vm-js bindings so URLSearchParams dispatches through
    // `webidl_vm_js::host_from_hooks()`.
    {
      let BrowserTab { host, .. } = &mut tab;
      let Some(realm) = host.executor.window_realm_mut() else {
        return Err(Error::Other(
          "expected vm-js WindowRealm to be active".to_string(),
        ));
      };
      let (vm, realm_ref, heap) = realm.vm_realm_and_heap_mut();
      // WindowRealm installs handcrafted URLSearchParams by default; delete it first so the
      // generated bindings are installed and dispatch through the WebIDL host slot.
      {
        let mut scope = heap.scope();
        let global = realm_ref.global_object();
        scope
          .push_root(Value::Object(global))
          .expect("push root global");
        let key_s = scope
          .alloc_string("URLSearchParams")
          .expect("alloc URLSearchParams key");
        scope
          .push_root(Value::String(key_s))
          .expect("push root URLSearchParams key");
        let key = PropertyKey::from_string(key_s);
        scope
          .delete_property_or_throw(global, key)
          .map_err(|err| Error::Other(err.to_string()))?;
      }
      crate::js::bindings::install_url_search_params_bindings_vm_js(vm, heap, realm_ref)
        .map_err(|err| Error::Other(err.to_string()))?;
    }

    {
      let BrowserTab {
        host, event_loop, ..
      } = &mut tab;
      let spec = ScriptElementSpec {
        base_url: host.base_url.clone(),
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
        parser_inserted: false,
        node_id: None,
        script_type: ScriptType::Classic,
      };
      host.executor.execute_classic_script(
        "globalThis.__got = new URLSearchParams('a=1').get('a');",
        &spec,
        None,
        host.document.as_mut(),
        event_loop,
      )?;
    }

    let got = {
      let BrowserTab { host, .. } = &mut tab;
      let Some(realm) = host.executor.window_realm_mut() else {
        return Err(Error::Other(
          "expected vm-js WindowRealm to be active".to_string(),
        ));
      };
      let (_vm, realm_ref, heap) = realm.vm_realm_and_heap_mut();
      let mut scope = heap.scope();
      let global = realm_ref.global_object();
      scope
        .push_root(Value::Object(global))
        .expect("push root global");
      let key_s = scope.alloc_string("__got").expect("alloc __got");
      scope
        .push_root(Value::String(key_s))
        .expect("push root __got key");
      let key = PropertyKey::from_string(key_s);
      let value = scope
        .heap()
        .object_get_own_data_property_value(global, &key)
        .expect("get __got")
        .unwrap_or(Value::Undefined);
      let Value::String(s) = value else {
        return Err(Error::Other(format!(
          "expected __got to be a string, got {value:?}"
        )));
      };
      scope
        .heap()
        .get_string(s)
        .expect("get string")
        .to_utf8_lossy()
    };

    assert_eq!(got, "1");
    Ok(())
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
  fn dynamic_script_discovery_skips_full_dom_scan_with_vmjs_insertion_steps() -> Result<()> {
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
    }
 
    let script_url = "https://example.invalid/dyn.js";
    let mut entries: HashMap<String, FetchedResource> = HashMap::new();
    entries.insert(
      script_url.to_string(),
      FetchedResource::new(
        br#"document.documentElement.setAttribute('data-dyn', '1');"#.to_vec(),
        Some("text/javascript".to_string()),
      ),
    );
 
    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(MapFetcher {
      entries: Arc::new(entries),
    });
 
    let mut body = String::new();
    // Keep this large enough to make O(N) scans noticeable without slowing the test suite down.
    for _ in 0..5000 {
      body.push_str("<div></div>");
    }
 
    let html = format!(
      r#"<!doctype html><html><head></head><body>{body}<script>
        const s = document.createElement('script');
        s.src = {script_url:?};
        document.head.appendChild(s);
      </script></body></html>"#
    );
 
    let mut tab = BrowserTab::from_html_with_vmjs_and_document_url_and_fetcher(
      &html,
      "https://example.invalid/index.html",
      RenderOptions::default(),
      fetcher,
    )?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;
 
    let dom = tab.dom();
    let document_element = dom.document_element().expect("documentElement should exist");
    assert_eq!(
      dom.get_attribute(document_element, "data-dyn")
        .expect("get_attribute should succeed"),
      Some("1"),
      "expected dynamically inserted script to execute",
    );
 
    assert_eq!(
      tab.host.dynamic_script_full_scan_count(),
      0,
      "expected vm-js insertion steps to bypass full dynamic script discovery scans",
    );
 
    Ok(())
  }

  #[test]
  fn dynamic_script_prepare_does_not_bump_dom_mutation_generation_for_script_already_started(
  ) -> Result<()> {
    let log = Rc::new(RefCell::new(Vec::new()));
    let (mut host, mut event_loop) = build_host("<!doctype html><html><body></body></html>", log)?;
    host.document.set_viewport(1, 1);

    let script = host.mutate_dom(|dom| {
      let script = dom.create_element("script", "");
      dom
        .set_attribute(script, "src", "https://example.com/dyn.js")
        .expect("set_attribute");
      let body = dom.body().expect("expected a <body> element");
      dom.append_child(body, script).expect("append_child");
      (script, true)
    });

    // Render once so the dom2-backed renderer has an up-to-date `last_seen_dom_mutation_generation`.
    host.document.render_frame()?;
    assert!(
      !host.document.is_dirty(),
      "expected document to be clean after first render"
    );
    assert!(host.document.render_if_needed()?.is_none());

    assert!(
      !host.dom().node(script).script_already_started,
      "expected dynamic script to start with script_already_started=false"
    );

    let generation_before = host.document.dom_mutation_generation();

    // Build a spec that matches the inserted dynamic script but avoids any script execution (the
    // fetch task is queued and not run in this test).
    let spec = ScriptElementSpec {
      base_url: host.base_url.clone(),
      src: Some("https://example.com/dyn.js".to_string()),
      src_attr_present: true,
      inline_text: String::new(),
      async_attr: false,
      force_async: host.dom().node(script).script_force_async,
      defer_attr: false,
      nomodule_attr: false,
      crossorigin: None,
      integrity_attr_present: false,
      integrity: None,
      referrer_policy: None,
      fetch_priority: None,
      parser_inserted: false,
      node_id: Some(script),
      script_type: ScriptType::Classic,
    };

    host.register_and_schedule_dynamic_script(
      script,
      spec.clone(),
      spec.base_url.clone(),
      &mut event_loop,
    )?;

    let generation_after = host.document.dom_mutation_generation();
    assert_eq!(
      generation_after, generation_before,
      "script_already_started internal slot updates should not bump dom2::Document::mutation_generation"
    );
    assert!(
      host.dom().node(script).script_already_started,
      "expected dynamic script preparation to mark script_already_started=true"
    );

    // Ensure the dom2-backed renderer does not treat the internal-slot update as a real DOM change.
    assert!(
      !host.document.is_dirty(),
      "expected document to remain clean after script_already_started update"
    );
    assert!(host.document.render_if_needed()?.is_none());
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
        Err(Error::Other(
          "unexpected call to ResourceFetcher::fetch".to_string(),
        ))
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
        let mut res = FetchedResource::new(
          body.as_bytes().to_vec(),
          Some("application/javascript".to_string()),
        );
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
        (
          "https://example.com/a.js".to_string(),
          FetchDestination::Script
        ),
        (
          "https://example.com/b.js".to_string(),
          FetchDestination::ScriptCors
        ),
        (
          "https://example.com/m.js".to_string(),
          FetchDestination::ScriptCors
        ),
      ]
    );
    Ok(())
  }

  #[test]
  fn module_script_execution_task_source_is_script() -> Result<()> {
    struct TaskSourceRecordingExecutor {
      observed: Rc<Cell<Option<TaskSource>>>,
    }

    impl BrowserTabJsExecutor for TaskSourceRecordingExecutor {
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
        _script_id: HtmlScriptId,
        _script_text: &str,
        _spec: &ScriptElementSpec,
        _current_script: Option<NodeId>,
        _document: &mut BrowserDocumentDom2,
        event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<ModuleScriptExecutionStatus> {
        let running = event_loop
          .currently_running_task()
          .expect("module script should execute within an event-loop task");
        self.observed.set(Some(running.source));
        Ok(ModuleScriptExecutionStatus::Completed)
      }
    }

    let observed = Rc::new(Cell::new(None));
    let mut js_options = JsExecutionOptions::default();
    js_options.supports_module_scripts = true;

    let document = BrowserDocumentDom2::from_html(
      r#"<!doctype html>
      <script type="module" async>
        // Ensure the body is non-empty so the scheduler actually runs module execution.
        export const answer = 42;
      </script>"#,
      RenderOptions::default(),
    )?;

    let mut host = BrowserTabHost::new(
      document,
      Box::new(TaskSourceRecordingExecutor {
        observed: Rc::clone(&observed),
      }),
      TraceHandle::default(),
      js_options,
    )?;
    host.reset_scripting_state(None, ReferrerPolicy::default())?;

    let mut event_loop = EventLoop::new();
    event_loop
      .register_microtask_checkpoint_hook(BrowserTabHost::executor_microtask_checkpoint_hook)?;

    let discovered = host.discover_scripts_best_effort(None);
    assert_eq!(discovered.len(), 1, "expected one module <script> element");
    for (node_id, spec) in discovered {
      let base_url_at_discovery = spec.base_url.clone();
      host.register_and_schedule_script(node_id, spec, base_url_at_discovery, &mut event_loop)?;
    }

    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    assert_eq!(
      observed.get(),
      Some(TaskSource::Script),
      "module execution tasks should run on the Script task source"
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

      fn fetch_with_request(
        &self,
        req: crate::resource::FetchRequest<'_>,
      ) -> Result<FetchedResource> {
        self.fetch(req.url)
      }
    }

    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(FailingFetcher);
    let renderer = crate::FastRender::builder()
      .dom_scripting_enabled(true)
      .fetcher(fetcher)
      .build()?;
    let document = BrowserDocumentDom2::new(
      renderer,
      r#"<!doctype html>
        <div id="marker"></div>
        <script type="module" src="https://example.com/m.js"></script>
        <script>
          const marker = document.getElementById('marker');
          const mod = document.querySelector('script[type="module"]');
          mod.addEventListener('error', () => {
            marker.setAttribute('data-state', 'listener');
            queueMicrotask(() => { marker.setAttribute('data-state', 'microtask'); });
          });
        </script>"#,
      RenderOptions::default(),
    )?;

    let mut js_options = JsExecutionOptions::default();
    js_options.supports_module_scripts = true;

    let mut host = BrowserTabHost::new(
      document,
      Box::new(crate::api::VmJsBrowserTabExecutor::default()),
      TraceHandle::default(),
      js_options,
    )?;
    host.reset_scripting_state(
      Some("https://example.com/doc.html".to_string()),
      ReferrerPolicy::default(),
    )?;
    let mut event_loop = EventLoop::new();

    let mut discovered = host.discover_scripts_best_effort(Some("https://example.com/doc.html"));
    assert_eq!(discovered.len(), 2);
    // Ensure the classic script that registers the error handler runs before the module script
    // fetch fails.
    discovered.sort_by_key(|(_, spec)| matches!(spec.script_type, ScriptType::Module));
    for (node_id, spec) in discovered.drain(..) {
      let base_url_at_discovery = spec.base_url.clone();
      host.register_and_schedule_script(node_id, spec, base_url_at_discovery, &mut event_loop)?;
    }
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    let marker = host
      .dom()
      .get_element_by_id("marker")
      .expect("expected marker element to exist");
    assert_eq!(
      host.dom().get_attribute(marker, "data-state").expect("get data-state"),
      Some("microtask"),
      "expected module script error event listener to run with an active EventLoop (so queueMicrotask works)"
    );
    Ok(())
  }

  #[test]
  fn deferred_module_scripts_with_top_level_await_execute_in_order_and_block_lifecycle_events(
  ) -> Result<()> {
    use crate::clock::VirtualClock;
    use std::time::Duration;

    let html = r#"<!doctype html>
      <div id="marker"></div>
      <script>
        const marker = document.getElementById('marker');
        marker.setAttribute('data-log', '');
        document.addEventListener('DOMContentLoaded', () => {
          marker.setAttribute('data-log', marker.getAttribute('data-log') + 'dcl,');
        });
        window.addEventListener('load', () => {
          marker.setAttribute('data-log', marker.getAttribute('data-log') + 'load,');
        });
      </script>
      <script type="module">
        const marker = document.getElementById('marker');
        marker.setAttribute('data-log', marker.getAttribute('data-log') + 'm1-start,');
        await new Promise(r => setTimeout(r, 10));
        marker.setAttribute('data-log', marker.getAttribute('data-log') + 'm1-end,');
      </script>
      <script type="module">
        const marker = document.getElementById('marker');
        marker.setAttribute('data-log', marker.getAttribute('data-log') + 'm2,');
      </script>"#;

    let mut js_options = JsExecutionOptions::default();
    js_options.supports_module_scripts = true;

    let clock = Arc::new(VirtualClock::new());
    let event_loop = EventLoop::with_clock(clock.clone());

    let mut tab = BrowserTab::from_html_with_event_loop_and_js_execution_options(
      html,
      RenderOptions::default(),
      crate::api::VmJsBrowserTabExecutor::default(),
      event_loop,
      js_options,
    )?;

    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    let marker = tab
      .host
      .dom()
      .get_element_by_id("marker")
      .expect("expected #marker element to exist");
    let log = tab
      .host
      .dom()
      .get_attribute(marker, "data-log")
      .expect("get data-log")
      .unwrap_or("")
      .to_string();

    assert_eq!(
      log,
      "m1-start,",
      "expected module 1 to start and then block on top-level await"
    );
    assert!(
      !log.contains("m2,"),
      "expected module 2 to remain blocked on module 1 evaluation"
    );
    assert!(
      !log.contains("dcl,"),
      "expected DOMContentLoaded to wait for deferred module scripts"
    );
    assert!(
      !log.contains("load,"),
      "expected load to wait for module evaluation to complete"
    );

    clock.advance(Duration::from_millis(20));
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    let log = tab
      .host
      .dom()
      .get_attribute(marker, "data-log")
      .expect("get data-log")
      .unwrap_or("")
      .to_string();

    assert_eq!(log, "m1-start,m1-end,m2,dcl,load,");
    Ok(())
  }

  fn build_tab_for_next_tick_due_in_tests(
    clock: Arc<crate::js::clock::VirtualClock>,
  ) -> Result<BrowserTab> {
    let event_loop = EventLoop::with_clock(clock);
    let mut tab = BrowserTab::from_html_with_event_loop_and_js_execution_options(
      "<!doctype html><html></html>",
      RenderOptions::default(),
      NoopExecutor::default(),
      event_loop,
      JsExecutionOptions::default(),
    )?;
    // Drain lifecycle tasks scheduled during construction so tests start from a fully idle tab.
    tab.event_loop.clear_all_pending_work();
    // Clear the initial render dirtiness so `next_tick_due_in` reflects scheduler state rather than
    // the first-paint requirement.
    let _ = tab.render_if_needed()?;
    Ok(tab)
  }

  #[test]
  fn next_tick_due_in_prefers_timer_over_pending_raf() -> Result<()> {
    use crate::js::clock::VirtualClock;
    use std::time::Duration;

    let clock = Arc::new(VirtualClock::new());
    let mut tab = build_tab_for_next_tick_due_in_tests(clock)?;

    tab
      .event_loop
      .request_animation_frame(|_, _, _| Ok(()))
      .expect("requestAnimationFrame should succeed");
    tab
      .event_loop
      .set_timeout(Duration::from_millis(5), |_, _| Ok(()))
      .expect("setTimeout should succeed");

    assert_eq!(tab.next_tick_due_in(), Some(Duration::from_millis(5)));
    Ok(())
  }

  #[test]
  fn next_tick_due_in_returns_raf_cadence_for_pending_raf() -> Result<()> {
    use crate::js::clock::VirtualClock;

    let clock = Arc::new(VirtualClock::new());
    let mut tab = build_tab_for_next_tick_due_in_tests(clock)?;

    tab
      .event_loop
      .request_animation_frame(|_, _, _| Ok(()))
      .expect("requestAnimationFrame should succeed");

    assert_eq!(
      tab.next_tick_due_in(),
      Some(RAF_TICK_CADENCE)
    );
    Ok(())
  }

  #[test]
  fn next_tick_due_in_ignores_raf_when_hidden() -> Result<()> {
    use crate::js::clock::VirtualClock;

    let clock = Arc::new(VirtualClock::new());
    let mut tab = build_tab_for_next_tick_due_in_tests(clock)?;

    tab
      .event_loop
      .request_animation_frame(|_, _, _| Ok(()))
      .expect("requestAnimationFrame should succeed");
    tab
      .host
      .document
      .set_visibility_state(DocumentVisibilityState::Hidden);

    assert_eq!(tab.next_tick_due_in(), None);
    Ok(())
  }

  #[test]
  fn next_tick_due_in_returns_zero_for_due_timer_even_with_raf() -> Result<()> {
    use crate::js::clock::VirtualClock;
    use std::time::Duration;

    let clock = Arc::new(VirtualClock::new());
    let mut tab = build_tab_for_next_tick_due_in_tests(clock)?;

    tab
      .event_loop
      .request_animation_frame(|_, _, _| Ok(()))
      .expect("requestAnimationFrame should succeed");
    tab
      .event_loop
      .set_timeout(Duration::from_millis(0), |_, _| Ok(()))
      .expect("setTimeout should succeed");

    assert_eq!(tab.next_tick_due_in(), Some(Duration::ZERO));
    Ok(())
  }

  #[test]
  fn browser_tab_exposes_event_loop_timer_query_methods() -> Result<()> {
    use crate::js::clock::VirtualClock;
    use std::time::Duration;

    let clock = Arc::new(VirtualClock::new());
    let event_loop = EventLoop::with_clock(clock.clone());

    let mut tab = BrowserTab::from_html_with_event_loop_and_js_execution_options(
      "",
      RenderOptions::default(),
      crate::api::VmJsBrowserTabExecutor::default(),
      event_loop,
      JsExecutionOptions::default(),
    )?;

    let delay = Duration::from_millis(25);
    let id = tab
      .event_loop
      .set_timeout(delay, |_host, _event_loop| Ok(()))?;

    assert!(tab.has_pending_timers());
    assert_eq!(tab.next_timer_due_in(), Some(delay));

    tab.event_loop.clear_timeout(id);

    assert!(!tab.has_pending_timers());
    assert_eq!(tab.next_timer_due_in(), None);

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
        _script_id: HtmlScriptId,
        script_text: &str,
        spec: &ScriptElementSpec,
        current_script: Option<NodeId>,
        document: &mut BrowserDocumentDom2,
        event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<ModuleScriptExecutionStatus> {
        self.execute_classic_script(script_text, spec, current_script, document, event_loop)?;
        Ok(ModuleScriptExecutionStatus::Completed)
      }
    }

    let log: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let document = BrowserDocumentDom2::from_html(
      "<!doctype html><html><body></body></html>",
      RenderOptions::default(),
    )?;
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
      (
        id_a.expect("missing script_id for a.js"),
        id_b.expect("missing script_id for b.js"),
      )
    };

    // Complete fetch for B first. When `force_async=true` is plumbed through, scripts behave like
    // async scripts and execute in completion order (B then A). If `force_async` is ignored, the
    // scheduler treats them as in-order-asap scripts and would execute A before B.
    let actions_b = host
      .scheduler
      .classic_fetch_completed(id_b, "B".to_string())
      .expect("classic_fetch_completed for B");
    host.apply_scheduler_actions(actions_b, &mut event_loop)?;
    let actions_a = host
      .scheduler
      .classic_fetch_completed(id_a, "A".to_string())
      .expect("classic_fetch_completed for A");
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
      let map = self
        .sources
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
      let bytes = map.get(url).cloned().ok_or_else(|| {
        Error::Other(format!(
          "ScriptSourceFetcher has no source registered for url={url}"
        ))
      })?;
      Ok(crate::resource::FetchedResource::new(
        bytes,
        Some("application/javascript".to_string()),
      ))
    }

    fn fetch_with_request(
      &self,
      req: crate::resource::FetchRequest<'_>,
    ) -> Result<crate::resource::FetchedResource> {
      self.fetch(req.url)
    }
  }

  #[derive(Default)]
  struct RecordingScriptFetcher {
    sources: Mutex<HashMap<String, Vec<u8>>>,
    calls: Mutex<Vec<String>>,
  }

  impl RecordingScriptFetcher {
    fn new(sources: &[(&str, &str)]) -> Self {
      let mut map = HashMap::new();
      for (url, source) in sources {
        map.insert((*url).to_string(), (*source).as_bytes().to_vec());
      }
      Self {
        sources: Mutex::new(map),
        calls: Mutex::new(Vec::new()),
      }
    }

    fn calls(&self) -> Vec<String> {
      self
        .calls
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
    }
  }

  impl crate::resource::ResourceFetcher for RecordingScriptFetcher {
    fn fetch(&self, url: &str) -> Result<crate::resource::FetchedResource> {
      self.fetch_with_request(crate::resource::FetchRequest::new(
        url,
        crate::resource::FetchDestination::Other,
      ))
    }

    fn fetch_with_request(
      &self,
      req: crate::resource::FetchRequest<'_>,
    ) -> Result<crate::resource::FetchedResource> {
      self
        .calls
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(req.url.to_string());
      let bytes = self
        .sources
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(req.url)
        .cloned()
        .ok_or_else(|| Error::Other(format!("RecordingScriptFetcher has no source for url={}", req.url)))?;
      let mut res = crate::resource::FetchedResource::new(
        bytes,
        Some("application/javascript".to_string()),
      );
      // Mirror HTTP fetches so downstream validations (status/CORS) remain deterministic.
      res.status = Some(200);
      res.final_url = Some(req.url.to_string());
      res.access_control_allow_origin = Some("*".to_string());
      res.access_control_allow_credentials = true;
      Ok(res)
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
    let mut event_loop = EventLoop::new();
    event_loop
      .register_microtask_checkpoint_hook(BrowserTabHost::executor_microtask_checkpoint_hook)?;
    Ok((host, event_loop))
  }

  fn build_host(
    html: &str,
    log: Rc<RefCell<Vec<String>>>,
  ) -> Result<(BrowserTabHost, EventLoop<BrowserTabHost>)> {
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
      _script_id: HtmlScriptId,
      _script_text: &str,
      _spec: &ScriptElementSpec,
      _current_script: Option<NodeId>,
      _document: &mut BrowserDocumentDom2,
      _event_loop: &mut EventLoop<BrowserTabHost>,
    ) -> Result<ModuleScriptExecutionStatus> {
      Ok(ModuleScriptExecutionStatus::Completed)
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
      let credentials_mode = spec
        .crossorigin
        .unwrap_or(crate::resource::CorsMode::Anonymous)
        .credentials_mode();
      let req = FetchRequest::new(url, FetchDestination::ScriptCors)
        .with_credentials_mode(credentials_mode);
      fetcher.fetch_with_request(req)?;
      Ok(())
    }
  }

  #[test]
  fn browser_tab_wants_ticks_for_queued_tasks() -> Result<()> {
    let mut tab = BrowserTab::from_html("", RenderOptions::default(), NoopExecutor::default())?;
    match tab.run_until_stable(10)? {
      RunUntilStableOutcome::Stable { .. } => {}
      other => return Err(Error::Other(format!("expected stable tab, got {other:?}"))),
    }
    assert!(
      !tab.wants_ticks(),
      "expected empty tab to not want ticks after reaching stability"
    );

    tab
      .event_loop
      .queue_task(TaskSource::Script, |_host, _event_loop| Ok(()))?;

    assert!(
      tab.wants_ticks(),
      "expected queued event-loop task to make BrowserTab want ticks"
    );
    Ok(())
  }

  #[test]
  fn browser_tab_wants_ticks_for_scheduled_future_timers() -> Result<()> {
    let mut tab = BrowserTab::from_html("", RenderOptions::default(), NoopExecutor::default())?;
    match tab.run_until_stable(10)? {
      RunUntilStableOutcome::Stable { .. } => {}
      other => return Err(Error::Other(format!("expected stable tab, got {other:?}"))),
    }
    assert!(
      !tab.wants_ticks(),
      "expected empty tab to not want ticks after reaching stability"
    );

    tab.event_loop.set_timeout(std::time::Duration::from_secs(60), |_host, _event_loop| {
      Ok(())
    })?;

    assert!(
      tab.event_loop.is_idle(),
      "expected EventLoop::is_idle to ignore future timers"
    );
    assert!(
      tab.wants_ticks(),
      "expected scheduled future timer to make BrowserTab want ticks"
    );

    Ok(())
  }

  #[test]
  fn browser_tab_translates_renderer_preorder_ids_to_dom2_node_ids_with_comments_and_wbr() -> Result<()> {
    fn find_renderer_element_by_id<'a>(
      root: &'a crate::dom::DomNode,
      id_value: &str,
    ) -> Option<&'a crate::dom::DomNode> {
      let mut stack = vec![root];
      while let Some(node) = stack.pop() {
        if node.is_element() && node.get_attribute_ref("id") == Some(id_value) {
          return Some(node);
        }
        for child in node.children.iter().rev() {
          stack.push(child);
        }
      }
      None
    }

    fn find_renderer_text_node<'a>(
      root: &'a crate::dom::DomNode,
      content: &str,
    ) -> Option<&'a crate::dom::DomNode> {
      let mut stack = vec![root];
      while let Some(node) = stack.pop() {
        if matches!(
          &node.node_type,
          crate::dom::DomNodeType::Text { content: text } if text == content
        ) {
          return Some(node);
        }
        for child in node.children.iter().rev() {
          stack.push(child);
        }
      }
      None
    }

    let html = concat!(
      "<!doctype html><html><body>",
      "<span id=before>Before</span>",
      "<wbr id=wbr>",
      "<div id=target>Target</div>",
      "</body></html>",
    );
    let mut tab = BrowserTab::from_html(
      html,
      RenderOptions::new().with_viewport(32, 32),
      NoopExecutor::default(),
    )?;

    // Ensure the DOM contains at least one comment node and that it appears before the target in
    // tree order (comment nodes are not rendered and must not shift renderer preorder ids).
    {
      let dom = tab.dom_mut();
      let body = dom.body().expect("expected HTML <body>");
      let comment = dom.create_comment("hi");
      let reference = dom.node(body).children.first().copied();
      dom
        .insert_before(body, comment, reference)
        .expect("insert comment");
    }

    tab.render_frame()?;

    let dom = tab.dom();
    let mut has_comment = false;
    let mut stack = vec![dom.root()];
    while let Some(id) = stack.pop() {
      if matches!(dom.node(id).kind, crate::dom2::NodeKind::Comment { .. }) {
        has_comment = true;
        break;
      }
      for &child in dom.node(id).children.iter().rev() {
        stack.push(child);
      }
    }
    assert!(has_comment, "expected dom2 document to contain a comment node");

    let renderer_dom = dom.to_renderer_dom();
    let preorder_ids = crate::dom::enumerate_dom_ids(&renderer_dom);

    // Verify that the renderer preorder id for #target maps back to the correct dom2 `NodeId` even
    // with comment nodes (skipped) and `<wbr>` ZWSP injection (extra renderer node).
    let target_renderer =
      find_renderer_element_by_id(&renderer_dom, "target").expect("target element in renderer DOM");
    let target_preorder_id = *preorder_ids
      .get(&(target_renderer as *const crate::dom::DomNode))
      .expect("renderer preorder id for #target");
    let target_dom2 = dom.get_element_by_id("target").expect("#target in dom2");
    assert_eq!(
      tab.dom_node_for_renderer_preorder(target_preorder_id),
      Some(target_dom2)
    );

    // The synthetic `<wbr>` ZWSP text node in the renderer DOM should map back to a stable dom2 id.
    let wbr_dom2 = dom.get_element_by_id("wbr").expect("#wbr in dom2");
    let wbr_renderer =
      find_renderer_element_by_id(&renderer_dom, "wbr").expect("wbr element in renderer DOM");
    let zwsp_renderer =
      find_renderer_text_node(wbr_renderer, "\u{200B}").expect("ZWSP node under <wbr>");
    let zwsp_preorder_id = *preorder_ids
      .get(&(zwsp_renderer as *const crate::dom::DomNode))
      .expect("renderer preorder id for wbr ZWSP");

    let expected_dom2_for_zwsp = dom
      .node(wbr_dom2)
      .children
      .iter()
      .copied()
      .find(|&child| {
        matches!(
          &dom.node(child).kind,
          crate::dom2::NodeKind::Text { content } if content == "\u{200B}"
        )
      })
      .unwrap_or(wbr_dom2);

    assert_eq!(
      tab.dom_node_for_renderer_preorder(zwsp_preorder_id),
      Some(expected_dom2_for_zwsp)
    );

    // Renderer preorder ids are 1-based.
    assert_eq!(tab.dom_node_for_renderer_preorder(0), None);

    Ok(())
  }

  struct WindowRealmExecutor {
    realm: WindowRealm,
    log: Rc<RefCell<Vec<String>>>,
  }

  impl WindowRealmExecutor {
    fn new(log: Rc<RefCell<Vec<String>>>) -> Result<Self> {
      let realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))
        .map_err(|err| Error::Other(format!("failed to create WindowRealm: {err}")))?;
      Ok(Self { realm, log })
    }
  }

  impl BrowserTabJsExecutor for WindowRealmExecutor {
    fn execute_classic_script(
      &mut self,
      script_text: &str,
      _spec: &ScriptElementSpec,
      _current_script: Option<NodeId>,
      document: &mut BrowserDocumentDom2,
      event_loop: &mut EventLoop<BrowserTabHost>,
    ) -> Result<()> {
      self.log.borrow_mut().push(format!("script:{script_text}"));
      let host_ctx: &mut dyn VmHost = document;
      let mut hooks = VmJsEventLoopHooks::<BrowserTabHost>::new(host_ctx);
      hooks.set_event_loop(event_loop);
      self
        .realm
        .exec_script_with_host_and_hooks(host_ctx, &mut hooks, script_text)
        .map_err(|err| Error::Other(err.to_string()))?;
      if let Some(err) = hooks.finish(self.realm.heap_mut()) {
        return Err(err);
      }
      Ok(())
    }

    fn execute_module_script(
      &mut self,
      _script_id: HtmlScriptId,
      script_text: &str,
      spec: &ScriptElementSpec,
      current_script: Option<NodeId>,
      document: &mut BrowserDocumentDom2,
      event_loop: &mut EventLoop<BrowserTabHost>,
    ) -> Result<ModuleScriptExecutionStatus> {
      self.execute_classic_script(script_text, spec, current_script, document, event_loop)?;
      Ok(ModuleScriptExecutionStatus::Completed)
    }

    fn window_realm_mut(&mut self) -> Option<&mut crate::js::WindowRealm> {
      Some(&mut self.realm)
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
  fn realtime_animations_sample_event_loop_clock() -> Result<()> {
    let clock: Arc<VirtualClock> = Arc::new(VirtualClock::new());
    let event_loop = EventLoop::with_clock(clock.clone() as Arc<dyn Clock>);

    let html = r#"<!doctype html>
      <style>
        html, body { margin: 0; width: 100%; height: 100%; background: rgb(255, 255, 255); }
        #box {
          width: 100%;
          height: 100%;
          background: rgb(0, 0, 0);
          animation: fade 1s linear forwards;
        }
        @keyframes fade { from { opacity: 0; } to { opacity: 1; } }
      </style>
      <div id="box"></div>"#;

    let options = RenderOptions::new().with_viewport(1, 1);
    let mut tab =
      BrowserTab::from_html_with_event_loop(html, options, NoopExecutor::default(), event_loop)?;
    tab.host.document.set_realtime_animations_enabled(true);

    let start = rgba_at(&tab.render_frame()?, 0, 0);
    assert!(
      start[0] >= 250 && start[1] >= 250 && start[2] >= 250 && start[3] == 255,
      "expected animation start frame to be opaque white, got {start:?}"
    );

    clock.advance(Duration::from_millis(500));
    let mid = rgba_at(&tab.render_frame()?, 0, 0);
    assert!(
      mid[0] < 240 && mid[1] < 240 && mid[2] < 240,
      "expected 500ms frame to be visibly mid-animation (not white), got {mid:?}"
    );
    assert!(
      mid[0] > 10 && mid[1] > 10 && mid[2] > 10,
      "expected 500ms frame to be visibly mid-animation (not black), got {mid:?}"
    );

    // Advance far past the 1s animation duration so the fill-mode forwards state should be
    // completely black. This makes the test robust against slow wall-clock rendering when the
    // document's animation clock is misconfigured.
    clock.advance(Duration::from_secs(10));
    let end = rgba_at(&tab.render_frame()?, 0, 0);
    assert!(
      end[0] <= 10 && end[1] <= 10 && end[2] <= 10 && end[3] == 255,
      "expected animation end frame to be opaque black, got {end:?}"
    );

    Ok(())
  }

  #[test]
  fn browser_tab_tick_animation_frame_syncs_css_animation_time_to_event_loop() -> Result<()> {
    let html = r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; background: rgb(255, 255, 255); }
            #box {
              width: 10px;
              height: 10px;
              background: rgb(255, 0, 0);
              animation: move 200ms linear infinite;
            }
            @keyframes move {
              from { transform: translateX(0px); }
              to { transform: translateX(20px); }
            }
          </style>
        </head>
        <body>
          <div id="box"></div>
        </body>
      </html>"#;
    let options = RenderOptions::new().with_viewport(32, 16);

    let clock = Arc::new(VirtualClock::new());
    let clock_for_loop: Arc<dyn Clock> = clock.clone();
    let event_loop = EventLoop::<BrowserTabHost>::with_clock(clock_for_loop);
    let mut tab = BrowserTab::from_html_with_event_loop(
      html,
      options,
      NoopExecutor::default(),
      event_loop,
    )?;

    let frame_0 = tab
      .tick_animation_frame()?
      .expect("expected initial frame at t=0ms");
    assert_eq!(rgba_at(&frame_0, 2, 2), [255, 0, 0, 255]);
    assert_eq!(rgba_at(&frame_0, 12, 2), [255, 255, 255, 255]);

    clock.set_now(Duration::from_millis(100));
    let frame_100 = tab
      .tick_animation_frame()?
      .expect("expected frame at t=100ms");
    assert_eq!(rgba_at(&frame_100, 2, 2), [255, 255, 255, 255]);
    assert_eq!(rgba_at(&frame_100, 12, 2), [255, 0, 0, 255]);

    Ok(())
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
  fn script_load_event_runs_after_microtask_checkpoint_for_blocking_external_script() -> Result<()>
  {
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
    let (mut host, mut event_loop) =
      build_host("<script async src=a.js></script>", Rc::clone(&log))?;
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
        _script_id: HtmlScriptId,
        script_text: &str,
        _spec: &ScriptElementSpec,
        _current_script: Option<NodeId>,
        _document: &mut BrowserDocumentDom2,
        _event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<ModuleScriptExecutionStatus> {
        // Tests in this module primarily cover classic script execution; implement module execution
        // by reusing the same "log the script text" behavior.
        self.log.borrow_mut().push(script_text.to_string());
        Ok(ModuleScriptExecutionStatus::Completed)
      }
    }

    let js_options = JsExecutionOptions {
      max_script_bytes: 1,
      ..JsExecutionOptions::default()
    };
    let script_log: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let mut host = BrowserTabHost::new(
      BrowserDocumentDom2::from_html(
        "<script src=a.js></script><script>B</script>",
        RenderOptions::default(),
      )?,
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
    let html = format!(r#"<script src="a.js" integrity="{wrong}"></script><script>B</script>"#);
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
    let html = format!(r#"<script src="a.js" integrity="{integrity}"></script><script>B</script>"#);
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
    let html = r#"<script src="a.js" integrity="sha512-deadbeef"></script><script>B</script>"#;
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
    host.reset_scripting_state(
      Some("https://example.com/doc.html".to_string()),
      ReferrerPolicy::default(),
    )?;
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
    host.reset_scripting_state(
      Some("https://example.com/doc.html".to_string()),
      ReferrerPolicy::default(),
    )?;
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

    host.register_and_schedule_script(node_id, spec, base_url_at_discovery, &mut event_loop)?;

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
        _script_id: HtmlScriptId,
        script_text: &str,
        _spec: &ScriptElementSpec,
        _current_script: Option<NodeId>,
        _document: &mut BrowserDocumentDom2,
        _event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<ModuleScriptExecutionStatus> {
        self.log.borrow_mut().push(script_text.to_string());
        if script_text == "bad" {
          return Err(Error::Other("boom".to_string()));
        }
        Ok(ModuleScriptExecutionStatus::Completed)
      }
    }

    let script_log: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let mut host = BrowserTabHost::new(
      BrowserDocumentDom2::from_html(
        "<script>bad</script><script>ok</script>",
        RenderOptions::default(),
      )?,
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
    assert_eq!(
      &*script_log.borrow(),
      &["bad".to_string(), "ok".to_string()]
    );
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
    let bytes = encoding_rs::SHIFT_JIS
      .encode("console.log('デ')")
      .0
      .into_owned();
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
    let discovered =
      host
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
    host.register_and_schedule_script(
      node_id,
      spec.clone(),
      spec.base_url.clone(),
      &mut event_loop,
    )?;

    // `QueueScriptEventTask` dispatches as an element task, so run the event loop.
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    assert_eq!(&*event_log.borrow(), &["error".to_string()]);
    Ok(())
  }

  #[test]
  fn script_onload_property_fires_for_external_script_success() -> Result<()> {
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
    }

    let script_url = "https://example.invalid/external.js";
    let mut entries: HashMap<String, FetchedResource> = HashMap::new();
    entries.insert(
      script_url.to_string(),
      FetchedResource::new(
        br#"Promise.resolve().then(() => { push('s'); });"#.to_vec(),
        Some("text/javascript".to_string()),
      ),
    );

    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(MapFetcher {
      entries: Arc::new(entries),
    });

    let html = format!(
      r#"<!doctype html><html><head><script>
        window.__log = "";
        window.push = (c) => {{ window.__log += c; }};

        const s = document.createElement('script');
        window.__script = s;
        s.src = {script_url:?};
        s.addEventListener('load', () => {{ push('l'); }});
        s.onload = function (e) {{
          window.__thisOk = (this === window.__script) ? '1' : '0';
          window.__typeOk = (e && e.type === 'load') ? '1' : '0';
          push('h');
          Promise.resolve().then(() => {{
            push('m');
            document.documentElement.setAttribute('data-log', window.__log);
            document.documentElement.setAttribute('data-this-ok', window.__thisOk);
            document.documentElement.setAttribute('data-type-ok', window.__typeOk);
          }});
        }};
        document.head.appendChild(s);
      </script></head><body></body></html>"#
    );

    let mut tab = BrowserTab::from_html_with_vmjs_and_document_url_and_fetcher(
      &html,
      "https://example.invalid/index.html",
      RenderOptions::default(),
      fetcher,
    )?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    let dom = tab.dom();
    let document_element = dom.document_element().expect("documentElement should exist");
    let log = dom
      .get_attribute(document_element, "data-log")
      .expect("get_attribute should succeed")
      .expect("expected onload microtask to write data-log");
    assert!(
      log.contains('s'),
      "expected external script microtask to run before load event; got log={log:?}"
    );
    assert!(
      log.contains('l'),
      "expected addEventListener('load') listener to run; got log={log:?}"
    );
    assert!(
      log.contains('h'),
      "expected script.onload handler property to run; got log={log:?}"
    );
    assert!(
      log.contains('m'),
      "expected microtask queued by onload to run; got log={log:?}"
    );
    let m_idx = log.find('m').expect("expected microtask marker");
    assert!(
      m_idx > log.find('l').unwrap_or(usize::MAX) && m_idx > log.find('h').unwrap_or(usize::MAX),
      "expected microtask to run after load listeners; got log={log:?}"
    );
    assert_eq!(
      dom.get_attribute(document_element, "data-this-ok")
        .expect("get_attribute should succeed"),
      Some("1"),
      "expected script.onload to receive this=script element",
    );
    assert_eq!(
      dom.get_attribute(document_element, "data-type-ok")
        .expect("get_attribute should succeed"),
      Some("1"),
      "expected script.onload to receive event argument with correct type",
    );

    Ok(())
  }

  #[test]
  fn script_onerror_property_fires_for_missing_src_attribute() -> Result<()> {
    #[derive(Clone)]
    struct NoFetchExpected;

    impl ResourceFetcher for NoFetchExpected {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        Err(Error::Other(format!(
          "unexpected fetch for missing-src onerror test: {url}"
        )))
      }
    }

    let html = r#"<!doctype html><html><head><script>
      window.__log = "";
      window.push = (c) => { window.__log += c; };

      const s = document.createElement('script');
      window.__script = s;
      s.setAttribute('src', '');
      s.addEventListener('error', () => { push('l'); });
      s.onerror = function (e) {
        window.__thisOk = (this === window.__script) ? '1' : '0';
        window.__typeOk = (e && e.type === 'error') ? '1' : '0';
        push('h');
        Promise.resolve().then(() => {
          push('m');
          document.documentElement.setAttribute('data-log', window.__log);
          document.documentElement.setAttribute('data-this-ok', window.__thisOk);
          document.documentElement.setAttribute('data-type-ok', window.__typeOk);
        });
      };
      document.head.appendChild(s);
    </script></head><body></body></html>"#;

    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(NoFetchExpected);
    let mut tab = BrowserTab::from_html_with_vmjs_and_document_url_and_fetcher(
      html,
      "https://example.invalid/index.html",
      RenderOptions::default(),
      fetcher,
    )?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    let dom = tab.dom();
    let document_element = dom.document_element().expect("documentElement should exist");
    let log = dom
      .get_attribute(document_element, "data-log")
      .expect("get_attribute should succeed")
      .expect("expected onerror microtask to write data-log");
    assert!(
      log.contains('l'),
      "expected addEventListener('error') listener to run; got log={log:?}"
    );
    assert!(
      log.contains('h'),
      "expected script.onerror handler property to run; got log={log:?}"
    );
    assert!(
      log.contains('m'),
      "expected microtask queued by onerror to run; got log={log:?}"
    );
    let m_idx = log.find('m').expect("expected microtask marker");
    assert!(
      m_idx > log.find('l').unwrap_or(usize::MAX) && m_idx > log.find('h').unwrap_or(usize::MAX),
      "expected microtask to run after error listeners; got log={log:?}"
    );
    assert_eq!(
      dom.get_attribute(document_element, "data-this-ok")
        .expect("get_attribute should succeed"),
      Some("1"),
      "expected script.onerror to receive this=script element",
    );
    assert_eq!(
      dom.get_attribute(document_element, "data-type-ok")
        .expect("get_attribute should succeed"),
      Some("1"),
      "expected script.onerror to receive event argument with correct type",
    );
    Ok(())
  }

  #[test]
  fn dispatches_error_event_for_invalid_module_src_attribute() -> Result<()> {
    let js_options = JsExecutionOptions {
      supports_module_scripts: true,
      ..JsExecutionOptions::default()
    };
    let mut host = BrowserTabHost::new(
      BrowserDocumentDom2::from_html(
        "<script type=\"module\" src></script>",
        RenderOptions::default(),
      )?,
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
    host.register_and_schedule_script(
      node_id,
      spec.clone(),
      spec.base_url.clone(),
      &mut event_loop,
    )?;

    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    assert_eq!(&*event_log.borrow(), &["error".to_string()]);
    Ok(())
  }

  #[test]
  fn dispatches_error_event_for_invalid_importmap_src_attribute() -> Result<()> {
    let js_options = JsExecutionOptions {
      supports_module_scripts: true,
      ..JsExecutionOptions::default()
    };
    let mut host = BrowserTabHost::new(
      BrowserDocumentDom2::from_html(
        "<script type=\"importmap\" src></script>",
        RenderOptions::default(),
      )?,
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
    host.register_and_schedule_script(
      node_id,
      spec.clone(),
      spec.base_url.clone(),
      &mut event_loop,
    )?;

    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    assert_eq!(&*event_log.borrow(), &["error".to_string()]);
    Ok(())
  }

  #[test]
  fn importmap_src_attribute_is_ignored_when_module_scripts_disabled() -> Result<()> {
    // Browsers without module support treat `type="importmap"` as an unknown script type, so `src`
    // must not dispatch an error event.
    let mut host = BrowserTabHost::new(
      BrowserDocumentDom2::from_html(
        "<script type=\"importmap\" src></script>",
        RenderOptions::default(),
      )?,
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
    host.register_and_schedule_script(
      node_id,
      spec.clone(),
      spec.base_url.clone(),
      &mut event_loop,
    )?;

    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    assert!(
      event_log.borrow().is_empty(),
      "expected importmap scripts to be ignored when module scripts are disabled"
    );
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
  fn streaming_parser_allows_async_external_scripts_to_execute_before_later_parser_scripts(
  ) -> Result<()> {
    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let executor = TestExecutor {
      log: Rc::clone(&log),
    };
    let mut tab = BrowserTab::from_html("", RenderOptions::default(), executor)?;

    // Force parsing to yield to the event loop so async scripts can interleave with streaming parse
    // slices (mirroring how browsers can execute fast async scripts mid-parse).
    let mut js_execution_options = tab.js_execution_options();
    js_execution_options.dom_parse_budget = crate::js::ParseBudget::new(1);
    tab.set_js_execution_options(js_execution_options);

    tab.host.reset_scripting_state(None, ReferrerPolicy::default())?;
    tab.register_script_source("https://example.com/a.js", "A");

    let html = "<!doctype html><html><body>\
      <script async src=\"https://example.com/a.js\"></script>\
      <script>B</script>\
      </body></html>";
    let _ = tab.parse_html_streaming_and_schedule_scripts(
      html,
      Some("https://example.com/"),
      &RenderOptions::default(),
    )?;

    assert!(
      log.borrow().is_empty(),
      "expected parse initialization to queue work without running the event loop"
    );

    assert_eq!(
      tab.run_event_loop_until_idle(RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );

    assert_eq!(
      &*log.borrow(),
      &[
        "script:A".to_string(),
        "microtask:A".to_string(),
        "script:B".to_string(),
        "microtask:B".to_string()
      ]
    );
    Ok(())
  }

  #[test]
  fn streaming_parser_allows_async_module_scripts_to_execute_before_later_parser_scripts(
  ) -> Result<()> {
    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let executor = TestExecutor { log: Rc::clone(&log) };
    let mut tab = BrowserTab::from_html("", RenderOptions::default(), executor)?;

    // Enable module scripts and force parsing to yield so the async module script can execute before
    // the later parser-inserted classic script is discovered.
    let mut js_execution_options = tab.js_execution_options();
    js_execution_options.dom_parse_budget = crate::js::ParseBudget::new(1);
    js_execution_options.supports_module_scripts = true;
    tab.set_js_execution_options(js_execution_options);

    tab.host.reset_scripting_state(None, ReferrerPolicy::default())?;
    tab.register_script_source("https://example.com/m.js", "M");

    let html = "<!doctype html><html><body>\
      <script type=\"module\" async src=\"https://example.com/m.js\"></script>\
      <script>B</script>\
      </body></html>";
    let _ =
      tab.parse_html_streaming_and_schedule_scripts(html, Some("https://example.com/"), &RenderOptions::default())?;

    assert!(
      log.borrow().is_empty(),
      "expected parse initialization to queue work without running the event loop"
    );

    assert_eq!(
      tab.run_event_loop_until_idle(RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );

    assert_eq!(
      &*log.borrow(),
      &[
        "module:M".to_string(),
        "microtask:M".to_string(),
        "script:B".to_string(),
        "microtask:B".to_string()
      ]
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
        _script_id: HtmlScriptId,
        script_text: &str,
        spec: &ScriptElementSpec,
        current_script: Option<NodeId>,
        document: &mut BrowserDocumentDom2,
        event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<ModuleScriptExecutionStatus> {
        self.execute_classic_script(script_text, spec, current_script, document, event_loop)?;
        Ok(ModuleScriptExecutionStatus::Completed)
      }
    }

    let executed = Rc::new(Cell::new(false));
    // Use a fetcher-backed script source instead of `register_script_source` so this regression test
    // proves async scripts can interleave with parsing via event-loop tasks.
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
  fn streaming_parser_does_not_deadlock_when_async_script_fetch_fails() -> Result<()> {
    #[derive(Default)]
    struct RejectingFetcher;

    impl ResourceFetcher for RejectingFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        Err(Error::Other(format!("fetch blocked in test for {url}")))
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        self.fetch(req.url)
      }
    }

    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let executor = TestExecutor { log: Rc::clone(&log) };

    // Force parsing to yield so we exercise parse-resume scheduling even though the async script
    // never successfully fetches.
    let mut js_execution_options = JsExecutionOptions::default();
    js_execution_options.dom_parse_budget = crate::js::ParseBudget::new(1);

    let html = "<!doctype html><html><body>\
      <script async src=\"https://example.com/missing.js\"></script>\
      <script>B</script>\
      </body></html>";

    let mut tab = BrowserTab::from_html_with_document_url_and_fetcher_and_js_execution_options(
      html,
      "https://example.com/",
      RenderOptions::default(),
      executor,
      Arc::new(RejectingFetcher),
      js_execution_options,
    )?;

    assert!(
      log.borrow().is_empty(),
      "expected initial parse slice to yield before reaching the later inline script"
    );

    assert_eq!(
      tab.run_event_loop_until_idle(RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );

    assert_eq!(
      &*log.borrow(),
      &["script:B".to_string(), "microtask:B".to_string()]
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
        _script_id: HtmlScriptId,
        script_text: &str,
        spec: &ScriptElementSpec,
        current_script: Option<NodeId>,
        document: &mut BrowserDocumentDom2,
        event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<ModuleScriptExecutionStatus> {
        self.execute_classic_script(script_text, spec, current_script, document, event_loop)?;
        Ok(ModuleScriptExecutionStatus::Completed)
      }
    }

    let seen = Rc::new(RefCell::new(Vec::<bool>::new()));
    let _tab = BrowserTab::from_html(
      "<!doctype html><script>noop</script>",
      RenderOptions::default(),
      RecordingExecutor {
        seen: Rc::clone(&seen),
      },
    )?;

    assert_eq!(
      &*seen.borrow(),
      &[false],
      "expected parser-inserted <script> to have force_async=false"
    );
    Ok(())
  }

  #[test]
  fn executor_after_microtask_checkpoint_runs_after_explicit_and_post_task_checkpoints() -> Result<()> {
    let calls = Rc::new(Cell::new(0));
    let executor = AfterMicrotaskCheckpointCountingExecutor {
      calls: Rc::clone(&calls),
    };
    let mut tab = BrowserTab::from_html("", RenderOptions::default(), executor)?;

    // Discard any parsing/lifecycle work queued while constructing the tab so this test only
    // observes checkpoints we trigger explicitly below.
    calls.set(0);
    tab.event_loop.clear_all_pending_work();

    tab.event_loop.perform_microtask_checkpoint(&mut tab.host)?;
    assert_eq!(
      calls.get(),
      1,
      "expected after_microtask_checkpoint to run after explicit microtask checkpoints"
    );

    tab
      .event_loop
      .queue_task(TaskSource::Script, |_host, _event_loop| Ok(()))?;
    assert!(
      tab.event_loop.run_next_task(&mut tab.host)?,
      "expected queued task to run"
    );
    assert_eq!(
      calls.get(),
      2,
      "expected after_microtask_checkpoint to run after the implicit post-task checkpoint"
    );

    Ok(())
  }

  #[test]
  fn tick_frame_invokes_executor_after_microtask_checkpoint_once() -> Result<()> {
    let calls = Rc::new(Cell::new(0));
    let executor = AfterMicrotaskCheckpointCountingExecutor {
      calls: Rc::clone(&calls),
    };
    let mut tab = BrowserTab::from_html("", RenderOptions::default(), executor)?;

    // Discard any parsing/lifecycle work queued while constructing the tab so this test only
    // observes checkpoints triggered by `tick_frame`.
    calls.set(0);
    tab.event_loop.clear_all_pending_work();

    tab
      .event_loop
      .queue_task(TaskSource::Script, |_host, _event_loop| Ok(()))?;
    let _ = tab.tick_frame()?;

    assert_eq!(
      calls.get(),
      1,
      "expected tick_frame to invoke after_microtask_checkpoint once (not twice)"
    );
    Ok(())
  }

  #[test]
  fn from_html_with_event_loop_preserves_existing_microtask_checkpoint_hooks() -> Result<()> {
    // Embeddings can register microtask checkpoint hooks (e.g. promise rejection tracking). Passing
    // an `EventLoop` into `BrowserTab` must not clobber existing hooks.
    let counter = Arc::new(AtomicUsize::new(0));
    let _guard = MicrotaskCheckpointTestCounterGuard::install(Arc::clone(&counter));
    let mut event_loop = EventLoop::<BrowserTabHost>::new();
    event_loop.register_microtask_checkpoint_hook(microtask_checkpoint_counting_hook)?;
    let calls = Rc::new(Cell::new(0));
    let executor = AfterMicrotaskCheckpointCountingExecutor {
      calls: Rc::clone(&calls),
    };
    let mut tab =
      BrowserTab::from_html_with_event_loop("", RenderOptions::default(), executor, event_loop)?;

    // Ensure we only observe the checkpoint we trigger below.
    counter.store(0, Ordering::SeqCst);
    calls.set(0);
    tab.event_loop.clear_all_pending_work();

    tab
      .event_loop
      .queue_microtask(|_host, _event_loop| Ok(()))?;
    tab.event_loop.perform_microtask_checkpoint(&mut tab.host)?;

    assert_eq!(
      counter.load(Ordering::SeqCst),
      1,
      "expected embedding microtask checkpoint hook to survive BrowserTab construction"
    );
    assert_eq!(
      calls.get(),
      1,
      "expected executor after_microtask_checkpoint hook to remain installed"
    );
    Ok(())
  }

  #[test]
  fn tick_frame_microtask_only_checkpoint_invokes_executor_once() -> Result<()> {
    let calls = Rc::new(Cell::new(0));
    let executor = AfterMicrotaskCheckpointCountingExecutor {
      calls: Rc::clone(&calls),
    };
    let mut tab = BrowserTab::from_html("", RenderOptions::default(), executor)?;

    // Discard any parsing/lifecycle work queued while constructing the tab so this test only
    // observes checkpoints triggered by `tick_frame`.
    calls.set(0);
    tab.event_loop.clear_all_pending_work();

    tab
      .event_loop
      .queue_microtask(|_host, _event_loop| Ok(()))?;
    let _ = tab.tick_frame()?;

    assert_eq!(
      calls.get(),
      1,
      "expected tick_frame microtask-only checkpoint to invoke after_microtask_checkpoint once (not twice)"
    );
    Ok(())
  }

  #[test]
  fn tick_frame_post_raf_microtask_checkpoint_invokes_executor_once() -> Result<()> {
    let calls = Rc::new(Cell::new(0));
    let executor = AfterMicrotaskCheckpointCountingExecutor {
      calls: Rc::clone(&calls),
    };
    let mut tab = BrowserTab::from_html("", RenderOptions::default(), executor)?;

    // Discard any parsing/lifecycle work queued while constructing the tab so this test only
    // observes checkpoints triggered by `tick_frame`.
    calls.set(0);
    tab.event_loop.clear_all_pending_work();

    tab.event_loop.request_animation_frame(|_host, event_loop, _timestamp| {
      event_loop.queue_microtask(|_host, _event_loop| Ok(()))?;
      Ok(())
    })?;

    let _ = tab.tick_frame()?;

    assert_eq!(
      calls.get(),
      1,
      "expected tick_frame post-rAF microtask checkpoint to invoke after_microtask_checkpoint once (not twice)"
    );
    Ok(())
  }

  #[test]
  fn promise_rejection_tracker_coexists_with_executor_microtask_checkpoint_hook() -> Result<()> {
    let after_checkpoint_calls = Arc::new(AtomicUsize::new(0));
    let executor = CountingVmJsExecutor::new(Arc::clone(&after_checkpoint_calls));
    let mut tab = BrowserTab::from_html("", RenderOptions::default(), executor)?;

    after_checkpoint_calls.store(0, Ordering::SeqCst);
    // Ensure we only run tasks queued by this test.
    tab.event_loop.clear_all_pending_work();

    let spec = ScriptElementSpec {
      base_url: tab.host.base_url.clone(),
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
      parser_inserted: false,
      node_id: None,
      script_type: ScriptType::Classic,
    };

    tab.host.executor.execute_classic_script(
      r#"
      globalThis.__unhandled = false;
      globalThis.addEventListener('unhandledrejection', (e) => { globalThis.__unhandled = true; e.preventDefault(); });
      Promise.reject('boom');
      "#,
      &spec,
      None,
      tab.host.document.as_mut(),
      &mut tab.event_loop,
    )?;

    // Run the rejection-tracker microtask-checkpoint hook, which should queue the unhandledrejection
    // event task.
    tab.event_loop.perform_microtask_checkpoint(&mut tab.host)?;
    tab
      .event_loop
      .run_until_idle(&mut tab.host, RunLimits::unbounded())?;

    assert!(
      after_checkpoint_calls.load(Ordering::SeqCst) > 0,
      "expected executor after_microtask_checkpoint hook to run during checkpoints"
    );

    let realm = tab
      .host
      .executor
      .window_realm_mut()
      .expect("expected vm-js WindowRealm to be active");
    let unhandled = realm
      .exec_script("globalThis.__unhandled")
      .map_err(|err| Error::Other(err.to_string()))?;
    assert!(
      matches!(unhandled, Value::Bool(true)),
      "expected unhandledrejection listener to run, got {unhandled:?}"
    );

    Ok(())
  }

  #[test]
  fn browser_tab_wants_ticks_considers_pending_timers_even_when_event_loop_is_idle() -> Result<()> {
    use crate::js::clock::VirtualClock;

    let clock = Arc::new(VirtualClock::new());
    let event_loop = EventLoop::with_clock(clock);
    let mut tab = BrowserTab::from_html_with_event_loop_and_js_execution_options(
      "",
      RenderOptions::default(),
      crate::api::VmJsBrowserTabExecutor::default(),
      event_loop,
      JsExecutionOptions::default(),
    )?;

    // Start from a fully rendered/clean document with no other queued work so we can isolate timer
    // bookkeeping.
    let _ = tab.render_frame()?;
    tab.event_loop.clear_all_pending_work();

    {
      let BrowserTab { host, event_loop, .. } = &mut tab;
      let spec = ScriptElementSpec {
        base_url: host.base_url.clone(),
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
        parser_inserted: false,
        node_id: None,
        script_type: ScriptType::Classic,
      };
      host.executor.execute_classic_script(
        "globalThis.__timeoutId = setTimeout(() => {}, 60000);",
        &spec,
        None,
        host.document.as_mut(),
        event_loop,
      )?;
    }

    tab
      .event_loop
      .run_until_idle(&mut tab.host, RunLimits::unbounded())?;

    assert!(
      !tab.host.document.is_dirty(),
      "expected document to be clean so wants_ticks reflects timer state"
    );
    assert!(
      tab.event_loop.is_idle(),
      "expected EventLoop::is_idle to ignore future timers"
    );
    assert!(
      tab.wants_ticks(),
      "expected BrowserTab::wants_ticks to keep ticking when timers are scheduled"
    );

    // Clearing the timeout should return the tab to a quiescent state.
    {
      let BrowserTab { host, event_loop, .. } = &mut tab;
      let spec = ScriptElementSpec {
        base_url: host.base_url.clone(),
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
        parser_inserted: false,
        node_id: None,
        script_type: ScriptType::Classic,
      };
      host.executor.execute_classic_script(
        "clearTimeout(globalThis.__timeoutId);",
        &spec,
        None,
        host.document.as_mut(),
        event_loop,
      )?;
    }

    tab
      .event_loop
      .run_until_idle(&mut tab.host, RunLimits::unbounded())?;

    assert!(
      tab.event_loop.is_idle(),
      "expected EventLoop to remain idle after clearing the timeout"
    );
    assert!(
      !tab.wants_ticks(),
      "expected BrowserTab::wants_ticks to return false after clearing the last timer"
    );

    Ok(())
  }

  #[test]
  fn browser_tab_wants_ticks_considers_queued_microtasks_and_tasks() -> Result<()> {
    use crate::js::clock::VirtualClock;

    let clock = Arc::new(VirtualClock::new());
    let event_loop = EventLoop::with_clock(clock);
    let mut tab = BrowserTab::from_html_with_event_loop_and_js_execution_options(
      "",
      RenderOptions::default(),
      crate::api::VmJsBrowserTabExecutor::default(),
      event_loop,
      JsExecutionOptions::default(),
    )?;

    let _ = tab.render_frame()?;
    tab.event_loop.clear_all_pending_work();

    // Microtask should make the tab want ticks even when no render invalidation exists.
    tab
      .event_loop
      .queue_microtask(|_host, _event_loop| Ok(()))?;
    assert!(!tab.event_loop.is_idle());
    assert!(tab.wants_ticks());
    tab
      .event_loop
      .run_until_idle(&mut tab.host, RunLimits::unbounded())?;
    assert!(tab.event_loop.is_idle());
    assert!(!tab.wants_ticks());

    // Normal task should also make the tab want ticks.
    tab
      .event_loop
      .queue_task(TaskSource::DOMManipulation, |_host, _event_loop| Ok(()))?;
    assert!(!tab.event_loop.is_idle());
    assert!(tab.wants_ticks());
    tab
      .event_loop
      .run_until_idle(&mut tab.host, RunLimits::unbounded())?;
    assert!(tab.event_loop.is_idle());
    assert!(!tab.wants_ticks());

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
    tab
      .host
      .reset_scripting_state(None, ReferrerPolicy::default())?;

    tab.event_loop.queue_microtask({
      let log = Rc::clone(&log);
      move |_host, _event_loop| {
        log.borrow_mut().push("pre".to_string());
        Ok(())
      }
    })?;

    // Simulate re-entrant parsing while already in JS execution (e.g. future document.write).
    let outer_guard = JsExecutionGuard::enter(&tab.host.js_execution_depth);
    let _ = tab.parse_html_streaming_and_schedule_scripts(
      "<script>A</script>",
      None,
      &RenderOptions::default(),
    )?;

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
  fn parser_inserted_inline_script_is_marked_already_started_during_preparation() -> Result<()> {
    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let executor = TestExecutor {
      log: Rc::clone(&log),
    };
    let tab = BrowserTab::from_html(
      "<!doctype html><html><body><script id=s>noop</script></body></html>",
      RenderOptions::default(),
      executor,
    )?;

    assert_eq!(
      &*log.borrow(),
      &["script:noop".to_string(), "microtask:noop".to_string()]
    );

    let script = tab
      .host
      .dom()
      .get_element_by_id("s")
      .expect("expected #s script to exist");
    assert!(
      tab.host.dom().node(script).script_already_started,
      "parser-inserted runnable scripts must be marked already started during preparation"
    );
    Ok(())
  }

  #[test]
  fn parser_inserted_external_script_is_marked_already_started_during_preparation() -> Result<()> {
    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let executor = TestExecutor {
      log: Rc::clone(&log),
    };
    let mut tab = BrowserTab::from_html(
      "<!doctype html><html><body><script id=s async src=a.js></script></body></html>",
      RenderOptions::default(),
      executor,
    )?;
    // Provide the external source and drive the event loop so the fetch/execution completes.
    tab
      .host
      .register_external_script_source("a.js".to_string(), "EXT".to_string());
    tab
      .event_loop
      .run_until_idle(&mut tab.host, RunLimits::unbounded())?;

    assert!(
      log.borrow().iter().any(|entry| entry == "script:EXT"),
      "expected external script to execute"
    );

    let script = tab
      .host
      .dom()
      .get_element_by_id("s")
      .expect("expected #s script to exist");
    assert!(
      tab.host.dom().node(script).script_already_started,
      "parser-inserted external scripts must be marked already started during preparation (before fetch/execution)"
    );
    Ok(())
  }

  #[test]
  fn empty_parser_inserted_script_can_execute_after_later_text_mutation_via_best_effort_scheduling(
  ) -> Result<()> {
    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let (mut host, mut event_loop) = build_host(
      "<!doctype html><html><body><script id=s></script></body></html>",
      Rc::clone(&log),
    )?;

    let mut discovered = host.discover_scripts_best_effort(None);
    assert_eq!(discovered.len(), 1);
    let (script, spec) = discovered.pop().unwrap();

    host.register_and_schedule_script(
      script,
      spec.clone(),
      spec.base_url.clone(),
      &mut event_loop,
    )?;

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
    let executor = TestExecutor {
      log: Rc::clone(&log),
    };
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
  fn unsupported_type_parser_inserted_script_can_execute_after_type_and_children_mutation(
  ) -> Result<()> {
    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let executor = TestExecutor {
      log: Rc::clone(&log),
    };
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
      dom
        .remove_attribute(script, "type")
        .expect("remove_attribute");
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
  fn empty_parser_inserted_script_sets_force_async_for_later_external_script_execution(
  ) -> Result<()> {
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
        _script_id: HtmlScriptId,
        script_text: &str,
        spec: &ScriptElementSpec,
        current_script: Option<NodeId>,
        document: &mut BrowserDocumentDom2,
        event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<ModuleScriptExecutionStatus> {
        self.execute_classic_script(script_text, spec, current_script, document, event_loop)?;
        Ok(ModuleScriptExecutionStatus::Completed)
      }
    }

    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let executor = LoggingExecutor {
      log: Rc::clone(&log),
    };

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
      dom
        .set_attribute(script_b, "async", "")
        .expect("set_attribute");
      dom
        .remove_attribute(script_b, "async")
        .expect("remove_attribute");
      dom
        .set_attribute(script_b, "src", "b.js")
        .expect("set_attribute");
      dom.append_child(body, script_b).expect("append_child");
      (script_b, true)
    });
    assert!(
      !tab
        .host
        .dom()
        .has_attribute(script_b, "async")
        .unwrap_or(false),
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
      (
        id_a.expect("missing script_id for a.js"),
        id_b.expect("missing script_id for b.js"),
      )
    };

    // Verify the discovered specs match the internal-slot state.
    assert!(
      tab
        .host
        .scripts
        .get(&id_a)
        .expect("missing script entry for a.js")
        .spec
        .force_async,
      "expected force_async to be propagated for previously-failed parser-inserted script"
    );
    assert!(
      !tab
        .host
        .scripts
        .get(&id_b)
        .expect("missing script entry for b.js")
        .spec
        .force_async,
      "expected force_async to be cleared for in-order-asap script"
    );

    // Complete fetch for B first. Because A is async-like (force_async=true), it is not part of the
    // in-order-asap list; therefore B can execute immediately on fetch completion, before A.
    let actions_b = tab
      .host
      .scheduler
      .classic_fetch_completed(id_b, "B".to_string())?;
    tab
      .host
      .apply_scheduler_actions(actions_b, &mut tab.event_loop)?;
    let actions_a = tab
      .host
      .scheduler
      .classic_fetch_completed(id_a, "A".to_string())?;
    tab
      .host
      .apply_scheduler_actions(actions_a, &mut tab.event_loop)?;

    tab
      .event_loop
      .run_until_idle(&mut tab.host, RunLimits::unbounded())?;
    assert_eq!(&*log.borrow(), &["B".to_string(), "A".to_string()]);
    Ok(())
  }

  fn value_to_string(realm: &WindowRealm, value: Value) -> String {
    let Value::String(s) = value else {
      panic!("expected string, got {value:?}");
    };
    realm.heap().get_string(s).unwrap().to_utf8_lossy()
  }

  #[test]
  fn browser_tab_document_visibility_state_and_visibilitychange_event() -> Result<()> {
    let html = r#"<!doctype html>
      <html>
        <body>
          <script>
            globalThis.__initialVisibility = document.visibilityState;
            globalThis.__initialHidden = document.hidden;
            globalThis.__visibilityChangeCount = 0;
            globalThis.__onVisibilityChangeCount = 0;
            globalThis.__onVisibilityChangeOk = true;
            globalThis.__lastVisibility = null;
            globalThis.__lastHidden = null;
            document.onvisibilitychange = function (e) {
              globalThis.__onVisibilityChangeCount++;
              globalThis.__onVisibilityChangeOk = globalThis.__onVisibilityChangeOk && (
                this === document &&
                e && e.type === 'visibilitychange' &&
                e.target === document &&
                e.currentTarget === document &&
                e.eventPhase === 2
              );
            };
            document.addEventListener('visibilitychange', () => {
              globalThis.__visibilityChangeCount++;
              globalThis.__lastVisibility = document.visibilityState;
              globalThis.__lastHidden = document.hidden;
            });
          </script>
        </body>
      </html>"#;
    let mut tab = BrowserTab::from_html_with_vmjs_executor(html, RenderOptions::default())?;

    {
      let realm = tab
        .host
        .executor
        .window_realm_mut()
        .expect("expected vm-js WindowRealm");
      let initial_visibility = realm
        .exec_script("globalThis.__initialVisibility")
        .map_err(|err| Error::Other(err.to_string()))?;
      assert_eq!(value_to_string(realm, initial_visibility), "visible");

      let initial_hidden = realm
        .exec_script("globalThis.__initialHidden")
        .map_err(|err| Error::Other(err.to_string()))?;
      assert_eq!(initial_hidden, Value::Bool(false));
    }

    tab.set_visibility(DocumentVisibilityState::Hidden)?;
    // Event must be queued as a DOM manipulation task, not dispatched synchronously.
    {
      let realm = tab
        .host
        .executor
        .window_realm_mut()
        .expect("expected vm-js WindowRealm");
      let count = realm
        .exec_script("globalThis.__visibilityChangeCount")
        .map_err(|err| Error::Other(err.to_string()))?;
      assert_eq!(count, Value::Number(0.0));
      let on_count = realm
        .exec_script("globalThis.__onVisibilityChangeCount")
        .map_err(|err| Error::Other(err.to_string()))?;
      assert_eq!(on_count, Value::Number(0.0));
    }

    tab.run_event_loop_until_idle(RunLimits::unbounded())?;
    {
      let realm = tab
        .host
        .executor
        .window_realm_mut()
        .expect("expected vm-js WindowRealm");
      let count = realm
        .exec_script("globalThis.__visibilityChangeCount")
        .map_err(|err| Error::Other(err.to_string()))?;
      assert_eq!(count, Value::Number(1.0));

      let on_count = realm
        .exec_script("globalThis.__onVisibilityChangeCount")
        .map_err(|err| Error::Other(err.to_string()))?;
      assert_eq!(on_count, Value::Number(1.0));
      let on_ok = realm
        .exec_script("globalThis.__onVisibilityChangeOk")
        .map_err(|err| Error::Other(err.to_string()))?;
      assert_eq!(on_ok, Value::Bool(true));

      let last_visibility = realm
        .exec_script("globalThis.__lastVisibility")
        .map_err(|err| Error::Other(err.to_string()))?;
      assert_eq!(value_to_string(realm, last_visibility), "hidden");

      let last_hidden = realm
        .exec_script("globalThis.__lastHidden")
        .map_err(|err| Error::Other(err.to_string()))?;
      assert_eq!(last_hidden, Value::Bool(true));
    }

    tab.set_visibility(DocumentVisibilityState::Visible)?;
    {
      let realm = tab
        .host
        .executor
        .window_realm_mut()
        .expect("expected vm-js WindowRealm");
      let count = realm
        .exec_script("globalThis.__visibilityChangeCount")
        .map_err(|err| Error::Other(err.to_string()))?;
      assert_eq!(count, Value::Number(1.0));
      let on_count = realm
        .exec_script("globalThis.__onVisibilityChangeCount")
        .map_err(|err| Error::Other(err.to_string()))?;
      assert_eq!(on_count, Value::Number(1.0));
    }

    tab.run_event_loop_until_idle(RunLimits::unbounded())?;
    {
      let realm = tab
        .host
        .executor
        .window_realm_mut()
        .expect("expected vm-js WindowRealm");
      let count = realm
        .exec_script("globalThis.__visibilityChangeCount")
        .map_err(|err| Error::Other(err.to_string()))?;
      assert_eq!(count, Value::Number(2.0));

      let on_count = realm
        .exec_script("globalThis.__onVisibilityChangeCount")
        .map_err(|err| Error::Other(err.to_string()))?;
      assert_eq!(on_count, Value::Number(2.0));
      let on_ok = realm
        .exec_script("globalThis.__onVisibilityChangeOk")
        .map_err(|err| Error::Other(err.to_string()))?;
      assert_eq!(on_ok, Value::Bool(true));

      let last_visibility = realm
        .exec_script("globalThis.__lastVisibility")
        .map_err(|err| Error::Other(err.to_string()))?;
      assert_eq!(value_to_string(realm, last_visibility), "visible");

      let last_hidden = realm
        .exec_script("globalThis.__lastHidden")
        .map_err(|err| Error::Other(err.to_string()))?;
      assert_eq!(last_hidden, Value::Bool(false));
    }

    Ok(())
  }

  #[test]
  fn browser_tab_request_animation_frame_paused_when_hidden() -> Result<()> {
    use std::cell::Cell;
    use std::rc::Rc;

    let mut tab = BrowserTab::from_html("", RenderOptions::default(), NoopExecutor::default())?;

    let called: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let called_for_cb = Rc::clone(&called);
    tab
      .event_loop
      .request_animation_frame(move |_host, _event_loop, _ts| {
        called_for_cb.set(true);
        Ok(())
      })?;

    tab.set_visibility(DocumentVisibilityState::Hidden)?;
    let _ = tab.tick_animation_frame()?;
    assert!(
      !called.get(),
      "expected requestAnimationFrame callback to be paused while document is hidden"
    );

    tab.set_visibility(DocumentVisibilityState::Visible)?;
    let _ = tab.tick_animation_frame()?;
    assert!(
      called.get(),
      "expected requestAnimationFrame callback to run once document becomes visible"
    );

    Ok(())
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
    host: &mut BrowserTabHost,
    event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    MICROTASK_CHECKPOINT_TEST_COUNTER.with(|slot| {
      if let Some(counter) = slot.borrow().as_ref() {
        counter.fetch_add(1, Ordering::SeqCst);
      }
    });
    // BrowserTab's default microtask checkpoint hook already notifies the executor; this hook only
    // records metrics.
    let _ = (host, event_loop);
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
    hooks: &mut dyn VmHostHooks,
    callee: GcObject,
    _this: Value,
    _args: &[Value],
  ) -> std::result::Result<Value, VmError> {
    let slots = scope.heap().get_function_native_slots(callee)?;
    let id = match slots.get(0).copied().unwrap_or(Value::Undefined) {
      Value::Number(n) if n.is_finite() && n >= 0.0 && n <= u64::MAX as f64 => n as u64,
      _ => {
        return Err(VmError::TypeError(
          "__queueMicrotaskTest missing hook id slot",
        ))
      }
    };

    let counter = queue_microtask_hooks()
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .get(&id)
      .cloned()
      .ok_or(VmError::TypeError(
        "__queueMicrotaskTest hook id not registered",
      ))?;

    let Some(event_loop) = event_loop_mut_from_hooks::<BrowserTabHost>(hooks) else {
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

  fn alloc_key(
    scope: &mut vm_js::Scope<'_>,
    name: &str,
  ) -> std::result::Result<PropertyKey, VmError> {
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
      .register_microtask_checkpoint_hook(microtask_checkpoint_counting_hook)?;
    tab.host.reset_scripting_state(None, ReferrerPolicy::default())?;

    // Use a small budget that still reaches the first `</script>` boundary in the initial parse
    // call (first pump requests input, second pump yields the script boundary).
    tab.host.js_execution_options.dom_parse_budget = crate::js::ParseBudget::new(2);

    let _ = tab.parse_html_streaming_and_schedule_scripts(
      "<script>A</script>",
      None,
      &RenderOptions::default(),
    )?;

    assert_eq!(
      counter.load(Ordering::SeqCst),
      2,
      "expected exactly one pre-script + one post-script microtask checkpoint during initial streaming parse"
    );
    Ok(())
  }

  #[test]
  fn timer_microtask_checkpoints_notify_executor_and_promise_rejection_hook_composes() -> Result<()> {
    use crate::api::VmJsBrowserTabExecutor;
    use crate::web::events::EventListenerInvoker;

    struct RecordingVmJsExecutor {
      inner: VmJsBrowserTabExecutor,
      after_microtask_checkpoint_calls: Arc<AtomicUsize>,
    }

    impl BrowserTabJsExecutor for RecordingVmJsExecutor {
      fn execute_classic_script(
        &mut self,
        script_text: &str,
        spec: &ScriptElementSpec,
        current_script: Option<NodeId>,
        document: &mut BrowserDocumentDom2,
        event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<()> {
        self
          .inner
          .execute_classic_script(script_text, spec, current_script, document, event_loop)
      }

      fn execute_module_script(
        &mut self,
        script_id: HtmlScriptId,
        script_text: &str,
        spec: &ScriptElementSpec,
        current_script: Option<NodeId>,
        document: &mut BrowserDocumentDom2,
        event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<ModuleScriptExecutionStatus> {
        self
          .inner
          .execute_module_script(script_id, script_text, spec, current_script, document, event_loop)
      }

      fn supports_module_graph_fetch(&self) -> bool {
        self.inner.supports_module_graph_fetch()
      }

      fn fetch_module_graph(
        &mut self,
        spec: &ScriptElementSpec,
        fetcher: Arc<dyn ResourceFetcher>,
        document: &mut BrowserDocumentDom2,
        event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<()> {
        self
          .inner
          .fetch_module_graph(spec, fetcher, document, event_loop)
      }

      fn execute_import_map_script(
        &mut self,
        script_text: &str,
        spec: &ScriptElementSpec,
        current_script: Option<NodeId>,
        document: &mut BrowserDocumentDom2,
        event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<()> {
        self
          .inner
          .execute_import_map_script(script_text, spec, current_script, document, event_loop)
      }

      fn after_microtask_checkpoint(
        &mut self,
        document: &mut BrowserDocumentDom2,
        event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<()> {
        self
          .after_microtask_checkpoint_calls
          .fetch_add(1, Ordering::SeqCst);
        self.inner.after_microtask_checkpoint(document, event_loop)
      }

      fn take_navigation_request(&mut self) -> Option<LocationNavigationRequest> {
        self.inner.take_navigation_request()
      }

      fn on_document_base_url_updated(&mut self, base_url: Option<&str>) {
        self.inner.on_document_base_url_updated(base_url)
      }

      fn on_navigation_committed(&mut self, document_url: Option<&str>) {
        self.inner.on_navigation_committed(document_url)
      }

      fn reset_for_navigation(
        &mut self,
        document_url: Option<&str>,
        document: &mut BrowserDocumentDom2,
        current_script: &CurrentScriptStateHandle,
        js_execution_options: JsExecutionOptions,
      ) -> Result<()> {
        self.inner.reset_for_navigation(
          document_url,
          document,
          current_script,
          js_execution_options,
        )
      }

      fn set_webidl_bindings_host(&mut self, host: &mut dyn webidl_vm_js::WebIdlBindingsHost) {
        self.inner.set_webidl_bindings_host(host)
      }

      fn dispatch_lifecycle_event(
        &mut self,
        target: EventTargetId,
        event: &Event,
        document: &mut BrowserDocumentDom2,
        event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<()> {
        self
          .inner
          .dispatch_lifecycle_event(target, event, document, event_loop)
      }

      fn window_realm_mut(&mut self) -> Option<&mut WindowRealm> {
        self.inner.window_realm_mut()
      }

      fn event_listener_invoker(&self) -> Option<Box<dyn EventListenerInvoker>> {
        self.inner.event_listener_invoker()
      }
    }

    fn get_global_prop(realm: &mut WindowRealm, name: &str) -> Result<Value> {
      let (_vm, realm_ref, heap) = realm.vm_realm_and_heap_mut();
      let mut scope = heap.scope();
      let global = realm_ref.global_object();
      scope
        .push_root(Value::Object(global))
        .map_err(|err| Error::Other(err.to_string()))?;
      let key_s = scope
        .alloc_string(name)
        .map_err(|err| Error::Other(err.to_string()))?;
      scope
        .push_root(Value::String(key_s))
        .map_err(|err| Error::Other(err.to_string()))?;
      let key = PropertyKey::from_string(key_s);
      Ok(
        scope
          .heap()
          .object_get_own_data_property_value(global, &key)
          .map_err(|err| Error::Other(err.to_string()))?
          .unwrap_or(Value::Undefined),
      )
    }

    let after_checkpoint_calls = Arc::new(AtomicUsize::new(0));
    let executor = RecordingVmJsExecutor {
      inner: VmJsBrowserTabExecutor::default(),
      after_microtask_checkpoint_calls: Arc::clone(&after_checkpoint_calls),
    };
    let mut tab = BrowserTab::from_html("", RenderOptions::default(), executor)?;

    // Ensure no constructor/parsing tasks interfere with this test's expected turn ordering.
    tab.event_loop.clear_all_pending_work();

    // --- Timer: executor hook should run after the timer task's microtask checkpoint.
    let before_timer = after_checkpoint_calls.load(Ordering::SeqCst);
    {
      let BrowserTab { host, event_loop, .. } = &mut tab;
      let spec = ScriptElementSpec {
        base_url: host.base_url.clone(),
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
        parser_inserted: false,
        node_id: None,
        script_type: ScriptType::Classic,
      };
      host.executor.execute_classic_script(
        "globalThis.__timer_ran = false;\n\
         setTimeout(function () { globalThis.__timer_ran = true; }, 0);\n",
        &spec,
        None,
        host.document.as_mut(),
        event_loop,
      )?;
    }
    tab
      .event_loop
      .run_until_idle(&mut tab.host, RunLimits::unbounded())?;

    let after_timer = after_checkpoint_calls.load(Ordering::SeqCst);
    assert!(
      after_timer > before_timer,
      "expected after_microtask_checkpoint to run after timer task checkpoint"
    );
    {
      let BrowserTabHost { executor, .. } = &mut tab.host;
      let Some(realm) = executor.window_realm_mut() else {
        return Err(Error::Other("expected vm-js WindowRealm to be active".to_string()));
      };
      assert!(
        matches!(get_global_prop(realm, "__timer_ran")?, Value::Bool(true)),
        "expected timer callback to run"
      );
    }

    // --- Promise rejection: hook must still run and must not clobber executor notifications.
    tab.event_loop.clear_all_pending_work();
    let before_rejection = after_checkpoint_calls.load(Ordering::SeqCst);
    {
      let BrowserTab { host, event_loop, .. } = &mut tab;
      let spec = ScriptElementSpec {
        base_url: host.base_url.clone(),
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
        parser_inserted: false,
        node_id: None,
        script_type: ScriptType::Classic,
      };
      host.executor.execute_classic_script(
        "globalThis.__unhandled = '';\n\
         addEventListener('unhandledrejection', function (e) { globalThis.__unhandled = e.reason; e.preventDefault(); });\n\
         Promise.reject('boom');\n",
        &spec,
        None,
        host.document.as_mut(),
        event_loop,
      )?;
    }
    tab
      .event_loop
      .perform_microtask_checkpoint(&mut tab.host)?;
    tab
      .event_loop
      .run_until_idle(&mut tab.host, RunLimits::unbounded())?;

    let after_rejection = after_checkpoint_calls.load(Ordering::SeqCst);
    assert!(
      after_rejection > before_rejection,
      "expected executor after_microtask_checkpoint calls to continue after promise rejection hook registration"
    );
    {
      let BrowserTabHost { executor, .. } = &mut tab.host;
      let Some(realm) = executor.window_realm_mut() else {
        return Err(Error::Other("expected vm-js WindowRealm to be active".to_string()));
      };
      let unhandled = get_global_prop(realm, "__unhandled")?;
      assert_eq!(
        value_to_string(realm, unhandled),
        "boom"
      );
    }

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
    if host
      .as_any_mut()
      .downcast_mut::<BrowserDocumentDom2>()
      .is_some()
    {
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
  fn accesskit_set_value_updates_dom_state_and_dispatches_input_event() -> Result<()> {
    let mut tab = BrowserTab::from_html_with_vmjs_executor(
      "<!doctype html><html><body></body></html>",
      RenderOptions::default(),
    )?;

    // Create an input element whose current value starts as "old".
    let input_id = tab.host.mutate_dom(|dom| {
      let input = dom.create_element("input", "");
      dom.set_attribute(input, "id", "x").expect("set_attribute");
      dom
        .set_attribute(input, "value", "old")
        .expect("set_attribute");
      let body = dom.body().expect("expected <body>");
      dom.append_child(body, input).expect("append_child");
      (input, true)
    });

    assert_eq!(tab.dom().input_value(input_id).expect("input_value"), "old");

    // Install an `input` listener that reads `event.target.value`. This specifically verifies that
    // the form-control state is updated *before* the trusted `input` event is dispatched.
    {
      let host = &mut tab.host;
      let event_loop = &mut tab.event_loop;

      let mut hooks = VmJsEventLoopHooks::<BrowserTabHost>::new_with_host(host)?;
      hooks.set_event_loop(event_loop);
      let (host_ctx, realm) = host.vm_host_and_window_realm()?;
      realm
        .exec_script_with_host_and_hooks(
          host_ctx,
          &mut hooks,
          r#"
          globalThis.__seen = '';
          const el = document.getElementById('x');
          el.addEventListener('input', (e) => { globalThis.__seen = e.target.value; });
          "#,
        )
        .map_err(|err| Error::Other(err.to_string()))?;
      if let Some(err) = hooks.finish(realm.heap_mut()) {
        return Err(err);
      }
    }

    tab.dispatch_set_value_action(input_id, "new")?;

    assert_eq!(tab.dom().input_value(input_id).expect("input_value"), "new");

    let realm = tab
      .host
      .executor
      .window_realm_mut()
      .expect("expected vm-js WindowRealm");
    let seen = realm
      .exec_script("globalThis.__seen")
      .map_err(|err| Error::Other(err.to_string()))?;
    assert_eq!(value_to_string(realm, seen), "new");
    Ok(())
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
      dom
        .set_attribute(button, "id", "btn")
        .expect("set_attribute");
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
      let host = &mut tab.host;
      let event_loop = &mut tab.event_loop;

      let mut hooks = VmJsEventLoopHooks::<BrowserTabHost>::new_with_host(host)?;
      hooks.set_event_loop(event_loop);
      let (host_ctx, realm) = host.vm_host_and_window_realm()?;
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
    }

    // Host dispatch: should invoke the JS listener with a real vm-js `VmHost`.
    tab.dispatch_click_event(button_id)?;

    let realm = tab
      .host
      .executor
      .window_realm_mut()
      .expect("vm-js executor should expose a WindowRealm");
    let ok = realm
      .exec_script("globalThis.__host_ok")
      .map_err(|err| Error::Other(err.to_string()))?;
    assert!(matches!(ok, Value::Bool(true)));
    Ok(())
  }

  #[test]
  fn vmjs_window_realm_host_fallback_realm_executes_promise_jobs() -> Result<()> {
    // Construct a host whose executor does not expose a WindowRealm. `BrowserTabHost` must still be
    // able to service `WindowRealmHost::vm_host_and_window_realm` for vm-js Promise jobs routed
    // through the host event loop.
    let document =
      BrowserDocumentDom2::from_html("<!doctype html><html></html>", RenderOptions::default())?;
    let mut host = BrowserTabHost::new(
      document,
      Box::new(NoopExecutor::default()),
      TraceHandle::default(),
      JsExecutionOptions::default(),
    )?;
    let mut event_loop = EventLoop::<BrowserTabHost>::new();

    // Execute a script in the host's fallback realm and enqueue a Promise job onto the host event
    // loop microtask queue.
    {
      let (host_ctx, realm) = host.vm_host_and_window_realm()?;
      let mut hooks = VmJsEventLoopHooks::<BrowserTabHost>::new_with_vm_host_and_window_realm(
        host_ctx,
        realm,
        None,
      );
      hooks.set_event_loop(&mut event_loop);
      realm
        .exec_script_with_host_and_hooks(
          host_ctx,
          &mut hooks,
          "globalThis.__x = 0; Promise.resolve().then(() => { globalThis.__x = 1; });",
        )
        .map_err(|err| Error::Other(err.to_string()))?;
      if let Some(err) = hooks.finish(realm.heap_mut()) {
        return Err(err);
      }
    }

    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );

    let x = {
      let (_host_ctx, realm) = host.vm_host_and_window_realm()?;
      realm.exec_script("globalThis.__x").map_err(|err| Error::Other(err.to_string()))?
    };
    assert_eq!(x, Value::Number(1.0));
    Ok(())
  }

  #[test]
  fn host_dispatched_click_event_reuses_single_js_event_object_per_dispatch() -> Result<()> {
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

    // Register two click listeners. The first stores the event object and writes a custom property;
    // the second must observe the same JS object identity and the custom property.
    {
      let host = &mut tab.host;
      let event_loop = &mut tab.event_loop;

      let mut hooks = VmJsEventLoopHooks::<BrowserTabHost>::new_with_host(host)?;
      hooks.set_event_loop(event_loop);
      let (host_ctx, realm) = host.vm_host_and_window_realm()?;
      realm
        .exec_script_with_host_and_hooks(
          host_ctx,
          &mut hooks,
          r#"
          const btn = document.getElementById('btn');
          globalThis.__first = undefined;
          globalThis.__same = false;
          globalThis.__mark2 = undefined;

          btn.addEventListener('click', (e) => {
            globalThis.__first = e;
            e.__mark = 123;
          });
          btn.addEventListener('click', (e) => {
            globalThis.__same = (e === globalThis.__first);
            globalThis.__mark2 = e.__mark;
          });
          "#,
        )
        .map_err(|err| Error::Other(err.to_string()))?;
      if let Some(err) = hooks.finish(realm.heap_mut()) {
        return Err(err);
      }
    }

    tab.dispatch_click_event(button_id)?;

    let realm = tab
      .host
      .executor
      .window_realm_mut()
      .expect("vm-js executor should expose a WindowRealm");
    let same = realm.exec_script("globalThis.__same").map_err(|err| Error::Other(err.to_string()))?;
    let mark2 =
      realm.exec_script("globalThis.__mark2").map_err(|err| Error::Other(err.to_string()))?;
    assert!(matches!(same, Value::Bool(true)));
    assert!(matches!(mark2, Value::Number(n) if n == 123.0));
    Ok(())
  }

  #[test]
  fn host_dispatched_drag_event_exposes_data_transfer_payload() -> Result<()> {
    let mut tab = BrowserTab::from_html_with_vmjs_executor(
      "<!doctype html><html><body></body></html>",
      RenderOptions::default(),
    )?;

    let target_id = tab.host.mutate_dom(|dom| {
      let target = dom.create_element("div", "");
      dom
        .set_attribute(target, "id", "target")
        .expect("set_attribute");
      let body = dom.body().expect("expected <body>");
      dom.append_child(body, target).expect("append_child");
      (target, true)
    });

    {
      let host = &mut tab.host;
      let event_loop = &mut tab.event_loop;

      let mut hooks = VmJsEventLoopHooks::<BrowserTabHost>::new_with_host(host)?;
      hooks.set_event_loop(event_loop);
      let (host_ctx, realm) = host.vm_host_and_window_realm()?;
      realm
        .exec_script_with_host_and_hooks(
          host_ctx,
          &mut hooks,
          r#"
          const el = document.getElementById('target');
          globalThis.__got = undefined;
          globalThis.__dt = undefined;
          globalThis.__same = undefined;
          globalThis.__trusted = undefined;

          el.addEventListener('dragstart', (e) => {
            globalThis.__trusted = e.isTrusted;
            if (globalThis.__dt === undefined) {
              globalThis.__dt = e.dataTransfer;
              globalThis.__same = true;
            } else {
              globalThis.__same = (e.dataTransfer === globalThis.__dt);
            }
            globalThis.__got = e.dataTransfer && e.dataTransfer.getData('text/plain');
          });
          "#,
        )
        .map_err(|err| Error::Other(err.to_string()))?;
      if let Some(err) = hooks.finish(realm.heap_mut()) {
        return Err(err);
      }
    }

    let baseline_roots = {
      let realm = tab
        .host
        .executor
        .window_realm_mut()
        .expect("vm-js executor should expose a WindowRealm");
      realm.heap().persistent_root_count()
    };

    let dt_id = tab.create_data_transfer_for_text("hello")?;

    let after_create_roots = {
      let realm = tab
        .host
        .executor
        .window_realm_mut()
        .expect("vm-js executor should expose a WindowRealm");
      realm.heap().persistent_root_count()
    };
    assert_eq!(
      after_create_roots,
      baseline_roots + 1,
      "expected DataTransfer handle to add one persistent root"
    );

    let mouse = MouseEvent {
      client_x: 1.0,
      client_y: 2.0,
      button: 0,
      buttons: 0,
      detail: 0,
      ctrl_key: false,
      shift_key: false,
      alt_key: false,
      meta_key: false,
      related_target: None,
    };
    let init = EventInit {
      bubbles: true,
      cancelable: true,
      composed: false,
    };

    tab.dispatch_drag_event(target_id, "dragstart", init, mouse, Some(dt_id))?;
    tab.dispatch_drag_event(target_id, "dragstart", init, mouse, Some(dt_id))?;

    {
      let realm = tab
        .host
        .executor
        .window_realm_mut()
        .expect("vm-js executor should expose a WindowRealm");
      let got =
        realm.exec_script("globalThis.__got").map_err(|err| Error::Other(err.to_string()))?;
      let same =
        realm.exec_script("globalThis.__same").map_err(|err| Error::Other(err.to_string()))?;
      let trusted = realm
        .exec_script("globalThis.__trusted")
        .map_err(|err| Error::Other(err.to_string()))?;
      let Value::String(got_s) = got else {
        return Err(Error::Other(format!(
          "expected __got to be a string, got {got:?}"
        )));
      };
      let got = realm
        .heap()
        .get_string(got_s)
        .map_err(|err| Error::Other(err.to_string()))?
        .to_utf8_lossy();
      assert_eq!(got.as_str(), "hello");
      assert!(matches!(same, Value::Bool(true)));
      assert!(matches!(trusted, Value::Bool(true)));
    }

    tab.release_data_transfer(dt_id);

    let after_release_roots = {
      let realm = tab
        .host
        .executor
        .window_realm_mut()
        .expect("vm-js executor should expose a WindowRealm");
      realm.heap().persistent_root_count()
    };
    assert_eq!(
      after_release_roots, baseline_roots,
      "expected DataTransfer release to remove persistent root"
    );

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
      .object_set_prototype(func, Some(realm_ref.intrinsics().function_prototype()))?;
    scope.push_root(Value::Object(func))?;

    let key = alloc_key(&mut scope, "__queueMicrotaskTest")?;
    scope.define_property(global, key, data_desc(Value::Object(func)))?;

    Ok(())
  }

  #[derive(Default)]
  struct VmJsLifecycleExecutor {
    microtask_hook_id: u64,
    realm: Option<WindowRealm>,
  }

  impl VmJsLifecycleExecutor {
    fn new(microtask_hook_id: u64) -> Self {
      Self {
        microtask_hook_id,
        realm: None,
      }
    }

    fn ensure_realm(&mut self) -> Result<()> {
      if self.realm.is_some() {
        return Ok(());
      }
      let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))
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
      document: &mut BrowserDocumentDom2,
      event_loop: &mut EventLoop<BrowserTabHost>,
    ) -> Result<()> {
      self.ensure_realm()?;
      let realm = self.realm.as_mut().expect("realm should be initialized");
      let host_ctx: &mut dyn VmHost = document;
      let mut hooks = VmJsEventLoopHooks::<BrowserTabHost>::new(host_ctx);
      hooks.set_event_loop(event_loop);
      realm
        .exec_script_with_host_and_hooks(host_ctx, &mut hooks, script_text)
        .map_err(|err| Error::Other(err.to_string()))?;
      if let Some(err) = hooks.finish(realm.heap_mut()) {
        return Err(err);
      }
      Ok(())
    }

    fn execute_module_script(
      &mut self,
      _script_id: HtmlScriptId,
      script_text: &str,
      spec: &ScriptElementSpec,
      current_script: Option<NodeId>,
      document: &mut BrowserDocumentDom2,
      event_loop: &mut EventLoop<BrowserTabHost>,
    ) -> Result<ModuleScriptExecutionStatus> {
      // These lifecycle tests exercise event dispatch and microtask checkpoints; module-specific
      // semantics are validated by dedicated tests elsewhere. Treat module scripts like classic
      // scripts here so the executor satisfies the `BrowserTabJsExecutor` contract.
      self.execute_classic_script(script_text, spec, current_script, document, event_loop)?;
      Ok(ModuleScriptExecutionStatus::Completed)
    }

    fn dispatch_lifecycle_event(
      &mut self,
      target: EventTargetId,
      event: &Event,
      document: &mut BrowserDocumentDom2,
      event_loop: &mut EventLoop<BrowserTabHost>,
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
      let source =
        format!("(function(){{const e=new Event({type_lit},{init_lit});{dispatch_expr}}})();",);

      let host_ctx: &mut dyn VmHost = document;
      let mut hooks = VmJsEventLoopHooks::<BrowserTabHost>::new(host_ctx);
      hooks.set_event_loop(event_loop);
      realm
        .exec_script_with_host_and_hooks(host_ctx, &mut hooks, &source)
        .map_err(|err| Error::Other(err.to_string()))?;
      if let Some(err) = hooks.finish(realm.heap_mut()) {
        return Err(err);
      }
      Ok(())
    }

    fn window_realm_mut(&mut self) -> Option<&mut crate::js::WindowRealm> {
      self.realm.as_mut()
    }
  }

  #[test]
  fn vm_js_document_ready_state_tracks_document_lifecycle_transitions() -> Result<()> {
    let document =
      BrowserDocumentDom2::from_html("<!doctype html><html></html>", RenderOptions::default())?;
    let mut host = BrowserTabHost::new(
      document,
      Box::new(NoopExecutor::default()),
      TraceHandle::default(),
      JsExecutionOptions::default(),
    )?;
    let mut event_loop = EventLoop::<BrowserTabHost>::new();
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))
      .map_err(|err| Error::Other(err.to_string()))?;

    let mut eval_ready_state = |host: &mut BrowserTabHost,
                                event_loop: &mut EventLoop<BrowserTabHost>,
                                realm: &mut WindowRealm|
     -> Result<String> {
      let host_ctx: &mut dyn VmHost = host.document.as_mut();
      let mut hooks = VmJsEventLoopHooks::<BrowserTabHost>::new(host_ctx);
      hooks.set_event_loop(event_loop);
      let value = realm
        .exec_script_with_host_and_hooks(host_ctx, &mut hooks, "document.readyState")
        .map_err(|err| Error::Other(err.to_string()))?;
      if let Some(err) = hooks.finish(realm.heap_mut()) {
        return Err(err);
      }
      Ok(value_to_string(realm, value))
    };

    assert_eq!(
      eval_ready_state(&mut host, &mut event_loop, &mut realm)?,
      "loading"
    );

    host.notify_parsing_completed(&mut event_loop)?;
    assert_eq!(
      eval_ready_state(&mut host, &mut event_loop, &mut realm)?,
      "interactive"
    );

    assert!(event_loop.run_next_task(&mut host)?);
    assert_eq!(
      eval_ready_state(&mut host, &mut event_loop, &mut realm)?,
      "interactive"
    );

    assert!(event_loop.run_next_task(&mut host)?);
    assert_eq!(
      eval_ready_state(&mut host, &mut event_loop, &mut realm)?,
      "interactive"
    );

    assert!(event_loop.run_next_task(&mut host)?);
    assert_eq!(
      eval_ready_state(&mut host, &mut event_loop, &mut realm)?,
      "complete"
    );
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

    let document =
      BrowserDocumentDom2::from_html("<!doctype html><html></html>", RenderOptions::default())?;
    let executor = VmJsLifecycleExecutor::new(microtask_hook_id);
    let mut host = BrowserTabHost::new(
      document,
      Box::new(executor),
      TraceHandle::default(),
      JsExecutionOptions::default(),
    )?;
    let mut event_loop = EventLoop::<BrowserTabHost>::new();

    // Register a JS listener that queues a microtask via the test-only native helper.
    let setup_script =
      "document.addEventListener('readystatechange', () => { __queueMicrotaskTest(); });";
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
      fetch_priority: None,
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
  fn browser_tab_rust_dom_event_dispatch_invokes_window_onstorage() -> Result<()> {
    let html = r#"<!doctype html>
      <html>
        <body>
          <script>
            globalThis.__seen = null;
            window.onstorage = (e) => { globalThis.__seen = e.type; };
          </script>
        </body>
      </html>"#;
    let mut tab = BrowserTab::from_html_with_vmjs_executor(html, RenderOptions::default())?;

    let mut event = Event::new(
      "storage",
      EventInit {
        bubbles: false,
        cancelable: false,
        composed: false,
      },
    );
    event.is_trusted = true;
    let _default_not_prevented = tab.host.dispatch_dom_event_in_event_loop(
      EventTargetId::Window,
      event,
      &mut tab.event_loop,
    )?;

    let realm = tab
      .host
      .executor
      .window_realm_mut()
      .expect("expected vm-js WindowRealm");
    let seen = realm
      .exec_script("globalThis.__seen")
      .map_err(|err| Error::Other(err.to_string()))?;
    assert_eq!(value_to_string(realm, seen), "storage");
    Ok(())
  }

  #[test]
  fn script_onload_property_fires_after_external_script_microtasks() -> Result<()> {
    let mut tab = BrowserTab::from_html_with_vmjs_executor("", RenderOptions::default())?;
    tab.register_script_source(
      "a.js",
      r#"
        globalThis.__log = "";
        globalThis.__script = document.currentScript;
        globalThis.__onload_this_ok = false;
        globalThis.__onload_event_type = "";
        globalThis.__script.onload = function (e) {
          globalThis.__log += "onload;";
          globalThis.__onload_this_ok = (this === globalThis.__script);
          globalThis.__onload_event_type = e.type;
        };
        globalThis.__log += "script;";
        Promise.resolve().then(() => { globalThis.__log += "microtask;"; });
      "#,
    );

    tab.navigate_to_html(r#"<!doctype html><script src="a.js"></script>"#, RenderOptions::default())?;
    assert!(matches!(
      tab.run_event_loop_until_idle(RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    ));

    let realm = tab
      .host
      .executor
      .window_realm_mut()
      .expect("expected vm-js WindowRealm");
    let log_value = realm
      .exec_script("globalThis.__log")
      .map_err(|err| Error::Other(err.to_string()))?;
    let log = value_to_string(realm, log_value);
    assert_eq!(log, "script;microtask;onload;");

    let this_ok = realm
      .exec_script("globalThis.__onload_this_ok")
      .map_err(|err| Error::Other(err.to_string()))?;
    assert!(
      matches!(this_ok, Value::Bool(true)),
      "expected onload handler to run with this=script element"
    );

    let event_type_value = realm
      .exec_script("globalThis.__onload_event_type")
      .map_err(|err| Error::Other(err.to_string()))?;
    let event_type = value_to_string(realm, event_type_value);
    assert_eq!(event_type, "load");
    Ok(())
  }

  #[test]
  fn browser_tab_script_execution_log_records_parse_time_script_order() -> Result<()> {
    use crate::js::{ScriptExecutionLogEntry, ScriptSourceSnapshot};
    use std::collections::VecDeque;

    let mut tab = BrowserTab::from_html_with_vmjs_executor("", RenderOptions::default())?;
    tab.enable_script_execution_log(16);
    tab.register_script_source(
      "https://example.com/a.js",
      r#"globalThis.__ext_ran = true;"#,
    );

    tab.navigate_to_html(
      r#"<!doctype html><html><head>
        <script id="ext" src="https://example.com/a.js"></script>
        <script id="inline">globalThis.__inline_ran = true;</script>
      </head><body></body></html>"#,
      RenderOptions::default(),
    )?;
    assert!(matches!(
      tab.run_event_loop_until_idle(RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    ));

    let dom = tab.dom();
    let ext_script = dom
      .get_element_by_id("ext")
      .expect("expected external script element");
    let inline_script = dom
      .get_element_by_id("inline")
      .expect("expected inline script element");
    let expected = VecDeque::from([
      ScriptExecutionLogEntry {
        script_id: ext_script.index(),
        source: ScriptSourceSnapshot::Url {
          url: "https://example.com/a.js".to_string(),
        },
        current_script_node_id: Some(ext_script.index()),
      },
      ScriptExecutionLogEntry {
        script_id: inline_script.index(),
        source: ScriptSourceSnapshot::Inline,
        current_script_node_id: Some(inline_script.index()),
      },
    ]);
    let log = tab
      .script_execution_log()
      .expect("expected script execution log to be enabled");
    assert_eq!(log.entries(), &expected);

    // Script execution log entries are per-navigation: a subsequent navigation should start with an
    // empty log.
    tab.navigate_to_html(
      r#"<!doctype html><script id="second">globalThis.__second_ran = true;</script>"#,
      RenderOptions::default(),
    )?;
    assert!(matches!(
      tab.run_event_loop_until_idle(RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    ));
    let dom = tab.dom();
    let second = dom.get_element_by_id("second").expect("expected script");
    let expected = VecDeque::from([ScriptExecutionLogEntry {
      script_id: second.index(),
      source: ScriptSourceSnapshot::Inline,
      current_script_node_id: Some(second.index()),
    }]);
    let log = tab
      .script_execution_log()
      .expect("expected script execution log to remain enabled");
    assert_eq!(log.entries(), &expected);

    Ok(())
  }

  #[test]
  fn script_onerror_property_fires_for_missing_async_external_script() -> Result<()> {
    let mut tab = BrowserTab::from_html_with_vmjs_executor("", RenderOptions::default())?;

    tab.navigate_to_html(
      r#"<!doctype html><html><head>
        <script async id="bad" src="missing.js"></script>
        <script>
          globalThis.__log = "";
          globalThis.__onerror_this_ok = false;
          globalThis.__onerror_event_type = "";
          const s = document.getElementById("bad");
          globalThis.__script = s;
          s.onerror = function (e) {
            globalThis.__log += "onerror;";
            globalThis.__onerror_this_ok = (this === globalThis.__script);
            globalThis.__onerror_event_type = e.type;
          };
          globalThis.__log += "inline;";
          Promise.resolve().then(() => { globalThis.__log += "microtask;"; });
        </script>
      </head><body></body></html>"#,
      RenderOptions::default(),
    )?;
    assert!(matches!(
      tab.run_event_loop_until_idle(RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    ));

    let realm = tab
      .host
      .executor
      .window_realm_mut()
      .expect("expected vm-js WindowRealm");
    let log_value = realm
      .exec_script("globalThis.__log")
      .map_err(|err| Error::Other(err.to_string()))?;
    let log = value_to_string(realm, log_value);
    assert_eq!(log, "inline;microtask;onerror;");

    let this_ok = realm
      .exec_script("globalThis.__onerror_this_ok")
      .map_err(|err| Error::Other(err.to_string()))?;
    assert!(
      matches!(this_ok, Value::Bool(true)),
      "expected onerror handler to run with this=script element"
    );

    let event_type_value = realm
      .exec_script("globalThis.__onerror_event_type")
      .map_err(|err| Error::Other(err.to_string()))?;
    let event_type = value_to_string(realm, event_type_value);
    assert_eq!(event_type, "error");
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

    assert_eq!(
      log.borrow().len(),
      1,
      "expected exactly one script execution"
    );

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
    assert_eq!(
      element_children[1], injected,
      "expected injected node after <script>"
    );
    assert_eq!(
      element_children[2], after,
      "expected #after node after injected markup"
    );

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

    assert_eq!(
      log.borrow().len(),
      1,
      "expected exactly one script execution"
    );

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
    assert_eq!(
      element_children.len(),
      4,
      "expected 4 element children in <body>"
    );
    assert_eq!(
      element_children[1], a,
      "expected #a to be first injected element"
    );
    assert_eq!(
      element_children[2], b,
      "expected #b to be second injected element"
    );
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

    assert_eq!(
      log.borrow().len(),
      1,
      "expected exactly one script execution"
    );

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
    assert_eq!(
      element_children.len(),
      3,
      "expected 3 element children in <body>"
    );
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
  fn vmjs_document_write_per_call_limit_throws_range_error_with_message() -> Result<()> {
    let html = "<!doctype html><html><body>\
      <script>document.write('hello');</script>\
      </body></html>";
    let options = RenderOptions::default().with_diagnostics_level(crate::api::DiagnosticsLevel::Basic);
    let js_options = JsExecutionOptions {
      max_document_write_bytes_per_call: 4,
      ..JsExecutionOptions::default()
    };
    let tab = BrowserTab::from_html_with_vmjs_and_js_execution_options(html, options, js_options)?;
    let diagnostics = tab
      .diagnostics_snapshot()
      .expect("expected BrowserTab diagnostics to be enabled");
    assert!(
      diagnostics
        .js_exceptions
        .iter()
        .any(|exc| exc.message == "RangeError: document.write exceeded max bytes per call (len=5, limit=4)"),
      "expected RangeError message to be captured, got: {diagnostics:?}"
    );
    Ok(())
  }

  #[test]
  fn vmjs_document_write_total_bytes_limit_throws_range_error_with_message() -> Result<()> {
    let html = "<!doctype html><html><body>\
      <script>document.write('ab');document.write('cd');</script>\
      </body></html>";
    let options = RenderOptions::default().with_diagnostics_level(crate::api::DiagnosticsLevel::Basic);
    let js_options = JsExecutionOptions {
      max_document_write_bytes_per_call: 16,
      max_document_write_bytes_total: 3,
      ..JsExecutionOptions::default()
    };
    let tab = BrowserTab::from_html_with_vmjs_and_js_execution_options(html, options, js_options)?;
    let diagnostics = tab
      .diagnostics_snapshot()
      .expect("expected BrowserTab diagnostics to be enabled");
    assert!(
      diagnostics
        .js_exceptions
        .iter()
        .any(|exc| exc.message == "RangeError: document.write exceeded max cumulative bytes (current=2, add=2, limit=3)"),
      "expected RangeError message to be captured, got: {diagnostics:?}"
    );
    Ok(())
  }

  #[test]
  fn vmjs_document_write_call_count_limit_throws_range_error_with_message() -> Result<()> {
    let html = "<!doctype html><html><body>\
      <script>document.write('a');document.write('b');</script>\
      </body></html>";
    let options = RenderOptions::default().with_diagnostics_level(crate::api::DiagnosticsLevel::Basic);
    let js_options = JsExecutionOptions {
      max_document_write_calls: 1,
      ..JsExecutionOptions::default()
    };
    let tab = BrowserTab::from_html_with_vmjs_and_js_execution_options(html, options, js_options)?;
    let diagnostics = tab
      .diagnostics_snapshot()
      .expect("expected BrowserTab diagnostics to be enabled");
    assert!(
      diagnostics.js_exceptions.iter().any(|exc| exc.message
        == "RangeError: document.write exceeded max call count (limit=1)"),
      "expected RangeError message to be captured, got: {diagnostics:?}"
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

  #[test]
  fn js_document_write_after_parsing_emits_warning_and_is_noop() -> Result<()> {
    let mut options = RenderOptions::default();
    options.diagnostics_level = crate::api::DiagnosticsLevel::Basic;
    let html = r#"<!doctype html><html><body>
      <script>
        setTimeout(() => {
          document.write('<div id="postparse1"></div>');
          document.write('<div id="postparse2"></div>');
          document.body.setAttribute("data-timeout", "1");
        }, 0);
      </script>
    </body></html>"#;

    let mut tab = BrowserTab::from_html_with_vmjs(html, options)?;
    assert_eq!(
      tab.run_event_loop_until_idle(RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );

    let dom = tab.dom();
    let body = dom.body().expect("body should exist");
    assert_eq!(
      dom.get_attribute(body, "data-timeout")
        .expect("get_attribute should succeed"),
      Some("1")
    );
    assert!(
      dom.get_element_by_id("postparse1").is_none(),
      "expected post-parse document.write to be a deterministic no-op"
    );
    assert!(
      dom.get_element_by_id("postparse2").is_none(),
      "expected post-parse document.write to be a deterministic no-op"
    );

    let diagnostics = tab
      .diagnostics_snapshot()
      .expect("diagnostics should be enabled");
    let warnings: Vec<_> = diagnostics
      .console_messages
      .iter()
      .filter(|msg| {
        msg.level == crate::api::ConsoleMessageLevel::Warn
          && msg.message == crate::js::document_write::DOCUMENT_WRITE_IGNORED_NO_PARSER_WARNING
      })
      .collect();
    assert_eq!(
      warnings.len(),
      1,
      "expected exactly one stable post-parse document.write warning, got console_messages={:?}",
      diagnostics.console_messages
    );

    Ok(())
  }

  fn exec_vm_js_dom_script(tab: &mut BrowserTab, source: &str) -> Result<()> {
    // Execute DOM shim script with the real `BrowserDocumentDom2` as the active `VmHost` context so
    // native bindings mutate the host DOM and route invalidations through `DomHost::mutate_dom`.
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))
      .map_err(|err| Error::Other(err.to_string()))?;

    let (host, event_loop) = (&mut tab.host, &mut tab.event_loop);
    let host_ctx: &mut dyn VmHost = host.document.as_mut();
    let mut hooks = VmJsEventLoopHooks::<BrowserTabHost>::new(host_ctx);
    hooks.set_event_loop(event_loop);
    realm
      .exec_script_with_host_and_hooks(host_ctx, &mut hooks, source)
      .map_err(|err| Error::Other(err.to_string()))?;
    if let Some(err) = hooks.finish(realm.heap_mut()) {
      return Err(err);
    }
    Ok(())
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
    )?;
    assert!(
      tab.render_if_needed()?.is_none(),
      "expected no-op reflected attribute writes to avoid invalidation"
    );

    exec_vm_js_dom_script(&mut tab, "document.documentElement.className = 'x';")?;

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
    )?;

    assert!(
      tab.render_if_needed()?.is_some(),
      "expected BrowserTab::render_if_needed to produce a new frame after JS DOM mutation"
    );
    assert!(tab.render_if_needed()?.is_none());

    // Exercise a mutation that removes children without inserting any replacement nodes (empty
    // `textContent`). This previously bypassed generation tracking for raw-pointer shims because it
    // edited `Node.children` directly without calling higher-level mutation APIs.
    exec_vm_js_dom_script(&mut tab, "document.body.textContent = '';")?;
    assert!(
      tab.render_if_needed()?.is_some(),
      "expected BrowserTab::render_if_needed to produce a new frame after JS DOM mutation (textContent clear)"
    );
    assert!(tab.render_if_needed()?.is_none());

    Ok(())
  }

  #[test]
  fn run_until_stable_renders_frame_when_realtime_animation_clock_advances() -> Result<()> {
    use std::time::Duration;

    let html = r#"
      <style>
        html, body { margin: 0; background: white; }
        #box {
          width: 10px;
          height: 10px;
          background: black;
          animation: fade 1000ms linear forwards;
        }
        @keyframes fade {
          from { opacity: 0; }
          to { opacity: 1; }
        }
      </style>
      <div id="box"></div>
    "#;
    let mut tab = BrowserTab::from_html(
      html,
      RenderOptions::new().with_viewport(20, 20),
      NoopExecutor::default(),
    )?;

    let clock = Arc::new(crate::js::VirtualClock::new());
    tab.host.document.set_animation_clock(clock.clone());
    tab.host.document.set_realtime_animations_enabled(true);

    let frame0 = tab.render_frame()?;
    let px0 = rgba_at(&frame0, 5, 5);

    clock.advance(Duration::from_millis(500));

    match tab.run_until_stable(1)? {
      RunUntilStableOutcome::Stable { frames_rendered } => {
        assert!(
          frames_rendered > 0,
          "expected run_until_stable to render at least one frame after animation clock advance"
        );
      }
      other => panic!("expected Stable outcome, got {other:?}"),
    }

    let frame1 = tab
      .render_if_needed()?
      .expect("expected run_until_stable to buffer a freshly rendered frame");
    let px1 = rgba_at(&frame1, 5, 5);
    assert_ne!(
      px1, px0,
      "expected pixels to change after advancing the realtime animation clock"
    );

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
      _script_id: HtmlScriptId,
      script_text: &str,
      spec: &ScriptElementSpec,
      current_script: Option<NodeId>,
      document: &mut BrowserDocumentDom2,
      event_loop: &mut EventLoop<BrowserTabHost>,
    ) -> Result<ModuleScriptExecutionStatus> {
      self.execute_classic_script(script_text, spec, current_script, document, event_loop)?;
      Ok(ModuleScriptExecutionStatus::Completed)
    }
  }

  fn log_index(log: &[String], needle: &str) -> Option<usize> {
    log.iter().position(|line| line.contains(needle))
  }

  #[test]
  fn parser_inserted_external_script_waits_for_script_blocking_stylesheet_and_imports() -> Result<()>
  {
    let temp = tempdir().map_err(Error::Io)?;
    std::fs::write(temp.path().join("imported.css"), "body { color: red; }").map_err(Error::Io)?;
    std::fs::write(temp.path().join("style.css"), r#"@import "imported.css";"#)
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
    let event_loop = EventLoop::new();
    let mut tab = BrowserTab {
      trace: TraceHandle::default(),
      trace_output: None,
      diagnostics: None,
      host,
      next_animation_frame_due: event_loop.now(),
      event_loop,
      pending_frame: None,
      history: TabHistory::new(),
      renderer_dom_mapping_cache: None,
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
    let event_loop = EventLoop::new();
    let mut tab = BrowserTab {
      trace: TraceHandle::default(),
      trace_output: None,
      diagnostics: None,
      host,
      next_animation_frame_due: event_loop.now(),
      event_loop,
      pending_frame: None,
      history: TabHistory::new(),
      renderer_dom_mapping_cache: None,
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
    let (mut host, mut event_loop) = build_host(
      "<script async src=\"https://example.com/a.js\"></script>",
      Rc::clone(&log),
    )?;

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
    host
      .script_blocking_stylesheets
      .register_blocking_stylesheet(0);

    host
      .register_external_script_source("https://example.com/a.js".to_string(), "ASYNC".to_string());

    let mut discovered = host.discover_scripts_best_effort(None);
    assert_eq!(discovered.len(), 1);
    let (node_id, spec) = discovered.pop().unwrap();
    let base_url_at_discovery = spec.base_url.clone();
    host.register_and_schedule_script(node_id, spec, base_url_at_discovery, &mut event_loop)?;

    let _ =
      event_loop.run_until_idle_handling_errors(&mut host, RunLimits::unbounded(), |_err| {})?;

    assert!(
      log.borrow().iter().any(|line| line == "script:ASYNC"),
      "expected async script to execute even with pending stylesheet; log={:?}",
      &*log.borrow()
    );
    Ok(())
  }

  #[test]
  fn defer_script_waits_for_script_blocking_stylesheet() -> Result<()> {
    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let (mut host, mut event_loop) =
      build_host("<script defer src=\"d.js\"></script>", Rc::clone(&log))?;
    host.register_external_script_source("d.js".to_string(), "D".to_string());
    host
      .script_blocking_stylesheets
      .register_blocking_stylesheet(0);

    let mut discovered = host.discover_scripts_best_effort(None);
    assert_eq!(discovered.len(), 1);
    let (node_id, spec) = discovered.pop().unwrap();
    let base_url_at_discovery = spec.base_url.clone();
    host.register_and_schedule_script(node_id, spec, base_url_at_discovery, &mut event_loop)?;
    let actions = host.scheduler.parsing_completed()?;
    host.apply_scheduler_actions(actions, &mut event_loop)?;
    host.notify_parsing_completed(&mut event_loop)?;

    let _ =
      event_loop.run_until_idle_handling_errors(&mut host, RunLimits::unbounded(), |_err| {})?;

    assert!(
      log.borrow().iter().all(|line| line != "script:D"),
      "expected deferred script to be blocked by pending stylesheet; log={:?}",
      &*log.borrow()
    );

    let log_for_task = Rc::clone(&log);
    event_loop.queue_task(TaskSource::Networking, move |host, event_loop| {
      host
        .script_blocking_stylesheets
        .unregister_blocking_stylesheet(0);
      log_for_task.borrow_mut().push("style_done".to_string());
      host.flush_stylesheet_blocked_script_tasks(event_loop)?;
      Ok(())
    })?;

    let _ =
      event_loop.run_until_idle_handling_errors(&mut host, RunLimits::unbounded(), |_err| {})?;

    let entries = log.borrow().clone();
    let style_idx = log_index(&entries, "style_done").expect("expected style_done marker");
    let script_idx = log_index(&entries, "script:D").expect("expected deferred script execution");
    assert!(
      style_idx < script_idx,
      "expected deferred script to execute after stylesheet completion; log={entries:?}"
    );
    Ok(())
  }

  #[test]
  fn parser_inserted_module_script_waits_for_script_blocking_stylesheet() -> Result<()> {
    let mut js_execution_options = JsExecutionOptions::default();
    js_execution_options.supports_module_scripts = true;

    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let (mut host, mut event_loop) = build_host_with_options(
      "<script type=\"module\" src=\"m.js\"></script>",
      Rc::clone(&log),
      js_execution_options,
    )?;
    host.register_external_script_source("m.js".to_string(), "MOD".to_string());
    host
      .script_blocking_stylesheets
      .register_blocking_stylesheet(0);

    let mut discovered = host.discover_scripts_best_effort(None);
    assert_eq!(discovered.len(), 1);
    let (node_id, spec) = discovered.pop().unwrap();
    let base_url_at_discovery = spec.base_url.clone();
    host.register_and_schedule_script(node_id, spec, base_url_at_discovery, &mut event_loop)?;
    let actions = host.scheduler.parsing_completed()?;
    host.apply_scheduler_actions(actions, &mut event_loop)?;
    host.notify_parsing_completed(&mut event_loop)?;

    let _ =
      event_loop.run_until_idle_handling_errors(&mut host, RunLimits::unbounded(), |_err| {})?;

    assert!(
      log.borrow().iter().all(|line| line != "module:MOD"),
      "expected parser-inserted module script to be blocked by pending stylesheet; log={:?}",
      &*log.borrow()
    );

    let log_for_task = Rc::clone(&log);
    event_loop.queue_task(TaskSource::Networking, move |host, event_loop| {
      host
        .script_blocking_stylesheets
        .unregister_blocking_stylesheet(0);
      log_for_task.borrow_mut().push("style_done".to_string());
      host.flush_stylesheet_blocked_script_tasks(event_loop)?;
      Ok(())
    })?;

    let _ =
      event_loop.run_until_idle_handling_errors(&mut host, RunLimits::unbounded(), |_err| {})?;

    let entries = log.borrow().clone();
    let style_idx = log_index(&entries, "style_done").expect("expected style_done marker");
    let module_idx = log_index(&entries, "module:MOD").expect("expected module script execution");
    assert!(
      style_idx < module_idx,
      "expected module script to execute after stylesheet completion; log={entries:?}"
    );
    Ok(())
  }

  #[test]
  fn lifecycle_events_are_observable_and_ordered_with_deferred_scripts_behind_script_blocking_stylesheets(
  ) -> Result<()> {
    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let (mut host, mut event_loop) =
      build_host("<script defer src=\"d.js\"></script>", Rc::clone(&log))?;
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
    add_js_event_listener_log(
      &mut host,
      EventTargetId::Window,
      "load",
      "load",
      Rc::clone(&log),
    )?;

    host
      .script_blocking_stylesheets
      .register_blocking_stylesheet(0);
    host
      .lifecycle
      .register_pending_load_blocker(LoadBlockerKind::StyleSheet);

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

    let _ =
      event_loop.run_until_idle_handling_errors(&mut host, RunLimits::unbounded(), |_err| {})?;

    // With a pending blocking stylesheet, the deferred script must not execute, and therefore
    // DOMContentLoaded/load must not fire.
    assert_eq!(&*log.borrow(), &["rs".to_string()]);

    let log_for_task = Rc::clone(&log);
    event_loop.queue_task(TaskSource::Networking, move |host, event_loop| {
      host
        .script_blocking_stylesheets
        .unregister_blocking_stylesheet(0);
      host
        .lifecycle
        .load_blocker_completed(LoadBlockerKind::StyleSheet, event_loop)?;
      log_for_task.borrow_mut().push("style_done".to_string());
      host.flush_stylesheet_blocked_script_tasks(event_loop)?;
      Ok(())
    })?;

    let _ =
      event_loop.run_until_idle_handling_errors(&mut host, RunLimits::unbounded(), |_err| {})?;

    assert_eq!(host.dom().ready_state(), DocumentReadyState::Complete);
    assert_eq!(
      &*log.borrow(),
      &[
        "rs".to_string(),
        "style_done".to_string(),
        "script:D".to_string(),
        "microtask:D".to_string(),
        "dom".to_string(),
        "rs".to_string(),
        "load".to_string(),
      ],
    );
    Ok(())
  }

  #[test]
  fn async_module_script_can_execute_before_later_parser_inserted_scripts() -> Result<()> {
    // Fast async module scripts (e.g. cache hits) can execute before later parser-inserted scripts.
    // Ensure the streaming parser yields to the event loop at async module script boundaries so the
    // module graph fetch + execution tasks run before parsing continues.
    let mut js_execution_options = JsExecutionOptions::default();
    js_execution_options.supports_module_scripts = true;

    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let renderer = crate::FastRender::builder()
      .dom_scripting_enabled(true)
      .build()?;
    let options = RenderOptions::default();
    let document = BrowserDocumentDom2::new(renderer, "", options.clone())?;

    let host = BrowserTabHost::new(
      document,
      Box::new(TestExecutor { log: Rc::clone(&log) }),
      TraceHandle::default(),
      js_execution_options,
    )?;
    let event_loop = EventLoop::new();
    let mut tab = BrowserTab {
      trace: TraceHandle::default(),
      trace_output: None,
      diagnostics: None,
      host,
      next_animation_frame_due: event_loop.now(),
      event_loop,
      pending_frame: None,
      history: TabHistory::new(),
      renderer_dom_mapping_cache: None,
    };

    let document_url = "https://example.com/".to_string();
    tab
      .host
      .reset_scripting_state(Some(document_url.clone()), ReferrerPolicy::default())?;
    tab.host.register_external_script_source(
      "https://example.com/module.js".to_string(),
      "MOD".to_string(),
    );

    let html = r#"<!doctype html>
      <script type="module" async src="https://example.com/module.js"></script>
      <script>CLASSIC</script>
    "#;
    tab.parse_html_streaming_and_schedule_scripts(html, Some(&document_url), &options)?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    let entries = log.borrow().clone();
    let module_idx = log_index(&entries, "module:MOD").expect("expected module script execution");
    let classic_idx =
      log_index(&entries, "script:CLASSIC").expect("expected classic script execution");
    assert!(
      module_idx < classic_idx,
      "expected async module script to execute before later parser-inserted scripts; log={entries:?}"
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
    let event_loop = EventLoop::new();
    let mut tab = BrowserTab {
      trace: TraceHandle::default(),
      trace_output: None,
      diagnostics: None,
      host,
      next_animation_frame_due: event_loop.now(),
      event_loop,
      pending_frame: None,
      history: TabHistory::new(),
      renderer_dom_mapping_cache: None,
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
        self.log.borrow_mut().push(format!(
          "{script_text}:before={has_before} after={has_after}"
        ));
        Ok(())
      }

      fn execute_module_script(
        &mut self,
        _script_id: HtmlScriptId,
        script_text: &str,
        spec: &ScriptElementSpec,
        current_script: Option<NodeId>,
        document: &mut BrowserDocumentDom2,
        event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<ModuleScriptExecutionStatus> {
        self.execute_classic_script(script_text, spec, current_script, document, event_loop)?;
        Ok(ModuleScriptExecutionStatus::Completed)
      }
    }

    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let executor = DomSnapshotExecutor {
      log: Rc::clone(&log),
    };
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
        _script_id: HtmlScriptId,
        script_text: &str,
        spec: &ScriptElementSpec,
        current_script: Option<NodeId>,
        document: &mut BrowserDocumentDom2,
        event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<ModuleScriptExecutionStatus> {
        self.execute_classic_script(script_text, spec, current_script, document, event_loop)?;
        Ok(ModuleScriptExecutionStatus::Completed)
      }
    }

    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let executor = SpecLoggingExecutor {
      log: Rc::clone(&log),
    };
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

    let event_loop = EventLoop::new();
    let mut tab = BrowserTab {
      trace: TraceHandle::default(),
      trace_output: None,
      diagnostics: None,
      host,
      next_animation_frame_due: event_loop.now(),
      event_loop,
      pending_frame: None,
      history: TabHistory::new(),
      renderer_dom_mapping_cache: None,
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
        _script_id: HtmlScriptId,
        script_text: &str,
        spec: &ScriptElementSpec,
        current_script: Option<NodeId>,
        document: &mut BrowserDocumentDom2,
        event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<ModuleScriptExecutionStatus> {
        self.execute_classic_script(script_text, spec, current_script, document, event_loop)?;
        Ok(ModuleScriptExecutionStatus::Completed)
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
        _script_id: HtmlScriptId,
        script_text: &str,
        spec: &ScriptElementSpec,
        current_script: Option<NodeId>,
        document: &mut BrowserDocumentDom2,
        event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<ModuleScriptExecutionStatus> {
        // Tests only exercise classic scripts today; treat module scripts the same way so the
        // executor remains usable as module support is incrementally added.
        self.execute_classic_script(script_text, spec, current_script, document, event_loop)?;
        Ok(ModuleScriptExecutionStatus::Completed)
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
    assert_eq!(
      tab.history.current().map(|e| e.url.as_str()),
      Some(page2_url)
    );
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
        _script_id: HtmlScriptId,
        script_text: &str,
        spec: &ScriptElementSpec,
        current_script: Option<NodeId>,
        document: &mut BrowserDocumentDom2,
        event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<ModuleScriptExecutionStatus> {
        // Navigation tests don't distinguish script types; treat module scripts as classic.
        self.execute_classic_script(script_text, spec, current_script, document, event_loop)?;
        Ok(ModuleScriptExecutionStatus::Completed)
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
    assert_eq!(
      tab.history.current().map(|e| e.url.as_str()),
      Some(page2_url)
    );
    assert!(
      !tab.history.can_go_back(),
      "expected replace to not push history"
    );
    Ok(())
  }

  #[test]
  fn click_handler_location_navigation_commits_navigation() -> Result<()> {
    let page1_url = "https://example.com/page1.html";
    let page2_url = "https://example.com/page2.html";

    let page1_html = format!(
      r#"<!doctype html><html><body>
        <div id=page1></div>
        <button id=btn>go</button>
        <script>
          document.getElementById("btn").addEventListener("click", () => {{
            location.href = {page2_url:?};
          }});
        </script>
      </body></html>"#
    );
    let page2_html = "<!doctype html><html><body><div id=page2></div></body></html>";

    let mut tab = BrowserTab::from_html_with_vmjs_executor("", RenderOptions::default())?;
    tab.register_html_source(page1_url, page1_html);
    tab.register_html_source(page2_url, page2_html);

    tab.navigate_to_url(page1_url, RenderOptions::default())?;
    let btn = tab
      .dom()
      .get_element_by_id("btn")
      .expect("expected #btn to exist on page1");

    tab.dispatch_click_event(btn)?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    assert!(
      tab.dom().get_element_by_id("page2").is_some(),
      "expected click-handler navigation to commit page2"
    );
    assert!(
      tab.dom().get_element_by_id("page1").is_none(),
      "expected click-handler navigation to replace page1 DOM"
    );
    Ok(())
  }

  #[test]
  fn script_onload_location_navigation_commits_navigation() -> Result<()> {
    let page1_url = "https://example.com/page1.html";
    let page2_url = "https://example.com/page2.html";
    let script_url = "https://example.com/a.js";

    let page1_html = format!(
      r#"<!doctype html><html><body>
        <div id=page1></div>
        <script>
          const s = document.createElement("script");
          s.src = {script_url:?};
          s.onload = () => {{
            location.href = {page2_url:?};
          }};
          document.body.appendChild(s);
        </script>
      </body></html>"#
    );
    let page2_html = "<!doctype html><html><body><div id=page2></div></body></html>";

    let mut tab = BrowserTab::from_html_with_vmjs_executor("", RenderOptions::default())?;
    tab.register_html_source(page1_url, page1_html);
    tab.register_html_source(page2_url, page2_html);
    tab.register_script_source(script_url, "/* loaded */");

    tab.navigate_to_url(page1_url, RenderOptions::default())?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    assert!(
      tab.dom().get_element_by_id("page2").is_some(),
      "expected script onload navigation to commit page2"
    );
    assert!(
      tab.dom().get_element_by_id("page1").is_none(),
      "expected script onload navigation to replace page1 DOM"
    );
    Ok(())
  }

  #[test]
  fn microtask_location_navigation_commits_navigation() -> Result<()> {
    let page1_url = "https://example.com/page1.html";
    let page2_url = "https://example.com/page2.html";

    let page1_html = format!(
      r#"<!doctype html><html><body>
        <div id=page1></div>
        <script>
          Promise.resolve().then(() => {{
            location.href = {page2_url:?};
          }});
        </script>
      </body></html>"#
    );
    let page2_html = "<!doctype html><html><body><div id=page2></div></body></html>";

    let mut tab = BrowserTab::from_html_with_vmjs_executor("", RenderOptions::default())?;
    tab.register_html_source(page1_url, page1_html);
    tab.register_html_source(page2_url, page2_html);

    tab.navigate_to_url(page1_url, RenderOptions::default())?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    assert!(
      tab.dom().get_element_by_id("page2").is_some(),
      "expected microtask navigation to commit page2"
    );
    assert!(
      tab.dom().get_element_by_id("page1").is_none(),
      "expected microtask navigation to replace page1 DOM"
    );
    Ok(())
  }

  #[test]
  fn beforeunload_can_cancel_location_navigation_without_clearing_event_loop_work() -> Result<()> {
    let page1_url = "https://example.com/page1.html";
    let page2_url = "https://example.com/page2.html";
    let page2_html = "<!doctype html><html><body><div id=page2></div></body></html>";
    let page1_html = format!(
      r#"<!doctype html><html><body>
        <div id=page1></div>
        <script>
          window.onbeforeunload = () => "stay";
          window.onpagehide = () => document.body.setAttribute("data-pagehide", "1");
          window.onunload = () => document.body.setAttribute("data-unload", "1");
          // Schedule work before attempting to navigate; this must still run if navigation is canceled.
          setTimeout(() => {{
            document.body.setAttribute("data-after", location.href);
          }}, 0);
          location.href = {page2_url:?};
        </script>
      </body></html>"#
    );

    let mut tab = BrowserTab::from_html_with_vmjs_executor("", RenderOptions::default())?;
    tab.register_html_source(page1_url, page1_html);
    tab.register_html_source(page2_url, page2_html);

    tab.navigate_to_url(page1_url, RenderOptions::default())?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    assert!(
      tab.dom().get_element_by_id("page1").is_some(),
      "expected navigation to remain on page1 after beforeunload cancellation"
    );
    assert!(
      tab.dom().get_element_by_id("page2").is_none(),
      "expected canceled navigation not to commit page2"
    );

    assert_eq!(
      tab.history.len(),
      1,
      "expected canceled navigation not to push a history entry"
    );
    assert_eq!(
      tab.history.current().map(|e| e.url.as_str()),
      Some(page1_url),
      "expected canceled navigation to keep the current history URL"
    );

    let dom = tab.dom();
    let body = dom.body().expect("body should exist");
    assert_eq!(
      dom.get_attribute(body, "data-after").expect("get_attribute should succeed"),
      Some(page1_url),
      "expected location.href to be restored after canceling navigation"
    );
    assert_eq!(
      dom.get_attribute(body, "data-pagehide").expect("get_attribute should succeed"),
      None,
      "expected pagehide not to fire when navigation is canceled"
    );
    assert_eq!(
      dom.get_attribute(body, "data-unload").expect("get_attribute should succeed"),
      None,
      "expected unload not to fire when navigation is canceled"
    );
    Ok(())
  }

  #[test]
  fn navigation_lifecycle_events_fire_in_order_with_persisted_false() -> Result<()> {
    let page1_url = "https://example.com/page1.html";
    let page2_url = "https://example.com/page2.html";

    let page1_html = format!(
      r#"<!doctype html><html><body>
        <div id=page1></div>
        <script>
          document.cookie = "log=;path=/";
          function getLog() {{
            const part = document.cookie.split("; ").find(p => p.startsWith("log="));
            return part ? part.slice(4) : "";
          }}
          function append(s) {{
            const cur = getLog();
            document.cookie = "log=" + (cur ? (cur + ",") : "") + s + ";path=/";
          }}
          window.addEventListener("beforeunload", () => append("beforeunload"));
          window.addEventListener("pagehide", (e) => append("pagehide:" + e.persisted));
          window.addEventListener("unload", () => append("unload"));
          location.href = {page2_url:?};
        </script>
      </body></html>"#
    );

    let page2_html = r#"<!doctype html><html><body>
        <div id=page2></div>
        <script>
          function getLog() {
            const part = document.cookie.split("; ").find(p => p.startsWith("log="));
            return part ? part.slice(4) : "";
          }
          function append(s) {
            const cur = getLog();
            document.cookie = "log=" + (cur ? (cur + ",") : "") + s + ";path=/";
          }
          window.addEventListener("pageshow", (e) => append("pageshow:" + e.persisted));
          document.addEventListener("DOMContentLoaded", () => append("DOMContentLoaded"));
          window.addEventListener("load", () => {
            append("load");
            document.body.setAttribute("data-log", getLog());
          });
        </script>
      </body></html>"#;

    let mut tab = BrowserTab::from_html_with_vmjs_executor("", RenderOptions::default())?;
    tab.register_html_source(page1_url, page1_html);
    tab.register_html_source(page2_url, page2_html.to_string());

    tab.navigate_to_url(page1_url, RenderOptions::default())?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    assert!(
      tab.dom().get_element_by_id("page2").is_some(),
      "expected navigation to commit page2"
    );
    let dom = tab.dom();
    let body = dom.body().expect("body should exist");
    assert_eq!(
      dom.get_attribute(body, "data-log").expect("get_attribute should succeed"),
      Some("beforeunload,pagehide:false,unload,pageshow:false,DOMContentLoaded,load"),
    );
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
    assert_eq!(
      tab.history.current().map(|e| e.url.as_str()),
      Some(page1_url)
    );

    // Simulate a script-triggered `location.replace(page2_url)` during the event loop.
    tab.host.pending_navigation = Some(LocationNavigationRequest {
      url: page2_url.to_string(),
      replace: true,
    });
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    assert_eq!(tab.history.len(), 1);
    assert_eq!(
      tab.history.current().map(|e| e.url.as_str()),
      Some(page2_url)
    );
    Ok(())
  }

  #[test]
  fn take_pending_navigation_request_exposes_js_driven_navigation_without_committing() -> Result<()> {
    let html = r#"<!doctype html><html><body>
        <button id="btn">go</button>
        <script>
          document.getElementById("btn").addEventListener("click", () => {
            window.location.href = "https://example.com/next";
          });
        </script>
      </body></html>"#;
    let mut tab = BrowserTab::from_html_with_vmjs_and_document_url(
      html,
      "https://example.com/start",
      RenderOptions::default(),
    )?;

    let button = tab
      .dom()
      .get_element_by_id("btn")
      .expect("expected button to exist");
    // `window.location` triggers a vm-js interrupt/termination to abort the current JS turn. The
    // dispatch helper surfaces that as an error; for this test we only care that the navigation
    // request is recorded.
    let _ = tab.dispatch_click_event(button);

    let req = tab
      .take_pending_navigation_request()
      .expect("expected JS-driven navigation request");
    assert_eq!(req.url, "https://example.com/next");
    assert!(!req.replace);
    assert!(
      tab.take_pending_navigation_request().is_none(),
      "expected navigation request to be cleared"
    );
    assert!(
      tab.dom().get_element_by_id("btn").is_some(),
      "expected navigation to not be committed by take_pending_navigation_request"
    );

    // Also cover the case where the request is already stored in `BrowserTabHost.pending_navigation`
    // (for example after script execution boundaries where the host polls the executor).
    use std::time::Duration;
    tab.host.pending_navigation = Some(LocationNavigationRequest {
      url: "https://example.com/host".to_string(),
      replace: true,
    });
    tab.host.pending_navigation_deadline =
      Some(RenderDeadline::new(Some(Duration::from_millis(5)), None));
    let (req, deadline) = tab
      .take_pending_navigation_request_with_deadline()
      .expect("expected host pending navigation request");
    assert_eq!(req.url, "https://example.com/host");
    assert!(req.replace);
    assert!(
      deadline.is_some_and(|d| d.is_enabled()),
      "expected deadline to be returned"
    );
    assert!(
      tab.host.pending_navigation.is_none() && tab.host.pending_navigation_deadline.is_none(),
      "expected host pending navigation state to be cleared"
    );

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
        _script_id: HtmlScriptId,
        script_text: &str,
        spec: &ScriptElementSpec,
        current_script: Option<NodeId>,
        document: &mut BrowserDocumentDom2,
        event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<ModuleScriptExecutionStatus> {
        self.execute_classic_script(script_text, spec, current_script, document, event_loop)?;
        Ok(ModuleScriptExecutionStatus::Completed)
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
    let event_loop = EventLoop::new();
    let mut tab = BrowserTab {
      trace: TraceHandle::default(),
      trace_output: None,
      diagnostics: None,
      host,
      next_animation_frame_due: event_loop.now(),
      event_loop,
      pending_frame: None,
      history: TabHistory::new(),
      renderer_dom_mapping_cache: None,
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
        _script_id: HtmlScriptId,
        script_text: &str,
        spec: &ScriptElementSpec,
        current_script: Option<NodeId>,
        document: &mut BrowserDocumentDom2,
        event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<ModuleScriptExecutionStatus> {
        self.execute_classic_script(script_text, spec, current_script, document, event_loop)?;
        Ok(ModuleScriptExecutionStatus::Completed)
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
    let event_loop = EventLoop::new();
    let mut tab = BrowserTab {
      trace: TraceHandle::default(),
      trace_output: None,
      diagnostics: None,
      host,
      next_animation_frame_due: event_loop.now(),
      event_loop,
      pending_frame: None,
      history: TabHistory::new(),
      renderer_dom_mapping_cache: None,
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
        _script_id: HtmlScriptId,
        script_text: &str,
        spec: &ScriptElementSpec,
        current_script: Option<NodeId>,
        document: &mut BrowserDocumentDom2,
        event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<ModuleScriptExecutionStatus> {
        self.execute_classic_script(script_text, spec, current_script, document, event_loop)?;
        Ok(ModuleScriptExecutionStatus::Completed)
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
        _script_id: HtmlScriptId,
        script_text: &str,
        spec: &ScriptElementSpec,
        current_script: Option<NodeId>,
        document: &mut BrowserDocumentDom2,
        event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<ModuleScriptExecutionStatus> {
        self.execute_classic_script(script_text, spec, current_script, document, event_loop)?;
        Ok(ModuleScriptExecutionStatus::Completed)
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
        _script_id: HtmlScriptId,
        script_text: &str,
        spec: &ScriptElementSpec,
        current_script: Option<NodeId>,
        document: &mut BrowserDocumentDom2,
        event_loop: &mut EventLoop<BrowserTabHost>,
      ) -> Result<ModuleScriptExecutionStatus> {
        self.execute_classic_script(script_text, spec, current_script, document, event_loop)?;
        Ok(ModuleScriptExecutionStatus::Completed)
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

    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(SlowStyleFetcher { url: style_url });
    let executor = SleepyNavigateExecutor {
      target_url: target_url.clone(),
      pending: None,
    };

    // Use a relatively generous timeout so this deadline-based test remains stable even under
    // parallel `cargo test` load. We'll force the deadline to elapse deterministically before
    // committing the navigation below.
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
    std::fs::write(temp.path().join("print.css"), "body { color: green; }").map_err(Error::Io)?;
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
    let event_loop = EventLoop::new();
    let mut tab = BrowserTab {
      trace: TraceHandle::default(),
      trace_output: None,
      diagnostics: None,
      host,
      next_animation_frame_due: event_loop.now(),
      event_loop,
      pending_frame: None,
      history: TabHistory::new(),
      renderer_dom_mapping_cache: None,
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
    let fetcher = Arc::new(ScriptSourceFetcher::new(&[(
      "https://evil.com/a.js",
      "EVIL",
    )]));
    let fetcher_for_renderer: Arc<dyn crate::resource::ResourceFetcher> = fetcher.clone();
    let (mut host, mut event_loop) = build_host_with_fetcher(
      "<script src=\"https://evil.com/a.js\"></script>",
      Rc::clone(&log),
      fetcher_for_renderer,
    )?;

    host.reset_scripting_state(
      Some("https://example.com/".to_string()),
      ReferrerPolicy::default(),
    )?;
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
    let (mut host, mut event_loop) = build_host_with_fetcher(
      "<script src=\"https://evil.com/a.js\"></script>",
      Rc::clone(&log),
      fetcher_for_renderer,
    )?;

    host.reset_scripting_state(
      Some("https://example.com/".to_string()),
      ReferrerPolicy::default(),
    )?;
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
  fn csp_blocks_external_module_entry() -> Result<()> {
    let mut js_options = JsExecutionOptions::default();
    js_options.supports_module_scripts = true;

    let fetcher = Arc::new(RecordingScriptFetcher::new(&[(
      "https://evil.com/a.js",
      r#"document.body.setAttribute("data-module-ran", "1");"#,
    )]));
    let fetcher_trait: Arc<dyn crate::resource::ResourceFetcher> = fetcher.clone();

    let html = r#"<!doctype html><head>
      <meta http-equiv="Content-Security-Policy" content="script-src 'self'">
      </head><body>
      <script type="module" src="https://evil.com/a.js"></script>
      </body>"#;
    let mut tab = BrowserTab::from_html_with_vmjs_and_document_url_and_fetcher_and_js_execution_options(
      html,
      "https://example.com/",
      RenderOptions::default(),
      fetcher_trait,
      js_options,
    )?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    assert!(
      fetcher.calls().is_empty(),
      "expected CSP to block module fetch, got calls={:?}",
      fetcher.calls()
    );
    let dom = tab.dom();
    let body = dom.body().expect("body should exist");
    assert_eq!(
      dom.get_attribute(body, "data-module-ran")
        .expect("get_attribute should succeed"),
      None
    );
    Ok(())
  }

  #[test]
  fn csp_blocks_inline_module_without_nonce_or_hash() -> Result<()> {
    let mut js_options = JsExecutionOptions::default();
    js_options.supports_module_scripts = true;

    let html = r#"<!doctype html><head>
      <meta http-equiv="Content-Security-Policy" content="script-src 'self'">
      </head><body>
      <script type="module">
        document.body.setAttribute("data-inline-module", "1");
      </script>
      </body>"#;
    let mut tab = BrowserTab::from_html_with_vmjs_and_document_url_and_fetcher_and_js_execution_options(
      html,
      "https://example.com/",
      RenderOptions::default(),
      Arc::new(RecordingScriptFetcher::new(&[])),
      js_options,
    )?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    let dom = tab.dom();
    let body = dom.body().expect("body should exist");
    assert_eq!(
      dom.get_attribute(body, "data-inline-module")
        .expect("get_attribute should succeed"),
      None
    );
    Ok(())
  }

  #[test]
  fn csp_allows_inline_module_with_matching_nonce() -> Result<()> {
    let mut js_options = JsExecutionOptions::default();
    js_options.supports_module_scripts = true;

    let html = r#"<!doctype html><head>
      <meta http-equiv="Content-Security-Policy" content="script-src 'nonce-abc'">
      </head><body>
      <script type="module" nonce="abc">
        document.body.setAttribute("data-inline-module-nonce", "1");
      </script>
      </body>"#;
    let mut tab = BrowserTab::from_html_with_vmjs_and_document_url_and_fetcher_and_js_execution_options(
      html,
      "https://example.com/",
      RenderOptions::default(),
      Arc::new(RecordingScriptFetcher::new(&[])),
      js_options,
    )?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    let dom = tab.dom();
    let body = dom.body().expect("body should exist");
    assert_eq!(
      dom.get_attribute(body, "data-inline-module-nonce")
        .expect("get_attribute should succeed"),
      Some("1")
    );
    Ok(())
  }

  #[test]
  fn csp_blocks_module_dependency_import() -> Result<()> {
    let mut js_options = JsExecutionOptions::default();
    js_options.supports_module_scripts = true;

    let fetcher = Arc::new(RecordingScriptFetcher::new(&[
      (
        "https://example.com/entry.js",
        r#"import "https://evil.com/dep.js";
document.body.setAttribute("data-entry", "1");"#,
      ),
      ("https://evil.com/dep.js", r#"export const x = 1;"#),
    ]));
    let fetcher_trait: Arc<dyn crate::resource::ResourceFetcher> = fetcher.clone();

    let html = r#"<!doctype html><head>
      <meta http-equiv="Content-Security-Policy" content="script-src 'self'">
      </head><body>
      <script type="module" src="https://example.com/entry.js"></script>
      </body>"#;
    let mut tab = BrowserTab::from_html_with_vmjs_and_document_url_and_fetcher_and_js_execution_options(
      html,
      "https://example.com/",
      RenderOptions::default(),
      fetcher_trait,
      js_options,
    )?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    assert_eq!(
      fetcher.calls(),
      vec!["https://example.com/entry.js".to_string()],
      "expected CSP to block module dependency fetch"
    );
    let dom = tab.dom();
    let body = dom.body().expect("body should exist");
    assert_eq!(
      dom.get_attribute(body, "data-entry")
        .expect("get_attribute should succeed"),
      None
    );
    Ok(())
  }

  #[test]
  fn csp_blocks_dynamic_import_of_disallowed_url() -> Result<()> {
    let mut js_options = JsExecutionOptions::default();
    js_options.supports_module_scripts = true;

    let fetcher = Arc::new(RecordingScriptFetcher::new(&[(
      "https://evil.com/dyn.js",
      r#"export const value = 1;"#,
    )]));
    let fetcher_trait: Arc<dyn crate::resource::ResourceFetcher> = fetcher.clone();

    let html = r#"<!doctype html><head>
      <meta http-equiv="Content-Security-Policy" content="script-src 'nonce-abc' 'self'">
      </head><body>
      <script type="module" nonce="abc">
        try {
          await import("https://evil.com/dyn.js");
          document.body.setAttribute("data-dynamic", "loaded");
        } catch (e) {
          document.body.setAttribute("data-dynamic", "blocked");
        }
      </script>
      </body>"#;
    let mut tab = BrowserTab::from_html_with_vmjs_and_document_url_and_fetcher_and_js_execution_options(
      html,
      "https://example.com/",
      RenderOptions::default(),
      fetcher_trait,
      js_options,
    )?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    assert!(
      fetcher.calls().is_empty(),
      "expected CSP to block dynamic import fetch, got calls={:?}",
      fetcher.calls()
    );
    let dom = tab.dom();
    let body = dom.body().expect("body should exist");
    assert_eq!(
      dom.get_attribute(body, "data-dynamic")
        .expect("get_attribute should succeed"),
      Some("blocked")
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
    host.js_events.add_js_event_listener(
      target,
      type_,
      callback,
      AddEventListenerOptions::default(),
    )?;
    Ok(())
  }

  #[test]
  fn lifecycle_events_are_observable_via_js_listeners_and_ordered_with_deferred_scripts(
  ) -> Result<()> {
    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let (mut host, mut event_loop) =
      build_host("<script defer src=\"d.js\"></script>", Rc::clone(&log))?;
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
    add_js_event_listener_log(
      &mut host,
      EventTargetId::Window,
      "load",
      "load",
      Rc::clone(&log),
    )?;

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
  fn window_load_waits_for_images_but_dom_content_loaded_does_not() -> Result<()> {
    #[derive(Debug, Default, Clone)]
    struct ImageFetcher {
      image_fetches: Arc<AtomicUsize>,
      destinations: Arc<Mutex<Vec<FetchDestination>>>,
    }

    impl ImageFetcher {
      fn image_fetch_count(&self) -> usize {
        self.image_fetches.load(Ordering::SeqCst)
      }
    }

    impl ResourceFetcher for ImageFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        Err(Error::Other("ImageFetcher::fetch should not be used".to_string()))
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        self
          .destinations
          .lock()
          .unwrap_or_else(|poisoned| poisoned.into_inner())
          .push(req.destination);

        if matches!(req.destination, FetchDestination::Image | FetchDestination::ImageCors) {
          self.image_fetches.fetch_add(1, Ordering::SeqCst);
          Ok(FetchedResource::new(
            // We don't decode in this test; any deterministic bytes are fine.
            b"fake-png-bytes".to_vec(),
            Some("image/png".to_string()),
          ))
        } else {
          Err(Error::Other(format!(
            "unexpected fetch destination {:?} for url={}",
            req.destination, req.url
          )))
        }
      }
    }

    let fetcher = Arc::new(ImageFetcher::default());
    let html = r#"<!doctype html><body>
      <img src="https://example.com/a.png">
      <script>
        document.addEventListener('DOMContentLoaded', function () {
          document.body.setAttribute('data-dom', '1');
        });
        window.addEventListener('load', function () {
          document.body.setAttribute('data-load', '1');
        });
      </script>
    </body>"#;

    let mut tab = BrowserTab::from_html_with_vmjs_and_document_url_and_fetcher(
      html,
      "https://example.com/",
      RenderOptions::default(),
      Arc::clone(&fetcher) as Arc<dyn ResourceFetcher>,
    )?;

    let body = tab.dom().body().expect("body should exist");
    assert_eq!(
      tab.dom()
        .get_attribute(body, "data-dom")
        .expect("get_attribute"),
      None
    );
    assert_eq!(
      tab.dom()
        .get_attribute(body, "data-load")
        .expect("get_attribute"),
      None
    );

    // DocumentLifecycle queues two tasks at parsing completion:
    // - DOMContentLoaded barrier
    // - DOMContentLoaded
    {
      let BrowserTab { host, event_loop, .. } = &mut tab;
      assert!(event_loop.run_next_task(host)?); // barrier
      assert!(event_loop.run_next_task(host)?); // DOMContentLoaded
    }

    // DOMContentLoaded must be observable even while the image is still pending.
    assert_eq!(
      tab.dom()
        .get_attribute(body, "data-dom")
        .expect("get_attribute"),
      Some("1")
    );
    assert_eq!(fetcher.image_fetch_count(), 0);
    assert_eq!(
      tab.dom()
        .get_attribute(body, "data-load")
        .expect("get_attribute"),
      None
    );

    // Next turn: image networking task runs and completes the load blocker, which queues `load`.
    {
      let BrowserTab { host, event_loop, .. } = &mut tab;
      assert!(event_loop.run_next_task(host)?); // image fetch task
    }
    assert_eq!(fetcher.image_fetch_count(), 1);
    assert_eq!(
      tab.dom()
        .get_attribute(body, "data-load")
        .expect("get_attribute"),
      None
    );

    // Final turn: `load` event dispatch.
    {
      let BrowserTab { host, event_loop, .. } = &mut tab;
      assert!(event_loop.run_next_task(host)?); // load
    }

    assert_eq!(
      tab.dom()
        .get_attribute(body, "data-load")
        .expect("get_attribute"),
      Some("1")
    );
    Ok(())
  }

  #[test]
  fn window_load_does_not_wait_for_csp_blocked_images() -> Result<()> {
    #[derive(Debug, Default, Clone)]
    struct RecordingFetcher {
      calls: Arc<AtomicUsize>,
    }

    impl ResourceFetcher for RecordingFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        Err(Error::Other("RecordingFetcher::fetch should not be used".to_string()))
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        // If this is called, the page load logic attempted to fetch a resource. Record and return
        // deterministic bytes so the test can assert the call count precisely.
        let _ = req;
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(FetchedResource::new(
          b"fake-bytes".to_vec(),
          Some("application/octet-stream".to_string()),
        ))
      }
    }

    let fetcher = Arc::new(RecordingFetcher::default());
    let html = r#"<!doctype html>
      <head>
        <meta http-equiv="Content-Security-Policy" content="img-src 'none'">
      </head>
      <body>
        <img src="https://example.com/a.png">
        <script>
          document.addEventListener('DOMContentLoaded', function () {
            document.body.setAttribute('data-dom', '1');
          });
          window.addEventListener('load', function () {
            document.body.setAttribute('data-load', '1');
          });
        </script>
      </body>"#;

    let mut tab = BrowserTab::from_html_with_vmjs_and_document_url_and_fetcher(
      html,
      "https://example.com/",
      RenderOptions::default(),
      Arc::clone(&fetcher) as Arc<dyn ResourceFetcher>,
    )?;

    let body = tab.dom().body().expect("body should exist");

    // DOMContentLoaded barrier + DOMContentLoaded.
    {
      let BrowserTab { host, event_loop, .. } = &mut tab;
      assert!(event_loop.run_next_task(host)?); // barrier
      assert!(event_loop.run_next_task(host)?); // DOMContentLoaded
    }

    assert_eq!(
      tab.dom()
        .get_attribute(body, "data-dom")
        .expect("get_attribute"),
      Some("1")
    );
    assert_eq!(
      tab.dom()
        .get_attribute(body, "data-load")
        .expect("get_attribute"),
      None
    );

    // CSP should suppress the image fetch; only the `load` task should remain.
    assert_eq!(
      fetcher.calls.load(Ordering::SeqCst),
      0,
      "expected CSP to prevent image fetch"
    );

    // `load` event dispatch.
    {
      let BrowserTab { host, event_loop, .. } = &mut tab;
      assert!(event_loop.run_next_task(host)?);
    }

    assert_eq!(
      tab.dom()
        .get_attribute(body, "data-load")
        .expect("get_attribute"),
      Some("1")
    );
    assert_eq!(
      fetcher.calls.load(Ordering::SeqCst),
      0,
      "expected CSP to prevent image fetch even after window load"
    );
    Ok(())
  }

  #[test]
  fn window_load_waits_for_input_type_image_src() -> Result<()> {
    #[derive(Debug, Default, Clone)]
    struct ImageFetcher {
      image_fetches: Arc<AtomicUsize>,
    }

    impl ImageFetcher {
      fn image_fetch_count(&self) -> usize {
        self.image_fetches.load(Ordering::SeqCst)
      }
    }

    impl ResourceFetcher for ImageFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        Err(Error::Other("ImageFetcher::fetch should not be used".to_string()))
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        if matches!(req.destination, FetchDestination::Image | FetchDestination::ImageCors) {
          self.image_fetches.fetch_add(1, Ordering::SeqCst);
          Ok(FetchedResource::new(
            b"fake-png-bytes".to_vec(),
            Some("image/png".to_string()),
          ))
        } else {
          Err(Error::Other(format!(
            "unexpected fetch destination {:?} for url={}",
            req.destination, req.url
          )))
        }
      }
    }

    let fetcher = Arc::new(ImageFetcher::default());
    let html = r#"<!doctype html><body>
      <input type="image" src="https://example.com/a.png">
      <script>
        document.addEventListener('DOMContentLoaded', function () {
          document.body.setAttribute('data-dom', '1');
        });
        window.addEventListener('load', function () {
          document.body.setAttribute('data-load', '1');
        });
      </script>
    </body>"#;

    let mut tab = BrowserTab::from_html_with_vmjs_and_document_url_and_fetcher(
      html,
      "https://example.com/",
      RenderOptions::default(),
      Arc::clone(&fetcher) as Arc<dyn ResourceFetcher>,
    )?;

    let body = tab.dom().body().expect("body should exist");

    // DocumentLifecycle queues two tasks at parsing completion:
    // - DOMContentLoaded barrier
    // - DOMContentLoaded
    {
      let BrowserTab { host, event_loop, .. } = &mut tab;
      assert!(event_loop.run_next_task(host)?); // barrier
      assert!(event_loop.run_next_task(host)?); // DOMContentLoaded
    }

    assert_eq!(
      tab.dom()
        .get_attribute(body, "data-dom")
        .expect("get_attribute"),
      Some("1")
    );
    assert_eq!(
      tab.dom()
        .get_attribute(body, "data-load")
        .expect("get_attribute"),
      None
    );
    assert_eq!(fetcher.image_fetch_count(), 0);

    // Next turn: image networking task runs and completes the load blocker, which queues `load`.
    {
      let BrowserTab { host, event_loop, .. } = &mut tab;
      assert!(event_loop.run_next_task(host)?); // image fetch task
    }
    assert_eq!(fetcher.image_fetch_count(), 1);

    // Final turn: `load` event dispatch.
    {
      let BrowserTab { host, event_loop, .. } = &mut tab;
      assert!(event_loop.run_next_task(host)?); // load
    }

    assert_eq!(
      tab.dom()
        .get_attribute(body, "data-load")
        .expect("get_attribute"),
      Some("1")
    );
    Ok(())
  }

  #[test]
  fn window_load_waits_for_link_icon_href() -> Result<()> {
    #[derive(Debug, Default, Clone)]
    struct IconFetcher {
      image_fetches: Arc<AtomicUsize>,
    }

    impl IconFetcher {
      fn image_fetch_count(&self) -> usize {
        self.image_fetches.load(Ordering::SeqCst)
      }
    }

    impl ResourceFetcher for IconFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        Err(Error::Other("IconFetcher::fetch should not be used".to_string()))
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        if matches!(req.destination, FetchDestination::Image | FetchDestination::ImageCors) {
          self.image_fetches.fetch_add(1, Ordering::SeqCst);
          Ok(FetchedResource::new(
            b"fake-png-bytes".to_vec(),
            Some("image/png".to_string()),
          ))
        } else {
          Err(Error::Other(format!(
            "unexpected fetch destination {:?} for url={}",
            req.destination, req.url
          )))
        }
      }
    }

    let fetcher = Arc::new(IconFetcher::default());
    let html = r#"<!doctype html>
      <head>
        <link rel="icon" href="https://example.com/favicon.ico">
      </head>
      <body>
        <script>
          document.addEventListener('DOMContentLoaded', function () {
            document.body.setAttribute('data-dom', '1');
          });
          window.addEventListener('load', function () {
            document.body.setAttribute('data-load', '1');
          });
        </script>
      </body>"#;

    let mut tab = BrowserTab::from_html_with_vmjs_and_document_url_and_fetcher(
      html,
      "https://example.com/",
      RenderOptions::default(),
      Arc::clone(&fetcher) as Arc<dyn ResourceFetcher>,
    )?;

    let body = tab.dom().body().expect("body should exist");

    // DOMContentLoaded barrier + DOMContentLoaded.
    {
      let BrowserTab { host, event_loop, .. } = &mut tab;
      assert!(event_loop.run_next_task(host)?); // barrier
      assert!(event_loop.run_next_task(host)?); // DOMContentLoaded
    }

    assert_eq!(
      tab.dom()
        .get_attribute(body, "data-dom")
        .expect("get_attribute"),
      Some("1")
    );
    assert_eq!(fetcher.image_fetch_count(), 0);
    assert_eq!(
      tab.dom()
        .get_attribute(body, "data-load")
        .expect("get_attribute"),
      None
    );

    // Icon fetch task turn.
    {
      let BrowserTab { host, event_loop, .. } = &mut tab;
      assert!(event_loop.run_next_task(host)?);
    }
    assert_eq!(fetcher.image_fetch_count(), 1);

    // `load` event dispatch.
    {
      let BrowserTab { host, event_loop, .. } = &mut tab;
      assert!(event_loop.run_next_task(host)?);
    }
    assert_eq!(
      tab.dom()
        .get_attribute(body, "data-load")
        .expect("get_attribute"),
      Some("1")
    );
    Ok(())
  }

  #[test]
  fn window_load_waits_for_video_poster() -> Result<()> {
    #[derive(Debug, Default, Clone)]
    struct PosterFetcher {
      image_fetches: Arc<AtomicUsize>,
    }

    impl PosterFetcher {
      fn image_fetch_count(&self) -> usize {
        self.image_fetches.load(Ordering::SeqCst)
      }
    }

    impl ResourceFetcher for PosterFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        Err(Error::Other("PosterFetcher::fetch should not be used".to_string()))
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        if matches!(req.destination, FetchDestination::Image | FetchDestination::ImageCors) {
          self.image_fetches.fetch_add(1, Ordering::SeqCst);
          Ok(FetchedResource::new(
            b"fake-png-bytes".to_vec(),
            Some("image/png".to_string()),
          ))
        } else {
          Err(Error::Other(format!(
            "unexpected fetch destination {:?} for url={}",
            req.destination, req.url
          )))
        }
      }
    }

    let fetcher = Arc::new(PosterFetcher::default());
    let html = r#"<!doctype html><body>
      <video poster="https://example.com/poster.png"></video>
      <script>
        document.addEventListener('DOMContentLoaded', function () {
          document.body.setAttribute('data-dom', '1');
        });
        window.addEventListener('load', function () {
          document.body.setAttribute('data-load', '1');
        });
      </script>
    </body>"#;

    let mut tab = BrowserTab::from_html_with_vmjs_and_document_url_and_fetcher(
      html,
      "https://example.com/",
      RenderOptions::default(),
      Arc::clone(&fetcher) as Arc<dyn ResourceFetcher>,
    )?;

    let body = tab.dom().body().expect("body should exist");

    // DOMContentLoaded barrier + DOMContentLoaded.
    {
      let BrowserTab { host, event_loop, .. } = &mut tab;
      assert!(event_loop.run_next_task(host)?); // barrier
      assert!(event_loop.run_next_task(host)?); // DOMContentLoaded
    }

    assert_eq!(
      tab.dom()
        .get_attribute(body, "data-dom")
        .expect("get_attribute"),
      Some("1")
    );
    assert_eq!(fetcher.image_fetch_count(), 0);
    assert_eq!(
      tab.dom()
        .get_attribute(body, "data-load")
        .expect("get_attribute"),
      None
    );

    // Poster fetch task turn.
    {
      let BrowserTab { host, event_loop, .. } = &mut tab;
      assert!(event_loop.run_next_task(host)?);
    }
    assert_eq!(fetcher.image_fetch_count(), 1);

    // `load` event dispatch.
    {
      let BrowserTab { host, event_loop, .. } = &mut tab;
      assert!(event_loop.run_next_task(host)?);
    }
    assert_eq!(
      tab.dom()
        .get_attribute(body, "data-load")
        .expect("get_attribute"),
      Some("1")
    );
    Ok(())
  }

  #[test]
  fn window_load_waits_for_images_loaded_after_dom_content_loaded_via_src_mutation() -> Result<()> {
    #[derive(Debug, Default, Clone)]
    struct RecordingFetcher {
      urls: Arc<Mutex<Vec<String>>>,
    }

    impl ResourceFetcher for RecordingFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        Err(Error::Other("RecordingFetcher::fetch should not be used".to_string()))
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        if !matches!(req.destination, FetchDestination::Image | FetchDestination::ImageCors) {
          return Err(Error::Other(format!(
            "unexpected fetch destination {:?} for url={}",
            req.destination, req.url
          )));
        }
        self
          .urls
          .lock()
          .unwrap_or_else(|poisoned| poisoned.into_inner())
          .push(req.url.to_string());
        Ok(FetchedResource::new(
          b"fake-png-bytes".to_vec(),
          Some("image/png".to_string()),
        ))
      }
    }

    let fetcher = Arc::new(RecordingFetcher::default());
    let html = r#"<!doctype html><body>
      <img id="i" src="https://example.com/a.png">
      <script>
        document.addEventListener('DOMContentLoaded', function () {
          document.body.setAttribute('data-dom', '1');
          queueMicrotask(function () {
            document.getElementById('i').setAttribute('src', 'https://example.com/b.png');
          });
        });
        window.addEventListener('load', function () {
          document.body.setAttribute('data-load', '1');
        });
      </script>
    </body>"#;

    let mut tab = BrowserTab::from_html_with_vmjs_and_document_url_and_fetcher(
      html,
      "https://example.com/",
      RenderOptions::default(),
      Arc::clone(&fetcher) as Arc<dyn ResourceFetcher>,
    )?;

    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    let dom = tab.dom();
    let body = dom.body().expect("body should exist");
    assert_eq!(
      dom.get_attribute(body, "data-dom")
        .expect("get_attribute"),
      Some("1")
    );
    assert_eq!(
      dom.get_attribute(body, "data-load")
        .expect("get_attribute"),
      Some("1")
    );

    let urls = fetcher
      .urls
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .clone();
    assert_eq!(
      urls,
      vec!["https://example.com/b.png".to_string()],
      "expected only the final src URL to be fetched"
    );
    Ok(())
  }

  #[test]
  fn window_load_fires_even_if_image_fetch_fails() -> Result<()> {
    #[derive(Debug, Default, Clone)]
    struct FailingImageFetcher {
      calls: Arc<AtomicUsize>,
    }

    impl ResourceFetcher for FailingImageFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        Err(Error::Other("FailingImageFetcher::fetch should not be used".to_string()))
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        if !matches!(req.destination, FetchDestination::Image | FetchDestination::ImageCors) {
          return Err(Error::Other(format!(
            "unexpected fetch destination {:?} for url={}",
            req.destination, req.url
          )));
        }
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(Error::Other("simulated image fetch failure".to_string()))
      }
    }

    let fetcher = Arc::new(FailingImageFetcher::default());
    let html = r#"<!doctype html><body>
      <img src="https://example.com/a.png">
      <script>
        document.addEventListener('DOMContentLoaded', function () {
          document.body.setAttribute('data-dom', '1');
        });
        window.addEventListener('load', function () {
          document.body.setAttribute('data-load', '1');
        });
      </script>
    </body>"#;

    let mut tab = BrowserTab::from_html_with_vmjs_and_document_url_and_fetcher(
      html,
      "https://example.com/",
      RenderOptions::default(),
      Arc::clone(&fetcher) as Arc<dyn ResourceFetcher>,
    )?;

    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    let dom = tab.dom();
    let body = dom.body().expect("body should exist");
    assert_eq!(
      dom.get_attribute(body, "data-dom")
        .expect("get_attribute"),
      Some("1")
    );
    assert_eq!(
      dom.get_attribute(body, "data-load")
        .expect("get_attribute"),
      Some("1")
    );
    assert_eq!(
      fetcher.calls.load(Ordering::SeqCst),
      1,
      "expected failing image fetch to be attempted once"
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
      dom
        .get_attribute(body, "data-module-ran")
        .expect("get_attribute should succeed"),
      Some("1")
    );
    assert_eq!(
      dom
        .get_attribute(body, "data-current-script-null")
        .expect("get_attribute should succeed"),
      Some("1")
    );
    assert_eq!(
      dom
        .get_attribute(body, "data-top-level-this-undefined")
        .expect("get_attribute should succeed"),
      Some("1")
    );
    Ok(())
  }

  #[test]
  fn module_top_level_await_promise_resolve_settles_via_microtasks() -> Result<()> {
    let mut js_options = JsExecutionOptions::default();
    js_options.supports_module_scripts = true;

    let mut tab = BrowserTab::from_html_with_js_execution_options(
      r#"<!doctype html><body>
        <script type="module">
          await Promise.resolve();
          document.body.setAttribute("data-tla", "1");
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
      dom
        .get_attribute(body, "data-tla")
        .expect("get_attribute should succeed"),
      Some("1")
    );
    Ok(())
  }

  #[test]
  fn module_top_level_await_that_never_settles_errors_deterministically() -> Result<()> {
    let mut js_options = JsExecutionOptions::default();
    js_options.supports_module_scripts = true;

    let mut options = RenderOptions::default();
    options.diagnostics_level = crate::api::DiagnosticsLevel::Basic;

    let mut tab = BrowserTab::from_html_with_js_execution_options(
      r#"<!doctype html><body>
        <script type="module">
          await new Promise(() => {});
          document.body.setAttribute("data-never", "1");
        </script>
      </body>"#,
      options,
      crate::api::VmJsBrowserTabExecutor::default(),
      js_options,
    )?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    let dom = tab.dom();
    let body = dom.body().expect("body should exist");
    assert_eq!(
      dom
        .get_attribute(body, "data-never")
        .expect("get_attribute should succeed"),
      None
    );

    let diagnostics = tab
      .diagnostics
      .as_ref()
      .expect("diagnostics should be enabled")
      .clone()
      .into_inner();
    assert!(
      diagnostics
        .js_exceptions
        .iter()
        .any(|exc| exc
          .message
          .contains("module top-level await did not settle before the event loop became idle")),
      "expected async module evaluation failure to be reported, got js_exceptions={:?}",
      diagnostics.js_exceptions
    );
    assert!(
      !diagnostics
        .js_exceptions
        .iter()
        .any(|exc| exc.message.contains("asynchronous module loading/evaluation is not supported")),
      "unexpected unsupported-module-evaluation diagnostic, got js_exceptions={:?}",
      diagnostics.js_exceptions
    );
    Ok(())
  }

  #[test]
  fn module_top_level_await_that_never_settles_after_draining_microtasks_aborts_on_idle() -> Result<()> {
    let mut js_options = JsExecutionOptions::default();
    js_options.supports_module_scripts = true;

    let mut options = RenderOptions::default();
    options.diagnostics_level = crate::api::DiagnosticsLevel::Basic;

    let mut tab = BrowserTab::from_html_with_js_execution_options(
      r#"<!doctype html><body>
        <script type="module">
          // Ensure the microtask checkpoint runs at least one microtask before the module is left
          // pending. The executor should still treat the loop as quiescent once all work is drained.
          queueMicrotask(() => {});
          await new Promise(() => {});
          document.body.setAttribute("data-never", "1");
        </script>
      </body>"#,
      options,
      crate::api::VmJsBrowserTabExecutor::default(),
      js_options,
    )?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    assert!(
      tab.host.pending_module_executions.is_empty(),
      "expected pending module executions to be finalized even when top-level await never settles"
    );

    let diagnostics = tab
      .diagnostics
      .as_ref()
      .expect("diagnostics should be enabled")
      .clone()
      .into_inner();
    assert!(
      diagnostics
        .js_exceptions
        .iter()
        .any(|exc| exc
          .message
          .contains("module top-level await did not settle before the event loop became idle")),
      "expected async module evaluation failure to be reported, got js_exceptions={:?}",
      diagnostics.js_exceptions
    );
    Ok(())
  }

  #[test]
  fn module_top_level_await_that_never_settles_aborts_after_turn_budget_even_if_tasks_keep_running()
  -> Result<()> {
    let mut js_options = JsExecutionOptions::default();
    js_options.supports_module_scripts = true;
    // Keep a very small budget so this test completes quickly.
    js_options.event_loop_run_limits.max_tasks = 3;
    js_options.event_loop_run_limits.max_microtasks = 1000;
    js_options.event_loop_run_limits.max_wall_time = None;

    let mut options = RenderOptions::default();
    options.diagnostics_level = crate::api::DiagnosticsLevel::Basic;

    let mut tab = BrowserTab::from_html_with_js_execution_options(
      r#"<!doctype html><body>
        <script type="module">
          // Keep the event loop non-quiescent, and ensure each turn drains microtasks so we exercise
          // the module TLA turn-budget accounting.
          setInterval(() => Promise.resolve().then(() => {}), 0);
          await new Promise(() => {});
          document.body.setAttribute("data-never", "1");
        </script>
      </body>"#,
      options,
      crate::api::VmJsBrowserTabExecutor::default(),
      js_options,
    )?;

    // Run a bounded number of tasks to avoid the interval hanging the test.
    tab.run_event_loop_until_idle(RunLimits {
      max_tasks: 20,
      max_microtasks: 10_000,
      max_wall_time: None,
    })?;

    let diagnostics = tab
      .diagnostics
      .as_ref()
      .expect("diagnostics should be enabled")
      .clone()
      .into_inner();
    assert!(
      diagnostics
        .js_exceptions
        .iter()
        .any(|exc| exc
          .message
          .contains("module top-level await did not settle within the configured task budget")),
      "expected async module evaluation failure due to turn budget, got js_exceptions={:?}",
      diagnostics.js_exceptions
    );
    Ok(())
  }

  #[test]
  fn module_top_level_await_resumes_after_timer_task() -> Result<()> {
    let mut js_options = JsExecutionOptions::default();
    js_options.supports_module_scripts = true;

    let mut options = RenderOptions::default();
    options.diagnostics_level = crate::api::DiagnosticsLevel::Basic;

    let mut tab = BrowserTab::from_html_with_js_execution_options(
      r#"<!doctype html><body>
        <script type="module">
          await new Promise((resolve) => setTimeout(resolve, 0));
          document.body.setAttribute("data-timer", "ok");
        </script>
      </body>"#,
      options,
      crate::api::VmJsBrowserTabExecutor::default(),
      js_options,
    )?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    let dom = tab.dom();
    let body = dom.body().expect("body should exist");
    assert_eq!(
      dom
        .get_attribute(body, "data-timer")
        .expect("get_attribute should succeed"),
      Some("ok")
    );

    let diagnostics = tab
      .diagnostics
      .as_ref()
      .expect("diagnostics should be enabled")
      .clone()
      .into_inner();
    assert!(
      !diagnostics
        .js_exceptions
        .iter()
        .any(|exc| exc.message.contains("asynchronous module loading/evaluation is not supported")),
      "unexpected unsupported-module-evaluation diagnostic, got js_exceptions={:?}",
      diagnostics.js_exceptions
    );

    Ok(())
  }

  #[test]
  fn module_top_level_await_resumes_after_fetch_task() -> Result<()> {
    let mut js_options = JsExecutionOptions::default();
    js_options.supports_module_scripts = true;

    let mut tab = BrowserTab::from_html_with_js_execution_options(
      r#"<!doctype html><body>
        <script type="module">
          const res = await fetch("data:text/plain,hello");
          const text = await res.text();
          document.body.setAttribute("data-fetch", text);
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
      dom
        .get_attribute(body, "data-fetch")
        .expect("get_attribute should succeed"),
      Some("hello")
    );
    Ok(())
  }

  #[test]
  fn module_top_level_await_blocks_later_ordered_module_scripts_and_domcontentloaded() -> Result<()> {
    let mut js_options = JsExecutionOptions::default();
    js_options.supports_module_scripts = true;

    let html = r#"<!doctype html><body>
      <script>
        globalThis.__log = [];
        document.addEventListener("DOMContentLoaded", () => globalThis.__log.push("DOMContentLoaded"));
        window.addEventListener("load", () => {
          globalThis.__log.push("load");
          document.body.setAttribute("data-log", globalThis.__log.join(","));
        });
      </script>
      <script type="module">
        globalThis.__log.push("m1-start");
        await new Promise((resolve) => setTimeout(resolve, 0));
        globalThis.__log.push("m1-end");
      </script>
      <script type="module">
        globalThis.__log.push("m2");
      </script>
    </body>"#;

    let mut tab = BrowserTab::from_html_with_js_execution_options(
      html,
      RenderOptions::default(),
      crate::api::VmJsBrowserTabExecutor::default(),
      js_options,
    )?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    let dom = tab.dom();
    let body = dom.body().expect("body should exist");
    assert_eq!(
      dom.get_attribute(body, "data-log")
        .expect("get_attribute should succeed"),
      Some("m1-start,m1-end,m2,DOMContentLoaded,load"),
      "expected ordered module scripts to execute sequentially even across top-level await, and to delay DOMContentLoaded/load",
    );
    Ok(())
  }

  #[test]
  fn module_top_level_await_blocks_later_classic_defer_scripts_in_post_parse_queue() -> Result<()> {
    let mut js_options = JsExecutionOptions::default();
    js_options.supports_module_scripts = true;

    let classic_source = r#"globalThis.__log.push("classic-defer");"#;
    let b64 = BASE64_STANDARD.encode(classic_source.as_bytes());
    let classic_url = Url::parse(&format!("data:text/javascript;base64,{b64}"))
      .expect("data URL should parse")
      .to_string();

    let html = format!(
      r#"<!doctype html><body>
        <script>
          globalThis.__log = [];
          document.addEventListener("DOMContentLoaded", () => globalThis.__log.push("DOMContentLoaded"));
          window.addEventListener("load", () => {{
            globalThis.__log.push("load");
            document.body.setAttribute("data-log", globalThis.__log.join(","));
          }});
        </script>
        <script type="module">
          globalThis.__log.push("m1-start");
          await new Promise((resolve) => setTimeout(resolve, 0));
          globalThis.__log.push("m1-end");
        </script>
        <script defer src="{classic_url}"></script>
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
      dom.get_attribute(body, "data-log")
        .expect("get_attribute should succeed"),
      Some("m1-start,m1-end,classic-defer,DOMContentLoaded,load"),
      "expected classic defer scripts to remain blocked behind a prior ordered module script whose evaluation is pending due to top-level await",
    );

    Ok(())
  }

  #[test]
  fn module_top_level_await_blocks_mixed_ordered_asap_classic_and_module_scripts() -> Result<()> {
    let mut js_options = JsExecutionOptions::default();
    js_options.supports_module_scripts = true;

    let classic_source = r#"globalThis.__log.push("classic");"#;
    let b64 = BASE64_STANDARD.encode(classic_source.as_bytes());
    let classic_url = Url::parse(&format!("data:text/javascript;base64,{b64}"))
      .expect("data URL should parse")
      .to_string();

    let html = format!(
      r#"<!doctype html><body>
        <script>
          globalThis.__log = [];
          window.addEventListener("load", () => {{
            globalThis.__log.push("load");
            document.body.setAttribute("data-log", globalThis.__log.join(","));
          }});

          const mod = document.createElement("script");
          mod.type = "module";
          mod.async = false;
          mod.textContent = `
            globalThis.__log.push("m1-start");
            await new Promise((resolve) => setTimeout(resolve, 0));
            globalThis.__log.push("m1-end");
          `;

          const classic = document.createElement("script");
          classic.async = false;
          classic.src = "{classic_url}";

          document.body.appendChild(mod);
          document.body.appendChild(classic);
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
      dom.get_attribute(body, "data-log")
        .expect("get_attribute should succeed"),
      Some("m1-start,m1-end,classic,load"),
      "expected ordered-asap scripts to preserve sequential execution even when a module script has pending top-level await",
    );

    Ok(())
  }

  #[test]
  fn module_script_load_event_waits_for_top_level_await_to_complete() -> Result<()> {
    let mut js_options = JsExecutionOptions::default();
    js_options.supports_module_scripts = true;

    let module_source = r#"
      globalThis.__log.push("module-start");
      await new Promise((resolve) => setTimeout(resolve, 0));
      globalThis.__log.push("module-end");
    "#;
    let b64 = BASE64_STANDARD.encode(module_source.as_bytes());
    let entry_url = Url::parse(&format!("data:text/javascript;base64,{b64}"))
      .expect("data URL should parse")
      .to_string();

    let html = format!(
      r#"<!doctype html><body>
        <script type="module" src="{entry_url}"></script>
        <script>
          globalThis.__log = [];
          const mod = document.querySelector('script[type="module"]');
          mod.addEventListener("load", () => {{
            globalThis.__log.push("load-event");
            document.body.setAttribute("data-log", globalThis.__log.join(","));
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
      dom.get_attribute(body, "data-log")
        .expect("get_attribute should succeed"),
      Some("module-start,module-end,load-event"),
      "expected module <script> load event to fire only after module evaluation completes (including top-level await)",
    );
    Ok(())
  }

  #[test]
  fn module_script_error_event_fires_for_synchronous_module_evaluation_error() -> Result<()> {
    let mut js_options = JsExecutionOptions::default();
    js_options.supports_module_scripts = true;
 
    let module_source = r#"
      globalThis.__log.push("module-start");
      throw new Error("boom");
    "#;
    let b64 = BASE64_STANDARD.encode(module_source.as_bytes());
    let entry_url = Url::parse(&format!("data:text/javascript;base64,{b64}"))
      .expect("data URL should parse")
      .to_string();
 
    let html = format!(
      r#"<!doctype html><body>
        <script type="module" src="{entry_url}"></script>
        <script>
          globalThis.__log = [];
          const mod = document.querySelector('script[type="module"]');
          mod.addEventListener("load", () => {{
            globalThis.__log.push("load-event");
            document.body.setAttribute("data-log", globalThis.__log.join(","));
          }});
          mod.addEventListener("error", () => {{
            globalThis.__log.push("error-event");
            document.body.setAttribute("data-log", globalThis.__log.join(","));
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
      dom.get_attribute(body, "data-log")
        .expect("get_attribute should succeed"),
      Some("module-start,error-event"),
      "expected module <script> to fire an error event (not load) when evaluation throws synchronously"
    );
    Ok(())
  }
 
  #[test]
  fn module_top_level_await_rejection_is_reported_as_uncaught_exception() -> Result<()> {
    let mut js_options = JsExecutionOptions::default();
    js_options.supports_module_scripts = true;

    let mut options = RenderOptions::default();
    options.diagnostics_level = crate::api::DiagnosticsLevel::Basic;

    let mut tab = BrowserTab::from_html_with_js_execution_options(
      r#"<!doctype html><body>
        <script type="module">
          await Promise.reject(new Error("boom"));
          document.body.setAttribute("data-after", "unreachable");
        </script>
      </body>"#,
      options,
      crate::api::VmJsBrowserTabExecutor::default(),
      js_options,
    )?;

    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    let diagnostics = tab
      .diagnostics
      .as_ref()
      .expect("diagnostics should be enabled")
      .clone()
      .into_inner();
    assert!(
      diagnostics
        .js_exceptions
        .iter()
        .any(|exc| exc.message.contains("boom")),
      "expected top-level await rejection to be reported, got js_exceptions={:?}",
      diagnostics.js_exceptions
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
      dom
        .get_attribute(body, "data-entry-url")
        .expect("get_attribute should succeed"),
      Some(entry_url.as_str())
    );
    assert_eq!(
      dom
        .get_attribute(body, "data-dep-url")
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
      dom
        .get_attribute(body, "data-dynamic-import")
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
      dom
        .get_attribute(body, "data-microtask-dynamic-import")
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
      dom
        .get_attribute(body, "data-importmap")
        .expect("get_attribute should succeed"),
      Some("123")
    );
    Ok(())
  }

  #[test]
  fn importmap_warnings_surface_as_console_warn_diagnostics_and_map_is_applied() -> Result<()> {
    let mut js_options = JsExecutionOptions::default();
    js_options.supports_module_scripts = true;

    // Map a bare specifier to a self-contained `data:` module to avoid network dependencies.
    let mapped = "data:text/javascript,export%20default%20123%3B";

    // Include an unknown top-level key to intentionally trigger an import map warning.
    let html = format!(
      r#"<!doctype html><body>
        <script type="importmap">{{"imports":{{"react":"{mapped}"}}, "unexpected": 1}}</script>
        <script type="module">
          import x from "react";
          document.body.setAttribute("data-importmap", String(x));
        </script>
      </body>"#
    );

    let mut tab = BrowserTab::from_html_with_js_execution_options(
      &html,
      RenderOptions::new().with_diagnostics_level(crate::api::DiagnosticsLevel::Basic),
      crate::api::VmJsBrowserTabExecutor::default(),
      js_options,
    )?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    let dom = tab.dom();
    let body = dom.body().expect("body should exist");
    assert_eq!(
      dom
        .get_attribute(body, "data-importmap")
        .expect("get_attribute should succeed"),
      Some("123")
    );

    let diagnostics = tab
      .diagnostics_snapshot()
      .expect("expected diagnostics to be enabled");
    assert!(
      diagnostics.console_messages.iter().any(|m| {
        m.level == crate::api::ConsoleMessageLevel::Warn
          && m.message.contains("importmap:")
          && m.message.contains("unknown top-level key")
          && m.message.contains("\"unexpected\"")
      }),
      "expected an import map warning to surface as a console warning; got: {diagnostics:?}"
    );

    Ok(())
  }

  #[test]
  fn importmap_parse_failure_emits_console_error_diagnostic() -> Result<()> {
    let mut js_options = JsExecutionOptions::default();
    js_options.supports_module_scripts = true;

    let html = r#"<!doctype html><body>
      <script type="importmap">{"imports":{"react":}}</script>
      <script type="module">
        import x from "react";
        document.body.setAttribute("data-importmap", String(x));
      </script>
    </body>"#;

    let mut tab = BrowserTab::from_html_with_js_execution_options(
      html,
      RenderOptions::new().with_diagnostics_level(crate::api::DiagnosticsLevel::Basic),
      crate::api::VmJsBrowserTabExecutor::default(),
      js_options,
    )?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    let diagnostics = tab
      .diagnostics_snapshot()
      .expect("expected diagnostics to be enabled");
    assert!(
      diagnostics.console_messages.iter().any(|m| {
        m.level == crate::api::ConsoleMessageLevel::Error
          && m.message.contains("importmap:")
          && m.message.contains("SyntaxError")
      }),
      "expected an import map parse error to surface as a console error; got: {diagnostics:?}"
    );

    Ok(())
  }

  #[test]
  fn importmap_parse_failure_dispatches_window_error_event_but_not_script_error_event() -> Result<()> {
    let mut js_options = JsExecutionOptions::default();
    js_options.supports_module_scripts = true;

    let html = r#"<!doctype html><body>
      <script>
        window.onerror = function (msg) {
          document.body.setAttribute("data-window-onerror", String(msg));
        };
        window.addEventListener("error", function (e) {
          document.body.setAttribute("data-window-error", String(e.message || ""));
        });
        const im = document.createElement("script");
        im.type = "importmap";
        im.addEventListener("error", function () {
          document.body.setAttribute("data-importmap-script-error", "1");
        });
        im.textContent = "{\"imports\":{\"react\":}}";
        document.body.appendChild(im);
      </script>
    </body>"#;

    let mut tab = BrowserTab::from_html_with_js_execution_options(
      html,
      RenderOptions::default(),
      crate::api::VmJsBrowserTabExecutor::default(),
      js_options,
    )?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    let dom = tab.dom();
    let body = dom.body().expect("body should exist");
    assert_eq!(
      dom.get_attribute(body, "data-importmap-script-error")
        .expect("get_attribute should succeed"),
      None,
      "expected import map parse failures to not fire `<script>` element error events"
    );
    assert!(
      dom.get_attribute(body, "data-window-error")
        .expect("get_attribute should succeed")
        .is_some(),
      "expected import map parse failures to dispatch a window error event"
    );
    assert!(
      dom.get_attribute(body, "data-window-onerror")
        .expect("get_attribute should succeed")
        .is_some(),
      "expected import map parse failures to invoke window.onerror"
    );
    Ok(())
  }

  #[test]
  fn invalid_importmap_does_not_abort_later_module_scripts_and_is_not_applied() -> Result<()> {
    let mut js_options = JsExecutionOptions::default();
    js_options.supports_module_scripts = true;

    let html = r#"<!doctype html><body>
      <script type="importmap">{"imports":{"react":}}</script>
      <script type="module">
        document.body.setAttribute("data-module-ok", "1");
      </script>
      <script type="module">
        import x from "react";
        document.body.setAttribute("data-module-bare", String(x));
      </script>
    </body>"#;

    let mut tab = BrowserTab::from_html_with_js_execution_options(
      html,
      RenderOptions::default(),
      crate::api::VmJsBrowserTabExecutor::default(),
      js_options,
    )?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    let dom = tab.dom();
    let body = dom.body().expect("body should exist");
    assert_eq!(
      dom.get_attribute(body, "data-module-ok")
        .expect("get_attribute should succeed"),
      Some("1"),
      "expected later module scripts to continue running after an import map parse failure"
    );
    assert_eq!(
      dom.get_attribute(body, "data-module-bare")
        .expect("get_attribute should succeed"),
      None,
      "expected invalid import maps to not be registered/applied to later module scripts"
    );
    Ok(())
  }

  #[test]
  fn importmap_malformed_integrity_section_emits_console_error_diagnostic() -> Result<()> {
    let mut js_options = JsExecutionOptions::default();
    js_options.supports_module_scripts = true;

    // Valid JSON but invalid import map shape: `"integrity"` must be an object.
    let html = r#"<!doctype html><body>
      <script type="importmap">{"imports":{"react":"data:text/javascript,export%20default%20123%3B"},"integrity":1}</script>
      <script type="module">
        document.body.setAttribute("data-module-ok", "1");
      </script>
      <script type="module">
        import x from "react";
        document.body.setAttribute("data-module-bare", String(x));
      </script>
    </body>"#;

    let mut tab = BrowserTab::from_html_with_js_execution_options(
      html,
      RenderOptions::new().with_diagnostics_level(crate::api::DiagnosticsLevel::Basic),
      crate::api::VmJsBrowserTabExecutor::default(),
      js_options,
    )?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    let dom = tab.dom();
    let body = dom.body().expect("body should exist");
    assert_eq!(
      dom.get_attribute(body, "data-module-ok")
        .expect("get_attribute should succeed"),
      Some("1"),
      "expected later module scripts to continue running after an import map registration error"
    );
    assert_eq!(
      dom.get_attribute(body, "data-module-bare")
        .expect("get_attribute should succeed"),
      None,
      "expected malformed import maps to not be registered/applied to later module scripts"
    );

    let diagnostics = tab
      .diagnostics_snapshot()
      .expect("expected diagnostics to be enabled");
    assert!(
      diagnostics.console_messages.iter().any(|m| {
        m.level == crate::api::ConsoleMessageLevel::Error
          && m.message.contains("importmap:")
          && m.message.contains("TypeError")
          && m.message.contains("\"integrity\"")
      }),
      "expected an import map integrity type error to surface as a console error; got: {diagnostics:?}"
    );
    Ok(())
  }

  #[test]
  fn import_maps_do_not_override_already_resolved_url_like_specifiers() -> Result<()> {
    let mut js_options = JsExecutionOptions::default();
    js_options.supports_module_scripts = true;

    let mut tab = BrowserTab::from_html_with_js_execution_options(
      "<!doctype html><html></html>",
      RenderOptions::default(),
      crate::api::VmJsBrowserTabExecutor::default(),
      js_options,
    )?;

    let doc_url = "https://example.com/doc.html";
    tab.register_html_source(
      doc_url,
      r#"<!doctype html><body>
        <script type="module" src="https://example.com/entry.js"></script>
      </body>"#,
    );

    // Entry module:
    // - imports a URL-like specifier (`/direct.js`) so it becomes part of the resolved module set,
    // - then dynamically inserts an import map attempting to remap that URL-like specifier,
    // - then inserts a second module script that re-imports `/direct.js`.
    //
    // The second import should still resolve to `direct.js`, not `changed.js`.
    tab.register_script_source(
      "https://example.com/entry.js",
      r#"import "/direct.js";
const importMap = document.createElement("script");
importMap.setAttribute("type", "importmap");
importMap.textContent = JSON.stringify({ imports: { "/direct.js": "/changed.js" } });
document.body.appendChild(importMap);

const second = document.createElement("script");
second.setAttribute("type", "module");
second.textContent = `import { marker } from "/direct.js";
document.body.setAttribute("data-marker", marker);`;
document.body.appendChild(second);"#,
    );
    tab.register_script_source(
      "https://example.com/direct.js",
      r#"export const marker = "direct";"#,
    );
    tab.register_script_source(
      "https://example.com/changed.js",
      r#"export const marker = "changed";"#,
    );

    tab.navigate_to_url(doc_url, RenderOptions::default())?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    let dom = tab.dom();
    let body = dom.body().expect("body should exist");
    assert_eq!(
      dom
        .get_attribute(body, "data-marker")
        .expect("get_attribute should succeed"),
      Some("direct")
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
      dom
        .get_attribute(body, "data-value")
        .expect("get_attribute should succeed"),
      Some("42")
    );
    Ok(())
  }

  #[test]
  fn module_script_integrity_match_executes() -> Result<()> {
    let mut js_options = JsExecutionOptions::default();
    js_options.supports_module_scripts = true;

    let source = r#"document.body.setAttribute("data-integrity", "ok");"#;
    let integrity = sri_sha256_token(source.as_bytes());
    let html = format!(
      r#"<!doctype html><body>
        <script type="module" src="https://example.com/a.js" integrity="{integrity}"></script>
      </body>"#
    );

    let mut tab = BrowserTab::from_html_with_js_execution_options(
      &html,
      RenderOptions::default(),
      crate::api::VmJsBrowserTabExecutor::default(),
      js_options,
    )?;
    tab.register_script_source("https://example.com/a.js", source);
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    let dom = tab.dom();
    let body = dom.body().expect("body should exist");
    assert_eq!(
      dom.get_attribute(body, "data-integrity")
        .expect("get_attribute should succeed"),
      Some("ok")
    );
    Ok(())
  }

  #[test]
  fn module_script_integrity_mismatch_dispatches_error_and_skips_execution() -> Result<()> {
    let mut js_options = JsExecutionOptions::default();
    js_options.supports_module_scripts = true;

    let source = r#"document.body.setAttribute("data-integrity", "ran");"#;
    let wrong = sri_sha256_token(b"other");
    let html = format!(
      r#"<!doctype html><body>
        <script type="module" src="https://example.com/a.js" integrity="{wrong}"></script>
      </body>"#
    );

    let mut tab = BrowserTab::from_html_with_js_execution_options(
      &html,
      RenderOptions::default(),
      crate::api::VmJsBrowserTabExecutor::default(),
      js_options,
    )?;
    tab.register_script_source("https://example.com/a.js", source);

    let event_log: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    tab.set_event_listener_invoker(Box::new(RecordingInvoker {
      log: Rc::clone(&event_log),
    }));

    let script_node_id = tab
      .host
      .scripts
      .values()
      .next()
      .expect("expected module script entry")
      .node_id;

    tab.dom().events().add_event_listener(
      EventTargetId::Node(script_node_id),
      "error",
      ListenerId::new(1),
      AddEventListenerOptions::default(),
    );

    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    assert_eq!(&*event_log.borrow(), &["error".to_string()]);

    let dom = tab.dom();
    let body = dom.body().expect("body should exist");
    assert_eq!(
      dom.get_attribute(body, "data-integrity")
        .expect("get_attribute should succeed"),
      None
    );
    assert_eq!(dom.ready_state(), DocumentReadyState::Complete);
    Ok(())
  }

  #[test]
  fn module_fetches_propagate_script_referrerpolicy() -> Result<()> {
    #[derive(Clone)]
    struct RecordingFetcher {
      entries: Arc<HashMap<String, FetchedResource>>,
      calls: Arc<Mutex<Vec<(String, ReferrerPolicy)>>>,
    }

    impl RecordingFetcher {
      fn new(entries: HashMap<String, FetchedResource>) -> Self {
        Self {
          entries: Arc::new(entries),
          calls: Arc::new(Mutex::new(Vec::new())),
        }
      }

      fn calls(&self) -> Vec<(String, ReferrerPolicy)> {
        self
          .calls
          .lock()
          .unwrap_or_else(|poisoned| poisoned.into_inner())
          .clone()
      }
    }

    impl ResourceFetcher for RecordingFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        self
          .entries
          .get(url)
          .cloned()
          .ok_or_else(|| Error::Other(format!("missing fetcher entry for {url}")))
      }

      fn fetch_with_request(&self, req: crate::resource::FetchRequest<'_>) -> Result<FetchedResource> {
        self
          .calls
          .lock()
          .unwrap_or_else(|poisoned| poisoned.into_inner())
          .push((req.url.to_string(), req.referrer_policy));
        self.fetch(req.url)
      }
    }

    let mut js_options = JsExecutionOptions::default();
    js_options.supports_module_scripts = true;

    let document_url = "https://example.invalid/doc.html";
    let entry_url = "https://example.invalid/entry.js";
    let dep_url = "https://example.invalid/dep.js";

    let entry_source = r#"import "./dep.js";
      document.body.setAttribute("data-rp", "ok");"#;
    let dep_source = r#"export const value = 1;"#;

    let mut entries: HashMap<String, FetchedResource> = HashMap::new();

    let mut entry_res = FetchedResource::new(
      entry_source.as_bytes().to_vec(),
      Some("application/javascript".to_string()),
    );
    entry_res.status = Some(200);
    entry_res.final_url = Some(entry_url.to_string());
    entry_res.access_control_allow_origin = Some("*".to_string());
    entry_res.access_control_allow_credentials = true;
    entries.insert(entry_url.to_string(), entry_res);

    let mut dep_res = FetchedResource::new(
      dep_source.as_bytes().to_vec(),
      Some("application/javascript".to_string()),
    );
    dep_res.status = Some(200);
    dep_res.final_url = Some(dep_url.to_string());
    dep_res.access_control_allow_origin = Some("*".to_string());
    dep_res.access_control_allow_credentials = true;
    entries.insert(dep_url.to_string(), dep_res);

    let fetcher = Arc::new(RecordingFetcher::new(entries));
    let fetcher_trait: Arc<dyn ResourceFetcher> = fetcher.clone();

    let html = format!(
      r#"<!doctype html><head><meta name="referrer" content="origin"></head><body>
        <script type="module" src="{entry_url}" referrerpolicy="no-referrer"></script>
      </body>"#
    );

    let mut tab = BrowserTab::from_html_with_document_url_and_fetcher_and_js_execution_options(
      &html,
      document_url,
      RenderOptions::default(),
      crate::api::VmJsBrowserTabExecutor::default(),
      fetcher_trait,
      js_options,
    )?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    let dom = tab.dom();
    let body = dom.body().expect("body should exist");
    assert_eq!(
      dom.get_attribute(body, "data-rp")
        .expect("get_attribute should succeed"),
      Some("ok")
    );

    let calls = fetcher.calls();
    assert!(
      calls.iter().any(|(url, policy)| url == entry_url && *policy == ReferrerPolicy::NoReferrer),
      "expected entry module fetch to use referrerpolicy=no-referrer, got {calls:?}"
    );
    assert!(
      calls.iter().any(|(url, policy)| url == dep_url && *policy == ReferrerPolicy::NoReferrer),
      "expected dependency module fetch to use referrerpolicy=no-referrer, got {calls:?}"
    );
    Ok(())
  }

  #[test]
  fn image_prefetch_honors_element_referrerpolicy_overrides() -> Result<()> {
    #[derive(Clone)]
    struct RecordingFetcher {
      entries: Arc<HashMap<String, FetchedResource>>,
      calls: Arc<Mutex<Vec<(String, ReferrerPolicy)>>>,
    }

    impl RecordingFetcher {
      fn new(entries: HashMap<String, FetchedResource>) -> Self {
        Self {
          entries: Arc::new(entries),
          calls: Arc::new(Mutex::new(Vec::new())),
        }
      }

      fn calls(&self) -> Vec<(String, ReferrerPolicy)> {
        self
          .calls
          .lock()
          .unwrap_or_else(|poisoned| poisoned.into_inner())
          .clone()
      }
    }

    impl ResourceFetcher for RecordingFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        self
          .entries
          .get(url)
          .cloned()
          .ok_or_else(|| Error::Other(format!("missing fetcher entry for {url}")))
      }

      fn fetch_with_request(&self, req: crate::resource::FetchRequest<'_>) -> Result<FetchedResource> {
        self
          .calls
          .lock()
          .unwrap_or_else(|poisoned| poisoned.into_inner())
          .push((req.url.to_string(), req.referrer_policy));
        self.fetch(req.url)
      }
    }

    let document_url = "https://example.invalid/doc.html";
    let img_override_url = "https://example.invalid/override.png";
    let video_poster_url = "https://example.invalid/poster.png";
    let img_default_url = "https://example.invalid/default.png";

    let mut entries: HashMap<String, FetchedResource> = HashMap::new();
    for url in [img_override_url, video_poster_url, img_default_url] {
      let mut res =
        FetchedResource::new(b"img".to_vec(), Some("image/png".to_string()));
      res.status = Some(200);
      res.final_url = Some(url.to_string());
      entries.insert(url.to_string(), res);
    }

    let fetcher = Arc::new(RecordingFetcher::new(entries));
    let fetcher_trait: Arc<dyn ResourceFetcher> = fetcher.clone();

    let html = format!(
      r#"<!doctype html><head><meta name="referrer" content="origin"></head><body>
        <img src="{img_override_url}" referrerpolicy="no-referrer">
        <video poster="{video_poster_url}" referrerpolicy="no-referrer"></video>
        <img src="{img_default_url}">
      </body>"#
    );

    let mut tab = BrowserTab::from_html_with_document_url_and_fetcher(
      &html,
      document_url,
      RenderOptions::default(),
      crate::api::VmJsBrowserTabExecutor::default(),
      fetcher_trait,
    )?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    let calls = fetcher.calls();
    assert!(
      calls.iter().any(|(url, policy)| url == img_override_url && *policy == ReferrerPolicy::NoReferrer),
      "expected <img referrerpolicy=no-referrer> to override document policy, got {calls:?}"
    );
    assert!(
      calls.iter().any(|(url, policy)| url == video_poster_url && *policy == ReferrerPolicy::NoReferrer),
      "expected <video poster referrerpolicy=no-referrer> to override document policy, got {calls:?}"
    );
    assert!(
      calls.iter().any(|(url, policy)| url == img_default_url && *policy == ReferrerPolicy::Origin),
      "expected element without referrerpolicy to use document policy, got {calls:?}"
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
      integrity_map.insert(module_url.clone(), serde_json::Value::String(integrity));
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
      dom
        .get_attribute(body, "data-integrity")
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

      fn fetch_with_request(
        &self,
        req: crate::resource::FetchRequest<'_>,
      ) -> Result<FetchedResource> {
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
      dom
        .get_attribute(body, "data-importmap")
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
      integrity_map.insert(module_url.clone(), serde_json::Value::String(integrity));
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
      dom
        .get_attribute(body, "data-integrity")
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

  #[test]
  fn next_wake_time_schedules_pending_animation_frame_callbacks() -> Result<()> {
    use crate::js::VirtualClock;
    use std::time::Duration;

    const DEFAULT_INTERVAL: Duration = Duration::from_nanos(16_666_667);
    assert_eq!(
      JsExecutionOptions::default().animation_frame_interval,
      DEFAULT_INTERVAL,
      "expected JsExecutionOptions::default to use a ~60fps frame interval"
    );

    let clock = Arc::new(VirtualClock::new());
    let event_loop = EventLoop::with_clock(clock);

    let mut tab = BrowserTab::from_html_with_event_loop_and_js_execution_options(
      "<!doctype html><html></html>",
      RenderOptions::default(),
      NoopExecutor::default(),
      event_loop,
      JsExecutionOptions::default(),
    )?;
    // Drain any lifecycle tasks scheduled during construction so we start from a fully idle tab.
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;
    // Rendering clears the "dirty" bit so rAF scheduling becomes the only reason to wake.
    let _ = tab.render_if_needed()?;

    tab
      .event_loop
      .request_animation_frame(|_host, event_loop, _timestamp| {
        // Queue another callback so we still have pending rAF work after running one frame turn.
        event_loop.request_animation_frame(|_host, _event_loop, _timestamp| Ok(()))?;
        Ok(())
      })?;

    // Drive one frame: this runs the first rAF callback, which schedules the next one, and sets
    // `next_animation_frame_due` to `now + animation_frame_interval`.
    let _ = tab.tick_frame()?;
    assert!(
      tab.event_loop.has_pending_animation_frame_callbacks(),
      "expected a rAF callback queued for the next frame"
    );

    let now = tab.event_loop.now();
    assert_eq!(tab.next_wake_time(), Some(now + DEFAULT_INTERVAL));
    Ok(())
  }

  #[test]
  fn next_tick_due_in_reports_timer_delay() -> Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let event_loop = EventLoop::<BrowserTabHost>::with_clock(clock.clone());
    let log: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let mut tab = BrowserTab::from_html_with_event_loop(
      "",
      RenderOptions::default(),
      TestExecutor { log },
      event_loop,
    )?;
    tab.event_loop.clear_all_pending_work();
    // Rendering clears the dirty bit so timer scheduling becomes the only wake reason.
    let _ = tab.render_if_needed()?;

    tab
      .event_loop
      .set_timeout(Duration::from_millis(100), |_host, _event_loop| Ok(()))?;

    assert_eq!(tab.next_tick_due_in(), Some(Duration::from_millis(100)));
    clock.advance(Duration::from_millis(20));
    assert_eq!(tab.next_tick_due_in(), Some(Duration::from_millis(80)));
    Ok(())
  }

  #[test]
  fn next_wake_time_respects_updated_animation_frame_interval() -> Result<()> {
    use crate::js::VirtualClock;
    use std::time::Duration;

    let clock = Arc::new(VirtualClock::new());
    let event_loop = EventLoop::with_clock(clock);

    let mut tab = BrowserTab::from_html_with_event_loop_and_js_execution_options(
      "<!doctype html><html></html>",
      RenderOptions::default(),
      NoopExecutor::default(),
      event_loop,
      JsExecutionOptions::default(),
    )?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;
    let _ = tab.render_if_needed()?;

    tab
      .event_loop
      .request_animation_frame(|_host, event_loop, _timestamp| {
        event_loop.request_animation_frame(|_host, _event_loop, _timestamp| Ok(()))?;
        Ok(())
      })?;

    let _ = tab.tick_frame()?;

    let now = tab.event_loop.now();
    let before = tab.next_wake_time().expect("expected wake time with pending rAF");

    let new_interval = Duration::from_millis(40);
    let mut options = tab.js_execution_options();
    options.animation_frame_interval = new_interval;
    tab.set_js_execution_options(options);

    let after = tab.next_wake_time().expect("expected wake time with pending rAF");
    assert_eq!(after, now + new_interval);
    assert_ne!(
      before, after,
      "expected animation_frame_interval update to change next_wake_time"
    );
    Ok(())
  }

  #[test]
  fn next_tick_due_in_returns_none_when_idle() -> Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let event_loop = EventLoop::<BrowserTabHost>::with_clock(clock.clone());
    let log: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let mut tab = BrowserTab::from_html_with_event_loop(
      "",
      RenderOptions::default(),
      TestExecutor { log },
      event_loop,
    )?;
    tab.event_loop.clear_all_pending_work();
    let _ = tab.render_if_needed()?;

    assert!(!tab.host.document.is_dirty());
    assert!(tab.event_loop.is_idle());
    assert!(!tab.event_loop.has_pending_timers());
    assert!(!tab.event_loop.has_pending_animation_frame_callbacks());

    assert_eq!(tab.next_tick_due_in(), None);
    Ok(())
  }

  #[test]
  fn next_tick_due_in_returns_zero_when_document_dirty() -> Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let event_loop = EventLoop::<BrowserTabHost>::with_clock(clock.clone());
    let log: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let mut tab = BrowserTab::from_html_with_event_loop(
      "",
      RenderOptions::default(),
      TestExecutor { log },
      event_loop,
    )?;
    tab.event_loop.clear_all_pending_work();
    let _ = tab.render_if_needed()?;

    let html = {
      let dom = tab.dom();
      dom
        .get_elements_by_tag_name_from(dom.root(), "html")
        .first()
        .copied()
        .expect("expected HTML element to exist")
    };
    tab
      .dom_mut()
      .set_attribute(html, "data-test-dirty", "1")
      .expect("set attribute");
    assert!(tab.host.document.is_dirty());

    assert_eq!(tab.next_tick_due_in(), Some(Duration::ZERO));
    Ok(())
  }

  #[test]
  fn next_tick_due_in_reports_animation_frame_cadence_when_visible() -> Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let event_loop = EventLoop::<BrowserTabHost>::with_clock(clock.clone());
    let log: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let mut tab = BrowserTab::from_html_with_event_loop(
      "",
      RenderOptions::default(),
      TestExecutor { log },
      event_loop,
    )?;
    tab.event_loop.clear_all_pending_work();
    let _ = tab.render_if_needed()?;

    tab
      .event_loop
      .request_animation_frame(|_host, _event_loop, _timestamp| Ok(()))?;

    assert_eq!(tab.host.document.visibility_state(), DocumentVisibilityState::Visible);
    assert_eq!(tab.next_tick_due_in(), Some(RAF_TICK_CADENCE));
    Ok(())
  }

  #[test]
  fn next_tick_due_in_reports_immediate_work() -> Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let event_loop = EventLoop::<BrowserTabHost>::with_clock(clock.clone());
    let log: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let mut tab = BrowserTab::from_html_with_event_loop(
      "",
      RenderOptions::default(),
      TestExecutor { log },
      event_loop,
    )?;
    tab.event_loop.clear_all_pending_work();
    let _ = tab.render_if_needed()?;

    tab
      .event_loop
      .queue_task(TaskSource::DOMManipulation, |_host, _event_loop| Ok(()))?;

    assert_eq!(tab.next_tick_due_in(), Some(Duration::ZERO));
    Ok(())
  }
}

#[cfg(test)]
mod dynamic_import {
  use super::*;

  fn js_options_with_module_scripts() -> JsExecutionOptions {
    let mut opts = JsExecutionOptions::default();
    opts.supports_module_scripts = true;
    opts
  }

  fn make_vmjs_tab() -> Result<BrowserTab> {
    BrowserTab::from_html_with_js_execution_options(
      "",
      RenderOptions::default(),
      crate::api::VmJsBrowserTabExecutor::default(),
      js_options_with_module_scripts(),
    )
  }

  fn assert_body_attr(tab: &BrowserTab, name: &str) -> Option<String> {
    let dom = tab.dom();
    let body = dom.body().expect("body should exist");
    dom
      .get_attribute(body, name)
      .expect("get_attribute should succeed")
      .map(|s| s.to_string())
  }

  #[test]
  fn resolves_relative_dynamic_import_in_inline_classic_script_against_document_base_url() -> Result<()> {
    let mut tab = make_vmjs_tab()?;

    let doc_url = "https://example.invalid/page.html";
    tab.register_script_source(
      "https://example.invalid/base/rel.js",
      "export default import.meta.url;",
    );
    // Register a fallback so resolution mistakes fail by assertion rather than fetch error.
    tab.register_script_source(
      "https://example.invalid/rel.js",
      "export default import.meta.url;",
    );

    tab.register_html_source(
      doc_url,
      r#"<!doctype html><head>
        <base href="https://example.invalid/base/">
      </head><body>
        <script>
          import("./rel.js")
            .then(m => { document.body.setAttribute("data-url", String(m.default)); })
            .catch(e => { document.body.setAttribute("data-err-name", String(e && e.name || "")); });
        </script>
      </body>"#,
    );

    tab.navigate_to_url(doc_url, RenderOptions::default())?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    assert_eq!(assert_body_attr(&tab, "data-err-name"), None);
    assert_eq!(
      assert_body_attr(&tab, "data-url").as_deref(),
      Some("https://example.invalid/base/rel.js")
    );
    Ok(())
  }

  #[test]
  fn resolves_relative_dynamic_import_in_module_against_referrer_module_url() -> Result<()> {
    let mut tab = make_vmjs_tab()?;

    let doc_url = "https://example.invalid/page.html";
    tab.register_script_source(
      "https://example.invalid/mod/main.js",
      r#"
        import("./rel.js")
          .then(m => { document.body.setAttribute("data-url", String(m.default)); })
          .catch(e => { document.body.setAttribute("data-err-name", String(e && e.name || "")); });
      "#,
    );
    tab.register_script_source(
      "https://example.invalid/mod/rel.js",
      "export default import.meta.url;",
    );
    // Register a fallback at the document-base location so incorrect resolution is observable.
    tab.register_script_source(
      "https://example.invalid/doc/rel.js",
      "export default import.meta.url;",
    );

    tab.register_html_source(
      doc_url,
      r#"<!doctype html><head>
        <base href="https://example.invalid/doc/">
      </head><body>
        <script type="module" src="https://example.invalid/mod/main.js"></script>
      </body>"#,
    );

    tab.navigate_to_url(doc_url, RenderOptions::default())?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    assert_eq!(assert_body_attr(&tab, "data-err-name"), None);
    assert_eq!(
      assert_body_attr(&tab, "data-url").as_deref(),
      Some("https://example.invalid/mod/rel.js")
    );
    Ok(())
  }

  #[test]
  fn dynamic_import_applies_import_map_in_classic_script() -> Result<()> {
    let mut tab = make_vmjs_tab()?;
    let doc_url = "https://example.invalid/page.html";

    tab.register_html_source(
      doc_url,
      r#"<!doctype html><body>
        <script type="importmap">{"imports":{"bare":"data:text/javascript,export%20default%2042%3B"}}</script>
        <script>
          import("bare")
            .then(m => { document.body.setAttribute("data-value", String(m.default)); })
            .catch(e => { document.body.setAttribute("data-err-name", String(e && e.name || "")); });
        </script>
      </body>"#,
    );

    tab.navigate_to_url(doc_url, RenderOptions::default())?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    assert_eq!(assert_body_attr(&tab, "data-err-name"), None);
    assert_eq!(assert_body_attr(&tab, "data-value").as_deref(), Some("42"));
    Ok(())
  }

  #[test]
  fn dynamic_import_applies_scoped_import_map_based_on_referrer_module_url() -> Result<()> {
    let mut tab = make_vmjs_tab()?;
    let doc_url = "https://example.invalid/page.html";

    tab.register_script_source(
      "https://example.invalid/scope/main.js",
      r#"
        import("bare")
          .then(m => { document.body.setAttribute("data-value", String(m.default)); })
          .catch(e => { document.body.setAttribute("data-err-name", String(e && e.name || "")); });
      "#,
    );

    tab.register_html_source(
      doc_url,
      r#"<!doctype html><body>
        <script type="importmap">
          {
            "imports": { "bare": "data:text/javascript,export%20default%20%22global%22%3B" },
            "scopes": {
              "https://example.invalid/scope/": { "bare": "data:text/javascript,export%20default%20%22scoped%22%3B" }
            }
          }
        </script>
        <script type="module" src="https://example.invalid/scope/main.js"></script>
      </body>"#,
    );

    tab.navigate_to_url(doc_url, RenderOptions::default())?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    assert_eq!(assert_body_attr(&tab, "data-err-name"), None);
    assert_eq!(assert_body_attr(&tab, "data-value").as_deref(), Some("scoped"));
    Ok(())
  }

  #[test]
  fn dynamic_import_caches_module_namespace_objects_and_does_not_reexecute_modules() -> Result<()> {
    let mut tab = make_vmjs_tab()?;
    let doc_url = "https://example.invalid/page.html";

    tab.register_script_source(
      "https://example.invalid/base/mod.js",
      r#"
        globalThis.__evals = (globalThis.__evals || 0) + 1;
        export const evals = globalThis.__evals;
      "#,
    );

    tab.register_html_source(
      doc_url,
      r#"<!doctype html><head>
        <base href="https://example.invalid/base/">
      </head><body>
        <script>
          import("./mod.js")
            .then(m1 => import("./mod.js").then(m2 => {
              document.body.setAttribute("data-same", String(m1 === m2));
              document.body.setAttribute("data-evals", String(globalThis.__evals));
            }))
            .catch(e => { document.body.setAttribute("data-err-name", String(e && e.name || "")); });
        </script>
      </body>"#,
    );

    tab.navigate_to_url(doc_url, RenderOptions::default())?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    assert_eq!(assert_body_attr(&tab, "data-err-name"), None);
    assert_eq!(assert_body_attr(&tab, "data-same").as_deref(), Some("true"));
    assert_eq!(assert_body_attr(&tab, "data-evals").as_deref(), Some("1"));
    Ok(())
  }

  #[test]
  fn dynamic_import_rejects_unmapped_bare_specifier_with_type_error() -> Result<()> {
    let mut tab = make_vmjs_tab()?;
    let doc_url = "https://example.invalid/page.html";

    tab.register_html_source(
      doc_url,
      r#"<!doctype html><body>
        <script>
          import("unmapped")
            .then(() => { document.body.setAttribute("data-ok", "1"); })
            .catch(e => { document.body.setAttribute("data-err-name", String(e && e.name || "")); });
        </script>
      </body>"#,
    );

    tab.navigate_to_url(doc_url, RenderOptions::default())?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    assert_eq!(assert_body_attr(&tab, "data-ok"), None);
    assert_eq!(assert_body_attr(&tab, "data-err-name").as_deref(), Some("TypeError"));
    Ok(())
  }

  #[test]
  fn dynamic_import_rejects_null_import_map_entry_with_type_error() -> Result<()> {
    let mut tab = make_vmjs_tab()?;
    let doc_url = "https://example.invalid/page.html";

    tab.register_html_source(
      doc_url,
      r#"<!doctype html><body>
        <script type="importmap">{"imports":{"blocked":null}}</script>
        <script>
          import("blocked")
            .then(() => { document.body.setAttribute("data-ok", "1"); })
            .catch(e => { document.body.setAttribute("data-err-name", String(e && e.name || "")); });
        </script>
      </body>"#,
    );

    tab.navigate_to_url(doc_url, RenderOptions::default())?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    assert_eq!(assert_body_attr(&tab, "data-ok"), None);
    assert_eq!(assert_body_attr(&tab, "data-err-name").as_deref(), Some("TypeError"));
    Ok(())
  }
}
