//! Canonical message-driven browser UI worker.
//!
//! This module contains the single production implementation of the UI↔worker protocol used by the
//! windowed `browser` app (`src/bin/browser.rs`) and the browser UI integration tests. It owns
//! per-tab state (document, interaction engine, history, cancellation) and renders on a dedicated
//! large-stack thread.

use crate::api::{
  BrowserDocument, BrowserTab, FastRenderConfig, FastRenderFactory, FastRenderPoolConfig,
  RenderOptions, VmJsBrowserTabExecutor,
};
use crate::debug::runtime::RuntimeToggles;
use crate::geometry::{Point, Rect, Size};
use crate::html::{find_document_favicon_url, find_document_title};
use crate::interaction::anchor_scroll::scroll_offset_for_fragment_target;
use crate::interaction::{
  hit_test_dom, FormSubmission, FormSubmissionMethod, HitTestKind, InteractionAction,
  InteractionEngine,
};
use crate::js::RunLimits;
use crate::paint::rasterize::fill_rect;
use crate::render_control::{
  push_stage_listener, DeadlineGuard, StageHeartbeat, StageListenerGuard,
};
use crate::resource::{
  origin_from_url, CachingFetcher, DocumentOrigin, HttpFetcher, ResourceFetcher,
};
use crate::scroll::ScrollState;
use crate::style::color::Rgba;
use crate::style::types::OrientationTransform;
use crate::style::types::CursorKeyword;
use crate::text::font_db::FontConfig;
use crate::tree::box_tree::{BoxNode, BoxType, ImageSelectionContext, ReplacedType};
use crate::ui::about_pages;
use crate::ui::browser_limits::BrowserLimits;
use crate::ui::cancel::{deadline_for, CancelGens, CancelSnapshot};
use crate::ui::clipboard;
use crate::ui::find_in_page::{FindIndex, FindMatch, FindOptions};
use crate::ui::history::TabHistory;
use crate::ui::messages::{
  BrowserMediaPreferences, CursorKind, DatalistOption, DownloadId, DownloadOutcome, NavigationReason,
  PointerButton, RenderedFrame, ScrollMetrics, TabId, UiToWorker, WorkerToUi,
};
use super::router_coalescer::UiToWorkerRouterCoalescer;
use crate::ui::protocol_limits::{MAX_FAVICON_BYTES, MAX_FAVICON_EDGE_PX};
use crate::ui::{resolve_link_url, validate_user_navigation_url_scheme};
use crate::web::events as web_events;
use image::imageops::FilterType;
use rustc_hash::FxHashSet;
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "browser_ui")]
use std::sync::atomic::AtomicUsize;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

// -----------------------------------------------------------------------------
// Test hooks
// -----------------------------------------------------------------------------

/// Global counter for how many `FastRender` instances were built by the UI worker.
///
/// This is a lightweight integration-test hook used to assert that tabs reuse a single renderer
/// across navigations (instead of rebuilding one per navigation).
#[cfg(feature = "browser_ui")]
static UI_WORKER_RENDERER_BUILD_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Global counter for how many scroll-induced hover syncs were executed by the UI worker.
///
/// A "scroll-induced hover sync" is when the worker re-runs pointer hover hit-testing after a
/// scroll changes the scroll offset (so content moves under a stationary cursor). Scroll bursts
/// should coalesce to a single sync per tab.
#[cfg(feature = "browser_ui")]
static UI_WORKER_SCROLL_HOVER_SYNC_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Returns the number of renderers built by the UI worker so far (test hook).
#[cfg(feature = "browser_ui")]
pub fn renderer_build_count_for_test() -> usize {
  UI_WORKER_RENDERER_BUILD_COUNT.load(Ordering::Relaxed)
}

/// Reset the per-process renderer build counter (test hook).
#[cfg(feature = "browser_ui")]
pub fn reset_renderer_build_count_for_test() {
  UI_WORKER_RENDERER_BUILD_COUNT.store(0, Ordering::Relaxed);
}

/// Returns the number of scroll-induced hover syncs executed so far (test hook).
#[cfg(feature = "browser_ui")]
pub fn scroll_hover_sync_count_for_test() -> usize {
  UI_WORKER_SCROLL_HOVER_SYNC_COUNT.load(Ordering::Relaxed)
}

/// Reset the per-process scroll-induced hover sync counter (test hook).
#[cfg(feature = "browser_ui")]
pub fn reset_scroll_hover_sync_count_for_test() {
  UI_WORKER_SCROLL_HOVER_SYNC_COUNT.store(0, Ordering::Relaxed);
}

/// Navigation URL that triggers the UI worker crash test when opted in.
#[cfg(feature = "browser_ui")]
pub const BROWSER_WORKER_CRASH_TEST_URL: &str = "crash://panic";

/// Handle to a spawned UI render worker thread.
///
/// The UI thread sends [`UiToWorker`] messages over `ui_tx`, and receives [`WorkerToUi`] updates on
/// `ui_rx`.
pub struct UiThreadWorkerHandle {
  pub ui_tx: Sender<UiToWorker>,
  pub ui_rx: Receiver<WorkerToUi>,
  pub join: std::thread::JoinHandle<()>,
}

impl UiThreadWorkerHandle {
  pub fn split(
    self,
  ) -> (
    Sender<UiToWorker>,
    Receiver<WorkerToUi>,
    std::thread::JoinHandle<()>,
  ) {
    (self.ui_tx, self.ui_rx, self.join)
  }

  pub fn join(self) -> std::thread::Result<()> {
    // Ensure the worker loop can observe channel closure before we block on joining.
    drop(self.ui_tx);
    self.join.join()
  }
}

/// Handle to the browser worker thread.
///
/// The UI thread sends [`UiToWorker`] messages over `tx`, and receives [`WorkerToUi`] updates on
/// `rx`.
pub struct BrowserWorkerHandle {
  pub tx: Sender<UiToWorker>,
  pub rx: Receiver<WorkerToUi>,
  pub join: std::thread::JoinHandle<()>,
}

#[derive(Debug, Clone)]
struct NavigationRequest {
  request: FormSubmission,
  apply_fragment_scroll: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum SiteKey {
  Origin(DocumentOrigin),
  /// Opaque/unknown site key (invalid URL, opaque origin).
  Opaque(String),
}

/// Maximum number of consecutive site-mismatch restarts for a single in-flight navigation.
///
/// This guards against redirect loops or a compromised renderer repeatedly committing a URL that
/// does not match the process it is running in.
const MAX_SITE_MISMATCH_RESTARTS: u8 = 3;

fn site_key_for_navigation(url: &str, parent_site: Option<&SiteKey>) -> SiteKey {
  let url = url.trim();

  // about:blank/about:srcdoc inherit the initiator origin when present (iframe/srcdoc patterns).
  // For top-level navigations without a parent, treat them as their own opaque site.
  if url.eq_ignore_ascii_case("about:blank") || url.eq_ignore_ascii_case("about:srcdoc") {
    if let Some(parent) = parent_site {
      return parent.clone();
    }
    return SiteKey::Opaque(url.to_string());
  }

  origin_from_url(url)
    .map(SiteKey::Origin)
    .unwrap_or_else(|| SiteKey::Opaque(url.to_string()))
}

// `UiToWorker::Tick` is the UI's periodic driver for time-based updates (CSS animations/transitions,
// animated images, JS timers/rAF, etc, and eventually media).
//
// The tick message carries an explicit `delta` so deterministic harnesses can drive animation time
// without relying on wall-clock time. The windowed browser UI supplies deltas based on its own tick
// scheduler.
//
// Do not treat ticks as a master clock for media playback: audio/video must be driven from a real
// master clock (audio device time when available) to avoid A/V drift. See `docs/media_clocking.md`.
const DEFAULT_TICK_INTERVAL: Duration = Duration::from_millis(16);

// -----------------------------------------------------------------------------
// Crash-isolation test hooks
// -----------------------------------------------------------------------------
//
// WARNING: These URLs are an *internal testing hook* that deliberately crashes the UI worker thread
// (simulating a renderer crash) so the browser can exercise crash recovery and multiprocess
// isolation logic.
//
// They are intentionally disabled by default and are only honored when the runtime toggle
// `FASTR_ENABLE_CRASH_URLS` is set to a truthy value. Do NOT enable this in normal browsing
// sessions.
const CRASH_URL_TOGGLE: &str = "FASTR_ENABLE_CRASH_URLS";

fn is_crash_panic_url(url: &str) -> bool {
  let Ok(parsed) = url::Url::parse(url.trim()) else {
    return false;
  };
  parsed.scheme().eq_ignore_ascii_case("crash")
    && parsed
      .host_str()
      .is_some_and(|host| host.eq_ignore_ascii_case("panic"))
}

/// Rate limit for debug logs emitted when renderer-preorder → dom2 mapping fails during JS event
/// dispatch.
///
/// Mouse move/hover events can be delivered at a very high frequency; keep logs bounded so mapping
/// bugs show up in integration test output without spamming.
const JS_EVENT_TARGET_MAPPING_LOG_INTERVAL: Duration = Duration::from_secs(1);

// -----------------------------------------------------------------------------
// Favicon loading
// -----------------------------------------------------------------------------

// Favicon payload sizing is kept in `ui::protocol_limits` so the UI and worker share the same
// invariants.

// -----------------------------------------------------------------------------
// Visited link state
// -----------------------------------------------------------------------------

/// Maximum number of visited URLs stored per tab.
///
/// This is intentionally bounded to avoid untrusted pages inducing unbounded memory growth by
/// generating unique URLs.
const VISITED_URL_STORE_MAX_ENTRIES: usize = 5_000;

/// Approximate byte budget for the per-tab visited URL store (sum of URL string lengths).
///
/// This is a secondary guard in addition to `VISITED_URL_STORE_MAX_ENTRIES` so pathological long
/// URLs cannot dominate memory usage even if the entry count remains small.
const VISITED_URL_STORE_MAX_BYTES: usize = 1_000_000;

#[derive(Debug, Clone, Default)]
struct VisitedUrlStore {
  order: VecDeque<Arc<str>>,
  set: FxHashSet<Arc<str>>,
  bytes: usize,
}

impl VisitedUrlStore {
  fn contains(&self, url: &str) -> bool {
    self.set.contains(url)
  }

  fn insert(&mut self, url: Arc<str>) {
    if self.set.contains(&url) {
      return;
    }
    self.bytes = self.bytes.saturating_add(url.len());
    self.order.push_back(Arc::clone(&url));
    self.set.insert(url);
    self.evict_if_needed();
  }

  fn record_visited_url(&mut self, url: &str) {
    let Some(url) = canonical_visited_url_key(url) else {
      return;
    };
    self.insert(url);
  }

  fn evict_if_needed(&mut self) {
    while self.order.len() > VISITED_URL_STORE_MAX_ENTRIES || self.bytes > VISITED_URL_STORE_MAX_BYTES {
      let Some(old) = self.order.pop_front() else {
        break;
      };
      self.bytes = self.bytes.saturating_sub(old.len());
      self.set.remove(&old);
    }
  }
}

fn canonical_visited_url_key(url: &str) -> Option<Arc<str>> {
  let mut parsed = url::Url::parse(url).ok()?;
  parsed.set_fragment(None);
  Some(Arc::from(parsed.into_string()))
}

fn resolve_href_for_visited(base: Option<&url::Url>, href: &str) -> Option<url::Url> {
  let href = trim_ascii_whitespace(href);
  if href.is_empty() {
    return None;
  }

  if href
    .as_bytes()
    .get(.."javascript:".len())
    .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"javascript:"))
  {
    return None;
  }

  if let Some(base) = base {
    if let Ok(joined) = base.join(href) {
      if joined.scheme().eq_ignore_ascii_case("javascript") {
        return None;
      }
      return Some(joined);
    }
  }

  let absolute = url::Url::parse(href).ok()?;
  (!absolute.scheme().eq_ignore_ascii_case("javascript")).then_some(absolute)
}

fn visited_link_node_ids_for_dom(
  dom: &crate::dom::DomNode,
  base_url: &str,
  store: &VisitedUrlStore,
) -> FxHashSet<usize> {
  let base_parsed = url::Url::parse(base_url).ok();
  let mut visited: FxHashSet<usize> = FxHashSet::default();

  let mut stack: Vec<&crate::dom::DomNode> = vec![dom];
  let mut next_id = 1usize;
  while let Some(node) = stack.pop() {
    let node_id = next_id;
    next_id += 1;

    if node.tag_name().is_some_and(|tag| {
      tag.eq_ignore_ascii_case("a") || tag.eq_ignore_ascii_case("area")
    }) {
      if let Some(href) = node.get_attribute_ref("href") {
        if let Some(mut resolved) = resolve_href_for_visited(base_parsed.as_ref(), href) {
          resolved.set_fragment(None);
          if store.contains(resolved.as_str()) {
            visited.insert(node_id);
          }
        }
      }
    }

    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }

  visited
}

// -----------------------------------------------------------------------------
// Download progress throttling
// -----------------------------------------------------------------------------

/// Minimum interval between `WorkerToUi::DownloadProgress` messages for a single download.
///
/// Large/fast downloads can otherwise generate extremely high message rates, waking the UI thread
/// frequently and increasing cross-thread channel overhead.
const DOWNLOAD_PROGRESS_MIN_INTERVAL: Duration = Duration::from_millis(75);

/// If bytes are still increasing but we haven't crossed [`DOWNLOAD_PROGRESS_MIN_BYTES`], emit a
/// progress update once this much time has passed.
const DOWNLOAD_PROGRESS_MAX_INTERVAL: Duration = Duration::from_millis(500);

/// Minimum received-byte delta to emit an update (subject to [`DOWNLOAD_PROGRESS_MIN_INTERVAL`]).
const DOWNLOAD_PROGRESS_MIN_BYTES: u64 = 256 * 1024;

fn should_emit_download_progress(
  received_bytes: u64,
  last_sent_bytes: u64,
  elapsed_since_last_sent: Duration,
  is_final: bool,
) -> bool {
  // Never suppress the final progress update: the UI should observe completion before
  // `WorkerToUi::DownloadFinished`.
  if is_final {
    return received_bytes != last_sent_bytes;
  }

  if received_bytes <= last_sent_bytes {
    return false;
  }

  // Time-based rate limit (caps update rate on very fast downloads).
  if elapsed_since_last_sent < DOWNLOAD_PROGRESS_MIN_INTERVAL {
    return false;
  }

  let delta = received_bytes - last_sent_bytes;
  if delta >= DOWNLOAD_PROGRESS_MIN_BYTES {
    return true;
  }

  // Slow transfers: still emit occasional updates so UI shows liveness.
  elapsed_since_last_sent >= DOWNLOAD_PROGRESS_MAX_INTERVAL
}

// -----------------------------------------------------------------------------
// JS post-navigation pump
// -----------------------------------------------------------------------------
//
// The UI worker renders using `BrowserDocument` (dom1) but also maintains a JS-capable `BrowserTab`
// (dom2) for event dispatch and (eventually) DOM-driven rendering. After committing a navigation we
// run a small bounded JS "pump" so pages that build UI during initial load (DOMContentLoaded
// handlers, deferred scripts, etc) can execute without waiting for a user event.
//
// Budgets must remain tight so hostile pages cannot hang the worker.
// DOMContentLoaded is queued behind a barrier task (see `js::document_lifecycle`), so we need to
// allow at least 2 tasks to ensure the lifecycle event itself can run.
const POST_NAV_JS_PUMP_MAX_TASKS: usize = 4;
const POST_NAV_JS_PUMP_MAX_MICROTASKS: usize = 10_000;
const POST_NAV_JS_PUMP_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(50);

// JS DOM event-dispatch pump
// -----------------------------------------------------------------------------
//
// When JS event handlers mutate the DOM, the UI worker must:
// - run a microtask checkpoint (Promises/queueMicrotask),
// - and then schedule a repaint so the renderer DOM can be resynced from `dom2`.
//
// Keep the budgets extremely tight: pointer-move can dispatch events frequently, and untrusted pages
// must not be able to hang the worker.
const DOM_EVENT_JS_PUMP_MAX_TASKS: usize = 8;
const DOM_EVENT_JS_PUMP_MAX_MICROTASKS: usize = 10_000;
const DOM_EVENT_JS_PUMP_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(2);
#[derive(Debug, Clone, Default)]
struct FindInPageWorkerState {
  query: String,
  case_sensitive: bool,
  matches: Vec<FindMatch>,
  active_match_index: Option<usize>,
}

struct TabState {
  history: TabHistory,
  loading: bool,
  pending_history_entry: bool,
  viewport_css: (u32, u32),
  dpr: f32,
  scroll_state: ScrollState,
  /// Last scroll state sent to the UI (either via `FrameReady.scroll_state` or a standalone
  /// `ScrollStateUpdated`).
  ///
  /// This allows the worker to emit `ScrollStateUpdated` only when a scroll change occurs without
  /// a corresponding `FrameReady` (e.g. cancelled paints), avoiding redundant messages in the hot
  /// paint path.
  last_reported_scroll_state: ScrollState,
  /// True when the next paint was triggered by a scroll message and we should coalesce any
  /// immediately-following scroll events before rendering.
  scroll_coalesce: bool,
  /// True when the next paint job was triggered by a scroll message (`UiToWorker::Scroll` /
  /// `UiToWorker::ScrollTo`).
  ///
  /// This is used to optionally apply a small paint-time deadline for scroll-triggered repaints so
  /// the worker can bail out quickly under heavy pages.
  next_paint_is_scroll: bool,
  document: Option<BrowserDocument>,
  js_tab: Option<BrowserTab>,
  /// Cached mapping from renderer pre-order ids (used by hit-testing/layout) back into the `dom2`
  /// NodeIds used by the JS tab.
  ///
  /// We cannot assume `dom2::NodeId` indices match renderer pre-order ids: `dom2` includes nodes
  /// that renderer snapshots drop (doctype/comments), and the renderer can synthesize nodes (e.g.
  /// `<wbr>` ZWSP text). Use `dom2::RendererDomMapping` instead.
  js_dom_mapping_generation: u64,
  js_dom_mapping: Option<crate::dom2::RendererDomMapping>,
  /// Debug-log rate limiter for missing JS DOM mappings, keyed by event type.
  js_dom_mapping_miss_log_last: HashMap<&'static str, Instant>,
  /// True when the JS tab's DOM has changed and needs to be synced into `document` before the next
  /// paint.
  js_dom_dirty: bool,
  /// Mutation generation of the JS tab's DOM last observed by the worker (used to detect changes
  /// between ticks and event dispatch).
  js_dom_mutation_generation: u64,
  interaction: InteractionEngine,
  cancel: CancelGens,
  last_committed_url: Option<String>,
  last_base_url: Option<String>,
  visited_urls: VisitedUrlStore,

  last_pointer_pos_css: Option<(f32, f32)>,
  pending_hover_sync_pos_css: Option<(f32, f32)>,
  last_pointer_click_count: u8,
  pointer_buttons: u16,
  last_hovered_dom_node_id: Option<usize>,
  last_hovered_dom_element_id: Option<String>,
  last_hovered_dom2_node: Option<crate::dom2::NodeId>,
  last_hovered_url: Option<String>,
  last_cursor: CursorKind,

  pending_navigation: Option<NavigationRequest>,
  needs_repaint: bool,
  force_repaint: bool,

  tick_time: Duration,

  /// Site key (origin) of the last successfully committed navigation.
  ///
  /// This is used to enforce site isolation invariants: navigations that commit to a different site
  /// than the renderer they ran in are treated as a site mismatch and restarted in a fresh
  /// renderer.
  site_key: Option<SiteKey>,
  /// Number of consecutive site-mismatch restarts for the current in-flight navigation.
  site_mismatch_restarts: u8,

  find: FindInPageWorkerState,
}

impl TabState {
  fn new(cancel: CancelGens) -> Self {
    Self {
      history: TabHistory::new(),
      loading: false,
      pending_history_entry: false,
      viewport_css: (800, 600),
      dpr: 1.0,
      scroll_state: ScrollState::default(),
      last_reported_scroll_state: ScrollState::default(),
      scroll_coalesce: false,
      next_paint_is_scroll: false,
      document: None,
      js_tab: None,
      js_dom_mapping_generation: 0,
      js_dom_mapping: None,
      js_dom_mapping_miss_log_last: HashMap::new(),
      js_dom_dirty: false,
      js_dom_mutation_generation: 0,
      interaction: InteractionEngine::new(),
      cancel,
      last_committed_url: None,
      last_base_url: None,
      visited_urls: VisitedUrlStore::default(),
      last_pointer_pos_css: None,
      pending_hover_sync_pos_css: None,
      last_pointer_click_count: 0,
      pointer_buttons: 0,
      last_hovered_dom_node_id: None,
      last_hovered_dom_element_id: None,
      last_hovered_dom2_node: None,
      last_hovered_url: None,
      last_cursor: CursorKind::Default,
      pending_navigation: None,
      needs_repaint: false,
      force_repaint: false,
      tick_time: Duration::ZERO,
      site_key: None,
      site_mismatch_restarts: 0,
      find: FindInPageWorkerState::default(),
    }
  }

  fn sync_js_viewport_state(&mut self) {
    let Some(js_tab) = self.js_tab.as_mut() else {
      return;
    };
    js_tab.set_viewport(self.viewport_css.0, self.viewport_css.1);
    js_tab.set_device_pixel_ratio(self.dpr);
  }

  fn sync_js_scroll_state(&mut self) {
    let Some(js_tab) = self.js_tab.as_mut() else {
      return;
    };
    js_tab.set_scroll_state(self.scroll_state.clone());
  }
}

fn sync_render_dom_from_js_tab(tab_id: TabId, tab: &mut TabState, ui_tx: &Sender<WorkerToUi>) {
  let Some(doc) = tab.document.as_mut() else {
    return;
  };
  let Some(js_tab) = tab.js_tab.as_ref() else {
    tab.js_dom_dirty = false;
    tab.js_dom_mutation_generation = 0;
    tab.js_dom_mapping_generation = 0;
    tab.js_dom_mapping = None;
    tab.js_dom_mapping_miss_log_last.clear();
    return;
  };

  let dom2 = js_tab.dom();
  let generation = dom2.mutation_generation();

  // Converting the live `dom2` tree into the renderer's DOM snapshot can be expensive and may panic
  // if `to_renderer_dom` hits an internal consistency bug. Keep it isolated so a single bad page
  // does not crash the UI worker thread.
  let (mut dom_snapshot, mapping) = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    let snapshot = dom2.to_renderer_dom_with_mapping();
    (snapshot.dom, snapshot.mapping)
  })) {
    Ok(snapshot) => snapshot,
    Err(_) => {
      let _ = ui_tx.send(WorkerToUi::DebugLog {
        tab_id,
        line: "panic while snapshotting JS DOM into renderer DOM".to_string(),
      });
      tab.js_dom_dirty = false;
      tab.js_dom_mutation_generation = generation;
      tab.js_dom_mapping_generation = 0;
      tab.js_dom_mapping = None;
      tab.js_dom_mapping_miss_log_last.clear();
      return;
    }
  };

  // Preserve live form control state stored in dom2 (values/checkedness/selectedness) when syncing
  // to the renderer DOM snapshot. Without this, the UI-side interaction engine can update dom1
  // attributes and later dom2→dom1 resync would clobber user edits.
  //
  // This projects dom2's internal form-control state slots into the snapshot DOM attributes used by
  // the renderer:
  // - input.value -> value= (except type=file; never leak user file paths/bytes),
  // - input.checked -> checked for checkbox/radio,
  // - textarea.value -> data-fastr-value (dirty only),
  // - option.selected -> selected.
  dom2.project_form_control_state_into_renderer_dom_snapshot(&mut dom_snapshot, &mapping);

  // Replace the renderer document's DOM in-place so we preserve its configured viewport/dpr/scroll
  // state/animation clock.
  doc.mutate_dom(|dom| {
    *dom = dom_snapshot;
    true
  });
  if let Some(committed_url) = tab.last_committed_url.as_deref() {
    // After syncing dom2 → dom1, recompute the effective base URL so relative URL resolution (links
    // and subresources) respects any JS-inserted/modified `<base href>`.
    let new_base_url = crate::html::document_base_url(doc.dom(), Some(committed_url));
    if new_base_url != tab.last_base_url {
      tab.last_base_url = new_base_url.clone();
      doc.set_navigation_urls(tab.last_committed_url.clone(), new_base_url.clone());
    }
  }
  tab.js_dom_mapping_generation = generation;
  tab.js_dom_mapping = Some(mapping);
  tab.js_dom_dirty = false;
  tab.js_dom_mutation_generation = generation;

  // The DOM replacement can change renderer preorder ids and invalidate cached hover targets.
  // Queue a best-effort hover resync so cursor/hover state (and JS hover transitions) reflect the
  // new DOM without requiring the UI to send a synthetic pointer move.
  tab.last_hovered_dom_node_id = None;
  tab.last_hovered_dom_element_id = None;
  tab.last_hovered_dom2_node = None;
  tab.pending_hover_sync_pos_css = tab.pending_hover_sync_pos_css.or(tab.last_pointer_pos_css);
}

fn forward_stage_heartbeats(tab_id: TabId, sender: Sender<WorkerToUi>) -> StageListenerGuard {
  let listener = Arc::new(move |stage: StageHeartbeat| {
    // Best-effort: UI might have dropped its receiver.
    let _ = sender.send(WorkerToUi::Stage { tab_id, stage });
  });
  push_stage_listener(Some(listener))
}

fn viewport_point_for_pos_css(scroll: &ScrollState, pos_css: (f32, f32)) -> Point {
  // The UI uses a sentinel `(-1, -1)` position to indicate that the pointer left the page image.
  //
  // `InteractionEngine` converts viewport points into page points by translating with
  // `scroll.viewport`. If we passed the sentinel directly it would translate to
  // `(scroll_x-1, scroll_y-1)` and still hit-test within the page.
  if pos_css.0.is_finite() && pos_css.1.is_finite() && pos_css.0 >= 0.0 && pos_css.1 >= 0.0 {
    Point::new(pos_css.0, pos_css.1)
  } else {
    let sx = if scroll.viewport.x.is_finite() {
      scroll.viewport.x
    } else {
      0.0
    };
    let sy = if scroll.viewport.y.is_finite() {
      scroll.viewport.y
    } else {
      0.0
    };
    Point::new(-sx - 1.0, -sy - 1.0)
  }
}

fn fragment_tree_for_hit_testing(
  doc: &BrowserDocument,
  scroll: &ScrollState,
) -> Option<crate::FragmentTree> {
  if scroll.viewport == Point::ZERO && scroll.elements.is_empty() {
    return None;
  }
  doc
    .prepared()
    .map(|prepared| prepared.fragment_tree_for_geometry(scroll))
}

fn trim_ascii_whitespace(value: &str) -> &str {
  // HTML attribute parsing ignores leading/trailing ASCII whitespace (TAB/LF/FF/CR/SPACE) but does
  // not treat all Unicode whitespace as ignorable (e.g. NBSP).
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
}

fn dom_node_type_eq(a: &crate::dom::DomNodeType, b: &crate::dom::DomNodeType) -> bool {
  use crate::dom::DomNodeType;
  match (a, b) {
    (
      DomNodeType::Document {
        quirks_mode: a_quirks,
        scripting_enabled: a_scripting,
        is_html_document: a_html,
      },
      DomNodeType::Document {
        quirks_mode: b_quirks,
        scripting_enabled: b_scripting,
        is_html_document: b_html,
      },
    ) => a_quirks == b_quirks && a_scripting == b_scripting && a_html == b_html,
    (
      DomNodeType::ShadowRoot {
        mode: a_mode,
        delegates_focus: a_delegates,
      },
      DomNodeType::ShadowRoot {
        mode: b_mode,
        delegates_focus: b_delegates,
      },
    ) => a_mode == b_mode && a_delegates == b_delegates,
    (
      DomNodeType::Slot {
        namespace: a_ns,
        attributes: a_attrs,
        assigned: a_assigned,
      },
      DomNodeType::Slot {
        namespace: b_ns,
        attributes: b_attrs,
        assigned: b_assigned,
      },
    ) => a_ns == b_ns && a_attrs == b_attrs && a_assigned == b_assigned,
    (
      DomNodeType::Element {
        tag_name: a_tag,
        namespace: a_ns,
        attributes: a_attrs,
      },
      DomNodeType::Element {
        tag_name: b_tag,
        namespace: b_ns,
        attributes: b_attrs,
      },
    ) => a_tag == b_tag && a_ns == b_ns && a_attrs == b_attrs,
    (DomNodeType::Text { content: a_content }, DomNodeType::Text { content: b_content }) => {
      a_content == b_content
    }
    _ => false,
  }
}

fn dom_tree_eq(a: &crate::dom::DomNode, b: &crate::dom::DomNode) -> bool {
  // Avoid recursion (deep/degenerate DOM trees can be hostile input).
  let mut stack: Vec<(&crate::dom::DomNode, &crate::dom::DomNode)> = vec![(a, b)];
  while let Some((a, b)) = stack.pop() {
    if !dom_node_type_eq(&a.node_type, &b.node_type) {
      return false;
    }
    if a.children.len() != b.children.len() {
      return false;
    }
    // Push in reverse so we compare children in document order.
    for (ac, bc) in a.children.iter().zip(b.children.iter()).rev() {
      stack.push((ac, bc));
    }
  }
  true
}

fn dom_input_type(node: &crate::dom::DomNode) -> &str {
  node
    .get_attribute_ref("type")
    .map(trim_ascii_whitespace)
    .filter(|v| !v.is_empty())
    .unwrap_or("text")
}

fn dom_is_text_input(node: &crate::dom::DomNode) -> bool {
  if !node
    .tag_name()
    .is_some_and(|tag| tag.eq_ignore_ascii_case("input"))
  {
    return false;
  }

  let t = dom_input_type(node);
  // Match the interaction engine's MVP heuristic: treat non-button-ish, non-choice-ish inputs as
  // text controls.
  !t.eq_ignore_ascii_case("checkbox")
    && !t.eq_ignore_ascii_case("radio")
    && !t.eq_ignore_ascii_case("button")
    && !t.eq_ignore_ascii_case("submit")
    && !t.eq_ignore_ascii_case("reset")
    && !t.eq_ignore_ascii_case("hidden")
    && !t.eq_ignore_ascii_case("range")
    && !t.eq_ignore_ascii_case("color")
    && !t.eq_ignore_ascii_case("file")
    && !t.eq_ignore_ascii_case("image")
}

fn dom_is_input(node: &crate::dom::DomNode) -> bool {
  node
    .tag_name()
    .is_some_and(|tag| tag.eq_ignore_ascii_case("input"))
}

fn dom_is_textarea(node: &crate::dom::DomNode) -> bool {
  node
    .tag_name()
    .is_some_and(|tag| tag.eq_ignore_ascii_case("textarea"))
}

fn dom_is_select(node: &crate::dom::DomNode) -> bool {
  node
    .tag_name()
    .is_some_and(|tag| tag.eq_ignore_ascii_case("select"))
}

fn dom_is_button(node: &crate::dom::DomNode) -> bool {
  node
    .tag_name()
    .is_some_and(|tag| tag.eq_ignore_ascii_case("button"))
}

fn dom_is_video_controls(node: &crate::dom::DomNode) -> bool {
  node
    .tag_name()
    .is_some_and(|tag| tag.eq_ignore_ascii_case("video"))
    && node.get_attribute_ref("controls").is_some()
}

fn mouse_event_button(button: PointerButton) -> i16 {
  match button {
    PointerButton::Primary => 0,
    PointerButton::Middle => 1,
    PointerButton::Secondary => 2,
    PointerButton::Back => 3,
    PointerButton::Forward => 4,
    PointerButton::Other(code) => i16::try_from(code).unwrap_or(i16::MAX),
    PointerButton::None => 0,
  }
}

fn mouse_buttons_mask_for_button(button: PointerButton) -> u16 {
  match button {
    PointerButton::Primary => 1,
    PointerButton::Secondary => 2,
    PointerButton::Middle => 4,
    PointerButton::Back => 8,
    PointerButton::Forward => 16,
    _ => 0,
  }
}

fn mouse_client_coord(value: f32) -> f64 {
  if value.is_finite() {
    value as f64
  } else {
    0.0
  }
}

fn js_dom_node_for_preorder_id(
  js_tab: &mut BrowserTab,
  preorder_id: usize,
  element_id: Option<&str>,
  _js_dom_mapping_generation: &mut u64,
  js_dom_mapping: &mut Option<crate::dom2::RendererDomMapping>,
) -> Option<crate::dom2::NodeId> {
  js_dom_mapping
    .as_ref()
    .and_then(|mapping| mapping.node_id_for_preorder(preorder_id))
    .or_else(|| element_id.and_then(|id| js_tab.dom().get_element_by_id(id)))
    // Fallback to the JS tab's cached renderer-preorder mapping (rebuilt when the dom2 document's
    // mutation generation changes). This is stable across dom2 insertions/removals via `NodeId` and
    // accounts for non-rendered/synthetic nodes (comments, `<wbr>` ZWSP injection, etc).
    .or_else(|| js_tab.dom2_node_for_renderer_preorder(preorder_id))
}

fn js_dom_node_for_preorder_id_with_log(
  ui_tx: &Sender<WorkerToUi>,
  tab_id: TabId,
  debug_log_enabled: bool,
  js_tab: &mut BrowserTab,
  preorder_id: usize,
  element_id: Option<&str>,
  js_dom_mapping_generation: &mut u64,
  js_dom_mapping: &mut Option<crate::dom2::RendererDomMapping>,
  js_dom_mapping_miss_log_last: &mut HashMap<&'static str, Instant>,
  event_name: &'static str,
) -> Option<crate::dom2::NodeId> {
  let mapping_cache_existed = js_dom_mapping.is_some();
  let node_id = js_dom_node_for_preorder_id(
    js_tab,
    preorder_id,
    element_id,
    js_dom_mapping_generation,
    js_dom_mapping,
  );
  if debug_log_enabled && node_id.is_none() {
    let now = Instant::now();
    let should_emit = match js_dom_mapping_miss_log_last.get(event_name) {
      None => true,
      Some(last) => now.duration_since(*last) >= JS_EVENT_TARGET_MAPPING_LOG_INTERVAL,
    };
    if should_emit {
      js_dom_mapping_miss_log_last.insert(event_name, now);
      let _ = ui_tx.send(WorkerToUi::DebugLog {
        tab_id,
        line: format!(
          "js event target mapping failed: type={event_name} preorder_id={preorder_id} element_id_present={} mapping_cache_present={mapping_cache_existed}",
          element_id.is_some(),
        ),
      });
    }
  }
  node_id
}

fn dom_node_by_preorder_id<'a>(
  root: &'a crate::dom::DomNode,
  preorder_id: usize,
) -> Option<&'a crate::dom::DomNode> {
  if preorder_id == 0 {
    return None;
  }
  let mut next_id = 1usize;
  let mut stack: Vec<&crate::dom::DomNode> = vec![root];
  while let Some(node) = stack.pop() {
    if next_id == preorder_id {
      return Some(node);
    }
    next_id += 1;
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

fn mirror_dom1_radio_group_state_into_dom2(
  js_tab: &mut BrowserTab,
  dom_mapping: Option<&crate::dom2::RendererDomMapping>,
  dom: &crate::dom::DomNode,
  active_preorder_id: usize,
) {
  use crate::dom::DomNodeType;

  // Prefer the renderer↔dom2 mapping cached from the last dom2→dom1 snapshot: it remains valid
  // across dom2 form-state mutations (which don't restructure the DOM) and avoids rebuilding the
  // preorder map on every user interaction.
  let mut owned_mapping: Option<crate::dom2::RendererDomMapping> = None;
  let mapping = match dom_mapping {
    Some(mapping) => mapping,
    None => {
      owned_mapping = Some(js_tab.dom().build_renderer_preorder_mapping());
      owned_mapping.as_ref().unwrap()
    }
  };

  // Build a lightweight 1-based preorder index for form-owner resolution. This mirrors
  // `interaction::dom_index::DomIndex::build`, but works on an immutable `DomNode` tree.
  let mut nodes_by_id: Vec<&crate::dom::DomNode> = vec![dom];
  let mut parent_by_id: Vec<usize> = vec![0];
  let mut id_by_element_id: std::collections::HashMap<String, usize> =
    std::collections::HashMap::new();
  let mut stack: Vec<(&crate::dom::DomNode, usize, bool)> = vec![(dom, 0, false)];
  while let Some((node, parent_id, in_template_contents)) = stack.pop() {
    let id = nodes_by_id.len();
    nodes_by_id.push(node);
    parent_by_id.push(parent_id);

    if !in_template_contents {
      if let Some(element_id) = node.get_attribute_ref("id") {
        // Keep the first occurrence to match `getElementById`.
        id_by_element_id.entry(element_id.to_string()).or_insert(id);
      }
    }

    let child_in_template_contents = in_template_contents || node.is_template_element();
    for child in node.children.iter().rev() {
      stack.push((child, id, child_in_template_contents));
    }
  }

  let Some(active) = nodes_by_id.get(active_preorder_id).copied() else {
    return;
  };
  if !active
    .tag_name()
    .is_some_and(|tag| tag.eq_ignore_ascii_case("input"))
    || !dom_input_type(active).eq_ignore_ascii_case("radio")
  {
    return;
  }

  fn is_root_boundary(node: &crate::dom::DomNode) -> bool {
    matches!(
      node.node_type,
      DomNodeType::Document { .. } | DomNodeType::ShadowRoot { .. }
    )
  }

  fn is_form_element(node: &crate::dom::DomNode) -> bool {
    node
      .tag_name()
      .is_some_and(|tag| tag.eq_ignore_ascii_case("form"))
      && matches!(node.namespace(), Some(ns) if ns.is_empty() || ns == crate::dom::HTML_NAMESPACE)
  }

  fn tree_root_boundary(
    nodes_by_id: &[&crate::dom::DomNode],
    parent_by_id: &[usize],
    node_id: usize,
  ) -> usize {
    let mut current = node_id;
    while current != 0 {
      if nodes_by_id
        .get(current)
        .is_some_and(|node| is_root_boundary(node))
      {
        return current;
      }
      current = parent_by_id.get(current).copied().unwrap_or(0);
    }
    node_id
  }

  fn form_owner(
    nodes_by_id: &[&crate::dom::DomNode],
    parent_by_id: &[usize],
    id_by_element_id: &std::collections::HashMap<String, usize>,
    node_id: usize,
  ) -> Option<usize> {
    let root_boundary = tree_root_boundary(nodes_by_id, parent_by_id, node_id);

    // Prefer the element's explicit `form=` association.
    let form_attr = nodes_by_id
      .get(node_id)
      .and_then(|node| node.get_attribute_ref("form").map(trim_ascii_whitespace))
      .filter(|v| !v.is_empty());
    if let Some(form_attr) = form_attr {
      if let Some(&id) = id_by_element_id.get(form_attr) {
        if nodes_by_id.get(id).is_some_and(|node| is_form_element(node))
          && tree_root_boundary(nodes_by_id, parent_by_id, id) == root_boundary
        {
          return Some(id);
        }
      }
    }

    // Otherwise, walk ancestors to find the nearest `<form>`, stopping at the tree-root boundary.
    let mut current = node_id;
    while current != 0 {
      let parent = parent_by_id.get(current).copied().unwrap_or(0);
      if parent == 0 {
        break;
      }
      current = parent;
      if current == root_boundary {
        break;
      }
      if nodes_by_id.get(current).is_some_and(|node| is_form_element(node)) {
        return Some(current);
      }
    }

    None
  }

  let group_name = active.get_attribute_ref("name").unwrap_or("");
  let active_form =
    form_owner(&nodes_by_id, &parent_by_id, &id_by_element_id, active_preorder_id);
  let active_root = if active_form.is_none() {
    tree_root_boundary(&nodes_by_id, &parent_by_id, active_preorder_id)
  } else {
    0
  };

  for node_id in 1..nodes_by_id.len() {
    let Some(node) = nodes_by_id.get(node_id).copied() else {
      continue;
    };
    if !node
      .tag_name()
      .is_some_and(|tag| tag.eq_ignore_ascii_case("input"))
      || !dom_input_type(node).eq_ignore_ascii_case("radio")
    {
      continue;
    }
    if node.get_attribute_ref("name").unwrap_or("") != group_name {
      continue;
    }

    let owner = form_owner(&nodes_by_id, &parent_by_id, &id_by_element_id, node_id);
    if let Some(active_form) = active_form {
      if owner != Some(active_form) {
        continue;
      }
    } else {
      if owner.is_some() {
        continue;
      }
      if tree_root_boundary(&nodes_by_id, &parent_by_id, node_id) != active_root {
        continue;
      }
    }

    let desired_checked = node.get_attribute_ref("checked").is_some();
    let Some(dom2_node) = mapping.node_id_for_preorder(node_id) else {
      continue;
    };
    let should_set = match js_tab.dom().input_checked(dom2_node) {
      Ok(current) => current != desired_checked,
      Err(_) => true,
    };
    if should_set {
      let _ = js_tab.dom_mut().set_input_checked(dom2_node, desired_checked);
    }
  }
}

fn mirror_dom1_form_control_state_into_dom2(
  js_tab: &mut BrowserTab,
  dom_mapping: Option<&crate::dom2::RendererDomMapping>,
  dom: &crate::dom::DomNode,
  preorder_id: usize,
  element_id: Option<&str>,
) {
  let Some(dom_node) = dom_node_by_preorder_id(dom, preorder_id) else {
    return;
  };

  let dom2_node_by_id = element_id.and_then(|id| js_tab.dom().get_element_by_id(id));
  let dom2_node = dom2_node_by_id
    .or_else(|| dom_mapping.and_then(|mapping| mapping.node_id_for_preorder(preorder_id)))
    .or_else(|| js_tab.dom2_node_for_renderer_preorder(preorder_id));
  let Some(dom2_node) = dom2_node else {
    return;
  };

  let Some(tag) = dom_node.tag_name() else {
    return;
  };

  // Mirror a subset of interactive form controls so:
  // - JS event handlers observe user edits applied by the UI-side interaction engine, and
  // - dom2→dom1 resync doesn't clobber UI-driven state stored only in dom1.
  if tag.eq_ignore_ascii_case("input") {
    let ty = dom_input_type(dom_node);
    if ty.eq_ignore_ascii_case("checkbox") {
      let checked = dom_node.get_attribute_ref("checked").is_some();
      let should_set = match js_tab.dom().input_checked(dom2_node) {
        Ok(current) => current != checked,
        Err(_) => true,
      };
      if should_set {
        let _ = js_tab.dom_mut().set_input_checked(dom2_node, checked);
      }

      // `InteractionEngine` default checkbox activation also clears a small set of auxiliary
      // checkbox-related attributes in the renderer snapshot. Mirror those mutations into dom2 so JS
      // sees a consistent attribute surface (`hasAttribute`, `getAttribute`) and future dom2→dom1
      // snapshots don't resurrect stale state (notably `:indeterminate` selectors and ARIA mixed).
      let indeterminate = dom_node.get_attribute_ref("indeterminate").is_some();
      let _ = js_tab
        .dom_mut()
        .set_bool_attribute(dom2_node, "indeterminate", indeterminate);
      match dom_node.get_attribute_ref("aria-checked") {
        Some(value) => {
          let _ = js_tab.dom_mut().set_attribute(dom2_node, "aria-checked", value);
        }
        None => {
          let _ = js_tab.dom_mut().remove_attribute(dom2_node, "aria-checked");
        }
      }
      return;
    }
    if ty.eq_ignore_ascii_case("radio") {
      // Activating a radio in dom1 can uncheck other radios in the same group. Mirror the whole
      // group into dom2 so JS doesn't observe stale checkedness.
      mirror_dom1_radio_group_state_into_dom2(js_tab, dom_mapping, dom, preorder_id);
      return;
    }
    if ty.eq_ignore_ascii_case("file") {
      let sync_attr = |js_tab: &mut BrowserTab,
                       node_id: crate::dom2::NodeId,
                       name: &'static str,
                       desired: Option<&str>| {
        let current = js_tab.dom().get_attribute(node_id, name).ok().flatten();
        match desired {
          Some(desired) => {
            if current.as_deref() != Some(desired) {
              let _ = js_tab.dom_mut().set_attribute(node_id, name, desired);
            }
          }
          None => {
            if current.is_some() {
              let _ = js_tab.dom_mut().remove_attribute(node_id, name);
            }
          }
        }
      };
      let desired_file_value = dom_node
        .get_attribute_ref("data-fastr-file-value")
        .filter(|v| !v.is_empty());
      sync_attr(js_tab, dom2_node, "data-fastr-file-value", desired_file_value);
      let desired_files = dom_node
        .get_attribute_ref("data-fastr-files")
        .filter(|v| !v.is_empty());
      sync_attr(js_tab, dom2_node, "data-fastr-files", desired_files);
      return;
    }

    // Avoid marking non-user-editable inputs (submit/reset/button/etc) as "dirty" in dom2. Those
    // controls expose their label via the content attribute and are not mutated by the UI-side
    // interaction engine.
    if ty.eq_ignore_ascii_case("button")
      || ty.eq_ignore_ascii_case("submit")
      || ty.eq_ignore_ascii_case("reset")
      || ty.eq_ignore_ascii_case("hidden")
      || ty.eq_ignore_ascii_case("image")
    {
      return;
    }

    let value = dom_node.get_attribute_ref("value").unwrap_or("");
    let should_set = match js_tab.dom().input_value(dom2_node) {
      Ok(current) => current != value,
      Err(_) => true,
    };
    if should_set {
      let _ = js_tab.dom_mut().set_input_value(dom2_node, value);
    }
    return;
  }

  if tag.eq_ignore_ascii_case("textarea") {
    let value = crate::dom::textarea_current_value(dom_node);
    let should_set = match js_tab.dom().textarea_value(dom2_node) {
      Ok(current) => current != value,
      Err(_) => true,
    };
    if should_set {
      let _ = js_tab.dom_mut().set_textarea_value(dom2_node, &value);
    }
    return;
  }

  if tag.eq_ignore_ascii_case("option") {
    let selected = dom_node.get_attribute_ref("selected").is_some();
    let should_set = match js_tab.dom().option_selected(dom2_node) {
      Ok(current) => current != selected,
      Err(_) => true,
    };
    if should_set {
      let _ = js_tab.dom_mut().set_option_selected(dom2_node, selected);
    }
    return;
  }
}

fn js_find_form_owner_for_submitter(
  dom: &crate::dom2::Document,
  submitter: crate::dom2::NodeId,
) -> Option<crate::dom2::NodeId> {
  use crate::dom2::NodeKind;

  let is_form_element = |node_id: crate::dom2::NodeId| -> bool {
    let node = dom.node(node_id);
    match &node.kind {
      NodeKind::Element {
        tag_name,
        namespace,
        ..
      } => {
        tag_name.eq_ignore_ascii_case("form")
          && (namespace.is_empty() || namespace == crate::dom::HTML_NAMESPACE)
      }
      _ => false,
    }
  };

  // Form owner resolution: prefer the submitter's explicit `form=` association.
  if let Ok(Some(form_attr)) = dom.get_attribute(submitter, "form") {
    let form_attr = trim_ascii_whitespace(&form_attr);
    if !form_attr.is_empty() {
      if let Some(form_id) = dom
        .get_element_by_id(form_attr)
        .filter(|id| is_form_element(*id))
      {
        return Some(form_id);
      }
    }
  }

  // Otherwise, walk ancestors to find the nearest `<form>`.
  let mut current = Some(submitter);
  while let Some(node_id) = current {
    if is_form_element(node_id) {
      return Some(node_id);
    }
    current = dom.node(node_id).parent;
  }
  None
}

fn cursor_kind_from_css_cursor(cursor: CursorKeyword) -> Option<CursorKind> {
  match cursor {
    CursorKeyword::Auto => None,
    CursorKeyword::None => Some(CursorKind::Hidden),
    CursorKeyword::Pointer => Some(CursorKind::Pointer),
    CursorKeyword::Text | CursorKeyword::VerticalText => Some(CursorKind::Text),
    CursorKeyword::Crosshair => Some(CursorKind::Crosshair),
    CursorKeyword::NotAllowed | CursorKeyword::NoDrop => Some(CursorKind::NotAllowed),
    CursorKeyword::Grab => Some(CursorKind::Grab),
    CursorKeyword::Grabbing => Some(CursorKind::Grabbing),
    _ => Some(CursorKind::Default),
  }
}
fn compute_scroll_metrics(
  doc: Option<&BrowserDocument>,
  viewport_css: (u32, u32),
  scroll_state: &ScrollState,
) -> ScrollMetrics {
  // `viewport_css` is already clamped by `BrowserLimits` when received from the UI, but keep this
  // helper robust when called from other code paths.
  let viewport_css = (viewport_css.0.max(1), viewport_css.1.max(1));
  let viewport_size = Size::new(viewport_css.0 as f32, viewport_css.1 as f32);

  let mut bounds = crate::scroll::ScrollBounds {
    min_x: 0.0,
    min_y: 0.0,
    max_x: 0.0,
    max_y: 0.0,
  };

  if let Some(prepared) = doc.and_then(|doc| doc.prepared()) {
    let chain =
      crate::scroll::build_scroll_chain(&prepared.fragment_tree().root, viewport_size, &[]);
    if let Some(root) = chain.last() {
      bounds = root.bounds;
    }
  }

  let sanitize_axis = |v: f32| if v.is_finite() { v.max(0.0) } else { 0.0 };
  let max_scroll_x = sanitize_axis(bounds.max_x);
  let max_scroll_y = sanitize_axis(bounds.max_y);

  let sanitize_scroll = |v: f32| if v.is_finite() { v.max(0.0) } else { 0.0 };
  let scroll_css = (
    sanitize_scroll(scroll_state.viewport.x),
    sanitize_scroll(scroll_state.viewport.y),
  );

  ScrollMetrics {
    viewport_css,
    scroll_css,
    bounds_css: crate::scroll::ScrollBounds {
      min_x: 0.0,
      min_y: 0.0,
      max_x: max_scroll_x,
      max_y: max_scroll_y,
    },
    content_css: (
      viewport_size.width + max_scroll_x,
      viewport_size.height + max_scroll_y,
    ),
  }
}

fn base_url_for_links<'a>(
  last_base_url: Option<&'a str>,
  last_committed_url: Option<&'a str>,
) -> &'a str {
  last_base_url
    .or(last_committed_url)
    .unwrap_or(about_pages::ABOUT_BASE_URL)
}

fn find_replaced_image_for_styled_node<'a>(
  root: &'a BoxNode,
  styled_node_id: usize,
) -> Option<&'a ReplacedType> {
  let mut stack: Vec<&BoxNode> = vec![root];
  while let Some(node) = stack.pop() {
    if node.styled_node_id == Some(styled_node_id) {
      if let BoxType::Replaced(replaced) = &node.box_type {
        if matches!(&replaced.replaced_type, ReplacedType::Image { .. }) {
          return Some(&replaced.replaced_type);
        }
      }
    }

    if let Some(body) = node.footnote_body.as_deref() {
      stack.push(body);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }

  None
}

/// Returns `true` when the renderer DOM contains CSS time-based effects that require periodic
/// sampling (keyframe animations or transitions).
///
/// Note: this is a CSS-only helper. The UI protocol's `RenderedFrame.wants_ticks` flag may also be
/// `true` for other time-based effects (e.g. JS timers/rAF, animated images) depending on which
/// subsystems are enabled for the tab.
fn document_wants_ticks(doc: &BrowserDocument) -> bool {
  doc.prepared().is_some_and(|prepared| {
    let tree = prepared.fragment_tree();
    !tree.keyframes.is_empty() || tree.transition_state.is_some()
  })
}

fn normalize_url_without_fragment(mut url: url::Url) -> url::Url {
  url.set_fragment(None);
  url
}

fn resolve_href_against(base: &url::Url, href: &str) -> Option<url::Url> {
  url::Url::parse(href).ok().or_else(|| base.join(href).ok())
}

fn favicon_fallback_url_for_origin(committed_url: &str) -> Option<String> {
  let parsed = url::Url::parse(committed_url).ok()?;
  match parsed.scheme() {
    "http" | "https" => {}
    _ => return None,
  }
  parsed.join("/favicon.ico").ok().map(|url| url.to_string())
}

fn discover_favicon_url(
  doc: &BrowserDocument,
  committed_url: &str,
  base_url: Option<&str>,
) -> Option<String> {
  // Don't attempt favicon discovery for internal `about:` pages.
  if about_pages::is_about_url(committed_url) {
    return None;
  }

  let base_for_resolution = base_url.unwrap_or(committed_url);
  find_document_favicon_url(doc.dom(), base_for_resolution)
    .or_else(|| favicon_fallback_url_for_origin(committed_url))
}

fn load_favicon_rgba_from_image_cache(
  image_cache: &crate::image_loader::ImageCache,
  favicon_url: &str,
) -> Option<(Vec<u8>, u32, u32)> {
  let image = image_cache.load(favicon_url).ok()?;

  if image.is_vector {
    let svg = image.svg_content.as_deref()?;
    let pixmap = image_cache
      .render_svg_pixmap_at_size(
        svg,
        MAX_FAVICON_EDGE_PX,
        MAX_FAVICON_EDGE_PX,
        favicon_url,
        1.0,
      )
      .ok()?;
    let (w, h) = (pixmap.width(), pixmap.height());
    if w == 0 || h == 0 {
      return None;
    }
    if w > MAX_FAVICON_EDGE_PX || h > MAX_FAVICON_EDGE_PX {
      return None;
    }
    let rgba = pixmap.data().to_vec();
    let expected_len = (w as usize).saturating_mul(h as usize).saturating_mul(4);
    if rgba.len() != expected_len || rgba.len() > MAX_FAVICON_BYTES {
      return None;
    }
    return Some((rgba, w, h));
  }

  let orientation = image.orientation.unwrap_or(OrientationTransform::IDENTITY);
  let mut rgba = image.to_oriented_rgba(orientation);

  let (src_w, src_h) = rgba.dimensions();
  if src_w == 0 || src_h == 0 {
    return None;
  }

  // Downscale (never upscale) each axis independently toward our cap.
  let target_w = src_w.min(MAX_FAVICON_EDGE_PX);
  let target_h = src_h.min(MAX_FAVICON_EDGE_PX);
  if target_w != src_w || target_h != src_h {
    rgba = image::imageops::resize(&rgba, target_w, target_h, FilterType::Triangle);
  }

  let (w, h) = rgba.dimensions();
  if w == 0 || h == 0 {
    return None;
  }
  if w > MAX_FAVICON_EDGE_PX || h > MAX_FAVICON_EDGE_PX {
    return None;
  }

  let mut data = rgba.into_raw();
  let expected_len = (w as usize).saturating_mul(h as usize).saturating_mul(4);
  if data.len() != expected_len || data.len() > MAX_FAVICON_BYTES {
    return None;
  }

  // Premultiply alpha for egui/wgpu rendering (egui uses premultiplied-alpha textures).
  for pixel in data.chunks_exact_mut(4) {
    let alpha = pixel[3] as f32 / 255.0;
    pixel[0] = (pixel[0] as f32 * alpha).round() as u8;
    pixel[1] = (pixel[1] as f32 * alpha).round() as u8;
    pixel[2] = (pixel[2] as f32 * alpha).round() as u8;
  }

  Some((data, w, h))
}

/// Returns the fully-resolved target URL when `href` is a same-document navigation that only
/// changes the fragment (e.g. `#target`).
fn same_document_fragment_target(current_url: &str, href: &str) -> Option<url::Url> {
  let current_parsed = url::Url::parse(current_url).ok()?;
  let target_parsed = resolve_href_against(&current_parsed, href)?;

  let current_base = normalize_url_without_fragment(current_parsed.clone());
  let target_base = normalize_url_without_fragment(target_parsed.clone());
  if current_base != target_base {
    return None;
  }

  // Only treat this as a fragment navigation when either side actually has a fragment component.
  // (Pure same-URL navigations still trigger a reload.)
  if current_parsed.fragment().is_none() && target_parsed.fragment().is_none() {
    return None;
  }

  // Ignore no-op navigations to the exact same URL string.
  (current_url != target_parsed.as_str()).then_some(target_parsed)
}

fn url_fragment(url: &str) -> Option<&str> {
  url.split_once('#').map(|(_, fragment)| fragment)
}

fn apply_original_fragment_to_final_url(original_url: &str, final_url: &str) -> String {
  let Some(fragment) = url_fragment(original_url) else {
    return final_url.to_string();
  };
  if final_url.contains('#') {
    return final_url.to_string();
  }
  format!("{final_url}#{fragment}")
}

fn styled_node_anchor_css(
  box_tree: &crate::BoxTree,
  fragment_tree: &crate::FragmentTree,
  scroll_state: &ScrollState,
  styled_node_id: usize,
) -> Option<Rect> {
  // BoxTree: find the first box produced by the element.
  let box_id = {
    let mut stack: Vec<&crate::BoxNode> = vec![&box_tree.root];
    let mut found = None;
    while let Some(node) = stack.pop() {
      if node.styled_node_id == Some(styled_node_id) {
        found = Some(node.id);
        break;
      }
      if let Some(body) = node.footnote_body.as_deref() {
        stack.push(body);
      }
      for child in node.children.iter().rev() {
        stack.push(child);
      }
    }
    found?
  };

  // FragmentTree: compute absolute page-space bounds for the box.
  let page_rect = crate::interaction::absolute_bounds_for_box_id(fragment_tree, box_id)?;

  // Convert page-space bounds to viewport-local coords for UI positioning.
  Some(page_rect.translate(Point::new(
    -scroll_state.viewport.x,
    -scroll_state.viewport.y,
  )))
}

fn select_anchor_css(
  box_tree: &crate::BoxTree,
  fragment_tree: &crate::FragmentTree,
  scroll_state: &ScrollState,
  select_node_id: usize,
) -> Option<Rect> {
  styled_node_anchor_css(box_tree, fragment_tree, scroll_state, select_node_id)
}

fn compute_page_accessibility_snapshot(
  doc: &BrowserDocument,
  interaction: &InteractionEngine,
  scroll_state: &ScrollState,
) -> Option<(crate::accessibility::AccessibilityNode, Vec<(usize, Rect)>)> {
  let prepared = doc.prepared()?;

  let tree = crate::accessibility::build_accessibility_tree(
    prepared.styled_tree(),
    Some(interaction.interaction_state()),
  )
  .ok()?;

  // Build a mapping from BoxTree box id -> DOM preorder id (`StyledNode.node_id`).
  let mut box_dom_id: Vec<usize> = vec![0];
  let mut stack: Vec<&crate::BoxNode> = vec![&prepared.box_tree().root];
  while let Some(node) = stack.pop() {
    let box_id = node.id;
    if box_id >= box_dom_id.len() {
      box_dom_id.resize(box_id.saturating_add(1), 0);
    }
    if let Some(dom_id) = node.styled_node_id {
      box_dom_id[box_id] = dom_id;
    }
    if let Some(body) = node.footnote_body.as_deref() {
      stack.push(body);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }

  // Walk the paint-time geometry tree once and union fragment bounds for each DOM node.
  let fragment_tree = prepared.fragment_tree_for_geometry(scroll_state);

  let mut dom_bounds: Vec<Option<Rect>> = vec![None];

  struct Frame<'a> {
    node: &'a crate::tree::fragment_tree::FragmentNode,
    parent_offset: Point,
  }

  let mut stack: Vec<Frame<'_>> = Vec::new();
  for root in fragment_tree.additional_fragments.iter().rev() {
    stack.push(Frame {
      node: root,
      parent_offset: Point::ZERO,
    });
  }
  stack.push(Frame {
    node: &fragment_tree.root,
    parent_offset: Point::ZERO,
  });

  while let Some(frame) = stack.pop() {
    let abs_bounds = frame.node.bounds.translate(frame.parent_offset);
    if let Some(box_id) = frame.node.box_id() {
      let dom_id = box_dom_id.get(box_id).copied().unwrap_or(0);
      if dom_id != 0 {
        if dom_id >= dom_bounds.len() {
          dom_bounds.resize(dom_id.saturating_add(1), None);
        }
        dom_bounds[dom_id] = Some(match dom_bounds[dom_id] {
          Some(existing) => existing.union(abs_bounds),
          None => abs_bounds,
        });
      }
    }

    let child_parent_offset = abs_bounds.origin;
    for child in frame.node.children.iter().rev() {
      stack.push(Frame {
        node: child,
        parent_offset: child_parent_offset,
      });
    }
  }

  // Convert page-space bounds to viewport-local coordinates.
  let scroll_x = if scroll_state.viewport.x.is_finite() {
    scroll_state.viewport.x
  } else {
    0.0
  };
  let scroll_y = if scroll_state.viewport.y.is_finite() {
    scroll_state.viewport.y
  } else {
    0.0
  };
  let to_viewport = Point::new(-scroll_x, -scroll_y);

  let mut bounds_css: Vec<(usize, Rect)> = Vec::new();
  for (dom_id, rect) in dom_bounds.into_iter().enumerate().skip(1) {
    if let Some(rect) = rect {
      bounds_css.push((dom_id, rect.translate(to_viewport)));
    }
  }

  Some((tree, bounds_css))
}

#[derive(Debug, Clone, Copy)]
enum SelectRow {
  OptGroupLabel,
  Option { node_id: usize, disabled: bool },
}

fn is_ancestor_or_self(
  index: &crate::interaction::dom_index::DomIndex,
  ancestor: usize,
  mut node: usize,
) -> bool {
  while node != 0 {
    if node == ancestor {
      return true;
    }
    node = *index.parent.get(node).unwrap_or(&0);
  }
  false
}

fn has_disabled_optgroup_ancestor(
  index: &crate::interaction::dom_index::DomIndex,
  mut node_id: usize,
  root_id: usize,
) -> bool {
  while node_id != 0 && node_id != root_id {
    let parent = *index.parent.get(node_id).unwrap_or(&0);
    if parent == 0 || parent == root_id {
      break;
    }
    if index.node(parent).is_some_and(|node| {
      node
        .tag_name()
        .is_some_and(|tag| tag.eq_ignore_ascii_case("optgroup"))
        && node.get_attribute_ref("disabled").is_some()
    }) {
      return true;
    }
    node_id = parent;
  }
  false
}

fn collect_select_rows(
  index: &crate::interaction::dom_index::DomIndex,
  select_id: usize,
) -> Vec<SelectRow> {
  // Mirror `build_select_control`: `<optgroup>` contributes a label row followed by its descendants.
  let mut end = select_id;
  for id in (select_id + 1)..=index.len() {
    if is_ancestor_or_self(index, select_id, id) {
      end = id;
    } else {
      break;
    }
  }

  let mut rows = Vec::new();
  for id in (select_id + 1)..=end {
    let Some(node) = index.node(id) else {
      continue;
    };
    if !node.is_element() {
      continue;
    }
    let Some(tag) = node.tag_name() else {
      continue;
    };

    if tag.eq_ignore_ascii_case("optgroup") {
      rows.push(SelectRow::OptGroupLabel);
      continue;
    }

    if tag.eq_ignore_ascii_case("option") {
      let disabled = node.get_attribute_ref("disabled").is_some()
        || has_disabled_optgroup_ancestor(index, id, select_id);
      rows.push(SelectRow::Option {
        node_id: id,
        disabled,
      });
    }
  }

  rows
}

enum Job {
  Navigate {
    tab_id: TabId,
    request: NavigationRequest,
  },
  Paint {
    tab_id: TabId,
    force: bool,
    is_scroll: bool,
  },
}

struct JobOutput {
  tab_id: TabId,
  snapshot: CancelSnapshot,
  snapshot_kind: SnapshotKind,
  msgs: Vec<WorkerToUi>,
}

#[derive(Clone, Copy)]
enum SnapshotKind {
  Prepare,
  Paint,
}

fn combine_cancel_callbacks(
  primary: Arc<crate::render_control::CancelCallback>,
  secondary: Option<Arc<crate::render_control::CancelCallback>>,
) -> Arc<crate::render_control::CancelCallback> {
  match secondary {
    Some(secondary) => {
      let primary = Arc::clone(&primary);
      let secondary = Arc::clone(&secondary);
      Arc::new(move || primary() || secondary())
    }
    None => primary,
  }
}

fn scroll_paint_budget_from_env() -> Option<std::time::Duration> {
  let raw = std::env::var("FASTR_SCROLL_PAINT_BUDGET_MS").ok()?;
  let raw = raw.trim();
  if raw.is_empty() {
    return None;
  }
  let raw = raw.replace('_', "");
  let ms = raw.parse::<u64>().ok()?;
  (ms > 0).then_some(std::time::Duration::from_millis(ms))
}

struct ActiveDownload {
  cancel: Arc<AtomicBool>,
  done: Arc<AtomicBool>,
}

struct BrowserRuntime {
  ui_rx: Receiver<UiToWorker>,
  ui_tx: Sender<WorkerToUi>,
  factory: FastRenderFactory,
  base_runtime_toggles: Arc<RuntimeToggles>,
  runtime_toggles: Arc<RuntimeToggles>,
  debug_log_enabled: bool,
  media_prefs: BrowserMediaPreferences,
  limits: BrowserLimits,
  /// Optional paint-time deadline budget for scroll-triggered repaints.
  ///
  /// When configured, `run_paint` applies this as a `RenderDeadline` timeout for paints that were
  /// triggered by `UiToWorker::Scroll` / `ScrollTo`. This helps keep scrolling responsive by
  /// allowing slow rasterization work to be abandoned and retried later.
  scroll_paint_budget: Option<std::time::Duration>,
  download_dir: PathBuf,
  /// In-flight downloads keyed by ID.
  ///
  /// This is shared across threads so cancellation requests can take effect even while the main
  /// worker thread is busy (e.g. rendering a frame).
  downloads: Arc<Mutex<HashMap<DownloadId, ActiveDownload>>>,
  tabs: HashMap<TabId, TabState>,
  active_tab: Option<TabId>,
  /// Messages deferred during scroll coalescing that should be handled before blocking for the next
  /// message.
  deferred_msgs: VecDeque<UiToWorker>,
  #[cfg(test)]
  viewport_changed_handled_for_test: usize,
}


impl BrowserRuntime {
  fn compute_effective_scroll_state_from_prepared(
    prepared: &crate::api::PreparedDocument,
    scroll_state: &ScrollState,
  ) -> ScrollState {
    // Mirror `api::paint_fragment_tree_with_state` scroll adjustments so the UI's scroll model can
    // stay in sync with the eventual painted frame.
    let mut fragment_tree = prepared.fragment_tree().clone();
    let mut state = crate::scroll::resolve_effective_scroll_state_for_paint_mut(
      &mut fragment_tree,
      scroll_state.clone(),
      prepared.layout_viewport(),
    );

    // Keep element scroll offsets stable (wheel interaction already clamps), but canonicalize the
    // representation so NaNs/inf and explicit zero offsets don't cause spurious diffs.
    state.elements.retain(|_, offset| {
      offset.x = if offset.x.is_finite() {
        offset.x.max(0.0)
      } else {
        0.0
      };
      offset.y = if offset.y.is_finite() {
        offset.y.max(0.0)
      } else {
        0.0
      };
      *offset != Point::ZERO
    });

    // Keep deltas finite as well so protocol consumers never need to defend against NaN.
    state.viewport_delta = Point::new(
      if state.viewport_delta.x.is_finite() {
        state.viewport_delta.x
      } else {
        0.0
      },
      if state.viewport_delta.y.is_finite() {
        state.viewport_delta.y
      } else {
        0.0
      },
    );
    for delta in state.elements_delta.values_mut() {
      delta.x = if delta.x.is_finite() { delta.x } else { 0.0 };
      delta.y = if delta.y.is_finite() { delta.y } else { 0.0 };
    }

    state
  }

  fn new(
    ui_rx: Receiver<UiToWorker>,
    ui_tx: Sender<WorkerToUi>,
    factory: FastRenderFactory,
    downloads: Arc<Mutex<HashMap<DownloadId, ActiveDownload>>>,
  ) -> Self {
    let base_runtime_toggles = factory.runtime_toggles();
    Self {
      ui_rx,
      ui_tx,
      factory,
      runtime_toggles: Arc::clone(&base_runtime_toggles),
      base_runtime_toggles,
      debug_log_enabled: false,
      media_prefs: BrowserMediaPreferences::default(),
      limits: BrowserLimits::from_env(),
      scroll_paint_budget: scroll_paint_budget_from_env(),
      download_dir: crate::ui::downloads::default_download_dir(),
      downloads,
      tabs: HashMap::new(),
      active_tab: None,
      deferred_msgs: VecDeque::new(),
      #[cfg(test)]
      viewport_changed_handled_for_test: 0,
    }
  }

  fn recv_message_blocking(&mut self) -> Option<UiToWorker> {
    if let Some(msg) = self.deferred_msgs.pop_front() {
      return Some(msg);
    }
    self.ui_rx.recv().ok()
  }

  fn try_recv_message(&mut self) -> Option<UiToWorker> {
    if let Some(msg) = self.deferred_msgs.pop_front() {
      return Some(msg);
    }
    self.ui_rx.try_recv().ok()
  }

  fn preempt_cancel_callback_for_job(
    &self,
    job_tab_id: TabId,
  ) -> Option<Arc<crate::render_control::CancelCallback>> {
    let active_tab = self.active_tab?;
    if active_tab == job_tab_id {
      return None;
    }
    let active = self.tabs.get(&active_tab)?;
    let snapshot = active.cancel.snapshot_paint();
    Some(snapshot.cancel_callback_for_paint(&active.cancel))
  }

  fn run(&mut self) {
    loop {
      // If there is no pending work, block for the next message.
      if !self.has_pending_jobs() {
        let Some(msg) = self.recv_message_blocking() else {
          break;
        };
        self.handle_message(msg);
      }

      // If a navigation is pending, coalesce any queued messages (viewport changes, rapid scroll
      // events, etc) before we start the expensive prepare step. This preserves expected semantics
      // for initial navigations (e.g. a `ViewportChanged` sent immediately after `CreateTab` should
      // affect fragment-scroll clamping), while still allowing input events like PointerDown and
      // PointerUp to each trigger their own paint + frame.
      if self
        .tabs
        .values()
        .any(|tab| tab.pending_navigation.is_some())
      {
        self.drain_messages();
      }

      // Scroll events can arrive in rapid bursts. If we are about to repaint due to scrolling,
      // briefly coalesce immediately-following scroll messages so only the latest scroll position
      // produces a frame (see `browser_thread_worker::cancellation_rapid_scroll_coalesces_to_last_frame`).
      //
      // Avoid doing this while a navigation is pending: navigation already drains queued messages
      // before preparing, and we don't want scroll coalescing to add latency to navigations.
      if !self
        .tabs
        .values()
        .any(|tab| tab.pending_navigation.is_some())
        && self.tabs.values().any(|tab| tab.scroll_coalesce)
       {
         self.drain_scroll_burst();
       }

       // Scrolling can move content under a stationary cursor. Queue a hover hit-test during scroll
       // handling and flush it once per coalesced scroll burst (or before the next paint job).
       self.flush_pending_hover_syncs();

       let Some(job) = self.next_job() else {
         continue;
       };

      let output = self.run_job(job);

      // Messages might have arrived while we were preparing/painting. Drain and handle them before
      // deciding whether to emit the (potentially stale) output.
      self.drain_messages();

      let Some(output) = output else {
        continue;
      };

      if !self.is_output_still_current(&output) {
        // If we drop a paint job's output (typically because the UI bumped `CancelGens` while the
        // frame was in-flight), we still want the UI model to learn about any scroll changes that
        // occurred while that frame was cancelled. `FrameReady` carries `scroll_state`, but in this
        // case no `FrameReady` is emitted, so send a standalone scroll update when needed.
        if matches!(output.snapshot_kind, SnapshotKind::Paint) {
          if let Some(tab) = self.tabs.get_mut(&output.tab_id) {
            if tab.scroll_state != tab.last_reported_scroll_state {
              tab.last_reported_scroll_state = tab.scroll_state.clone();
              let _ = self.ui_tx.send(WorkerToUi::ScrollStateUpdated {
                tab_id: output.tab_id,
                scroll: tab.scroll_state.clone(),
              });
            }
          }
        }
        continue;
      }

      for msg in output.msgs {
        // `DebugLog` traffic can be very high volume. When the UI has not opted in, suppress it
        // entirely so we don't waste wakeups/channel traffic on messages that will never be shown.
        if !self.debug_log_enabled && matches!(&msg, WorkerToUi::DebugLog { .. }) {
          continue;
        }
        match &msg {
          WorkerToUi::FrameReady { tab_id, frame } => {
            if let Some(tab) = self.tabs.get_mut(tab_id) {
              tab.last_reported_scroll_state = frame.scroll_state.clone();
            }
          }
          WorkerToUi::ScrollStateUpdated { tab_id, scroll } => {
            if let Some(tab) = self.tabs.get_mut(tab_id) {
              tab.last_reported_scroll_state = scroll.clone();
            }
          }
          _ => {}
        }
        let _ = self.ui_tx.send(msg);
      }
    }
  }

  fn has_pending_jobs(&self) -> bool {
    self
      .tabs
      .values()
      .any(|tab| tab.pending_navigation.is_some() || tab.needs_repaint)
  }

  fn drain_messages(&mut self) {
    fn flush_pending(
      runtime: &mut BrowserRuntime,
      pending_viewport: &mut HashMap<TabId, ((u32, u32), f32)>,
      pending_pointer_moves: &mut HashMap<
        TabId,
        ((f32, f32), PointerButton, crate::ui::PointerModifiers),
      >,
      pending_find_queries: &mut HashMap<TabId, (String, bool)>,
    ) {
      for (tab_id, (viewport_css, dpr)) in pending_viewport.drain() {
        runtime.handle_message(UiToWorker::ViewportChanged {
          tab_id,
          viewport_css,
          dpr,
        });
      }
      for (tab_id, (pos_css, button, modifiers)) in pending_pointer_moves.drain() {
        runtime.handle_message(UiToWorker::PointerMove {
          tab_id,
          pos_css,
          button,
          modifiers,
        });
      }
      for (tab_id, (query, case_sensitive)) in pending_find_queries.drain() {
        runtime.handle_message(UiToWorker::FindQuery {
          tab_id,
          query,
          case_sensitive,
        });
      }
    }

    fn flush_scroll_ops(
      runtime: &mut BrowserRuntime,
      pending_scroll_to: &mut HashMap<TabId, (f32, f32)>,
      pending_scroll_delta: &mut HashMap<TabId, (f32, f32)>,
    ) {
      if pending_scroll_to.is_empty() && pending_scroll_delta.is_empty() {
        return;
      }

      // Deterministic ordering avoids test flakiness when multiple tabs are scrolling.
      let mut tab_ids: Vec<TabId> = pending_scroll_to
        .keys()
        .chain(pending_scroll_delta.keys())
        .copied()
        .collect();
      tab_ids.sort_by_key(|tab_id| tab_id.0);
      tab_ids.dedup();

      for tab_id in tab_ids {
        if let Some(pos_css) = pending_scroll_to.remove(&tab_id) {
          runtime.handle_message(UiToWorker::ScrollTo { tab_id, pos_css });
        }
        if let Some(delta_css) = pending_scroll_delta.remove(&tab_id) {
          if delta_css != (0.0, 0.0) {
            runtime.handle_message(UiToWorker::Scroll {
              tab_id,
              delta_css,
              pointer_css: None,
            });
          }
        }
      }
    }

    // Coalesce viewport updates so we only apply the latest viewport/dpr per tab before the next
    // paint. UI-side throttling exists, but if the worker is busy (layout/paint), multiple viewport
    // updates can still queue up.
    let mut pending_viewport: HashMap<TabId, ((u32, u32), f32)> = HashMap::new();

    // Coalesce pointer-move bursts so we only do one hit-test per tab before the next paint job.
    //
    // Pointer-move can arrive at a very high frequency (especially with high polling-rate mice).
    // The renderer only needs the *latest* pointer position before repainting, so collapsing
    // back-to-back moves avoids redundant DOM hit-testing work.
    let mut pending_pointer_moves: HashMap<
      TabId,
      ((f32, f32), PointerButton, crate::ui::PointerModifiers),
    > = HashMap::new();

    // Find-in-page query updates can arrive on every keystroke. If the render worker is busy (heavy
    // page / slow paint), many `FindQuery` messages can backlog in the unbounded channel. Rebuilding
    // the find index for each intermediate query is wasted work, so coalesce to the latest query per
    // tab.
    let mut pending_find_queries: HashMap<TabId, (String, bool)> = HashMap::new();

    // Coalesce scroll messages so the worker does not fall behind when the UI emits many scroll
    // updates (e.g. scrollbar thumb drags, rapid programmatic scrolling).
    //
    // - `ScrollTo` is last-wins per tab.
    // - `Scroll { pointer_css: None }` is summed per tab.
    //
    // Other messages act as barriers: before handling any non-scroll message, flush the pending
    // scroll state changes in a deterministic order.
    let mut pending_scroll_to: HashMap<TabId, (f32, f32)> = HashMap::new();
    let mut pending_scroll_delta: HashMap<TabId, (f32, f32)> = HashMap::new();

    while let Some(msg) = self.try_recv_message() {
      match msg {
        UiToWorker::ScrollTo { tab_id, pos_css } => {
          flush_pending(
            self,
            &mut pending_viewport,
            &mut pending_pointer_moves,
            &mut pending_find_queries,
          );
          // A later `ScrollTo` overrides any earlier relative scroll deltas.
          pending_scroll_delta.remove(&tab_id);
          pending_scroll_to.insert(tab_id, pos_css);
        }
        UiToWorker::Scroll {
          tab_id,
          delta_css,
          pointer_css,
        } => {
          if pointer_css.is_some() {
            flush_pending(
              self,
              &mut pending_viewport,
              &mut pending_pointer_moves,
              &mut pending_find_queries,
            );
            flush_scroll_ops(self, &mut pending_scroll_to, &mut pending_scroll_delta);
            self.handle_message(UiToWorker::Scroll {
              tab_id,
              delta_css,
              pointer_css,
            });
            continue;
          }

          flush_pending(
            self,
            &mut pending_viewport,
            &mut pending_pointer_moves,
            &mut pending_find_queries,
          );
          let entry = pending_scroll_delta.entry(tab_id).or_insert((0.0, 0.0));
          entry.0 += delta_css.0;
          entry.1 += delta_css.1;
        }
        UiToWorker::ViewportChanged {
          tab_id,
          viewport_css,
          dpr,
        } => {
          flush_scroll_ops(self, &mut pending_scroll_to, &mut pending_scroll_delta);
          pending_viewport.insert(tab_id, (viewport_css, dpr));
        }
        UiToWorker::PointerMove {
          tab_id,
          pos_css,
          button,
          modifiers,
        } => {
          flush_scroll_ops(self, &mut pending_scroll_to, &mut pending_scroll_delta);
          pending_pointer_moves.insert(tab_id, (pos_css, button, modifiers));
        }
        UiToWorker::FindQuery {
          tab_id,
          query,
          case_sensitive,
        } => {
          flush_scroll_ops(self, &mut pending_scroll_to, &mut pending_scroll_delta);
          pending_find_queries.insert(tab_id, (query, case_sensitive));
        }
        other => {
          flush_scroll_ops(self, &mut pending_scroll_to, &mut pending_scroll_delta);
          flush_pending(
            self,
            &mut pending_viewport,
            &mut pending_pointer_moves,
            &mut pending_find_queries,
          );
          self.handle_message(other);
        }
      }
    }

    flush_scroll_ops(self, &mut pending_scroll_to, &mut pending_scroll_delta);
    flush_pending(
      self,
      &mut pending_viewport,
      &mut pending_pointer_moves,
      &mut pending_find_queries,
    );
  }

  fn drain_scroll_burst(&mut self) {
    use std::time::{Duration, Instant};

    // Keep this short: we only want to capture back-to-back scroll wheel events that happen within
    // the same UI input burst.
    const COALESCE_WINDOW: Duration = Duration::from_millis(1);

    let deadline = Instant::now() + COALESCE_WINDOW;

    // Reuse the existing pointer-move coalescing logic during scroll bursts so we don't do
    // redundant hit-testing work while the user is scrolling.
    let mut pending_viewport: HashMap<TabId, ((u32, u32), f32)> = HashMap::new();
    let mut pending_pointer_moves: HashMap<
      TabId,
      ((f32, f32), PointerButton, crate::ui::PointerModifiers),
    > = HashMap::new();

    fn flush_pending(
      runtime: &mut BrowserRuntime,
      pending_viewport: &mut HashMap<TabId, ((u32, u32), f32)>,
      pending_pointer_moves: &mut HashMap<
        TabId,
        ((f32, f32), PointerButton, crate::ui::PointerModifiers),
      >,
    ) {
      for (tab_id, (viewport_css, dpr)) in pending_viewport.drain() {
        runtime.handle_message(UiToWorker::ViewportChanged {
          tab_id,
          viewport_css,
          dpr,
        });
      }
      for (tab_id, (pos_css, button, modifiers)) in pending_pointer_moves.drain() {
        runtime.handle_message(UiToWorker::PointerMove {
          tab_id,
          pos_css,
          button,
          modifiers,
        });
      }
    }

    loop {
      let msg = match self.try_recv_message() {
        Some(msg) => Some(msg),
        None => {
          let remaining = deadline.saturating_duration_since(Instant::now());
          if remaining.is_zero() {
            None
          } else {
            match self.ui_rx.recv_timeout(remaining) {
              Ok(msg) => Some(msg),
              Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
              Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => None,
            }
          }
        }
      };

      let Some(msg) = msg else {
        break;
      };

      match msg {
        UiToWorker::ViewportChanged {
          tab_id,
          viewport_css,
          dpr,
        } => {
          pending_viewport.insert(tab_id, (viewport_css, dpr));
        }
        UiToWorker::PointerMove {
          tab_id,
          pos_css,
          button,
          modifiers,
        } => {
          pending_pointer_moves.insert(tab_id, (pos_css, button, modifiers));
        }
        UiToWorker::Scroll { .. } | UiToWorker::ScrollTo { .. } => {
          flush_pending(self, &mut pending_viewport, &mut pending_pointer_moves);
          self.handle_message(msg);
        }
        other => {
          flush_pending(self, &mut pending_viewport, &mut pending_pointer_moves);
          // Defer non-coalescible messages (clicks, navigations, etc) until after we render the
          // coalesced scroll frame.
          self.deferred_msgs.push_front(other);
          break;
        }
      }
    }

    flush_pending(self, &mut pending_viewport, &mut pending_pointer_moves);
  }

  fn flush_pending_hover_syncs(&mut self) {
    let mut pending = Vec::new();
    for (&tab_id, tab) in self.tabs.iter_mut() {
      if let Some(pos_css) = tab.pending_hover_sync_pos_css.take() {
        pending.push((tab_id, pos_css));
      }
    }

    for (tab_id, pos_css) in pending {
      #[cfg(feature = "browser_ui")]
      UI_WORKER_SCROLL_HOVER_SYNC_COUNT.fetch_add(1, Ordering::Relaxed);

      self.handle_pointer_move(
        tab_id,
        pos_css,
        PointerButton::None,
        crate::ui::PointerModifiers::NONE,
      );
    }
  }

  fn handle_message(&mut self, msg: UiToWorker) {
    // Best-effort cleanup of completed downloads.
    {
      let mut downloads = self.downloads.lock().unwrap_or_else(|err| err.into_inner());
      downloads.retain(|_, download| !download.done.load(Ordering::Acquire));
    }

    match msg {
      UiToWorker::SetMediaPreferences { prefs } => {
        if prefs == self.media_prefs {
          return;
        }

        self.runtime_toggles = crate::ui::media_prefs::runtime_toggles_with_browser_media_prefs(
          &self.base_runtime_toggles,
          prefs,
        );
        self.media_prefs = prefs;

        // Updating media preferences can change `@media (prefers-*)` query results. Ensure existing
        // documents invalidate style/layout so the next paint reflects the new environment.
        for tab in self.tabs.values_mut() {
          if let Some(doc) = tab.document.as_mut() {
            doc.set_runtime_toggles(Some(Arc::clone(&self.runtime_toggles)));
            tab.cancel.bump_paint();
            tab.needs_repaint = true;
            tab.force_repaint = true;
          }
        }
      }
      UiToWorker::SetDebugLogEnabled { enabled } => {
        self.debug_log_enabled = enabled;
      }
      UiToWorker::CreateTab {
        tab_id,
        initial_url,
        cancel,
      } => {
        self.tabs.insert(tab_id, TabState::new(cancel));
        self.active_tab.get_or_insert(tab_id);
        if let Some(url) = initial_url {
          self.schedule_navigation(tab_id, url, NavigationReason::TypedUrl);
        }
      }
      UiToWorker::NewTab {
        tab_id,
        initial_url,
      } => {
        self.tabs.insert(tab_id, TabState::new(CancelGens::new()));
        self.active_tab.get_or_insert(tab_id);
        if let Some(url) = initial_url {
          self.schedule_navigation(tab_id, url, NavigationReason::TypedUrl);
        }
      }
      UiToWorker::CloseTab { tab_id } => {
        self.tabs.remove(&tab_id);
        if self.active_tab == Some(tab_id) {
          self.active_tab = None;
        }
      }
      UiToWorker::SetActiveTab { tab_id } => {
        if !self.tabs.contains_key(&tab_id) {
          return;
        }

        let prev_active = self.active_tab;
        self.active_tab = Some(tab_id);
        if prev_active == Some(tab_id) {
          return;
        }

        let Some(tab) = self.tabs.get_mut(&tab_id) else {
          return;
        };
        tab.last_pointer_pos_css = None;

        let dom_changed = if let Some(doc) = tab.document.as_mut() {
          doc.mutate_dom(|dom| tab.interaction.clear_pointer_state(dom))
        } else {
          tab.interaction.clear_pointer_state_without_dom();
          false
        };
        if dom_changed {
          tab.cancel.bump_paint();
          tab.needs_repaint = true;
        }

        // Switching tabs should clear any stale hover state (cursor + hovered URL) until the UI
        // sends the next pointer position for this tab.
        Self::maybe_emit_hover_changed(&self.ui_tx, tab_id, tab, None, CursorKind::Default);
      }
      UiToWorker::Navigate {
        tab_id,
        url,
        reason,
      } => {
        self.schedule_navigation(tab_id, url, reason);
      }
      UiToWorker::NavigateRequest {
        tab_id,
        request,
        reason,
      } => {
        self.schedule_navigation_request(tab_id, request, reason);
      }
      UiToWorker::GoBack { tab_id } => {
        let url = {
          let Some(tab) = self.tabs.get_mut(&tab_id) else {
            return;
          };
          // Best-effort: persist the current scroll position before moving in history. This matters
          // when a scroll message has updated `tab.scroll_state` but the paint job hasn't run yet.
          //
          // Only do this when we are not in the middle of a navigation: during an in-flight
          // navigation, the history index may already point at the pending entry while the UI is
          // still showing the previous document/scroll state.
          if !tab.loading {
            tab
              .history
              .update_scroll(tab.scroll_state.viewport.x, tab.scroll_state.viewport.y);
          }
          tab.history.go_back().map(|entry| entry.url.clone())
        };
        if let Some(url) = url {
          self.begin_navigation(
            tab_id,
            FormSubmission {
              url,
              method: FormSubmissionMethod::Get,
              headers: Vec::new(),
              body: None,
            },
            NavigationReason::BackForward,
            false,
          );
        }
      }
      UiToWorker::GoForward { tab_id } => {
        let url = {
          let Some(tab) = self.tabs.get_mut(&tab_id) else {
            return;
          };
          if !tab.loading {
            tab
              .history
              .update_scroll(tab.scroll_state.viewport.x, tab.scroll_state.viewport.y);
          }
          tab.history.go_forward().map(|entry| entry.url.clone())
        };
        if let Some(url) = url {
          self.begin_navigation(
            tab_id,
            FormSubmission {
              url,
              method: FormSubmissionMethod::Get,
              headers: Vec::new(),
              body: None,
            },
            NavigationReason::BackForward,
            false,
          );
        }
      }
      UiToWorker::Reload { tab_id } => {
        let url = {
          let Some(tab) = self.tabs.get_mut(&tab_id) else {
            return;
          };
          if !tab.loading {
            tab
              .history
              .update_scroll(tab.scroll_state.viewport.x, tab.scroll_state.viewport.y);
          }
          tab
            .history
            .reload_target()
            .map(|entry| entry.url.clone())
            .or_else(|| tab.last_committed_url.clone())
        };
        if let Some(url) = url {
          self.begin_navigation(
            tab_id,
            FormSubmission {
              url,
              method: FormSubmissionMethod::Get,
              headers: Vec::new(),
              body: None,
            },
            NavigationReason::Reload,
            false,
          );
        }
      }
      UiToWorker::StopLoading { tab_id } => {
        let Some(tab) = self.tabs.get_mut(&tab_id) else {
          return;
        };

        // No-op when there is nothing to cancel.
        if !tab.loading && tab.pending_navigation.is_none() {
          return;
        }

        // Defensive: the windowed UI bumps cancel gens before sending stop, but tests and other
        // callers may send this message directly.
        tab.cancel.bump_nav();

        tab.pending_navigation = None;
        tab.loading = false;

        if tab.pending_history_entry {
          tab.history.cancel_pending_navigation_entry();
        } else {
          tab.history.revert_to_committed();
        }
        tab.pending_history_entry = false;

        let can_go_back = tab.history.can_go_back();
        let can_go_forward = tab.history.can_go_forward();

        let _ = self.ui_tx.send(WorkerToUi::LoadingState {
          tab_id,
          loading: false,
        });

        if let Some(entry) = tab.history.current() {
          let _ = self.ui_tx.send(WorkerToUi::NavigationCommitted {
            tab_id,
            url: entry.url.clone(),
            title: entry.title.clone(),
            can_go_back,
            can_go_forward,
          });
        }
      }
      UiToWorker::Tick { tab_id, delta } => {
        self.handle_tick(tab_id, delta);
      }
      UiToWorker::ViewportChanged {
        tab_id,
        viewport_css,
        dpr,
      } => {
        #[cfg(test)]
        {
          self.viewport_changed_handled_for_test = self.viewport_changed_handled_for_test.saturating_add(1);
        }

        let Some(tab) = self.tabs.get_mut(&tab_id) else {
          return;
        };
        let clamp = self.limits.clamp_viewport_and_dpr(viewport_css, dpr);
        tab.viewport_css = clamp.viewport_css;
        tab.dpr = clamp.dpr;
        if let Some(text) = clamp.warning_text(&self.limits) {
          let _ = self.ui_tx.send(WorkerToUi::Warning { tab_id, text });
        }
        // Viewport changes should cancel any in-flight paints, but do not attempt to paint before
        // the first navigation completes (no document/layout cache yet).
        tab.cancel.bump_paint();

        if let Some(doc) = tab.document.as_mut() {
          tab.needs_repaint = true;
          tab.force_repaint = true;
          doc.set_viewport(tab.viewport_css.0, tab.viewport_css.1);
          doc.set_device_pixel_ratio(tab.dpr);
        }
        tab.sync_js_viewport_state();
      }
      UiToWorker::Scroll {
        tab_id,
        delta_css,
        pointer_css,
      } => {
        {
          let Some(tab) = self.tabs.get_mut(&tab_id) else {
            return;
          };

          // Ignore invalid/no-op scroll deltas.
          let delta_x = delta_css.0;
          let delta_y = delta_css.1;
          if (!delta_x.is_finite() && !delta_y.is_finite()) || (delta_x == 0.0 && delta_y == 0.0) {
            return;
          }
          let delta_x = if delta_x.is_finite() { delta_x } else { 0.0 };
          let delta_y = if delta_y.is_finite() { delta_y } else { 0.0 };

          let Some(doc) = tab.document.as_mut() else {
            // No document yet (e.g. scrolling during initial load). Still record the viewport scroll
            // so it can be applied when the first frame is rendered.
            let prev = tab.scroll_state.clone();
            let mut next = prev.clone();
            next.viewport.x = (next.viewport.x + delta_x).max(0.0);
            next.viewport.y = (next.viewport.y + delta_y).max(0.0);
            if next.viewport != prev.viewport {
              next.update_deltas_from(&prev);
              tab.scroll_state = next;
              tab.sync_js_scroll_state();
              if tab.loading {
                tab
                  .history
                  .update_scroll(tab.scroll_state.viewport.x, tab.scroll_state.viewport.y);
              }
            }
            return;
          };

          // When scrolling with a stationary pointer, the hovered element can change as content
          // moves under the cursor. Track the latest pointer position so we can re-run hover
          // hit-testing after applying scroll offsets.
          let pointer_pos_css =
            pointer_css.filter(|(x, y)| x.is_finite() && y.is_finite() && *x >= 0.0 && *y >= 0.0);

          let current_scroll = doc.scroll_state();
          let mut changed = false;
          let mut scroll_changed = false;
          let mut wheel_handled = false;
          let mut emit_scroll_state_updated = false;

          if let Some(pointer_css) = pointer_pos_css {
            // Give a focused `<input type=number>` under the pointer a chance to consume the wheel
            // gesture for numeric stepping (instead of scrolling the page).
            let scroll_snapshot = tab.scroll_state.clone();
            let engine = &mut tab.interaction;
            let hit_tree = fragment_tree_for_hit_testing(doc, &scroll_snapshot);
            if let Ok(step_result) = doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
              let hit_tree = hit_tree.as_ref().unwrap_or(fragment_tree);
              let step_result = engine.wheel_step_number_input(
                dom,
                box_tree,
                hit_tree,
                &scroll_snapshot,
                Point::new(pointer_css.0, pointer_css.1),
                delta_y,
              );
              let changed = step_result.unwrap_or(false);
              (changed, step_result)
            }) {
              if let Some(dom_changed) = step_result {
                wheel_handled = true;
                changed |= dom_changed;
              }
            }

            if wheel_handled {
              // Numeric stepping does not update scroll state.
            } else {
              // Apply scroll wheel deltas to the scroll container under the pointer (including element
              // scroll offsets like `<select size>` listboxes).
              match doc.wheel_scroll_at_viewport_point(
                Point::new(pointer_css.0, pointer_css.1),
                (delta_x, delta_y),
              ) {
                Ok(scrolled) => {
                  wheel_handled = true;
                  if scrolled {
                    let next = doc.scroll_state();
                    let mut effective = if let Some(prepared) = doc.prepared() {
                      emit_scroll_state_updated = true;
                      Self::compute_effective_scroll_state_from_prepared(prepared, &next)
                     } else {
                       next
                     };
                    effective.update_deltas_from(&current_scroll);
                    doc.set_scroll_state(effective.clone());
                    tab.scroll_state = effective;
                    if let Some(js_tab) = tab.js_tab.as_mut() {
                      js_tab.set_scroll_state(tab.scroll_state.clone());
                    }
                    scroll_changed = true;
                    changed = true;
                  }
                }
                Err(_) => {
                  // No cached layout yet; fall back to basic viewport scrolling below.
                }
              }
            }
          }

          // If no pointer position was provided (or we couldn't apply wheel scrolling at all), treat
          // this as a basic viewport scroll and clamp to the content bounds when possible.
          if !wheel_handled {
            let mut next = current_scroll.clone();

            // Apply the raw delta first. When cached layout artifacts are available, we'll
            // immediately derive the effective snapped/clamped scroll state (matching paint) below.
            let apply_axis = |current: f32, delta: f32| {
              if delta == 0.0 || !delta.is_finite() {
                return current;
              }
              let value = current + delta;
              if value.is_finite() {
                value.max(0.0)
              } else {
                current
              }
            };

            next.viewport.x = apply_axis(next.viewport.x, delta_x);
            next.viewport.y = apply_axis(next.viewport.y, delta_y);

            if next != current_scroll {
              // Apply scroll snap/clamp immediately when we have cached layout artifacts so the UI
              // can use the effective scroll state before the next paint finishes.
              if let Some(prepared) = doc.prepared() {
                let mut effective =
                  Self::compute_effective_scroll_state_from_prepared(prepared, &next);
                if effective.viewport != current_scroll.viewport
                  || effective.elements != current_scroll.elements
                {
                  effective.update_deltas_from(&current_scroll);
                  doc.set_scroll_state(effective.clone());
                  tab.scroll_state = effective;
                  tab.sync_js_scroll_state();
                  scroll_changed = true;
                  emit_scroll_state_updated = true;
                  changed = true;
                } else {
                  // Preserve the historical "overscroll repaint" behaviour: even if the requested
                  // delta clamps back to the current scroll offset, still schedule a repaint so
                  // callers that expect a frame-per-scroll (e.g. for texture translation/latency
                  // hiding) continue to receive one.
                  tab.force_repaint = true;
                  changed = true;
                }
              } else {
                // No cached layout yet; record the raw scroll offset for the first render.
                next.update_deltas_from(&current_scroll);
                doc.set_scroll_state(next.clone());
                tab.scroll_state = next;
                tab.sync_js_scroll_state();
                scroll_changed = true;
                changed = true;
              }
            }
          }

          if changed {
            if scroll_changed && emit_scroll_state_updated {
              // Emit an early scroll-state update so UIs can async-scroll the last painted texture
              // while waiting for the repaint.
              let _ = self.ui_tx.send(WorkerToUi::ScrollStateUpdated {
                tab_id,
                scroll: tab.scroll_state.clone(),
              });
              tab.last_reported_scroll_state = tab.scroll_state.clone();
            }
            tab.cancel.bump_paint();
            tab.needs_repaint = true;
            tab.scroll_coalesce = true;
            tab.next_paint_is_scroll = scroll_changed;
            if scroll_changed {
              tab.pending_hover_sync_pos_css = pointer_pos_css.or(tab.last_pointer_pos_css);
            }
          }
        }
      }
      UiToWorker::ScrollTo { tab_id, pos_css } => {
        let Some(tab) = self.tabs.get_mut(&tab_id) else {
          return;
        };

        let sanitize = |v: f32| if v.is_finite() { v.max(0.0) } else { 0.0 };
        let target = Point::new(sanitize(pos_css.0), sanitize(pos_css.1));

        if let Some(doc) = tab.document.as_mut() {
          let current = doc.scroll_state();
          let mut next = current.clone();
          next.viewport = target;

          // When cached layout artifacts are available, compute the effective scroll state that
          // paint will apply (scroll snap + clamping).
          let effective = doc
            .prepared()
            .map(|prepared| Self::compute_effective_scroll_state_from_prepared(prepared, &next));

          if let Some(mut effective) = effective {
            if effective.viewport != current.viewport || effective.elements != current.elements {
              effective.update_deltas_from(&current);
              doc.set_scroll_state(effective.clone());
              tab.scroll_state = effective;
              tab.sync_js_scroll_state();
              let _ = self.ui_tx.send(WorkerToUi::ScrollStateUpdated {
                tab_id,
                scroll: tab.scroll_state.clone(),
              });
              tab.last_reported_scroll_state = tab.scroll_state.clone();
              tab.cancel.bump_paint();
              tab.needs_repaint = true;
              tab.scroll_coalesce = true;
              tab.next_paint_is_scroll = true;
            }
          } else if next != current {
            // No cached layout yet; record the scroll position for the first frame.
            next.update_deltas_from(&current);
            doc.set_scroll_state(next.clone());
            tab.scroll_state = next;
            tab.sync_js_scroll_state();
            tab.cancel.bump_paint();
            tab.needs_repaint = true;
            tab.scroll_coalesce = true;
            tab.next_paint_is_scroll = true;
          }
        } else {
          // No document yet; still record the scroll position for the first frame.
          let prev = tab.scroll_state.clone();
          let mut next = prev.clone();
          next.viewport = target;
          if next.viewport != prev.viewport {
            next.update_deltas_from(&prev);
            tab.scroll_state = next;
            tab.sync_js_scroll_state();
            if tab.loading {
              tab
                .history
                .update_scroll(tab.scroll_state.viewport.x, tab.scroll_state.viewport.y);
            }
          }
        }
      }
      #[cfg(feature = "browser_ui")]
      UiToWorker::AccessKitAction { tab_id, request } => {
        self.handle_accesskit_action(tab_id, request);
      }
      UiToWorker::PointerMove {
        tab_id,
        pos_css,
        button,
        modifiers,
      } => {
        self.handle_pointer_move(tab_id, pos_css, button, modifiers);
      }
      UiToWorker::PointerDown {
        tab_id,
        pos_css,
        button,
        modifiers,
        click_count,
      } => {
        self.handle_pointer_down(tab_id, pos_css, button, modifiers, click_count);
      }
      UiToWorker::PointerUp {
        tab_id,
        pos_css,
        button,
        modifiers,
      } => {
        self.handle_pointer_up(tab_id, pos_css, button, modifiers);
      }
      UiToWorker::DropFiles { tab_id, pos_css, paths } => {
        self.handle_drop_files(tab_id, pos_css, paths);
      }
      UiToWorker::ContextMenuRequest {
        tab_id,
        pos_css,
        modifiers,
      } => {
        self.handle_context_menu_request(tab_id, pos_css, modifiers);
      }
      #[cfg(feature = "browser_ui")]
      UiToWorker::AccessKitAction {
        tab_id,
        action,
        target_node_id,
      } => {
        self.handle_accesskit_action(tab_id, action, target_node_id);
      }
      UiToWorker::SelectDropdownChoose {
        tab_id,
        select_node_id,
        option_node_id,
      } => {
        self.handle_select_dropdown_choose(tab_id, select_node_id, option_node_id);
      }
      UiToWorker::SelectDropdownCancel { tab_id } => {
        // The browser UI typically owns the dropdown overlay state, so cancellation is a no-op on
        // the worker side. Emit `SelectDropdownClosed` anyway so front-ends that expect an explicit
        // close notification can dismiss the popup deterministically.
        let _ = self.ui_tx.send(WorkerToUi::SelectDropdownClosed { tab_id });
      }
      UiToWorker::SelectDropdownPick {
        tab_id,
        select_node_id,
        item_index,
      } => {
        self.handle_select_dropdown_pick(tab_id, select_node_id, item_index);
      }
      UiToWorker::DatalistChoose {
        tab_id,
        input_node_id,
        option_node_id,
      } => {
        self.handle_datalist_choose(tab_id, input_node_id, option_node_id);
      }
      UiToWorker::DatalistCancel { tab_id } => {
        // Front-ends typically own the overlay state, so cancellation is a no-op on the worker
        // side. Emit `DatalistClosed` anyway so UIs can dismiss the popup deterministically.
        let _ = self.ui_tx.send(WorkerToUi::DatalistClosed { tab_id });
      }
      UiToWorker::DateTimePickerChoose {
        tab_id,
        input_node_id,
        value,
      } => {
        self.handle_date_time_picker_choose(tab_id, input_node_id, value);
      }
      UiToWorker::DateTimePickerCancel { tab_id } => {
        // The browser UI typically owns the picker overlay state, so cancellation is a no-op on the
        // worker side. Emit `DateTimePickerClosed` anyway so front-ends that expect an explicit
        // close notification can dismiss the popup deterministically.
        let _ = self.ui_tx.send(WorkerToUi::DateTimePickerClosed { tab_id });
      }
      UiToWorker::ColorPickerChoose {
        tab_id,
        input_node_id,
        value,
      } => {
        self.handle_color_picker_choose(tab_id, input_node_id, value);
      }
      UiToWorker::ColorPickerCancel { tab_id } => {
        // Front-ends typically own the picker overlay state, so cancellation is a no-op on the
        // worker side. Emit `ColorPickerClosed` anyway so front-ends that expect an explicit close
        // notification can dismiss the popup deterministically.
        let _ = self.ui_tx.send(WorkerToUi::ColorPickerClosed { tab_id });
      }
      UiToWorker::FilePickerChoose {
        tab_id,
        input_node_id,
        paths,
      } => {
        self.handle_file_picker_choose(tab_id, input_node_id, paths);
      }
      UiToWorker::FilePickerCancel { tab_id } => {
        // Front-ends typically own the picker overlay state, so cancellation is a no-op on the
        // worker side. Emit `FilePickerClosed` anyway so front-ends can dismiss the popup
        // deterministically.
        let _ = self.ui_tx.send(WorkerToUi::FilePickerClosed { tab_id });
      }
      UiToWorker::TextInput { tab_id, text } => {
        self.handle_text_input(tab_id, &text);
      }
      UiToWorker::A11ySetTextValue {
        tab_id,
        node_id,
        value,
      } => {
        // Screen readers typically set text values on the currently focused element, but some
        // platforms send a SetValue request without an explicit focus action. Mirror browser
        // behaviour by ensuring the node is focused (with focus-visible) first.
        self.handle_a11y_set_focus(tab_id, node_id);

        let Some(tab) = self.tabs.get_mut(&tab_id) else {
          return;
        };
        let Some(doc) = tab.document.as_mut() else {
          return;
        };

        let changed =
          doc.mutate_dom(|dom| tab.interaction.set_text_control_value(dom, node_id, &value));
        if changed {
          if let Some(js_tab) = tab.js_tab.as_mut() {
            let dom_snapshot = doc.dom();
            let element_id = dom_node_by_preorder_id(dom_snapshot, node_id)
              .and_then(|node| node.get_attribute_ref("id"));
            mirror_dom1_form_control_state_into_dom2(
              js_tab,
              tab.js_dom_mapping.as_ref(),
              dom_snapshot,
              node_id,
              element_id,
            );
            tab.js_dom_mutation_generation = js_tab.dom().mutation_generation();
          }
          tab.cancel.bump_paint();
          tab.needs_repaint = true;
        }
      }
      UiToWorker::A11ySetTextSelectionRange {
        tab_id,
        node_id,
        start,
        end,
      } => {
        // AccessKit selection updates are typically targeted at the focused text control, but keep
        // this robust by focusing the node first so `InteractionEngine::a11y_set_text_selection_range`
        // can apply the update.
        self.handle_a11y_set_focus(tab_id, node_id);

        let Some(tab) = self.tabs.get_mut(&tab_id) else {
          return;
        };
        let Some(doc) = tab.document.as_mut() else {
          return;
        };

        let changed = doc.mutate_dom(|dom| {
          tab
            .interaction
            .a11y_set_text_selection_range(dom, node_id, start, end)
        });
        if changed {
          tab.cancel.bump_paint();
          tab.needs_repaint = true;
        }
      }
      UiToWorker::ImePreedit {
        tab_id,
        text,
        cursor,
      } => {
        self.handle_ime_preedit(tab_id, &text, cursor);
      }
      UiToWorker::ImeCommit { tab_id, text } => {
        self.handle_ime_commit(tab_id, &text);
      }
      UiToWorker::ImeCancel { tab_id } => {
        self.handle_ime_cancel(tab_id);
      }
      UiToWorker::Copy { tab_id } => {
        self.handle_copy(tab_id);
      }
      UiToWorker::Cut { tab_id } => {
        self.handle_cut(tab_id);
      }
      UiToWorker::Paste { tab_id, text } => {
        self.handle_paste(tab_id, &text);
      }
      UiToWorker::SelectAll { tab_id } => {
        self.handle_select_all(tab_id);
      }
      UiToWorker::KeyAction { tab_id, key } => {
        self.handle_key_action(tab_id, key);
      }
      UiToWorker::MediaCommand {
        tab_id,
        node_id,
        command,
      } => {
        // Media playback is owned by the renderer/DOM subsystem; for now, treat this as an input
        // event and surface it via the debug log so front-ends can validate wiring.
        let _ = self.ui_tx.send(WorkerToUi::DebugLog {
          tab_id,
          line: format!("MediaCommand node_id={node_id} command={command:?}"),
        });
      }
      UiToWorker::A11ySetFocus { tab_id, node_id } => {
        self.handle_a11y_set_focus(tab_id, node_id);
      }
      UiToWorker::A11yActivate { tab_id, node_id } => {
        self.handle_a11y_activate(tab_id, node_id);
      }
      UiToWorker::A11yScrollIntoView { tab_id, node_id } => {
        self.handle_a11y_scroll_into_view(tab_id, node_id);
      }
      UiToWorker::SetDownloadDirectory { path } => {
        self.set_download_directory(path);
      }
      UiToWorker::StartDownload {
        tab_id,
        url,
        filename_hint,
      } => {
        self.start_download(tab_id, url, filename_hint);
      }
      UiToWorker::CancelDownload {
        tab_id: _,
        download_id,
      } => {
        self.cancel_download(download_id);
      }
      UiToWorker::FindQuery {
        tab_id,
        query,
        case_sensitive,
      } => {
        self.handle_find_query(tab_id, &query, case_sensitive);
      }
      UiToWorker::FindNext { tab_id } => {
        self.handle_find_next(tab_id);
      }
      UiToWorker::FindPrev { tab_id } => {
        self.handle_find_prev(tab_id);
      }
      UiToWorker::FindStop { tab_id } => {
        self.handle_find_stop(tab_id);
      }
      UiToWorker::RequestRepaint { tab_id, reason: _ } => {
        let Some(tab) = self.tabs.get_mut(&tab_id) else {
          return;
        };
        tab.cancel.bump_paint();
        tab.needs_repaint = true;
        tab.force_repaint = true;
      }
    }
  }

  #[cfg(feature = "browser_ui")]
  fn handle_accesskit_action(&mut self, tab_id: TabId, request: accesskit::ActionRequest) {
    // Currently we only support scroll-related accessibility actions. The goal is to let assistive
    // technologies request that a node becomes visible without having to focus it.
    match request.action {
      accesskit::Action::ScrollIntoView | accesskit::Action::ScrollToPoint => {}
      _ => return,
    }

    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };
    let Some(doc) = tab.document.as_mut() else {
      return;
    };

    let target_node_id: usize = match usize::try_from(request.target.0.get()) {
      Ok(id) => id,
      Err(_) => return,
    };

    let next_scroll = {
      let Some(prepared) = doc.prepared() else {
        return;
      };
      crate::interaction::focus_scroll::scroll_state_for_focus(
        prepared.box_tree(),
        prepared.fragment_tree(),
        &tab.scroll_state,
        target_node_id,
      )
    };

    let Some(next_scroll) = next_scroll else {
      return;
    };

    tab.scroll_state = next_scroll;
    doc.set_scroll_state(tab.scroll_state.clone());
    tab.sync_js_scroll_state();
    tab
      .history
      .update_scroll(tab.scroll_state.viewport.x, tab.scroll_state.viewport.y);
    let _ = self.ui_tx.send(WorkerToUi::ScrollStateUpdated {
      tab_id,
      scroll: tab.scroll_state.clone(),
    });
    tab.last_reported_scroll_state = tab.scroll_state.clone();

    tab.cancel.bump_paint();
    tab.needs_repaint = true;
    tab.scroll_coalesce = true;
  }

  fn set_download_directory(&mut self, path: PathBuf) {
    if path.as_os_str().is_empty() {
      return;
    }

    if let Err(err) = std::fs::create_dir_all(&path) {
      // Best-effort: keep the worker running even if the directory is invalid. Attach the message
      // to an existing tab if possible so front-ends can surface it.
      if self.debug_log_enabled {
        if let Some(tab_id) = self.active_tab.or_else(|| self.tabs.keys().next().copied()) {
          let _ = self.ui_tx.send(WorkerToUi::DebugLog {
            tab_id,
            line: format!("failed to create download dir {}: {err}", path.display()),
          });
        }
      }
      return;
    }

    self.download_dir = path;
  }

  fn start_download(&mut self, tab_id: TabId, url: String, filename_hint: Option<String>) {
    let download_id = DownloadId::new();

    let requested_name = filename_hint
      .as_deref()
      .map(str::trim)
      .filter(|v| !v.is_empty())
      .map(|v| v.to_string())
      .unwrap_or_else(|| crate::ui::downloads::filename_from_url(&url));

    let download_dir = self.download_dir.clone();
    let final_path = crate::ui::downloads::choose_unique_download_path(&download_dir, &requested_name);
    let part_path = crate::ui::downloads::part_path_for_final(&final_path);
    let file_name = final_path
      .file_name()
      .map(|name| name.to_string_lossy().to_string())
      .unwrap_or_else(|| requested_name.clone());

    if let Err(err) = std::fs::create_dir_all(&download_dir) {
      let _ = self.ui_tx.send(WorkerToUi::DownloadStarted {
        tab_id,
        download_id,
        url: url.clone(),
        file_name,
        path: final_path,
        total_bytes: None,
      });
      let _ = self.ui_tx.send(WorkerToUi::DownloadFinished {
        tab_id,
        download_id,
        outcome: DownloadOutcome::Failed {
          error: format!("failed to create download dir {}: {err}", download_dir.display()),
        },
      });
      return;
    }

    let cancel = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    {
      let mut downloads = self.downloads.lock().unwrap_or_else(|err| err.into_inner());
      downloads.insert(
        download_id,
        ActiveDownload {
          cancel: Arc::clone(&cancel),
          done: Arc::clone(&done),
        },
      );
    }

    let _ = self.ui_tx.send(WorkerToUi::DownloadStarted {
      tab_id,
      download_id,
      url: url.clone(),
      file_name,
      path: final_path.clone(),
      total_bytes: None,
    });

    let ui_tx = self.ui_tx.clone();
    let thread_name = format!("fastr-download-{}", download_id.0);
    let spawn_result = std::thread::Builder::new()
      .name(thread_name)
      .spawn(move || {
        struct DoneGuard(Arc<AtomicBool>);
        impl Drop for DoneGuard {
          fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
          }
        }
        let _done_guard = DoneGuard(done);

        let finish = |outcome: DownloadOutcome| {
          let _ = ui_tx.send(WorkerToUi::DownloadFinished {
            tab_id,
            download_id,
            outcome,
          });
        };

        // Best-effort cleanup helper (ignore errors: file may not exist / be already removed).
        let cleanup_part = || {
          let _ = std::fs::remove_file(&part_path);
        };

        if cancel.load(Ordering::Acquire) {
          cleanup_part();
          finish(DownloadOutcome::Cancelled);
          return;
        }

        let parsed = match url::Url::parse(&url) {
          Ok(parsed) => parsed,
          Err(err) => {
            cleanup_part();
            finish(DownloadOutcome::Failed {
              error: format!("invalid download URL {url:?}: {err}"),
            });
            return;
          }
        };

        let (mut reader, total_bytes): (Box<dyn std::io::Read>, Option<u64>) =
          match parsed.scheme() {
            "file" => {
              let path = match parsed.to_file_path() {
                Ok(path) => path,
                Err(()) => {
                  cleanup_part();
                  finish(DownloadOutcome::Failed {
                    error: format!("failed to convert file:// URL to path: {url:?}"),
                  });
                  return;
                }
              };
              let total = std::fs::metadata(&path).ok().map(|m| m.len());
              let file = match std::fs::File::open(&path) {
                Ok(file) => file,
                Err(err) => {
                  cleanup_part();
                  finish(DownloadOutcome::Failed {
                    error: format!("failed to open download source {}: {err}", path.display()),
                  });
                  return;
                }
              };
              (Box::new(file), total)
            }
            #[cfg(feature = "direct_network")]
            "http" | "https" => {
              let client = match reqwest::blocking::Client::builder()
                .redirect(reqwest::redirect::Policy::limited(10))
                .build()
              {
                Ok(client) => client,
                Err(err) => {
                  cleanup_part();
                  finish(DownloadOutcome::Failed {
                    error: format!("failed to build HTTP client: {err}"),
                  });
                  return;
                }
              };

              let resp = match client.get(&url).send() {
                Ok(resp) => resp,
                Err(err) => {
                  cleanup_part();
                  finish(DownloadOutcome::Failed {
                    error: format!("HTTP request failed for {url}: {err}"),
                  });
                  return;
                }
              };

              if !resp.status().is_success() {
                cleanup_part();
                finish(DownloadOutcome::Failed {
                  error: format!("HTTP status {} for {url}", resp.status()),
                });
                return;
              }

              let total = resp.content_length();
              (Box::new(resp), total)
            }
            #[cfg(not(feature = "direct_network"))]
            "http" | "https" => {
              cleanup_part();
              finish(DownloadOutcome::Failed {
                error:
                  "HTTP(S) downloads are disabled in this build (missing `direct_network` feature)"
                    .to_string(),
              });
              return;
            }
            other => {
              cleanup_part();
              finish(DownloadOutcome::Failed {
                error: format!("unsupported download URL scheme: {other}"),
              });
              return;
            }
          };

        let _ = ui_tx.send(WorkerToUi::DownloadProgress {
          tab_id,
          download_id,
          received_bytes: 0,
          total_bytes,
        });
        let mut last_progress_sent_at = Instant::now();
        let mut last_progress_sent_bytes: u64 = 0;

        let mut writer = match std::fs::OpenOptions::new()
          .write(true)
          .create_new(true)
          .open(&part_path)
        {
          Ok(file) => file,
          Err(err) => {
            cleanup_part();
            finish(DownloadOutcome::Failed {
              error: format!(
                "failed to create temp download file {}: {err}",
                part_path.display()
              ),
            });
            return;
          }
        };

        const READ_CHUNK: usize = 16 * 1024;

        let mut buf = vec![0u8; READ_CHUNK];
        let mut received: u64 = 0;

        loop {
          if cancel.load(Ordering::Acquire) {
            drop(writer);
            cleanup_part();
            finish(DownloadOutcome::Cancelled);
            return;
          }

          let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(err) => {
              drop(writer);
              cleanup_part();
              finish(DownloadOutcome::Failed {
                error: format!("download read failed: {err}"),
              });
              return;
            }
          };

          if let Err(err) = writer.write_all(&buf[..n]) {
            drop(writer);
            cleanup_part();
            finish(DownloadOutcome::Failed {
              error: format!("download write failed: {err}"),
            });
            return;
          }

          received = received.saturating_add(n as u64);

          let now = Instant::now();
          let elapsed = now.duration_since(last_progress_sent_at);
          if should_emit_download_progress(received, last_progress_sent_bytes, elapsed, false) {
            last_progress_sent_at = now;
            last_progress_sent_bytes = received;
            let _ = ui_tx.send(WorkerToUi::DownloadProgress {
              tab_id,
              download_id,
              received_bytes: received,
              total_bytes,
            });
          }

          // Cooperate with cancellation/other threads even when downloading from fast local sources.
          std::thread::yield_now();
        }

        if let Err(err) = writer.flush() {
          drop(writer);
          cleanup_part();
          finish(DownloadOutcome::Failed {
            error: format!("download flush failed: {err}"),
          });
          return;
        }
        drop(writer);

        if cancel.load(Ordering::Acquire) {
          cleanup_part();
          finish(DownloadOutcome::Cancelled);
          return;
        }

        if let Err(err) = std::fs::rename(&part_path, &final_path) {
          cleanup_part();
          finish(DownloadOutcome::Failed {
            error: format!(
              "failed to finalize download (rename {} -> {}): {err}",
              part_path.display(),
              final_path.display()
            ),
          });
          return;
        }

        if should_emit_download_progress(
          received,
          last_progress_sent_bytes,
          Duration::ZERO,
          true,
        ) {
          let _ = ui_tx.send(WorkerToUi::DownloadProgress {
            tab_id,
            download_id,
            received_bytes: received,
            total_bytes,
          });
        }

        finish(DownloadOutcome::Completed);
      });

    if let Err(err) = spawn_result {
      let _ = self
        .downloads
        .lock()
        .map(|mut downloads| downloads.remove(&download_id));
      let _ = self.ui_tx.send(WorkerToUi::DownloadFinished {
        tab_id,
        download_id,
        outcome: DownloadOutcome::Failed {
          error: format!("failed to spawn download thread: {err}"),
        },
      });
    }
  }

  fn cancel_download(&mut self, download_id: DownloadId) {
    let downloads = self.downloads.lock().unwrap_or_else(|err| err.into_inner());
    if let Some(download) = downloads.get(&download_id) {
      download.cancel.store(true, Ordering::Release);
    }
  }

  fn schedule_navigation(&mut self, tab_id: TabId, url: String, reason: NavigationReason) {
    let requested_url = url.trim().to_string();
    if requested_url.is_empty() {
      return;
    }

    match reason {
      NavigationReason::TypedUrl => {
        // Only normalize user-typed URLs. Back/forward/reload should preserve the exact URL
        // stored in history (the UI sends those URLs verbatim).
        let url = crate::ui::normalize_user_url(&requested_url).unwrap_or(requested_url);
        self.begin_navigation(
          tab_id,
          FormSubmission {
            url,
            method: FormSubmissionMethod::Get,
            headers: Vec::new(),
            body: None,
          },
          NavigationReason::TypedUrl,
          true,
        );
      }
      NavigationReason::LinkClick => {
        // Link clicks are resolved by the interaction engine against the current document base
        // URL, so we treat them as already-canonical.
        self.begin_navigation(
          tab_id,
          FormSubmission {
            url: requested_url,
            method: FormSubmissionMethod::Get,
            headers: Vec::new(),
            body: None,
          },
          NavigationReason::LinkClick,
          true,
        );
      }
      NavigationReason::Reload => {
        let (nav_url, push_history) = {
          let Some(tab) = self.tabs.get_mut(&tab_id) else {
            return;
          };
          let push_history = tab.history.current().is_none();
          let nav_url = tab
            .history
            .reload_target()
            .map(|entry| entry.url.clone())
            .unwrap_or_else(|| requested_url.clone());
          (nav_url, push_history)
        };
        self.begin_navigation(
          tab_id,
          FormSubmission {
            url: nav_url,
            method: FormSubmissionMethod::Get,
            headers: Vec::new(),
            body: None,
          },
          NavigationReason::Reload,
          push_history,
        );
      }
      NavigationReason::BackForward => {
        let nav_url = {
          let Some(tab) = self.tabs.get_mut(&tab_id) else {
            return;
          };

          if tab
            .history
            .current()
            .is_some_and(|entry| entry.url == requested_url)
          {
            Some(requested_url.clone())
          } else {
            tab
              .history
              .go_back_forward_to(&requested_url)
              .map(|entry| entry.url.clone())
          }
        };

        let Some(nav_url) = nav_url else {
          if self.debug_log_enabled {
            let _ = self.ui_tx.send(WorkerToUi::DebugLog {
              tab_id,
              line: format!("ignoring BackForward navigation to unknown URL: {requested_url}"),
            });
          }
          return;
        };

        self.begin_navigation(
          tab_id,
          FormSubmission {
            url: nav_url,
            method: FormSubmissionMethod::Get,
            headers: Vec::new(),
            body: None,
          },
          NavigationReason::BackForward,
          false,
        );
      }
    }
  }

  fn schedule_navigation_request(
    &mut self,
    tab_id: TabId,
    mut request: FormSubmission,
    reason: NavigationReason,
  ) {
    request.url = request.url.trim().to_string();
    if request.url.is_empty() {
      return;
    }
    match reason {
      NavigationReason::TypedUrl | NavigationReason::LinkClick => {
        self.begin_navigation(tab_id, request, reason, true);
      }
      NavigationReason::Reload => {
        let push_history = {
          let Some(tab) = self.tabs.get(&tab_id) else {
            return;
          };
          tab.history.current().is_none()
        };
        self.begin_navigation(tab_id, request, reason, push_history);
      }
      NavigationReason::BackForward => {
        self.begin_navigation(tab_id, request, reason, false);
      }
    }
  }

  fn begin_navigation(
    &mut self,
    tab_id: TabId,
    request: FormSubmission,
    reason: NavigationReason,
    push_history: bool,
  ) {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };

    // New navigation from the UI: reset any site-mismatch restart loop counter.
    tab.site_mismatch_restarts = 0;

    // Navigations replace the document (or at least its URL/scroll state); clear any stale hover
    // metadata until the next pointer move re-establishes it.
    Self::maybe_emit_hover_changed(&self.ui_tx, tab_id, tab, None, CursorKind::Default);

    let had_pending_navigation = tab.loading;
    let had_pending_history_entry = tab.pending_history_entry;
    let url = request.url.clone();

    // Record visited URL state for link-click navigations.
    //
    // This is stored per-tab (not global profile) for now; it is later used to synthesize
    // `InteractionState.visited_links` for newly loaded documents without mutating the DOM.
    if reason == NavigationReason::LinkClick {
      tab.visited_urls.record_visited_url(&url);
    }
    // Fragment-only navigation within the same document: update URL + scroll state in-place.
    //
    // Avoid a full reload/reprepare; we reuse the cached layout artifacts for hit-testing and
    // compute a new viewport offset for the fragment target.
    //
    // `Reload` must not take this path because callers expect a full reload.
    let request_is_plain_get = request.method == FormSubmissionMethod::Get
      && request.headers.is_empty()
      && request.body.is_none();
    if reason != NavigationReason::Reload && request_is_plain_get {
      if !tab.loading {
        if let (Some(current), Some(doc)) =
          (tab.last_committed_url.as_deref(), tab.document.as_mut())
        {
          if let Some(target_url) = same_document_fragment_target(current, &url) {
            let url_string = target_url.to_string();

            if push_history {
              // Persist current scroll position for the previous history entry before pushing a
              // new entry for the fragment navigation.
              //
              // Note: for back/forward navigations, the history index has already been moved by
              // the caller, so updating scroll here would corrupt the target entry.
              tab
                .history
                .update_scroll(tab.scroll_state.viewport.x, tab.scroll_state.viewport.y);
              tab.history.push(url_string.clone());
            }

            tab.last_committed_url = Some(url_string.clone());
            doc.set_document_url(Some(url_string.clone()));

            let fragment = target_url.fragment().unwrap_or("");
            let offset = if matches!(reason, NavigationReason::BackForward) {
              tab
                .history
                .current()
                .map(|entry| Point::new(entry.scroll_x, entry.scroll_y))
                .unwrap_or(Point::ZERO)
            } else if fragment.is_empty() {
              Point::ZERO
            } else {
              match doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
                let viewport = fragment_tree.viewport_size();
                let offset = scroll_offset_for_fragment_target(
                  dom,
                  box_tree,
                  fragment_tree,
                  fragment,
                  viewport,
                );
                (false, offset)
              }) {
                Ok(Some(offset)) => offset,
                Ok(None) => Point::ZERO,
                Err(err) => {
                  if self.debug_log_enabled {
                    let _ = self.ui_tx.send(WorkerToUi::DebugLog {
                      tab_id,
                      line: format!("fragment navigation scroll failed: {err}"),
                    });
                  }
                  tab.scroll_state.viewport
                }
              }
            };

            let prev_scroll = tab.scroll_state.clone();
            let mut next_scroll = prev_scroll.clone();
            next_scroll.viewport = offset;
            next_scroll.update_deltas_from(&prev_scroll);
            tab.scroll_state = next_scroll.clone();
            doc.set_scroll_state(next_scroll);
            if let Some(js_tab) = tab.js_tab.as_mut() {
              js_tab.set_scroll_state(tab.scroll_state.clone());
            }

            let title = find_document_title(doc.dom());
            if let Some(title) = title.as_deref() {
              tab.history.set_title(title.to_string());
            }
            tab.history.mark_committed();
            tab.site_key = Some(site_key_for_navigation(&url_string, None));
            let _ = self.ui_tx.send(WorkerToUi::NavigationCommitted {
              tab_id,
              url: url_string,
              title,
              can_go_back: tab.history.can_go_back(),
              can_go_forward: tab.history.can_go_forward(),
            });

            tab.cancel.bump_paint();
            tab.needs_repaint = true;
            return;
          }
        }
      }
    }

    // Full navigations replace the document; clear any active find-in-page results so the UI does
    // not continue displaying stale match counts for the previous page.
    if !tab.find.query.is_empty()
      || tab.find.case_sensitive
      || tab.find.active_match_index.is_some()
      || !tab.find.matches.is_empty()
    {
      tab.find = FindInPageWorkerState::default();
      let _ = self.ui_tx.send(WorkerToUi::FindResult {
        tab_id,
        query: String::new(),
        case_sensitive: false,
        match_count: 0,
        active_match_index: None,
      });
    }

    tab.cancel.bump_nav();
    tab.loading = true;
    tab.needs_repaint = false;
    tab.pending_navigation = Some(NavigationRequest {
      request,
      apply_fragment_scroll: matches!(
        reason,
        NavigationReason::TypedUrl | NavigationReason::LinkClick
      ),
    });
    if push_history {
      if !had_pending_navigation {
        // Persist the current scroll position before pushing a new history entry. This is required
        // for correct scroll restoration when a scroll message arrives and the subsequent paint is
        // pre-empted by a navigation job.
        tab
          .history
          .update_scroll(tab.scroll_state.viewport.x, tab.scroll_state.viewport.y);
      }
      if had_pending_history_entry {
        // If we already pushed a provisional history entry for an in-flight navigation, normally
        // replace it in-place to avoid cancelled URLs showing up in the back/forward list.
        //
        // Exception: preserve `about:newtab` so that navigating away from a newly-created tab still
        // leaves the new-tab page accessible via back navigation even when the initial navigation
        // is superseded before it commits.
        if tab
          .history
          .current()
          .is_some_and(|entry| entry.url == about_pages::ABOUT_NEWTAB)
        {
          tab.history.push(url.clone());
        } else {
          tab.history.replace_current_url(url.clone());
        }
      } else {
        tab.history.push(url.clone());
      }
    }
    tab.pending_history_entry = push_history;

    let should_crash = reason == NavigationReason::TypedUrl
      && self.runtime_toggles.truthy(CRASH_URL_TOGGLE)
      && is_crash_panic_url(&url);

    let _ = self
      .ui_tx
      .send(WorkerToUi::NavigationStarted { tab_id, url });
    let _ = self.ui_tx.send(WorkerToUi::LoadingState {
      tab_id,
      loading: true,
    });

    if should_crash {
      // See `CRASH_URL_TOGGLE` for safety/usage notes.
      panic!("deliberate UI worker crash requested via crash://panic");
    }
  }

  fn handle_tick(&mut self, tab_id: TabId, delta: Duration) {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };

    // ---------------------------------------------------------------------------
    // CSS animations/transitions
    // ---------------------------------------------------------------------------
    //
    // Only schedule animation sampling when the document contains time-dependent primitives.
    // `BrowserDocument` resolves time-based CSS animations/transitions to a deterministic settled
    // state unless `RenderOptions.animation_time` is set. Use ticks to advance that time (and mark
    // paint dirty) so animated pages can produce multiple frames without explicit UI interaction.
    if let Some(doc) = tab.document.as_mut() {
      if document_wants_ticks(doc) && delta != Duration::ZERO {
        tab.tick_time = tab.tick_time.checked_add(delta).unwrap_or(Duration::MAX);

        // `BrowserDocument` consumes time in milliseconds as `f32` today. Keep the UI worker's
        // timeline as a `Duration` to avoid cumulative float drift, then convert at the API
        // boundary.
        let time_ms = tab.tick_time.as_secs_f64() * 1000.0;
        let time_ms = if time_ms.is_finite() {
          (time_ms.min(f32::MAX as f64)) as f32
        } else {
          f32::MAX
        };
        doc.set_animation_time_ms(time_ms);
        tab.needs_repaint = true;
      }
    }

    // Drive JS timers + requestAnimationFrame callbacks when the tab has a JS runtime.
    if let Some(js_tab) = tab.js_tab.as_mut() {
      let generation_before = js_tab.dom().mutation_generation();
      let prev_generation = tab.js_dom_mutation_generation;
      let cancel_snapshot = tab.cancel.snapshot_paint();
      let cancel_callback = cancel_snapshot.cancel_callback_for_paint(&tab.cancel);
      let deadline = deadline_for(cancel_callback.clone(), None);
      let _deadline_guard = DeadlineGuard::install(Some(&deadline));
      if !cancel_callback() {
        if let Err(err) = js_tab.run_until_stable(/* max_frames */ 1) {
          if self.debug_log_enabled && !cancel_callback() {
            let _ = self.ui_tx.send(WorkerToUi::DebugLog {
              tab_id,
              line: format!("js tick failed: {err}"),
            });
          }
        }
      }
      let generation_after = js_tab.dom().mutation_generation();
      if generation_before != prev_generation || generation_after != generation_before {
        tab.js_dom_dirty = true;
        tab.cancel.bump_paint();
        tab.needs_repaint = true;
      }
      tab.js_dom_mutation_generation = generation_after;
    }
  }

  fn handle_find_query(&mut self, tab_id: TabId, query: &str, case_sensitive: bool) {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };

    let query_changed = tab.find.query != query || tab.find.case_sensitive != case_sensitive;
    tab.find.query = query.to_string();
    tab.find.case_sensitive = case_sensitive;
    if query_changed {
      tab.find.active_match_index = None;
    }

    if tab.find.query.is_empty() {
      tab.find.matches.clear();
      tab.find.active_match_index = None;
      let _ = self.ui_tx.send(WorkerToUi::FindResult {
        tab_id,
        query: String::new(),
        case_sensitive,
        match_count: 0,
        active_match_index: None,
      });

      // Force a repaint so any highlight overlays are cleared.
      if tab.document.is_some() {
        tab.cancel.bump_paint();
        tab.needs_repaint = true;
        tab.force_repaint = true;
      }
      return;
    }

    if let Some(doc) = tab.document.as_ref() {
      if doc.prepared().is_some() {
        Self::rebuild_find_matches(&mut tab.find, &tab.scroll_state, doc);
      } else {
        tab.find.matches.clear();
        tab.find.active_match_index = None;
      }
    }

    if tab.find.active_match_index.is_none() && !tab.find.matches.is_empty() {
      tab.find.active_match_index = Some(0);
    }

    let _ = self.ui_tx.send(WorkerToUi::FindResult {
      tab_id,
      query: tab.find.query.clone(),
      case_sensitive: tab.find.case_sensitive,
      match_count: tab.find.matches.len(),
      active_match_index: tab.find.active_match_index,
    });

    Self::scroll_to_active_find_match(&self.ui_tx, tab_id, tab);

    if tab.document.is_some() {
      tab.cancel.bump_paint();
      tab.needs_repaint = true;
      tab.force_repaint = true;
    }
  }

  fn handle_find_next(&mut self, tab_id: TabId) {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };

    if tab.find.query.is_empty() {
      return;
    }

    if tab.find.matches.is_empty() {
      if let Some(doc) = tab.document.as_ref() {
        if doc.prepared().is_some() {
          Self::rebuild_find_matches(&mut tab.find, &tab.scroll_state, doc);
        }
      }
    }

    if tab.find.matches.is_empty() {
      let _ = self.ui_tx.send(WorkerToUi::FindResult {
        tab_id,
        query: tab.find.query.clone(),
        case_sensitive: tab.find.case_sensitive,
        match_count: 0,
        active_match_index: None,
      });
      if tab.document.is_some() {
        tab.cancel.bump_paint();
        tab.needs_repaint = true;
        tab.force_repaint = true;
      }
      return;
    }

    let count = tab.find.matches.len();
    let next = tab.find.active_match_index.unwrap_or(0).saturating_add(1) % count;
    tab.find.active_match_index = Some(next);

    let _ = self.ui_tx.send(WorkerToUi::FindResult {
      tab_id,
      query: tab.find.query.clone(),
      case_sensitive: tab.find.case_sensitive,
      match_count: count,
      active_match_index: tab.find.active_match_index,
    });

    Self::scroll_to_active_find_match(&self.ui_tx, tab_id, tab);

    if tab.document.is_some() {
      tab.cancel.bump_paint();
      tab.needs_repaint = true;
      tab.force_repaint = true;
    }
  }

  fn handle_find_prev(&mut self, tab_id: TabId) {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };

    if tab.find.query.is_empty() {
      return;
    }

    if tab.find.matches.is_empty() {
      if let Some(doc) = tab.document.as_ref() {
        if doc.prepared().is_some() {
          Self::rebuild_find_matches(&mut tab.find, &tab.scroll_state, doc);
        }
      }
    }

    if tab.find.matches.is_empty() {
      let _ = self.ui_tx.send(WorkerToUi::FindResult {
        tab_id,
        query: tab.find.query.clone(),
        case_sensitive: tab.find.case_sensitive,
        match_count: 0,
        active_match_index: None,
      });
      if tab.document.is_some() {
        tab.cancel.bump_paint();
        tab.needs_repaint = true;
        tab.force_repaint = true;
      }
      return;
    }

    let count = tab.find.matches.len();
    let current = tab.find.active_match_index.unwrap_or(0) % count;
    let prev = if current == 0 { count - 1 } else { current - 1 };
    tab.find.active_match_index = Some(prev);

    let _ = self.ui_tx.send(WorkerToUi::FindResult {
      tab_id,
      query: tab.find.query.clone(),
      case_sensitive: tab.find.case_sensitive,
      match_count: count,
      active_match_index: tab.find.active_match_index,
    });

    Self::scroll_to_active_find_match(&self.ui_tx, tab_id, tab);

    if tab.document.is_some() {
      tab.cancel.bump_paint();
      tab.needs_repaint = true;
      tab.force_repaint = true;
    }
  }

  fn handle_find_stop(&mut self, tab_id: TabId) {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };

    tab.find = FindInPageWorkerState::default();

    let _ = self.ui_tx.send(WorkerToUi::FindResult {
      tab_id,
      query: String::new(),
      case_sensitive: false,
      match_count: 0,
      active_match_index: None,
    });

    if tab.document.is_some() {
      tab.cancel.bump_paint();
      tab.needs_repaint = true;
      tab.force_repaint = true;
    }
  }

  fn rebuild_find_matches(
    find: &mut FindInPageWorkerState,
    scroll: &ScrollState,
    doc: &BrowserDocument,
  ) {
    let Some(prepared) = doc.prepared() else {
      find.matches.clear();
      find.active_match_index = None;
      return;
    };

    let tree = prepared.fragment_tree_for_geometry(scroll);
    let index = FindIndex::build(&tree);
    find.matches = index.find(
      &find.query,
      FindOptions {
        case_sensitive: find.case_sensitive,
      },
    );

    if find.matches.is_empty() {
      find.active_match_index = None;
    } else {
      let max = find.matches.len() - 1;
      let current = find.active_match_index.unwrap_or(0).min(max);
      find.active_match_index = Some(current);
    }
  }

  fn scroll_to_active_find_match(ui_tx: &Sender<WorkerToUi>, tab_id: TabId, tab: &mut TabState) {
    let Some(doc) = tab.document.as_mut() else {
      return;
    };

    let Some(active) = tab.find.active_match_index else {
      return;
    };
    let Some(m) = tab.find.matches.get(active) else {
      return;
    };
    let bounds = m.bounds;
    if bounds == Rect::ZERO {
      return;
    }

    let viewport_w = tab.viewport_css.0 as f32;
    let viewport_h = tab.viewport_css.1 as f32;

    let mut target = tab.scroll_state.viewport;

    if bounds.min_y() < target.y {
      target.y = bounds.min_y();
    } else if bounds.max_y() > target.y + viewport_h {
      target.y = bounds.max_y() - viewport_h;
    }

    if bounds.min_x() < target.x {
      target.x = bounds.min_x();
    } else if bounds.max_x() > target.x + viewport_w {
      target.x = bounds.max_x() - viewport_w;
    }

    if !target.x.is_finite() {
      target.x = 0.0;
    }
    if !target.y.is_finite() {
      target.y = 0.0;
    }
    target.x = target.x.max(0.0);
    target.y = target.y.max(0.0);

    if let Some(prepared) = doc.prepared() {
      let viewport = Size::new(viewport_w, viewport_h);
      if let Some(root) =
        crate::scroll::build_scroll_chain(&prepared.fragment_tree().root, viewport, &[]).last()
      {
        target = root.bounds.clamp(target);
      }
    }

    if target != tab.scroll_state.viewport {
      let prev = tab.scroll_state.clone();
      let mut next = prev.clone();
      next.viewport = target;
      next.update_deltas_from(&prev);
      doc.set_scroll_state(next.clone());
      tab.scroll_state = next;
      tab.sync_js_scroll_state();
      tab
        .history
        .update_scroll(tab.scroll_state.viewport.x, tab.scroll_state.viewport.y);
      let _ = ui_tx.send(WorkerToUi::ScrollStateUpdated {
        tab_id,
        scroll: tab.scroll_state.clone(),
      });
      tab.last_reported_scroll_state = tab.scroll_state.clone();
    }
  }

  fn apply_find_highlight(tab: &TabState, dpr: f32, pixmap: &mut tiny_skia::Pixmap) {
    if tab.find.matches.is_empty() {
      return;
    }

    let viewport_w = tab.viewport_css.0 as f32;
    let viewport_h = tab.viewport_css.1 as f32;
    let viewport_css = Rect::from_xywh(0.0, 0.0, viewport_w, viewport_h);
    let viewport_page = Rect::from_xywh(
      tab.scroll_state.viewport.x,
      tab.scroll_state.viewport.y,
      viewport_w,
      viewport_h,
    );

    let highlight = Rgba::new(255, 235, 59, 0.25);
    let highlight_active = Rgba::new(255, 193, 7, 0.35);

    let active = tab.find.active_match_index;

    for (idx, m) in tab.find.matches.iter().enumerate() {
      if Some(idx) == active {
        continue;
      }
      if m.rects.is_empty() || m.bounds == Rect::ZERO {
        continue;
      }
      if m.bounds.intersection(viewport_page).is_none() {
        continue;
      }

      for rect in &m.rects {
        let local = Rect::from_xywh(
          rect.x() - tab.scroll_state.viewport.x,
          rect.y() - tab.scroll_state.viewport.y,
          rect.width(),
          rect.height(),
        );
        let Some(visible) = local.intersection(viewport_css) else {
          continue;
        };
        fill_rect(
          pixmap,
          visible.x() * dpr,
          visible.y() * dpr,
          visible.width() * dpr,
          visible.height() * dpr,
          highlight,
        );
      }
    }

    let Some(active) = active else {
      return;
    };
    let Some(m) = tab.find.matches.get(active) else {
      return;
    };
    if m.rects.is_empty() || m.bounds == Rect::ZERO {
      return;
    }
    if m.bounds.intersection(viewport_page).is_none() {
      return;
    }

    for rect in &m.rects {
      let local = Rect::from_xywh(
        rect.x() - tab.scroll_state.viewport.x,
        rect.y() - tab.scroll_state.viewport.y,
        rect.width(),
        rect.height(),
      );
      let Some(visible) = local.intersection(viewport_css) else {
        continue;
      };
      fill_rect(
        pixmap,
        visible.x() * dpr,
        visible.y() * dpr,
        visible.width() * dpr,
        visible.height() * dpr,
        highlight_active,
      );
    }
  }

  fn maybe_emit_hover_changed(
    ui_tx: &Sender<WorkerToUi>,
    tab_id: TabId,
    tab: &mut TabState,
    hovered_url: Option<String>,
    cursor: CursorKind,
  ) {
    if tab.last_cursor == cursor && tab.last_hovered_url.as_deref() == hovered_url.as_deref() {
      return;
    }
    tab.last_cursor = cursor;
    tab.last_hovered_url = hovered_url.clone();
    let _ = ui_tx.send(WorkerToUi::HoverChanged {
      tab_id,
      hovered_url,
      cursor,
    });
  }

  fn pump_js_event_loop_after_dom_event_dispatch(
    &mut self,
    tab_id: TabId,
    tab: &mut TabState,
    generation_before_dispatch: u64,
  ) {
    let Some(js_tab) = tab.js_tab.as_mut() else {
      return;
    };
    let cancel_snapshot = tab.cancel.snapshot_paint();
    let cancel_callback = cancel_snapshot.cancel_callback_for_paint(&tab.cancel);
    let deadline = deadline_for(cancel_callback.clone(), Some(DOM_EVENT_JS_PUMP_TIMEOUT));
    let _deadline_guard = DeadlineGuard::install(Some(&deadline));

    let run_limits = RunLimits {
      max_tasks: DOM_EVENT_JS_PUMP_MAX_TASKS,
      max_microtasks: DOM_EVENT_JS_PUMP_MAX_MICROTASKS,
      max_wall_time: Some(DOM_EVENT_JS_PUMP_TIMEOUT),
    };

    let prev_generation = tab.js_dom_mutation_generation;
    if let Err(err) = js_tab.run_event_loop_until_idle(run_limits) {
      if self.debug_log_enabled && !cancel_callback() {
        let _ = self.ui_tx.send(WorkerToUi::DebugLog {
          tab_id,
          line: format!("js event-loop pump failed: {err}"),
        });
      }
    }

    let generation_after_dispatch = js_tab.dom().mutation_generation();
    if generation_before_dispatch != prev_generation
      || generation_after_dispatch != generation_before_dispatch
    {
      tab.js_dom_dirty = true;
      tab.cancel.bump_paint();
      tab.needs_repaint = true;
    }
    tab.js_dom_mutation_generation = generation_after_dispatch;
  }

  fn handle_pointer_move(
    &mut self,
    tab_id: TabId,
    pos_css: (f32, f32),
    button: PointerButton,
    modifiers: crate::ui::PointerModifiers,
  ) {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };
    // If a real pointer move arrives, it supersedes any pending scroll-induced hover sync. The
    // pointer move will do a fresh hit-test using the latest scroll offset.
    tab.pending_hover_sync_pos_css = None;
    let pointer_in_page =
      pos_css.0.is_finite() && pos_css.1.is_finite() && pos_css.0 >= 0.0 && pos_css.1 >= 0.0;
    tab.last_pointer_pos_css = pointer_in_page.then_some(pos_css);
    let scroll = &tab.scroll_state;
    let viewport_point = viewport_point_for_pos_css(scroll, pos_css);
    let base_url =
      base_url_for_links(tab.last_base_url.as_deref(), tab.last_committed_url.as_deref());

    let (changed, hovered_url, cursor, hovered_dom_node_id, hovered_dom_element_id) = {
      let Some(doc) = tab.document.as_mut() else {
        return;
      };
      let engine = &mut tab.interaction;
      let hit_tree = fragment_tree_for_hit_testing(doc, scroll);
      match doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
        let fragment_tree = hit_tree.as_ref().unwrap_or(fragment_tree);
        let (changed, hit) =
          engine.pointer_move_and_hit(dom, box_tree, fragment_tree, scroll, viewport_point);
        let drag_drop_active = engine.drag_drop_active_kind().is_some();
        let (
          hovered_url,
          mut cursor,
          hovered_dom_node_id,
          hovered_dom_element_id,
          hover_is_drop_target,
        ) = if !pointer_in_page {
          (None, CursorKind::Default, None, None, false)
        } else {
          match hit {
            Some(hit) => {
              let crate::interaction::HitTestResult {
                css_cursor,
                is_selectable_text,
                dom_element_id,
                is_editable_text_drop_target_candidate,
                form_control_cursor,
                dom_node_id,
                kind,
                href,
                ..
              } = hit;

              let is_drop_target = if drag_drop_active && is_editable_text_drop_target_candidate {
                let dom_index = crate::interaction::dom_index::DomIndex::build(dom);
                let disabled =
                  crate::interaction::effective_disabled::is_effectively_disabled(dom_node_id, &dom_index);
                let inert_or_hidden =
                  crate::interaction::effective_disabled::is_effectively_inert_or_hidden(dom_node_id, &dom_index);
                !(disabled || inert_or_hidden)
              } else {
                false
              };

              // Prefer the computed `cursor` property (including UA stylesheet defaults) so hover
              // behaviour matches the platform. Only fall back to legacy heuristics when the computed
              // cursor is `auto`.
              let css_cursor_kind = cursor_kind_from_css_cursor(css_cursor);

              // `hovered_url` remains a semantic link property even when CSS overrides the cursor.
              let hovered_url = match kind {
                HitTestKind::Link => href
                  .as_deref()
                  .and_then(|href| resolve_link_url(base_url, href)),
                _ => None,
              };

              let cursor = match css_cursor_kind {
                Some(cursor) => cursor,
                None => match kind {
                  HitTestKind::Link => {
                    // Keep showing the hand cursor over links even when we reject the URL scheme
                    // (e.g. `javascript:`).
                    CursorKind::Pointer
                  }
                  HitTestKind::FormControl => form_control_cursor,
                  _ => {
                    if is_selectable_text {
                      CursorKind::Text
                    } else {
                      CursorKind::Default
                    }
                  }
                },
              };

              (
                hovered_url,
                cursor,
                Some(dom_node_id),
                dom_element_id,
                is_drop_target,
              )
            }
            None => (None, CursorKind::Default, None, None, false),
          }
        };

        if pointer_in_page && drag_drop_active {
          cursor = if hover_is_drop_target {
            CursorKind::Grabbing
          } else {
            CursorKind::NotAllowed
          };
        }
        (
          changed,
          (
            changed,
            hovered_url,
            cursor,
            hovered_dom_node_id,
            hovered_dom_element_id,
          ),
        )
      }) {
        Ok(changed) => changed,
        Err(_) => return,
      }
    };
    if changed {
      tab.cancel.bump_paint();
      tab.needs_repaint = true;
    }

    // `<input type="range">` updates its value continuously while dragging. Mirror those UI-driven
    // value changes into dom2 so JS reads the live value and dom2→dom1 resync can't clobber the
    // slider state.
    if changed {
      if let (Some(range_node_id), Some(dom_snapshot), Some(js_tab)) = (
        tab.interaction.active_range_drag_node_id(),
        tab.document.as_ref().map(|doc| doc.dom()),
        tab.js_tab.as_mut(),
      ) {
        let element_id = dom_node_by_preorder_id(dom_snapshot, range_node_id)
          .and_then(|node| node.get_attribute_ref("id"));
        mirror_dom1_form_control_state_into_dom2(
          js_tab,
          tab.js_dom_mapping.as_ref(),
          dom_snapshot,
          range_node_id,
          element_id,
        );
        tab.js_dom_mutation_generation = js_tab.dom().mutation_generation();
      }
    }

    Self::maybe_emit_hover_changed(&self.ui_tx, tab_id, tab, hovered_url, cursor);

    // ---------------------------------------------------------------------------
    // DOM mouse events (`mousemove` + hover transitions)
    // ---------------------------------------------------------------------------
    let prev_hovered_dom_node_id = tab.last_hovered_dom_node_id;
    let prev_target = tab.last_hovered_dom2_node;
    tab.last_hovered_dom_node_id = hovered_dom_node_id;
    tab.last_hovered_dom_element_id = hovered_dom_element_id.clone();

    let pointer_buttons = tab.pointer_buttons;
    let Some(js_tab) = tab.js_tab.as_mut() else {
      tab.last_hovered_dom2_node = None;
      return;
    };
    let js_mutation_generation_before_dispatch = js_tab.dom().mutation_generation();
    let mut dispatched_dom_event = false;

    let mouse_base = web_events::MouseEvent {
      client_x: mouse_client_coord(pos_css.0),
      client_y: mouse_client_coord(pos_css.1),
      button: mouse_event_button(button),
      buttons: pointer_buttons,
      detail: 0,
      ctrl_key: modifiers.ctrl(),
      shift_key: modifiers.shift(),
      alt_key: modifiers.alt(),
      meta_key: modifiers.meta(),
      related_target: None,
    };

    // Avoid repeated renderer-preorder → dom2 NodeId mapping on the hot `PointerMove` path.
    //
    // Hover-transition ordering is computed from dom2 ancestor chains, so we cache the resolved dom2
    // target across events and only resolve the current renderer preorder id once per move event.
    let current_target = if hovered_dom_node_id.is_some()
      && hovered_dom_node_id == prev_hovered_dom_node_id
      && prev_target.is_some()
    {
      prev_target
    } else {
      hovered_dom_node_id.and_then(|preorder_id| {
        js_dom_node_for_preorder_id_with_log(
          &self.ui_tx,
          tab_id,
          self.debug_log_enabled,
          js_tab,
          preorder_id,
          hovered_dom_element_id.as_deref(),
          &mut tab.js_dom_mapping_generation,
          &mut tab.js_dom_mapping,
          &mut tab.js_dom_mapping_miss_log_last,
          "mousemove",
        )
      })
    };
    tab.last_hovered_dom2_node = current_target;
    // Prefer the mapped JS DOM node ids when determining hover transitions: the renderer pre-order id
    // can shift under DOM mutations (especially when we fall back to `getElementById` for stability),
    // but the dom2 `NodeId` for an element remains stable across insertions/removals.
    let hover_changed = prev_target != current_target;

    if hover_changed {
      let should_mouseout = prev_target.is_some_and(|prev_node_id| {
        let has_listeners = {
          let dom = js_tab.dom();
          dom.events().has_listeners_for_dispatch(
            web_events::EventTargetId::Node(prev_node_id),
            "mouseout",
            dom,
            /* bubbles */ true,
            /* composed */ false,
          )
        };
        has_listeners
          || js_tab
            .has_event_handler_property(web_events::EventTargetId::Node(prev_node_id), "mouseout")
            .unwrap_or(false)
      });

      // out on previous target.
      if let Some(prev_node_id) = prev_target {
        let related = current_target.map(|id| web_events::EventTargetId::Node(id).normalize());

        let mut mouse = mouse_base;
        mouse.related_target = related;

        if should_mouseout {
          dispatched_dom_event = true;
          let _ = js_tab.dispatch_mouse_event(
            prev_node_id,
            "mouseout",
            web_events::EventInit {
              bubbles: true,
              cancelable: true,
              composed: false,
            },
            mouse,
          );
        }
      }

      // `mouseleave`/`mouseenter` are dispatched for each element boundary crossed.
      //
      // For example, moving from a parent to its child should dispatch:
      // - `mouseout` (parent → child)
      // - `mouseover` (child ← parent)
      // - `mouseenter` (child)
      // but should NOT dispatch `mouseleave` on the parent, since the pointer is still within it.
      let (prev_chain, current_chain) = {
        fn element_chain(
          dom: &crate::dom2::Document,
          start: crate::dom2::NodeId,
        ) -> Vec<crate::dom2::NodeId> {
          let mut chain = Vec::new();
          let mut current = Some(start);
          while let Some(id) = current {
            let node = dom.node(id);
            if matches!(node.kind, crate::dom2::NodeKind::Element { .. }) {
              chain.push(id);
            }
            current = node.parent;
          }
          chain
        }

        let dom = js_tab.dom();
        (
          prev_target.map(|id| element_chain(dom, id)).unwrap_or_default(),
          current_target
            .map(|id| element_chain(dom, id))
            .unwrap_or_default(),
        )
      };

      // Find the lowest common ancestor in the (target → root) chains.
      let lca_indices = current_chain
        .iter()
        .enumerate()
        .find_map(|(current_idx, node_id)| {
          prev_chain
            .iter()
            .position(|prev_id| prev_id == node_id)
            .map(|prev_idx| (prev_idx, current_idx))
        });

      let prev_exited = match lca_indices {
        Some((prev_idx, _)) => &prev_chain[..prev_idx],
        None => &prev_chain[..],
      };
      let current_entered = match lca_indices {
        Some((_, current_idx)) => &current_chain[..current_idx],
        None => &current_chain[..],
      };

      let related_for_leave =
        current_target.map(|id| web_events::EventTargetId::Node(id).normalize());
      for &node_id in prev_exited {
        let should_mouseleave = {
          let has_listeners = {
            let dom = js_tab.dom();
            dom.events().has_listeners_for_dispatch(
              web_events::EventTargetId::Node(node_id),
              "mouseleave",
              dom,
              /* bubbles */ false,
              /* composed */ false,
            )
          };
          has_listeners
            || js_tab
              .has_event_handler_property(web_events::EventTargetId::Node(node_id), "mouseleave")
              .unwrap_or(false)
        };
        if should_mouseleave {
          let mut mouse = mouse_base;
          mouse.related_target = related_for_leave;
          dispatched_dom_event = true;
          let _ = js_tab.dispatch_mouse_event(
            node_id,
            "mouseleave",
            web_events::EventInit {
              bubbles: false,
              cancelable: false,
              composed: false,
            },
            mouse,
          );
        }
      }

      let should_mouseover = current_target.is_some_and(|new_node_id| {
        let has_listeners = {
          let dom = js_tab.dom();
          dom.events().has_listeners_for_dispatch(
            web_events::EventTargetId::Node(new_node_id),
            "mouseover",
            dom,
            /* bubbles */ true,
            /* composed */ false,
          )
        };
        has_listeners
          || js_tab
            .has_event_handler_property(web_events::EventTargetId::Node(new_node_id), "mouseover")
            .unwrap_or(false)
      });

      // over on new target.
      if let Some(new_node_id) = current_target {
        let related = prev_target.map(|id| web_events::EventTargetId::Node(id).normalize());

        let mut mouse = mouse_base;
        mouse.related_target = related;

        if should_mouseover {
          dispatched_dom_event = true;
          let _ = js_tab.dispatch_mouse_event(
            new_node_id,
            "mouseover",
            web_events::EventInit {
              bubbles: true,
              cancelable: true,
              composed: false,
            },
            mouse,
          );
        }
      }

      let related_for_enter = prev_target.map(|id| web_events::EventTargetId::Node(id).normalize());
      for &node_id in current_entered.iter().rev() {
        let should_mouseenter = {
          let has_listeners = {
            let dom = js_tab.dom();
            dom.events().has_listeners_for_dispatch(
              web_events::EventTargetId::Node(node_id),
              "mouseenter",
              dom,
              /* bubbles */ false,
              /* composed */ false,
            )
          };
          has_listeners
            || js_tab
              .has_event_handler_property(web_events::EventTargetId::Node(node_id), "mouseenter")
              .unwrap_or(false)
        };
        if should_mouseenter {
          let mut mouse = mouse_base;
          mouse.related_target = related_for_enter;
          dispatched_dom_event = true;
          let _ = js_tab.dispatch_mouse_event(
            node_id,
            "mouseenter",
            web_events::EventInit {
              bubbles: false,
              cancelable: false,
              composed: false,
            },
            mouse,
          );
        }
      }
    }

    // `mousemove` should be dispatched after hover-transition events (`mouseout`/`mouseover`, etc.)
    // for browser-like ordering.
    let should_mousemove = current_target.is_some_and(|target_node_id| {
      let has_listeners = {
        let dom = js_tab.dom();
        dom.events().has_listeners_for_dispatch(
          web_events::EventTargetId::Node(target_node_id),
          "mousemove",
          dom,
          /* bubbles */ true,
          /* composed */ false,
        )
      };
      has_listeners
        || js_tab
          .has_event_handler_property(web_events::EventTargetId::Node(target_node_id), "mousemove")
          .unwrap_or(false)
    });
    if should_mousemove {
      if let Some(target_node_id) = current_target {
        dispatched_dom_event = true;
        let _ = js_tab.dispatch_mouse_event(
          target_node_id,
          "mousemove",
          web_events::EventInit {
            bubbles: true,
            cancelable: false,
            composed: false,
          },
          mouse_base,
        );
      }
    }

    if dispatched_dom_event {
      // Release our mutable borrow of `tab.js_tab` before running the follow-up pump (which borrows
      // it again).
      drop(js_tab);
      self.pump_js_event_loop_after_dom_event_dispatch(
        tab_id,
        tab,
        js_mutation_generation_before_dispatch,
      );
    }
  }

  fn handle_pointer_down(
    &mut self,
    tab_id: TabId,
    pos_css: (f32, f32),
    button: PointerButton,
    modifiers: crate::ui::PointerModifiers,
    click_count: u8,
  ) {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };
    tab.last_pointer_click_count = click_count;
    tab.pointer_buttons |= mouse_buttons_mask_for_button(button);
    let Some(doc) = tab.document.as_mut() else {
      return;
    };
    let scroll = &tab.scroll_state;
    let viewport_point = viewport_point_for_pos_css(scroll, pos_css);
    let engine = &mut tab.interaction;
    let hit_tree = fragment_tree_for_hit_testing(doc, scroll);

    let (changed, target_id, target_element_id) =
      match doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
        let fragment_tree = hit_tree.as_ref().unwrap_or(fragment_tree);

         let (changed, hit) = if matches!(button, PointerButton::Primary | PointerButton::Middle) {
           engine.pointer_down_with_click_count_and_hit(
             dom,
             box_tree,
             fragment_tree,
             scroll,
             viewport_point,
             button,
             modifiers,
             click_count,
           )
         } else {
           let page_point = viewport_point.translate(scroll.viewport);
           (false, hit_test_dom(dom, box_tree, fragment_tree, page_point))
         };

         let (target_id, target_element_id) = match hit {
           Some(hit) => (Some(hit.dom_node_id), hit.dom_element_id),
           None => (None, None),
         };

         (changed, (changed, target_id, target_element_id))
       }) {
        Ok(changed) => changed,
        Err(_) => return,
      };

    // `<input type="range">` updates its value on pointer down (jumping the knob to the click
    // position) and then continuously during drag. Mirror the initial change into dom2 before we
    // dispatch `"mousedown"` so JS can observe the updated value.
    if changed {
      if let (Some(range_node_id), Some(js_tab)) = (
        tab.interaction.active_range_drag_node_id(),
        tab.js_tab.as_mut(),
      ) {
        let dom_snapshot = doc.dom();
        let element_id = dom_node_by_preorder_id(dom_snapshot, range_node_id)
          .and_then(|node| node.get_attribute_ref("id"));
        mirror_dom1_form_control_state_into_dom2(
          js_tab,
          tab.js_dom_mapping.as_ref(),
          dom_snapshot,
          range_node_id,
          element_id,
        );
        tab.js_dom_mutation_generation = js_tab.dom().mutation_generation();
      }
    }

    if let Some(target_id) = target_id {
      let pointer_buttons = tab.pointer_buttons;
      let js_mutation_generation_before_dispatch =
        tab.js_tab.as_ref().map(|js_tab| js_tab.dom().mutation_generation());
      let mut dispatched_dom_event = false;
      if let Some(js_tab) = tab.js_tab.as_mut() {
        let target = js_dom_node_for_preorder_id_with_log(
          &self.ui_tx,
          tab_id,
          self.debug_log_enabled,
          js_tab,
          target_id,
          target_element_id.as_deref(),
          &mut tab.js_dom_mapping_generation,
          &mut tab.js_dom_mapping,
          &mut tab.js_dom_mapping_miss_log_last,
          "mousedown",
        );
        if let Some(node_id) = target {
          dispatched_dom_event = true;
          let mouse = web_events::MouseEvent {
            client_x: mouse_client_coord(pos_css.0),
            client_y: mouse_client_coord(pos_css.1),
            button: mouse_event_button(button),
            buttons: pointer_buttons,
            detail: click_count as i32,
            ctrl_key: modifiers.ctrl(),
            shift_key: modifiers.shift(),
            alt_key: modifiers.alt(),
            meta_key: modifiers.meta(),
            related_target: None,
          };
          if let Err(err) = js_tab.dispatch_mouse_event(
            node_id,
            "mousedown",
            web_events::EventInit {
              bubbles: true,
              cancelable: true,
              composed: false,
            },
            mouse,
          ) {
            if self.debug_log_enabled {
              let _ = self.ui_tx.send(WorkerToUi::DebugLog {
                tab_id,
                line: format!("js mousedown event dispatch failed: {err}"),
              });
            }
          }
        }
      }
      if dispatched_dom_event {
        if let Some(before) = js_mutation_generation_before_dispatch {
          self.pump_js_event_loop_after_dom_event_dispatch(tab_id, tab, before);
        }
      }
    }
    if changed {
      // Preserve existing repaint behaviour for interaction-engine state changes.
      tab.cancel.bump_paint();
      tab.needs_repaint = true;
    }
  }

  fn handle_pointer_up(
    &mut self,
    tab_id: TabId,
    pos_css: (f32, f32),
    button: PointerButton,
    modifiers: crate::ui::PointerModifiers,
  ) {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };
    tab.pointer_buttons &= !mouse_buttons_mask_for_button(button);
    let click_count = tab.last_pointer_click_count;

    if !matches!(button, PointerButton::Primary | PointerButton::Middle) {
      // Right-click/etc: no default interaction engine actions, but still dispatch a DOM `mouseup`
      // event so JS can observe non-primary buttons.
      let Some(doc) = tab.document.as_mut() else {
        return;
      };
      let scroll = &tab.scroll_state;
      let viewport_point = viewport_point_for_pos_css(scroll, pos_css);
      let pointer_buttons = tab.pointer_buttons;
      let js_mutation_generation_before_dispatch =
        tab.js_tab.as_ref().map(|js_tab| js_tab.dom().mutation_generation());
      let mut dispatched_dom_event = false;
      let hit_tree = fragment_tree_for_hit_testing(doc, scroll);

      let (target_id, target_element_id) = if tab.last_pointer_pos_css == Some(pos_css) {
        (
          tab.last_hovered_dom_node_id,
          tab.last_hovered_dom_element_id.clone(),
        )
      } else {
        match doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
          let fragment_tree = hit_tree.as_ref().unwrap_or(fragment_tree);

          let page_point = viewport_point.translate(scroll.viewport);
          let hit = hit_test_dom(dom, box_tree, fragment_tree, page_point);
          let (target_id, target_element_id) = match hit {
            Some(hit) => (Some(hit.dom_node_id), hit.dom_element_id),
            None => (None, None),
          };

          (false, (target_id, target_element_id))
        }) {
          Ok(result) => result,
          Err(_) => (None, None),
        }
      };

      if let Some(target_id) = target_id {
        if let Some(js_tab) = tab.js_tab.as_mut() {
          let target = js_dom_node_for_preorder_id_with_log(
            &self.ui_tx,
            tab_id,
            self.debug_log_enabled,
            js_tab,
            target_id,
            target_element_id.as_deref(),
            &mut tab.js_dom_mapping_generation,
            &mut tab.js_dom_mapping,
            &mut tab.js_dom_mapping_miss_log_last,
            "mouseup",
          );
          if let Some(node_id) = target {
            dispatched_dom_event = true;
            let mouse = web_events::MouseEvent {
              client_x: mouse_client_coord(pos_css.0),
              client_y: mouse_client_coord(pos_css.1),
              button: mouse_event_button(button),
              buttons: pointer_buttons,
              detail: click_count as i32,
              ctrl_key: modifiers.ctrl(),
              shift_key: modifiers.shift(),
              alt_key: modifiers.alt(),
              meta_key: modifiers.meta(),
              related_target: None,
            };
            if let Err(err) = js_tab.dispatch_mouse_event(
              node_id,
              "mouseup",
              web_events::EventInit {
                bubbles: true,
                cancelable: true,
                composed: false,
              },
              mouse,
            ) {
              if self.debug_log_enabled {
                let _ = self.ui_tx.send(WorkerToUi::DebugLog {
                  tab_id,
                  line: format!("js mouseup event dispatch failed: {err}"),
                });
              }
            }
          }
        }
      }
      if dispatched_dom_event {
        if let Some(before) = js_mutation_generation_before_dispatch {
          self.pump_js_event_loop_after_dom_event_dispatch(tab_id, tab, before);
        }
      }
      return;
    }

    let pointer_buttons = tab.pointer_buttons;

    let base_url =
      base_url_for_links(tab.last_base_url.as_deref(), tab.last_committed_url.as_deref());
    let document_url = tab
      .last_committed_url
      .as_deref()
      .unwrap_or(about_pages::ABOUT_BASE_URL);
    let scroll_snapshot = tab.scroll_state.clone();
    let viewport_point = viewport_point_for_pos_css(&scroll_snapshot, pos_css);
    let (
      dom_changed,
      action,
      anchor_css,
      picker_value,
      scroll_changed,
      mouseup_target,
      mouseup_target_element_id,
      click_target,
      click_target_element_id,
      form_submitter,
      form_submitter_element_id,
    ) = {
      let Some(doc) = tab.document.as_mut() else {
        return;
      };
      let engine = &mut tab.interaction;
      let hit_tree = fragment_tree_for_hit_testing(doc, &scroll_snapshot);
      let (
        dom_changed,
        action,
        anchor_css,
        picker_value,
        focus_scroll,
        mouseup_target,
        mouseup_target_element_id,
        click_target,
        click_target_element_id,
        form_submitter,
        form_submitter_element_id,
      ) = match doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
        let hit_tree = hit_tree.as_ref().unwrap_or(fragment_tree);
        let (dom_changed, action, up_hit) = engine.pointer_up_with_scroll_and_hit(
          dom,
          box_tree,
          hit_tree,
          &scroll_snapshot,
          viewport_point,
          button,
          modifiers,
          true,
          document_url,
          base_url,
        );

        let mouseup_target = up_hit.as_ref().map(|hit| hit.dom_node_id);
        let mouseup_target_element_id = mouseup_target.and_then(|target_id| {
          crate::dom::find_node_mut_by_preorder_id(dom, target_id)
            .and_then(|node| node.get_attribute_ref("id"))
            .map(|id| id.to_string())
        });

        let click_target = engine.take_last_click_target();
        let click_target_element_id = click_target.and_then(|target_id| {
          crate::dom::find_node_mut_by_preorder_id(dom, target_id)
            .and_then(|node| node.get_attribute_ref("id"))
            .map(|id| id.to_string())
        });

        let form_submitter = engine.take_last_form_submitter();
        let form_submitter_element_id = form_submitter.and_then(|submitter_id| {
          crate::dom::find_node_mut_by_preorder_id(dom, submitter_id)
            .and_then(|node| node.get_attribute_ref("id"))
            .map(|id| id.to_string())
        });

        let anchor_css = match &action {
          InteractionAction::OpenSelectDropdown { select_node_id, .. } => {
            select_anchor_css(box_tree, hit_tree, &scroll_snapshot, *select_node_id)
          }
          InteractionAction::OpenDateTimePicker { input_node_id, .. }
          | InteractionAction::OpenColorPicker { input_node_id }
          | InteractionAction::OpenFilePicker { input_node_id, .. } => styled_node_anchor_css(
            box_tree,
            hit_tree,
            &scroll_snapshot,
            *input_node_id,
          ),
          InteractionAction::OpenMediaControls { media_node_id, .. } => styled_node_anchor_css(
            box_tree,
            fragment_tree,
            &scroll_snapshot,
            *media_node_id,
          ),
          _ => None,
        };

        let picker_value = match &action {
          InteractionAction::OpenDateTimePicker { input_node_id, kind } => Some(
            crate::dom::find_node_mut_by_preorder_id(dom, *input_node_id)
              .map(|node| match *kind {
                crate::interaction::DateTimeInputKind::Date => {
                  crate::dom::input_date_value_string(node).unwrap_or_default()
                }
                crate::interaction::DateTimeInputKind::Time => {
                  crate::dom::input_time_value_string(node).unwrap_or_default()
                }
                crate::interaction::DateTimeInputKind::DateTimeLocal => {
                  crate::dom::input_datetime_local_value_string(node).unwrap_or_default()
                }
                crate::interaction::DateTimeInputKind::Month => {
                  crate::dom::input_month_value_string(node).unwrap_or_default()
                }
                crate::interaction::DateTimeInputKind::Week => {
                  crate::dom::input_week_value_string(node).unwrap_or_default()
                }
              })
              .unwrap_or_default(),
          ),
          InteractionAction::OpenColorPicker { input_node_id } => Some(
            crate::dom::find_node_mut_by_preorder_id(dom, *input_node_id)
              .and_then(|node| crate::dom::input_color_value_string(node))
              .unwrap_or_default(),
          ),
          _ => None,
        };

        let focus_scroll = match &action {
          InteractionAction::FocusChanged {
            node_id: Some(node_id),
          } => crate::interaction::focus_scroll::scroll_state_for_focus(
            box_tree,
            fragment_tree,
            &scroll_snapshot,
            *node_id,
          )
          .filter(|_| {
            // Pointer-driven focus changes (e.g. clicking a `<label>` that focuses a visually-hidden
            // checkbox) should not unexpectedly scroll the page away from the clicked content.
            //
            // Only apply focus scrolling when the focused element is the actual hit-test target at
            // the pointer location.
            up_hit
              .as_ref()
              .is_some_and(|hit| hit.styled_node_id == *node_id || hit.dom_node_id == *node_id)
          }),
          _ => None,
        };

        (
          dom_changed,
          (
            dom_changed,
            action,
            anchor_css,
            picker_value,
            focus_scroll,
            mouseup_target,
            mouseup_target_element_id,
            click_target,
            click_target_element_id,
            form_submitter,
            form_submitter_element_id,
          ),
        )
      }) {
        Ok(result) => result,
        Err(_) => return,
      };

      let mut scroll_changed = false;
      if let Some(next_scroll) = focus_scroll {
        tab.scroll_state = next_scroll;
        doc.set_scroll_state(tab.scroll_state.clone());
        tab.sync_js_scroll_state();
        scroll_changed = true;
        let _ = self.ui_tx.send(WorkerToUi::ScrollStateUpdated {
          tab_id,
          scroll: tab.scroll_state.clone(),
        });
        tab.last_reported_scroll_state = tab.scroll_state.clone();
      }

      (
        dom_changed,
        action,
        anchor_css,
        picker_value,
        scroll_changed,
        mouseup_target,
        mouseup_target_element_id,
        click_target,
        click_target_element_id,
        form_submitter,
        form_submitter_element_id,
      )
    };

    // Mirror any UI-driven form-control mutations from dom1 into the JS dom2 document before we
    // dispatch `"click"`/`"submit"` events. This ensures JS handlers observe updated state (e.g.
    // `checkbox.checked` after a click) and prevents dom2→dom1 resync from clobbering UI edits.
    if dom_changed {
      if let (Some(dom_snapshot), Some(js_tab)) = (
        tab.document.as_ref().map(|doc| doc.dom()),
        tab.js_tab.as_mut(),
      ) {
        let mapping = tab.js_dom_mapping.as_ref();
        if let Some(target_id) = click_target {
          mirror_dom1_form_control_state_into_dom2(
            js_tab,
            mapping,
            dom_snapshot,
            target_id,
            click_target_element_id.as_deref(),
          );
        }
        if let Some(submitter_id) = form_submitter {
          mirror_dom1_form_control_state_into_dom2(
            js_tab,
            mapping,
            dom_snapshot,
            submitter_id,
            form_submitter_element_id.as_deref(),
          );
        }
        // Keep the worker's cached JS mutation generation in sync with dom2 edits caused by
        // mirroring UI-driven form control state (dom1 → dom2). This prevents the paint pipeline
        // from treating these internal sync writes as "external" JS mutations that require a full
        // dom2 → dom1 resnapshot.
        tab.js_dom_mutation_generation = js_tab.dom().mutation_generation();
      }
    }
    let js_mutation_generation_before_dispatch =
      tab.js_tab.as_ref().map(|js_tab| js_tab.dom().mutation_generation());
    let mut dispatched_dom_event = false;

    if let Some(target_id) = mouseup_target {
      if let Some(js_tab) = tab.js_tab.as_mut() {
        let target = js_dom_node_for_preorder_id_with_log(
          &self.ui_tx,
          tab_id,
          self.debug_log_enabled,
          js_tab,
          target_id,
          mouseup_target_element_id.as_deref(),
          &mut tab.js_dom_mapping_generation,
          &mut tab.js_dom_mapping,
          &mut tab.js_dom_mapping_miss_log_last,
          "mouseup",
        );
        if let Some(node_id) = target {
          dispatched_dom_event = true;
          let mouse = web_events::MouseEvent {
            client_x: mouse_client_coord(pos_css.0),
            client_y: mouse_client_coord(pos_css.1),
            button: mouse_event_button(button),
            buttons: pointer_buttons,
            detail: click_count as i32,
            ctrl_key: modifiers.ctrl(),
            shift_key: modifiers.shift(),
            alt_key: modifiers.alt(),
            meta_key: modifiers.meta(),
            related_target: None,
          };
          if let Err(err) = js_tab.dispatch_mouse_event(
            node_id,
            "mouseup",
            web_events::EventInit {
              bubbles: true,
              cancelable: true,
              composed: false,
            },
            mouse,
          ) {
            if self.debug_log_enabled {
              let _ = self.ui_tx.send(WorkerToUi::DebugLog {
                tab_id,
                line: format!("js mouseup event dispatch failed: {err}"),
              });
            }
          }
        }
      }
    }

    let mut default_allowed = true;
    if let Some(target_id) = click_target {
      if let Some(js_tab) = tab.js_tab.as_mut() {
        let click_type: &'static str = match button {
          PointerButton::Middle => "auxclick",
          _ => "click",
        };

        let target = js_dom_node_for_preorder_id_with_log(
          &self.ui_tx,
          tab_id,
          self.debug_log_enabled,
          js_tab,
          target_id,
          click_target_element_id.as_deref(),
          &mut tab.js_dom_mapping_generation,
          &mut tab.js_dom_mapping,
          &mut tab.js_dom_mapping_miss_log_last,
          click_type,
        );

        if let Some(node_id) = target {
          dispatched_dom_event = true;
          let mouse = web_events::MouseEvent {
            client_x: mouse_client_coord(pos_css.0),
            client_y: mouse_client_coord(pos_css.1),
            button: mouse_event_button(button),
            buttons: pointer_buttons,
            detail: click_count as i32,
            ctrl_key: modifiers.ctrl(),
            shift_key: modifiers.shift(),
            alt_key: modifiers.alt(),
            meta_key: modifiers.meta(),
            related_target: None,
          };
          match js_tab.dispatch_mouse_event(
            node_id,
            click_type,
            web_events::EventInit {
              bubbles: true,
              cancelable: true,
              composed: false,
            },
            mouse,
          ) {
            Ok(allowed) => default_allowed = allowed,
            Err(err) => {
              if self.debug_log_enabled {
                let _ = self.ui_tx.send(WorkerToUi::DebugLog {
                  tab_id,
                  line: format!("js {click_type} event dispatch failed: {err}"),
                });
              }
            }
          }
        }
      }
    }

    // Double click: after dispatching the second click, dispatch `dblclick` at the same target.
    //
    // Note: this is a best-effort approximation driven by the UI-provided click_count.
    if click_count == 2 && matches!(button, PointerButton::Primary) {
      if let Some(target_id) = click_target {
        if let Some(js_tab) = tab.js_tab.as_mut() {
          let target = js_dom_node_for_preorder_id_with_log(
            &self.ui_tx,
            tab_id,
            self.debug_log_enabled,
            js_tab,
            target_id,
            click_target_element_id.as_deref(),
            &mut tab.js_dom_mapping_generation,
            &mut tab.js_dom_mapping,
            &mut tab.js_dom_mapping_miss_log_last,
            "dblclick",
          );
          if let Some(node_id) = target {
            dispatched_dom_event = true;
            let mouse = web_events::MouseEvent {
              client_x: mouse_client_coord(pos_css.0),
              client_y: mouse_client_coord(pos_css.1),
              button: mouse_event_button(button),
              buttons: pointer_buttons,
              detail: 2,
              ctrl_key: modifiers.ctrl(),
              shift_key: modifiers.shift(),
              alt_key: modifiers.alt(),
              meta_key: modifiers.meta(),
              related_target: None,
            };
            if let Err(err) = js_tab.dispatch_mouse_event(
              node_id,
              "dblclick",
              web_events::EventInit {
                bubbles: true,
                cancelable: true,
                composed: false,
              },
              mouse,
            ) {
              if self.debug_log_enabled {
                let _ = self.ui_tx.send(WorkerToUi::DebugLog {
                  tab_id,
                  line: format!("js dblclick event dispatch failed: {err}"),
                });
              }
            }
          }
        }
      }
    }

    // If a click triggers a form submission attempt, dispatch a cancelable `"submit"` event on the
    // form owner and honor `preventDefault()` before committing the navigation.
    if default_allowed {
      if let Some(submitter_id) = form_submitter {
        if let Some(js_tab) = tab.js_tab.as_mut() {
          let submitter_node = js_dom_node_for_preorder_id_with_log(
            &self.ui_tx,
            tab_id,
            self.debug_log_enabled,
            js_tab,
            submitter_id,
            form_submitter_element_id.as_deref(),
            &mut tab.js_dom_mapping_generation,
            &mut tab.js_dom_mapping,
            &mut tab.js_dom_mapping_miss_log_last,
            "submit",
          );
          if let Some(submitter_node) = submitter_node {
            if let Some(form_node) = js_find_form_owner_for_submitter(js_tab.dom(), submitter_node)
            {
              dispatched_dom_event = true;
              match js_tab.dispatch_submit_event(form_node) {
                Ok(allowed) => default_allowed = allowed,
                Err(err) => {
                  if self.debug_log_enabled {
                    let _ = self.ui_tx.send(WorkerToUi::DebugLog {
                      tab_id,
                      line: format!("js submit event dispatch failed: {err}"),
                    });
                  }
                }
              }
            }
          }
        }
      }
    }

    if dispatched_dom_event {
      if let Some(before) = js_mutation_generation_before_dispatch {
        self.pump_js_event_loop_after_dom_event_dispatch(tab_id, tab, before);
      }
    }

    let mut navigate_to: Option<String> = None;
    let mut navigate_request: Option<FormSubmission> = None;
    let mut open_in_new_tab: Option<String> = None;
    let mut open_in_new_tab_request: Option<FormSubmission> = None;
    let mut download_to_start: Option<(String, Option<String>)> = None;

    match action {
      InteractionAction::Navigate { href } => {
        if default_allowed {
          navigate_to = Some(href);
        } else if dom_changed || scroll_changed {
          tab.needs_repaint = true;
        }
      }
      InteractionAction::OpenInNewTab { href } => {
        if default_allowed {
          open_in_new_tab = Some(href);
        }
        if dom_changed || scroll_changed {
          tab.needs_repaint = true;
        }
      }
      InteractionAction::OpenInNewTabRequest { request } => {
        if default_allowed {
          open_in_new_tab_request = Some(request);
        }
        if dom_changed || scroll_changed {
          tab.needs_repaint = true;
        }
      }
      InteractionAction::Download { href, file_name } => {
        if default_allowed {
          download_to_start = Some((href, file_name));
        }
        // Downloads do not navigate away from the current page; repaint so visited-link styles and
        // other DOM mutations become visible.
        if dom_changed || scroll_changed {
          tab.needs_repaint = true;
        }
      }
      InteractionAction::NavigateRequest { request } => {
        if default_allowed {
          navigate_request = Some(request);
        } else if dom_changed || scroll_changed {
          tab.needs_repaint = true;
        }
      }
      InteractionAction::TextDrop { target_dom_id, text } => {
        let mut drop_default_allowed = default_allowed;
        let js_mutation_generation_before_dispatch =
          tab.js_tab.as_ref().map(|js_tab| js_tab.dom().mutation_generation());
        let mut dispatched_dom_event = false;
        if drop_default_allowed {
          if let Some(js_tab) = tab.js_tab.as_mut() {
            let target = js_dom_node_for_preorder_id_with_log(
              &self.ui_tx,
              tab_id,
              self.debug_log_enabled,
              js_tab,
              target_dom_id,
              mouseup_target_element_id.as_deref(),
              &mut tab.js_dom_mapping_generation,
              &mut tab.js_dom_mapping,
              &mut tab.js_dom_mapping_miss_log_last,
              "drop",
            );
            if let Some(node_id) = target {
              dispatched_dom_event = true;
              // `DragEvent` inherits from `MouseEvent` in the DOM. We don't currently model
              // `dataTransfer`, but exposing a MouseEvent-like shape keeps common `preventDefault()`
              // checks working.
              let mouse = web_events::MouseEvent {
                client_x: mouse_client_coord(pos_css.0),
                client_y: mouse_client_coord(pos_css.1),
                button: mouse_event_button(button),
                buttons: pointer_buttons,
                detail: click_count as i32,
                ctrl_key: modifiers.ctrl(),
                shift_key: modifiers.shift(),
                alt_key: modifiers.alt(),
                meta_key: modifiers.meta(),
                related_target: None,
              };
              match js_tab.dispatch_mouse_event(
                node_id,
                "drop",
                web_events::EventInit {
                  bubbles: true,
                  cancelable: true,
                  composed: false,
                },
                mouse,
              ) {
                Ok(allowed) => drop_default_allowed = allowed,
                Err(err) => {
                  if self.debug_log_enabled {
                    let _ = self.ui_tx.send(WorkerToUi::DebugLog {
                      tab_id,
                      line: format!("js drop event dispatch failed: {err}"),
                    });
                  }
                }
              }
            }
          }
        }

        // When the drop is not prevented, apply the default insertion to dom1 and then mirror the
        // resulting form-control state into dom2 before running the post-event JS pump. This keeps
        // dom2 in sync so resyncs from dom2 won't clobber the UI-side insertion, and ensures
        // microtasks queued by drop handlers observe the updated value (browser-like ordering: drop
        // handlers run, default action happens, then microtask checkpoint).
        let mut apply_changed = false;
        if drop_default_allowed {
          apply_changed = if let Some(doc) = tab.document.as_mut() {
            let engine = &mut tab.interaction;
            doc.mutate_dom(|dom| engine.apply_text_drop(dom, target_dom_id, &text))
          } else {
            false
          };

          if apply_changed {
            if let (Some(dom_snapshot), Some(js_tab)) = (
              tab.document.as_ref().map(|doc| doc.dom()),
              tab.js_tab.as_mut(),
            ) {
              let mapping = tab.js_dom_mapping.as_ref();
              mirror_dom1_form_control_state_into_dom2(
                js_tab,
                mapping,
                dom_snapshot,
                target_dom_id,
                mouseup_target_element_id.as_deref(),
              );
            }
          }

          if dispatched_dom_event {
            if let Some(before) = js_mutation_generation_before_dispatch {
              self.pump_js_event_loop_after_dom_event_dispatch(tab_id, tab, before);
            }
          }

          if dom_changed || scroll_changed || apply_changed {
            tab.needs_repaint = true;
          }
          if apply_changed {
            tab.cancel.bump_paint();
          }
        } else if dom_changed || scroll_changed {
          if dispatched_dom_event {
            if let Some(before) = js_mutation_generation_before_dispatch {
              self.pump_js_event_loop_after_dom_event_dispatch(tab_id, tab, before);
            }
          }
          tab.needs_repaint = true;
        }
      }
      InteractionAction::OpenSelectDropdown {
        select_node_id,
        control,
      } => {
        // Back-compat: older UIs listen for `OpenSelectDropdown`.
        let _ = self.ui_tx.send(WorkerToUi::OpenSelectDropdown {
          tab_id,
          select_node_id,
          control: control.clone(),
        });

        // Prefer anchoring the dropdown to the `<select>` control's box, falling back to the cursor
        // position when we cannot resolve the layout geometry (e.g. missing prepared tree).
        let cursor_anchor_css = Rect::from_xywh(viewport_point.x, viewport_point.y, 1.0, 1.0);
        let anchor_css = anchor_css
          .filter(|rect| rect.width() > 0.0 && rect.height() > 0.0)
          .unwrap_or(cursor_anchor_css);
        let _ = self.ui_tx.send(WorkerToUi::SelectDropdownOpened {
          tab_id,
          select_node_id,
          control,
          anchor_css,
        });
        if dom_changed || scroll_changed {
          tab.needs_repaint = true;
        }
      }
      InteractionAction::OpenDateTimePicker { input_node_id, kind } => {
        // Prefer anchoring the popup to the `<input>` control's box, falling back to the cursor
        // position when we cannot resolve the layout geometry (e.g. missing prepared tree).
        let cursor_anchor_css = Rect::from_xywh(viewport_point.x, viewport_point.y, 1.0, 1.0);
        let anchor_css = anchor_css
          .filter(|rect| rect.width() > 0.0 && rect.height() > 0.0)
          .unwrap_or(cursor_anchor_css);

        let value = picker_value.clone().unwrap_or_default();

        let _ = self.ui_tx.send(WorkerToUi::DateTimePickerOpened {
          tab_id,
          input_node_id,
          kind,
          value,
          anchor_css,
        });
        if dom_changed || scroll_changed {
          tab.needs_repaint = true;
        }
      }
      InteractionAction::OpenColorPicker { input_node_id } => {
        // Prefer anchoring the popup to the `<input>` control's box, falling back to the cursor
        // position when we cannot resolve the layout geometry (e.g. missing prepared tree).
        let cursor_anchor_css = Rect::from_xywh(viewport_point.x, viewport_point.y, 1.0, 1.0);
        let anchor_css = anchor_css
          .filter(|rect| rect.width() > 0.0 && rect.height() > 0.0)
          .unwrap_or(cursor_anchor_css);

        let value = picker_value.clone().unwrap_or_default();

        let _ = self.ui_tx.send(WorkerToUi::ColorPickerOpened {
          tab_id,
          input_node_id,
          value,
          anchor_css,
        });
        if dom_changed || scroll_changed {
          tab.needs_repaint = true;
        }
      }
      InteractionAction::OpenFilePicker {
        input_node_id,
        multiple,
        accept,
      } => {
        // Prefer anchoring the popup to the `<input>` control's box, falling back to the cursor
        // position when we cannot resolve the layout geometry (e.g. missing prepared tree).
        let cursor_anchor_css = Rect::from_xywh(viewport_point.x, viewport_point.y, 1.0, 1.0);
        let anchor_css = anchor_css
          .filter(|rect| rect.width() > 0.0 && rect.height() > 0.0)
          .unwrap_or(cursor_anchor_css);

        let _ = self.ui_tx.send(WorkerToUi::FilePickerOpened {
          tab_id,
          input_node_id,
          multiple,
          accept,
          anchor_css,
        });
        if dom_changed || scroll_changed {
          tab.needs_repaint = true;
        }
      }
      InteractionAction::OpenMediaControls { media_node_id, kind } => {
        // Prefer anchoring the overlay to the `<video>`/`<audio>` box, falling back to the cursor
        // position when we cannot resolve the layout geometry (e.g. missing prepared tree).
        let cursor_anchor_css = Rect::from_xywh(viewport_point.x, viewport_point.y, 1.0, 1.0);
        let anchor_css = anchor_css
          .filter(|rect| rect.width() > 0.0 && rect.height() > 0.0)
          .unwrap_or(cursor_anchor_css);

        let _ = self.ui_tx.send(WorkerToUi::MediaControlsOpened {
          tab_id,
          node_id: media_node_id,
          kind,
          anchor_css,
        });

        if dom_changed || scroll_changed {
          tab.needs_repaint = true;
        }
      }
      _ => {
        if dom_changed || scroll_changed {
          tab.needs_repaint = true;
        }
      }
    }

    // `start_download` mutates global worker state; ensure we end our borrow of `tab` first.
    //
    // `drop(tab)` would work but triggers the `dropping_references` lint; moving into `_` is the
    // conventional way to end the borrow early.
    let _ = tab;
    if let Some((href, file_name)) = download_to_start {
      self.start_download(tab_id, href, file_name);
    }
    if let Some(url) = open_in_new_tab {
      let _ = self.ui_tx.send(WorkerToUi::RequestOpenInNewTab { tab_id, url });
    }
    if let Some(request) = open_in_new_tab_request {
      let _ = self
        .ui_tx
        .send(WorkerToUi::RequestOpenInNewTabRequest { tab_id, request });
    }
    if let Some(url) = navigate_to {
      self.schedule_navigation(tab_id, url, NavigationReason::LinkClick);
    } else if let Some(request) = navigate_request {
      self.schedule_navigation_request(tab_id, request, NavigationReason::LinkClick);
    }
  }

  fn handle_drop_files(&mut self, tab_id: TabId, pos_css: (f32, f32), paths: Vec<PathBuf>) {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };
    let Some(doc) = tab.document.as_mut() else {
      return;
    };

    let scroll = &tab.scroll_state;
    let viewport_point = viewport_point_for_pos_css(scroll, pos_css);
    let hit_tree = fragment_tree_for_hit_testing(doc, scroll);

    // ---------------------------------------------------------------------------
    // JS `drop` event dispatch
    // ---------------------------------------------------------------------------
    //
    // When JavaScript is enabled for this tab, dispatch a trusted, cancelable `drop` event before
    // applying the default file-input drop behavior. If page JS cancels the event via
    // `preventDefault()`, suppress the default file-input selection.
    let (drop_target_id, drop_target_element_id) =
      match doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
        let fragment_tree = hit_tree.as_ref().unwrap_or(fragment_tree);

        let page_point = viewport_point.translate(scroll.viewport);
        let hit = hit_test_dom(dom, box_tree, fragment_tree, page_point);
        let target_id = hit.as_ref().map(|hit| hit.dom_node_id);
        let target_element_id = target_id.and_then(|target_id| {
          crate::dom::find_node_mut_by_preorder_id(dom, target_id)
            .and_then(|node| node.get_attribute_ref("id"))
            .map(|id| id.to_string())
        });

        (false, (target_id, target_element_id))
      }) {
        Ok(result) => result,
        Err(_) => (None, None),
      };

    if let Some(js_tab) = tab.js_tab.as_mut() {
      if let Some(target_id) = drop_target_id {
        let target = js_dom_node_for_preorder_id_with_log(
          &self.ui_tx,
          tab_id,
          self.debug_log_enabled,
          js_tab,
          target_id,
          drop_target_element_id.as_deref(),
          &mut tab.js_dom_mapping_generation,
          &mut tab.js_dom_mapping,
          &mut tab.js_dom_mapping_miss_log_last,
          "drop",
        );
        if let Some(node_id) = target {
          match js_tab.dispatch_drop_event_with_files(node_id, pos_css, &paths) {
            Ok(default_allowed) => {
              if !default_allowed {
                return;
              }
            }
            Err(err) => {
              // Best-effort: keep default behavior working even when JS event dispatch fails.
              let _ = self.ui_tx.send(WorkerToUi::DebugLog {
                tab_id,
                line: format!("js drop event dispatch failed: {err}"),
              });
            }
          }
        }
      }
    }

    let engine = &mut tab.interaction;
    let changed = match doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
      let fragment_tree = hit_tree.as_ref().unwrap_or(fragment_tree);

      let changed =
        engine.drop_files_with_scroll(dom, box_tree, fragment_tree, scroll, viewport_point, &paths);
      (changed, changed)
    }) {
      Ok(changed) => changed,
      Err(_) => false,
    };

    if changed {
      if let (Some(focused), Some(js_tab)) = (tab.interaction.focused_node_id(), tab.js_tab.as_mut())
      {
        let dom_snapshot = doc.dom();
        let element_id = dom_node_by_preorder_id(dom_snapshot, focused)
          .and_then(|node| node.get_attribute_ref("id"));
        mirror_dom1_form_control_state_into_dom2(
          js_tab,
          tab.js_dom_mapping.as_ref(),
          dom_snapshot,
          focused,
          element_id,
        );
        tab.js_dom_mutation_generation = js_tab.dom().mutation_generation();
      }
      tab.cancel.bump_paint();
      tab.needs_repaint = true;
    }
  }

  fn handle_context_menu_request(
    &mut self,
    tab_id: TabId,
    pos_css: (f32, f32),
    modifiers: crate::ui::PointerModifiers,
  ) {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };

    let base_url =
      base_url_for_links(tab.last_base_url.as_deref(), tab.last_committed_url.as_deref());
    let dpr = tab.dpr;
    let viewport = Size::new(tab.viewport_css.0 as f32, tab.viewport_css.1 as f32);
    let scroll = &tab.scroll_state;
    let viewport_point = viewport_point_for_pos_css(scroll, pos_css);
    let page_point = viewport_point.translate(scroll.viewport);

    let Some(doc) = tab.document.as_mut() else {
      let _ = self.ui_tx.send(WorkerToUi::ContextMenu {
        tab_id,
        pos_css,
        default_prevented: false,
        link_url: None,
        image_url: None,
        can_copy: false,
        can_cut: false,
        can_paste: false,
        can_select_all: false,
      });
      return;
    };

    struct HitContextMenuInfo {
      href: Option<String>,
      dispatch_target_id: Option<usize>,
      dispatch_target_element_id: Option<String>,
      image_url: Option<String>,
      text_control_target: Option<usize>,
      text_control_disabled: bool,
      text_control_readonly: bool,
    }

    let engine = &mut tab.interaction;
    let hit_tree = fragment_tree_for_hit_testing(doc, scroll);
    let (changed, hit_info) =
      match doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
        let hit_tree = hit_tree.as_ref().unwrap_or(fragment_tree);
        let hit = hit_test_dom(dom, box_tree, hit_tree, page_point);
        // `hit_test_dom` resolves `dom_node_id` to a *semantic* target (e.g. link ancestor). For JS
        // `contextmenu` dispatch, we want the deepest element under the cursor so listeners on nested
        // elements (like an `<img>` inside a link) fire correctly.
        let dispatch_target_id = hit.as_ref().map(|hit| {
          let dom_index = crate::interaction::dom_index::DomIndex::build(dom);
          let mut current = hit.styled_node_id;
          // 1) Prefer the styled node if it is an element.
          let mut found = dom_index
            .node(current)
            .is_some_and(|node| node.is_element())
            .then_some(current);
          // 2) Otherwise, climb ancestors until we find an element.
          if found.is_none() {
            while current != 0 {
              current = dom_index.parent.get(current).copied().unwrap_or(0);
              if current == 0 {
                break;
              }
              if dom_index
                .node(current)
                .is_some_and(|node| node.is_element())
              {
                found = Some(current);
                break;
              }
            }
          }
          // 3) Fallback to the semantic hit target (e.g. a link or form control).
          found.unwrap_or(hit.dom_node_id)
        });
        let dispatch_target_element_id = dispatch_target_id.and_then(|target_id| {
          crate::dom::find_node_mut_by_preorder_id(dom, target_id)
            .and_then(|node| node.get_attribute_ref("id"))
            .map(|id| id.to_string())
        });
        let href = hit
          .as_ref()
          .and_then(|hit| (hit.kind == HitTestKind::Link).then(|| hit.href.as_deref()))
          .flatten()
          .map(|href| href.to_string());

        let image_url = hit.as_ref().and_then(|hit| {
          let styled_id = hit.styled_node_id;
          if let Some(img) = find_replaced_image_for_styled_node(&box_tree.root, styled_id) {
            let selected = img.selected_image_source_for_context(ImageSelectionContext {
              device_pixel_ratio: dpr,
              slot_width: None,
              viewport: Some(viewport),
              media_context: None,
              font_size: None,
              root_font_size: None,
              base_url: Some(base_url),
            });
            resolve_link_url(base_url, selected.url)
          } else {
            let node = crate::dom::find_node_mut_by_preorder_id(dom, styled_id)?;
            // Match browser-style image context menu behaviour for `<img>` and `input type=image`.
            if node
              .tag_name()
              .is_some_and(|tag| tag.eq_ignore_ascii_case("img"))
            {
              node
                .get_attribute_ref("src")
                .and_then(|src| resolve_link_url(base_url, src))
            } else if node
              .tag_name()
              .is_some_and(|tag| tag.eq_ignore_ascii_case("input"))
              && dom_input_type(node).eq_ignore_ascii_case("image")
            {
              node
                .get_attribute_ref("src")
                .and_then(|src| resolve_link_url(base_url, src))
            } else {
              None
            }
          }
        });

      // Windowed UIs send `ContextMenuRequest` on right-click without a preceding `PointerDown`.
      // When a text control is clicked, mirror native browser behavior by focusing it and placing
      // the caret at the click position so subsequent Paste inserts at the expected offset.
      let mut changed = false;
      let mut text_control_target: Option<usize> = None;
      let mut text_control_disabled = false;
      let mut text_control_readonly = false;
      if let Some(hit) = hit.as_ref() {
        let node_id = hit.dom_node_id;
        let box_id = hit.box_id;
        if let Some(node) = crate::dom::find_node_mut_by_preorder_id(dom, node_id) {
          let is_text_control = dom_is_text_input(node) || dom_is_textarea(node);
          if is_text_control {
            text_control_target = Some(node_id);
            text_control_readonly = node.get_attribute_ref("readonly").is_some();

            let dom_index = crate::interaction::dom_index::DomIndex::build(dom);
            let disabled = crate::interaction::effective_disabled::is_effectively_disabled(node_id, &dom_index);
            let inert_or_hidden =
              crate::interaction::effective_disabled::is_effectively_inert_or_hidden(node_id, &dom_index);
            text_control_disabled = disabled || inert_or_hidden;

            if !text_control_disabled {
              let (focused_changed, _) = engine.focus_node_id(dom, Some(node_id), false);
              changed |= focused_changed;
              changed |= engine.set_text_caret_from_page_point(
                dom,
                box_tree,
                hit_tree,
                scroll,
                node_id,
                box_id,
                page_point,
              );
            }
          }
        }
      }

      (
        false,
        (
          changed,
            HitContextMenuInfo {
              href,
              dispatch_target_id,
              dispatch_target_element_id,
              image_url,
              text_control_target,
              text_control_disabled,
              text_control_readonly,
            },
        ),
      )
    }) {
      Ok(result) => result,
      Err(_) => (
        false,
        HitContextMenuInfo {
          href: None,
          dispatch_target_id: None,
          dispatch_target_element_id: None,
          image_url: None,
          text_control_target: None,
          text_control_disabled: false,
          text_control_readonly: false,
        },
      ),
    };

    let link_url = hit_info
      .href
      .as_deref()
      .and_then(|href| resolve_link_url(base_url, href));
    let image_url = hit_info.image_url.clone();

    if changed {
      tab.cancel.bump_paint();
      tab.needs_repaint = true;
    }

    // Dispatch a cancelable `contextmenu` event before opening the default UI context menu.
    //
    // If JS calls `preventDefault()`, report `default_prevented=true` so UIs can suppress the
    // default menu (matching browser behavior) while still clearing any pending context-menu state.
    let mut default_prevented = false;
    let js_mutation_generation_before_dispatch =
      tab.js_tab.as_ref().map(|js_tab| js_tab.dom().mutation_generation());
    let mut dispatched_dom_event = false;
    if let Some(target_id) = hit_info.dispatch_target_id {
      if let Some(js_tab) = tab.js_tab.as_mut() {
        let target = js_dom_node_for_preorder_id_with_log(
          &self.ui_tx,
          tab_id,
          self.debug_log_enabled,
          js_tab,
          target_id,
          hit_info.dispatch_target_element_id.as_deref(),
          &mut tab.js_dom_mapping_generation,
          &mut tab.js_dom_mapping,
          &mut tab.js_dom_mapping_miss_log_last,
          "contextmenu",
        );
        if let Some(node_id) = target {
          dispatched_dom_event = true;
          let mouse = web_events::MouseEvent {
            client_x: mouse_client_coord(pos_css.0),
            client_y: mouse_client_coord(pos_css.1),
            button: mouse_event_button(PointerButton::Secondary),
            buttons: tab.pointer_buttons | mouse_buttons_mask_for_button(PointerButton::Secondary),
            detail: 0,
            ctrl_key: modifiers.ctrl(),
            shift_key: modifiers.shift(),
            alt_key: modifiers.alt(),
            meta_key: modifiers.meta(),
            related_target: None,
          };
          match js_tab.dispatch_mouse_event(
            node_id,
            "contextmenu",
            web_events::EventInit {
              bubbles: true,
              cancelable: true,
              composed: false,
            },
            mouse,
          ) {
            Ok(allowed) => {
              if !allowed {
                default_prevented = true;
              }
            }
            Err(err) => {
              if self.debug_log_enabled {
                let _ = self.ui_tx.send(WorkerToUi::DebugLog {
                  tab_id,
                  line: format!("js contextmenu event dispatch failed: {err}"),
                });
              }
            }
          }
        }
      }
    }

    if dispatched_dom_event {
      if let Some(before) = js_mutation_generation_before_dispatch {
        self.pump_js_event_loop_after_dom_event_dispatch(tab_id, tab, before);
      }
    }

    let state = tab.interaction.interaction_state();
    let has_document_selection = state
      .document_selection
      .as_ref()
      .is_some_and(|sel| sel.has_highlight());
    let has_text_selection = hit_info.text_control_target.is_some_and(|node_id| {
      state
        .text_edit
        .as_ref()
        .is_some_and(|edit| edit.node_id == node_id && edit.selection.is_some())
    });

    let can_copy = has_text_selection || has_document_selection;
    let can_cut = has_text_selection
      && hit_info.text_control_target.is_some()
      && !hit_info.text_control_disabled
      && !hit_info.text_control_readonly;
    let can_paste = hit_info.text_control_target.is_some()
      && !hit_info.text_control_disabled
      && !hit_info.text_control_readonly;
    // Native browsers typically offer Select All from the page context menu even when no text is
    // currently selected (it selects the whole document in that case). Our interaction engine
    // already supports this via `InteractionEngine::clipboard_select_all`, so advertise it
    // whenever a document is loaded (unless the context menu target is a disabled/inert text
    // control).
    let can_select_all = if hit_info.text_control_target.is_some() {
      !hit_info.text_control_disabled
    } else {
      true
    };
    let _ = self.ui_tx.send(WorkerToUi::ContextMenu {
      tab_id,
      pos_css,
      default_prevented,
      link_url,
      image_url,
      can_copy,
      can_cut,
      can_paste,
      can_select_all,
    });
  }

  #[cfg(feature = "browser_ui")]
  fn handle_accesskit_action(
    &mut self,
    tab_id: TabId,
    action: accesskit::Action,
    target_node_id: Option<usize>,
  ) {
    match action {
      accesskit::Action::ShowContextMenu => {
        let pos_css = {
          let Some(tab) = self.tabs.get(&tab_id) else {
            return;
          };

          let target = target_node_id.or(tab.interaction.interaction_state().focused);
          let anchor_rect = target.and_then(|target| {
            tab.document.as_ref().and_then(|doc| {
              doc.prepared().and_then(|prepared| {
                styled_node_anchor_css(
                  prepared.box_tree(),
                  prepared.fragment_tree(),
                  &tab.scroll_state,
                  target,
                )
              })
            })
          });

          let mut pos = if let Some(rect) = anchor_rect {
            let center = rect.center();
            (center.x, center.y)
          } else {
            // Fallback: viewport center in viewport-local CSS pixels.
            (
              tab.viewport_css.0 as f32 / 2.0,
              tab.viewport_css.1 as f32 / 2.0,
            )
          };

          let max_x = tab.viewport_css.0 as f32;
          let max_y = tab.viewport_css.1 as f32;
          let sanitize = |v: f32, max: f32| {
            if v.is_finite() {
              v.clamp(0.0, max)
            } else {
              0.0
            }
          };
          pos.0 = sanitize(pos.0, max_x);
          pos.1 = sanitize(pos.1, max_y);
          pos
        };

        self.handle_context_menu_request(tab_id, pos_css);
      }
      _ => {}
    }
  }

  fn handle_select_dropdown_choose(
    &mut self,
    tab_id: TabId,
    select_node_id: usize,
    option_node_id: usize,
  ) {
    // Close the dropdown popup deterministically for any UI: `SelectDropdownChoose` always
    // corresponds to a user selecting an option in the dropdown overlay, so the popup should be
    // dismissed even if the selection is a no-op (choosing the currently-selected option).
    //
    // Note: the browser egui UI also closes the popup locally, but emitting this message keeps the
    // worker protocol symmetric with `SelectDropdownCancel` and `SelectDropdownPick` and supports
    // other front-ends that rely on worker-driven close notifications.
    let _ = self.ui_tx.send(WorkerToUi::SelectDropdownClosed { tab_id });

    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };
    let Some(doc) = tab.document.as_mut() else {
      return;
    };

    let engine = &mut tab.interaction;
    let dom_changed = doc
      .mutate_dom(|dom| engine.activate_select_option(dom, select_node_id, option_node_id, false));
    if dom_changed {
      if let Some(js_tab) = tab.js_tab.as_mut() {
        let dom_snapshot = doc.dom();
        let element_id = dom_node_by_preorder_id(dom_snapshot, option_node_id)
          .and_then(|node| node.get_attribute_ref("id"));
        mirror_dom1_form_control_state_into_dom2(
          js_tab,
          tab.js_dom_mapping.as_ref(),
          dom_snapshot,
          option_node_id,
          element_id,
        );
        tab.js_dom_mutation_generation = js_tab.dom().mutation_generation();
      }
      tab.cancel.bump_paint();
      tab.needs_repaint = true;
    }
  }

  fn handle_datalist_choose(
    &mut self,
    tab_id: TabId,
    input_node_id: usize,
    option_node_id: usize,
  ) {
    // Close the datalist popup deterministically for any UI: `DatalistChoose` always corresponds to
    // a user selecting an option in the suggestion overlay, so the popup should be dismissed even
    // if the selection is rejected (disabled option) or a no-op.
    let _ = self.ui_tx.send(WorkerToUi::DatalistClosed { tab_id });

    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };
    let Some(doc) = tab.document.as_mut() else {
      return;
    };

    let engine = &mut tab.interaction;
    let dom_changed =
      doc.mutate_dom(|dom| engine.activate_datalist_option(dom, input_node_id, option_node_id));
    if dom_changed {
      if let Some(js_tab) = tab.js_tab.as_mut() {
        let dom_snapshot = doc.dom();
        let element_id = dom_node_by_preorder_id(dom_snapshot, input_node_id)
          .and_then(|node| node.get_attribute_ref("id"));
        mirror_dom1_form_control_state_into_dom2(
          js_tab,
          tab.js_dom_mapping.as_ref(),
          dom_snapshot,
          input_node_id,
          element_id,
        );
        tab.js_dom_mutation_generation = js_tab.dom().mutation_generation();
      }
      tab.cancel.bump_paint();
      tab.needs_repaint = true;
    }
  }

  fn handle_select_dropdown_pick(
    &mut self,
    tab_id: TabId,
    select_node_id: usize,
    item_index: usize,
  ) {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };
    let Some(doc) = tab.document.as_mut() else {
      return;
    };

    let mut should_close = false;
    let mut selected_option: Option<usize> = None;
    let engine = &mut tab.interaction;
    let dom_changed = doc.mutate_dom(|dom| {
      let index = crate::interaction::dom_index::DomIndex::build(dom);
      let rows = collect_select_rows(&index, select_node_id);
      let row = rows.get(item_index).copied();
      match row {
        Some(SelectRow::Option { node_id, disabled }) if !disabled => {
          should_close = true;
          selected_option = Some(node_id);
          engine.activate_select_option(dom, select_node_id, node_id, false)
        }
        _ => false,
      }
    });

    if should_close {
      let _ = self.ui_tx.send(WorkerToUi::SelectDropdownClosed { tab_id });
    }

    if dom_changed {
      if let (Some(option_node_id), Some(js_tab)) = (selected_option, tab.js_tab.as_mut()) {
        let dom_snapshot = doc.dom();
        let element_id = dom_node_by_preorder_id(dom_snapshot, option_node_id)
          .and_then(|node| node.get_attribute_ref("id"));
        mirror_dom1_form_control_state_into_dom2(
          js_tab,
          tab.js_dom_mapping.as_ref(),
          dom_snapshot,
          option_node_id,
          element_id,
        );
        tab.js_dom_mutation_generation = js_tab.dom().mutation_generation();
      }
      tab.cancel.bump_paint();
      tab.needs_repaint = true;
    }
  }

  fn handle_text_input(&mut self, tab_id: TabId, text: &str) {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };
    let Some(doc) = tab.document.as_mut() else {
      return;
    };

    let mut datalist_open: Option<(usize, Vec<DatalistOption>)> = None;
    let box_tree_ptr = doc
      .prepared()
      .map(|prepared| prepared.box_tree() as *const crate::BoxTree);
    let changed = doc.mutate_dom(|dom| {
      // Prefer using cached layout artifacts when available so `<select>` typeahead can use the
      // painted option list (skipping options hidden via computed `display:none`, etc).
      let changed = match box_tree_ptr {
        Some(box_tree_ptr) => tab
          .interaction
          .text_input_with_box_tree(dom, Some(unsafe { &*box_tree_ptr }), text),
        None => tab.interaction.text_input(dom, text),
      };
      if changed {
        let Some(input_node_id) = tab.interaction.focused_node_id() else {
          return changed;
        };

        let index = crate::interaction::dom_index::DomIndex::build(dom);
        let Some(input) = index.node(input_node_id).filter(|node| dom_is_text_input(node)) else {
          return changed;
        };

        let query = input.get_attribute_ref("value").unwrap_or("");
        let Some(datalist_node_id) =
          crate::interaction::engine::resolve_associated_datalist(dom, input_node_id)
        else {
          return changed;
        };

        let mut options = Vec::new();
        for entry in crate::interaction::engine::collect_datalist_option_entries(dom, datalist_node_id)
        {
          if !crate::interaction::engine::datalist_option_matches_input_value(&entry.option, query) {
            continue;
          }
          options.push(DatalistOption {
            option_node_id: entry.node_id,
            value: entry.option.value,
            disabled: entry.option.disabled,
          });
        }

        if !options.is_empty() {
          datalist_open = Some((input_node_id, options));
        }
      }
      changed
    });

    if let Some((input_node_id, options)) = datalist_open {
      let anchor_css = doc
        .prepared()
        .and_then(|prepared| {
          styled_node_anchor_css(
            prepared.box_tree(),
            prepared.fragment_tree(),
            &tab.scroll_state,
            input_node_id,
          )
        })
        .filter(|rect| rect.width() > 0.0 && rect.height() > 0.0)
        .unwrap_or(Rect::from_xywh(0.0, 0.0, 1.0, 1.0));

      let _ = self.ui_tx.send(WorkerToUi::DatalistOpened {
        tab_id,
        input_node_id,
        options,
        anchor_css,
      });
    }

    if changed {
      if let (Some(focused), Some(js_tab)) = (tab.interaction.focused_node_id(), tab.js_tab.as_mut())
      {
        let dom_snapshot = doc.dom();
        let element_id = dom_node_by_preorder_id(dom_snapshot, focused)
          .and_then(|node| node.get_attribute_ref("id"));
        mirror_dom1_form_control_state_into_dom2(
          js_tab,
          tab.js_dom_mapping.as_ref(),
          dom_snapshot,
          focused,
          element_id,
        );
        tab.js_dom_mutation_generation = js_tab.dom().mutation_generation();
      }
      tab.cancel.bump_paint();
      tab.needs_repaint = true;
    }
  }

  fn handle_date_time_picker_choose(&mut self, tab_id: TabId, input_node_id: usize, value: String) {
    // Close the picker popup deterministically for any UI: `DateTimePickerChoose` always
    // corresponds to a user choosing a value in the picker overlay, so the popup should be
    // dismissed even if the selection is a no-op (choosing the currently-set value).
    let _ = self.ui_tx.send(WorkerToUi::DateTimePickerClosed { tab_id });

    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };
    let Some(doc) = tab.document.as_mut() else {
      return;
    };

    let engine = &mut tab.interaction;
    let dom_changed = doc.mutate_dom(|dom| engine.set_date_time_input_value(dom, input_node_id, &value));
    if dom_changed {
      if let Some(js_tab) = tab.js_tab.as_mut() {
        let dom_snapshot = doc.dom();
        let element_id = dom_node_by_preorder_id(dom_snapshot, input_node_id)
          .and_then(|node| node.get_attribute_ref("id"));
        mirror_dom1_form_control_state_into_dom2(
          js_tab,
          tab.js_dom_mapping.as_ref(),
          dom_snapshot,
          input_node_id,
          element_id,
        );
        tab.js_dom_mutation_generation = js_tab.dom().mutation_generation();
      }
      tab.cancel.bump_paint();
      tab.needs_repaint = true;
    }
  }

  fn handle_color_picker_choose(&mut self, tab_id: TabId, input_node_id: usize, value: String) {
    // Close the picker popup deterministically for any UI: `ColorPickerChoose` always corresponds
    // to a user choosing a value in the picker overlay, so the popup should be dismissed even if
    // the selection is a no-op (choosing the currently-set value).
    let _ = self.ui_tx.send(WorkerToUi::ColorPickerClosed { tab_id });

    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };
    let Some(doc) = tab.document.as_mut() else {
      return;
    };

    let engine = &mut tab.interaction;
    let dom_changed = doc.mutate_dom(|dom| engine.set_color_input_value(dom, input_node_id, &value));
    if dom_changed {
      if let Some(js_tab) = tab.js_tab.as_mut() {
        let dom_snapshot = doc.dom();
        let element_id = dom_node_by_preorder_id(dom_snapshot, input_node_id)
          .and_then(|node| node.get_attribute_ref("id"));
        mirror_dom1_form_control_state_into_dom2(
          js_tab,
          tab.js_dom_mapping.as_ref(),
          dom_snapshot,
          input_node_id,
          element_id,
        );
        tab.js_dom_mutation_generation = js_tab.dom().mutation_generation();
      }
      tab.cancel.bump_paint();
      tab.needs_repaint = true;
    }
  }

  fn handle_file_picker_choose(&mut self, tab_id: TabId, input_node_id: usize, paths: Vec<PathBuf>) {
    // Close the picker popup deterministically for any UI: `FilePickerChoose` always corresponds to
    // a user choosing a path in the picker overlay, so the popup should be dismissed even if the
    // selection is a no-op.
    let _ = self.ui_tx.send(WorkerToUi::FilePickerClosed { tab_id });

    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };
    let Some(doc) = tab.document.as_mut() else {
      return;
    };

    let engine = &mut tab.interaction;
    let changed = doc.mutate_dom(|dom| engine.file_picker_choose(dom, input_node_id, &paths));

    if changed {
      if let Some(js_tab) = tab.js_tab.as_mut() {
        let dom_snapshot = doc.dom();
        let element_id = dom_node_by_preorder_id(dom_snapshot, input_node_id)
          .and_then(|node| node.get_attribute_ref("id"));
        mirror_dom1_form_control_state_into_dom2(
          js_tab,
          tab.js_dom_mapping.as_ref(),
          dom_snapshot,
          input_node_id,
          element_id,
        );
        tab.js_dom_mutation_generation = js_tab.dom().mutation_generation();
      }
      tab.cancel.bump_paint();
      tab.needs_repaint = true;
    }
  }

  fn handle_ime_preedit(&mut self, tab_id: TabId, text: &str, cursor: Option<(usize, usize)>) {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };
    let Some(doc) = tab.document.as_mut() else {
      return;
    };

    let changed = doc.mutate_dom(|dom| tab.interaction.ime_preedit(dom, text, cursor));
    if changed {
      tab.cancel.bump_paint();
      tab.needs_repaint = true;
    }
  }

  fn handle_ime_commit(&mut self, tab_id: TabId, text: &str) {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };
    let Some(doc) = tab.document.as_mut() else {
      return;
    };

    let changed = doc.mutate_dom(|dom| tab.interaction.ime_commit(dom, text));
    if changed {
      if let (Some(focused), Some(js_tab)) = (tab.interaction.focused_node_id(), tab.js_tab.as_mut())
      {
        let dom_snapshot = doc.dom();
        let element_id = dom_node_by_preorder_id(dom_snapshot, focused)
          .and_then(|node| node.get_attribute_ref("id"));
        mirror_dom1_form_control_state_into_dom2(
          js_tab,
          tab.js_dom_mapping.as_ref(),
          dom_snapshot,
          focused,
          element_id,
        );
        tab.js_dom_mutation_generation = js_tab.dom().mutation_generation();
      }
      tab.cancel.bump_paint();
      tab.needs_repaint = true;
    }
  }

  fn handle_ime_cancel(&mut self, tab_id: TabId) {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };
    let Some(doc) = tab.document.as_mut() else {
      return;
    };

    let changed = doc.mutate_dom(|dom| tab.interaction.ime_cancel(dom));
    if changed {
      tab.cancel.bump_paint();
      tab.needs_repaint = true;
    }
  }

  fn handle_select_all(&mut self, tab_id: TabId) {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };
    let Some(doc) = tab.document.as_mut() else {
      return;
    };

    // Selecting text updates the focused control's caret/selection data attributes so the painter
    // can render highlights/caret state, but it should not require a full navigation refresh.
    let changed = doc.mutate_dom(|dom| tab.interaction.clipboard_select_all(dom));
    if changed {
      tab.cancel.bump_paint();
      tab.needs_repaint = true;
    }
  }

  fn handle_copy(&mut self, tab_id: TabId) {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };
    let Some(doc) = tab.document.as_mut() else {
      return;
    };

    let mut copied: Option<String> = None;
    if doc
      .mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
        copied = tab
          .interaction
          .clipboard_copy_with_layout(dom, box_tree, fragment_tree);
        (false, ())
      })
      .is_err()
    {
      // If we haven't rendered a frame yet, there is no cached layout to serialize the document
      // selection. Fall back to the focused text-control clipboard path.
      let _ = doc.mutate_dom(|dom| {
        copied = tab.interaction.clipboard_copy(dom);
        false
      });
    }

    if let Some(mut text) = copied {
      clipboard::clamp_clipboard_text_in_place(&mut text);
      let _ = self
        .ui_tx
        .send(WorkerToUi::SetClipboardText { tab_id, text });
    }
  }

  fn handle_cut(&mut self, tab_id: TabId) {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };
    let Some(doc) = tab.document.as_mut() else {
      return;
    };

    let mut cut_text: Option<String> = None;
    let changed = doc.mutate_dom(|dom| {
      let (dom_changed, text) = tab.interaction.clipboard_cut(dom);
      cut_text = text;
      dom_changed
    });

    if let Some(mut text) = cut_text {
      clipboard::clamp_clipboard_text_in_place(&mut text);
      let _ = self
        .ui_tx
        .send(WorkerToUi::SetClipboardText { tab_id, text });
    }

    if changed {
      if let (Some(focused), Some(js_tab)) = (tab.interaction.focused_node_id(), tab.js_tab.as_mut())
      {
        let dom_snapshot = doc.dom();
        let element_id = dom_node_by_preorder_id(dom_snapshot, focused)
          .and_then(|node| node.get_attribute_ref("id"));
        mirror_dom1_form_control_state_into_dom2(
          js_tab,
          tab.js_dom_mapping.as_ref(),
          dom_snapshot,
          focused,
          element_id,
        );
        tab.js_dom_mutation_generation = js_tab.dom().mutation_generation();
      }
      tab.cancel.bump_paint();
      tab.needs_repaint = true;
    }
  }

  fn handle_paste(&mut self, tab_id: TabId, text: &str) {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };
    let Some(doc) = tab.document.as_mut() else {
      return;
    };

    let text = clipboard::clamp_clipboard_text(text);
    let changed = doc.mutate_dom(|dom| tab.interaction.clipboard_paste(dom, text));
    if changed {
      if let (Some(focused), Some(js_tab)) = (tab.interaction.focused_node_id(), tab.js_tab.as_mut())
      {
        let dom_snapshot = doc.dom();
        let element_id = dom_node_by_preorder_id(dom_snapshot, focused)
          .and_then(|node| node.get_attribute_ref("id"));
        mirror_dom1_form_control_state_into_dom2(
          js_tab,
          tab.js_dom_mapping.as_ref(),
          dom_snapshot,
          focused,
          element_id,
        );
        tab.js_dom_mutation_generation = js_tab.dom().mutation_generation();
      }
      tab.cancel.bump_paint();
      tab.needs_repaint = true;
    }
  }

  fn handle_a11y_set_focus(&mut self, tab_id: TabId, node_id: usize) {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };
    let Some(doc) = tab.document.as_mut() else {
      return;
    };

    let scroll_snapshot = tab.scroll_state.clone();

    let (changed, next_scroll) = match doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
      let exists_and_element =
        match crate::dom::find_node_mut_by_preorder_id(dom, node_id) {
          Some(node) => node.is_element(),
          None => false,
        };
      if !exists_and_element {
        // Screen readers can act on stale ids; treat as a no-op and do not clear the current focus.
        return (false, (false, None));
      }

      let (dom_changed, _) = tab.interaction.focus_node_id(dom, Some(node_id), true);
      let focus_scroll = crate::interaction::focus_scroll::scroll_state_for_focus(
        box_tree,
        fragment_tree,
        &scroll_snapshot,
        node_id,
      );
      (dom_changed, (dom_changed, focus_scroll))
    }) {
      Ok((changed, scroll)) => (changed, scroll),
      Err(_) => {
        // No cached layout yet; focus without attempting focus scroll.
        let changed = doc.mutate_dom(|dom| {
          let exists_and_element =
            match crate::dom::find_node_mut_by_preorder_id(dom, node_id) {
              Some(node) => node.is_element(),
              None => false,
            };
          if !exists_and_element {
            return false;
          }
          tab.interaction.focus_node_id(dom, Some(node_id), true).0
        });
        (changed, None)
      }
    };

    let mut scroll_changed = false;
    if let Some(next_scroll) = next_scroll {
      if next_scroll != tab.scroll_state {
        tab.scroll_state = next_scroll;
        doc.set_scroll_state(tab.scroll_state.clone());
        tab.sync_js_scroll_state();
        scroll_changed = true;
        let _ = self.ui_tx.send(WorkerToUi::ScrollStateUpdated {
          tab_id,
          scroll: tab.scroll_state.clone(),
        });
        tab.last_reported_scroll_state = tab.scroll_state.clone();
      }
    }

    if changed || scroll_changed {
      tab.cancel.bump_paint();
      tab.needs_repaint = true;
      if scroll_changed {
        tab.scroll_coalesce = true;
      }
    }
  }

  fn handle_a11y_scroll_into_view(&mut self, tab_id: TabId, node_id: usize) {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };
    let Some(doc) = tab.document.as_mut() else {
      return;
    };

    let scroll_snapshot = tab.scroll_state.clone();
    let next_scroll = match doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
      let exists_and_element =
        match crate::dom::find_node_mut_by_preorder_id(dom, node_id) {
          Some(node) => node.is_element(),
          None => false,
        };
      if !exists_and_element {
        return (false, None);
      }
      let next = crate::interaction::focus_scroll::scroll_state_for_focus(
        box_tree,
        fragment_tree,
        &scroll_snapshot,
        node_id,
      );
      (false, next)
    }) {
      Ok(scroll) => scroll,
      Err(_) => None,
    };

    if let Some(next) = next_scroll {
      if next != tab.scroll_state {
        tab.scroll_state = next;
        doc.set_scroll_state(tab.scroll_state.clone());
        tab.sync_js_scroll_state();
        let _ = self.ui_tx.send(WorkerToUi::ScrollStateUpdated {
          tab_id,
          scroll: tab.scroll_state.clone(),
        });
        tab.last_reported_scroll_state = tab.scroll_state.clone();
        tab.cancel.bump_paint();
        tab.needs_repaint = true;
        tab.scroll_coalesce = true;
      }
    }
  }

  fn handle_a11y_activate(&mut self, tab_id: TabId, node_id: usize) {
    self.handle_a11y_set_focus(tab_id, node_id);

    // Avoid accidentally activating a different element when the requested node id is stale.
    let focused = self
      .tabs
      .get(&tab_id)
      .and_then(|tab| tab.interaction.focused_node_id());
    if focused != Some(node_id) {
      return;
    }

    // Reuse the existing keyboard activation path so navigation/form submission/toggling stays
    // consistent (including JS event dispatch).
    self.handle_key_action(tab_id, crate::interaction::KeyAction::Enter);
  }

  fn handle_key_action(&mut self, tab_id: TabId, key: crate::interaction::KeyAction) {
    let mut navigate_to: Option<String> = None;
    let mut navigate_request: Option<FormSubmission> = None;
    let mut keyboard_scroll: Option<UiToWorker> = None;
    let mut download_to_start: Option<(String, Option<String>)> = None;

    {
      let Some(tab) = self.tabs.get_mut(&tab_id) else {
        return;
      };
      let base_url =
        base_url_for_links(tab.last_base_url.as_deref(), tab.last_committed_url.as_deref());
      let document_url = tab
        .last_committed_url
        .as_deref()
        .unwrap_or(about_pages::ABOUT_BASE_URL);

      let Some(doc) = tab.document.as_mut() else {
        return;
      };

      let scroll_snapshot = tab.scroll_state.clone();
      let result = doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
        let (dom_changed, action) = tab.interaction.key_activate_with_layout_artifacts(
          dom,
          Some(box_tree),
          fragment_tree,
          key,
          document_url,
          base_url,
        );
        let submitter = tab.interaction.take_last_form_submitter();
        let submitter_element_id = submitter.and_then(|submitter_id| {
          crate::dom::find_node_mut_by_preorder_id(dom, submitter_id)
            .and_then(|node| node.get_attribute_ref("id"))
            .map(|id| id.to_string())
        });
        let focused = tab.interaction.focused_node_id();
        let (
          focused_element_id,
          focused_is_text_input,
          focused_is_input,
          focused_is_textarea,
          focused_is_select,
          focused_is_button,
          focused_is_video_controls,
        ) = focused
          .and_then(|focused_id| {
            crate::dom::find_node_mut_by_preorder_id(dom, focused_id).map(|node| {
              (
                node.get_attribute_ref("id").map(|id| id.to_string()),
                dom_is_text_input(node),
                dom_is_input(node),
                dom_is_textarea(node),
                dom_is_select(node),
                dom_is_button(node),
                dom_is_video_controls(node),
              )
            })
          })
          .unwrap_or((None, false, false, false, false, false, false));
        let focus_scroll = match &action {
          InteractionAction::FocusChanged {
            node_id: Some(node_id),
          } => crate::interaction::focus_scroll::scroll_state_for_focus(
            box_tree,
            fragment_tree,
            &scroll_snapshot,
            *node_id,
          ),
          _ => None,
        };
        (
          dom_changed,
          (
            dom_changed,
            action,
            focus_scroll,
            submitter,
            submitter_element_id,
            focused,
            focused_element_id,
            focused_is_text_input,
            focused_is_input,
            focused_is_textarea,
            focused_is_select,
            focused_is_button,
            focused_is_video_controls,
          ),
        )
      });
      let (
        changed,
        action,
        focus_scroll,
        form_submitter,
        form_submitter_element_id,
        focused,
        focused_element_id,
        focused_is_text_input,
        focused_is_input,
        focused_is_textarea,
        focused_is_select,
        focused_is_button,
        focused_is_video_controls,
      ) = match result {
        Ok(result) => result,
        Err(_) => {
          let mut action = InteractionAction::None;
          let mut submitter: Option<usize> = None;
          let mut submitter_element_id: Option<String> = None;
          let mut focused: Option<usize> = None;
          let mut focused_element_id: Option<String> = None;
          let mut focused_is_text_input = false;
          let mut focused_is_input = false;
          let mut focused_is_textarea = false;
          let mut focused_is_select = false;
          let mut focused_is_button = false;
          let mut focused_is_video_controls = false;
          let changed = doc.mutate_dom(|dom| {
            let (dom_changed, next_action) =
              tab
                .interaction
                .key_activate(dom, key, document_url, base_url);
            action = next_action;
            submitter = tab.interaction.take_last_form_submitter();
            submitter_element_id = submitter.and_then(|submitter_id| {
              crate::dom::find_node_mut_by_preorder_id(dom, submitter_id)
                .and_then(|node| node.get_attribute_ref("id"))
                .map(|id| id.to_string())
            });
            focused = tab.interaction.focused_node_id();
            let (id, is_text_input, is_input, is_textarea, is_select, is_button, is_video_controls) =
              focused
              .and_then(|focused_id| {
                crate::dom::find_node_mut_by_preorder_id(dom, focused_id).map(|node| {
                  (
                    node.get_attribute_ref("id").map(|id| id.to_string()),
                    dom_is_text_input(node),
                    dom_is_input(node),
                    dom_is_textarea(node),
                    dom_is_select(node),
                    dom_is_button(node),
                    dom_is_video_controls(node),
                  )
                })
              })
              .unwrap_or((None, false, false, false, false, false, false));
            focused_element_id = id;
            focused_is_text_input = is_text_input;
            focused_is_input = is_input;
            focused_is_textarea = is_textarea;
            focused_is_select = is_select;
            focused_is_button = is_button;
            focused_is_video_controls = is_video_controls;
            dom_changed
          });
          (
            changed,
            action,
            None,
            submitter,
            submitter_element_id,
            focused,
            focused_element_id,
            focused_is_text_input,
            focused_is_input,
            focused_is_textarea,
            focused_is_select,
            focused_is_button,
            focused_is_video_controls,
          )
        }
      };

      let mut scroll_changed = false;
      if let Some(next_scroll) = focus_scroll {
        tab.scroll_state = next_scroll;
        doc.set_scroll_state(tab.scroll_state.clone());
        tab.sync_js_scroll_state();
        scroll_changed = true;
        let _ = self.ui_tx.send(WorkerToUi::ScrollStateUpdated {
          tab_id,
          scroll: tab.scroll_state.clone(),
        });
        tab.last_reported_scroll_state = tab.scroll_state.clone();
      }

      let mut default_allowed = true;
      let mut dispatched_dom_event = false;

      // Keyboard activation should dispatch a cancelable `"click"` event on the activated element
      // before performing its default action (navigation, open-in-new-tab, submit, ...).
      //
      // Note: implicit form submission (Enter in a text input) does not fire a click event, so
      // only dispatch click when the activated element is not a text input (or is explicitly the
      // submitter).
      let mut click_target_id: Option<usize> = None;
      let mut click_target_element_id: Option<&str> = None;
      if matches!(
        action,
        InteractionAction::Navigate { .. }
          | InteractionAction::OpenInNewTab { .. }
          | InteractionAction::OpenInNewTabRequest { .. }
          | InteractionAction::Download { .. }
          | InteractionAction::NavigateRequest { .. }
      ) {
        if let Some(submitter_id) = form_submitter {
          if focused == Some(submitter_id) {
            click_target_id = Some(submitter_id);
            click_target_element_id = form_submitter_element_id.as_deref();
          }
        } else if let Some(focused_id) = focused {
          if !focused_is_text_input {
            click_target_id = Some(focused_id);
            click_target_element_id = focused_element_id.as_deref();
          }
        }
      }

      // If activation triggers a form submission attempt, dispatch a cancelable `"submit"` event
      // on the form owner and honor `preventDefault()` before committing the navigation.
      let mut submit_source_id: Option<usize> = None;
      let mut submit_source_element_id: Option<&str> = None;
      if let Some(submitter_id) = form_submitter {
        submit_source_id = Some(submitter_id);
        submit_source_element_id = form_submitter_element_id.as_deref();
      } else if focused_is_text_input
        && matches!(key, crate::interaction::KeyAction::Enter)
        && matches!(
          action,
          InteractionAction::Navigate { .. }
            | InteractionAction::OpenInNewTab { .. }
            | InteractionAction::NavigateRequest { .. }
            | InteractionAction::OpenInNewTabRequest { .. }
        )
      {
        submit_source_id = focused;
        submit_source_element_id = focused_element_id.as_deref();
      }

      // Mirror UI-driven form control changes (dom1) into dom2 before dispatching click/submit.
      //
      // This covers both:
      // - keyboard activation (click/submit), and
      // - text editing key actions (backspace/delete/range stepping/etc) where no DOM event is
      //   dispatched but dom2 still needs to observe the updated state.
      if changed {
        if let Some(js_tab) = tab.js_tab.as_mut() {
          let dom_snapshot = doc.dom();
          let mapping = tab.js_dom_mapping.as_ref();
          if let Some(focused_id) = focused {
            mirror_dom1_form_control_state_into_dom2(
              js_tab,
              mapping,
              dom_snapshot,
              focused_id,
              focused_element_id.as_deref(),
            );
          }
          if let Some(target_id) = click_target_id {
            mirror_dom1_form_control_state_into_dom2(
              js_tab,
              mapping,
              dom_snapshot,
              target_id,
              click_target_element_id,
            );
          }
          if let Some(source_id) = submit_source_id {
            mirror_dom1_form_control_state_into_dom2(
              js_tab,
              mapping,
              dom_snapshot,
              source_id,
              submit_source_element_id,
            );
          }
          // Keep the worker's cached JS mutation generation in sync with dom2 edits caused by
          // mirroring UI-driven form control state (dom1 → dom2). This prevents the paint pipeline
          // from treating these internal sync writes as "external" JS mutations that require a full
          // dom2 → dom1 resnapshot.
          tab.js_dom_mutation_generation = js_tab.dom().mutation_generation();
        }
      }

      let js_mutation_generation_before_dispatch =
        tab.js_tab.as_ref().map(|js_tab| js_tab.dom().mutation_generation());

      if let Some(target_id) = click_target_id {
        if let Some(js_tab) = tab.js_tab.as_mut() {
          let target = js_dom_node_for_preorder_id_with_log(
            &self.ui_tx,
            tab_id,
            self.debug_log_enabled,
            js_tab,
            target_id,
            click_target_element_id,
            &mut tab.js_dom_mapping_generation,
            &mut tab.js_dom_mapping,
            &mut tab.js_dom_mapping_miss_log_last,
            "click",
          );
          if let Some(node_id) = target {
            dispatched_dom_event = true;
            match js_tab.dispatch_click_event(node_id) {
              Ok(allowed) => default_allowed = allowed,
              Err(err) => {
                if self.debug_log_enabled {
                  let _ = self.ui_tx.send(WorkerToUi::DebugLog {
                    tab_id,
                    line: format!("js click event dispatch failed: {err}"),
                  });
                }
              }
            }
          }
        }
      }

      if default_allowed {
        if let Some(source_id) = submit_source_id {
          if let Some(js_tab) = tab.js_tab.as_mut() {
            let source_node =
              js_dom_node_for_preorder_id_with_log(
                &self.ui_tx,
                tab_id,
                self.debug_log_enabled,
                js_tab,
                source_id,
                submit_source_element_id,
                &mut tab.js_dom_mapping_generation,
                &mut tab.js_dom_mapping,
                &mut tab.js_dom_mapping_miss_log_last,
              "submit",
            );
            if let Some(source_node) = source_node {
              if let Some(form_node) = js_find_form_owner_for_submitter(js_tab.dom(), source_node) {
                dispatched_dom_event = true;
                match js_tab.dispatch_submit_event(form_node) {
                  Ok(allowed) => default_allowed = allowed,
                  Err(err) => {
                    if self.debug_log_enabled {
                      let _ = self.ui_tx.send(WorkerToUi::DebugLog {
                        tab_id,
                        line: format!("js submit event dispatch failed: {err}"),
                      });
                    }
                  }
                }
              }
            }
          }
        }
      }

      if dispatched_dom_event {
        if let Some(before) = js_mutation_generation_before_dispatch {
          self.pump_js_event_loop_after_dom_event_dispatch(tab_id, tab, before);
        }
      }

      let action_is_none = matches!(action, InteractionAction::None);
      match action {
        InteractionAction::Navigate { href } => {
          if default_allowed {
            navigate_to = Some(href);
          } else if changed || scroll_changed {
            tab.cancel.bump_paint();
            tab.needs_repaint = true;
          }
        }
        InteractionAction::OpenInNewTab { href } => {
          if default_allowed {
            let _ = self
              .ui_tx
              .send(WorkerToUi::RequestOpenInNewTab { tab_id, url: href });
          }
          if changed || scroll_changed {
            tab.cancel.bump_paint();
            tab.needs_repaint = true;
          }
        }
        InteractionAction::OpenInNewTabRequest { request } => {
          if default_allowed {
            let _ = self
              .ui_tx
              .send(WorkerToUi::RequestOpenInNewTabRequest { tab_id, request });
          }
          if changed || scroll_changed {
            tab.cancel.bump_paint();
            tab.needs_repaint = true;
          }
        }
        InteractionAction::Download { href, file_name } => {
          if default_allowed {
            download_to_start = Some((href, file_name));
          }
          if changed || scroll_changed {
            tab.cancel.bump_paint();
            tab.needs_repaint = true;
          }
        }
        InteractionAction::NavigateRequest { request } => {
          if default_allowed {
            navigate_request = Some(request);
          } else if changed || scroll_changed {
            tab.cancel.bump_paint();
            tab.needs_repaint = true;
          }
        }
        InteractionAction::OpenSelectDropdown {
          select_node_id,
          control,
        } => {
          // Back-compat: older UIs listen for `OpenSelectDropdown`.
          let _ = self.ui_tx.send(WorkerToUi::OpenSelectDropdown {
            tab_id,
            select_node_id,
            control: control.clone(),
          });

          let anchor_css = doc
            .prepared()
            .and_then(|prepared| {
              let tree = prepared.fragment_tree_for_geometry(&tab.scroll_state);
              select_anchor_css(prepared.box_tree(), &tree, &tab.scroll_state, select_node_id)
            })
            .filter(|rect| rect.width() > 0.0 && rect.height() > 0.0)
            .unwrap_or(Rect::from_xywh(0.0, 0.0, 1.0, 1.0));
          let _ = self.ui_tx.send(WorkerToUi::SelectDropdownOpened {
            tab_id,
            select_node_id,
            control,
            anchor_css,
          });
          if changed {
            tab.cancel.bump_paint();
            tab.needs_repaint = true;
          }
        }
        InteractionAction::OpenDateTimePicker { input_node_id, kind } => {
          let anchor_css = doc
            .prepared()
            .and_then(|prepared| {
              let tree = prepared.fragment_tree_for_geometry(&tab.scroll_state);
              styled_node_anchor_css(prepared.box_tree(), &tree, &tab.scroll_state, input_node_id)
            })
            .filter(|rect| rect.width() > 0.0 && rect.height() > 0.0)
            .unwrap_or(Rect::from_xywh(0.0, 0.0, 1.0, 1.0));

          let mut value: String = String::new();
          let _ = doc.mutate_dom(|dom| {
            value = crate::dom::find_node_mut_by_preorder_id(dom, input_node_id)
              .map(|node| match kind {
                crate::interaction::DateTimeInputKind::Date => {
                  crate::dom::input_date_value_string(node).unwrap_or_default()
                }
                crate::interaction::DateTimeInputKind::Time => {
                  crate::dom::input_time_value_string(node).unwrap_or_default()
                }
                crate::interaction::DateTimeInputKind::DateTimeLocal => {
                  crate::dom::input_datetime_local_value_string(node).unwrap_or_default()
                }
                crate::interaction::DateTimeInputKind::Month => {
                  crate::dom::input_month_value_string(node).unwrap_or_default()
                }
                crate::interaction::DateTimeInputKind::Week => {
                  crate::dom::input_week_value_string(node).unwrap_or_default()
                }
              })
              .unwrap_or_default();
            false
          });

          let _ = self.ui_tx.send(WorkerToUi::DateTimePickerOpened {
            tab_id,
            input_node_id,
            kind,
            value,
            anchor_css,
          });

          if changed {
            tab.cancel.bump_paint();
            tab.needs_repaint = true;
          }
        }
        InteractionAction::OpenColorPicker { input_node_id } => {
          let anchor_css = doc
            .prepared()
            .and_then(|prepared| {
              let tree = prepared.fragment_tree_for_geometry(&tab.scroll_state);
              styled_node_anchor_css(prepared.box_tree(), &tree, &tab.scroll_state, input_node_id)
            })
            .filter(|rect| rect.width() > 0.0 && rect.height() > 0.0)
            .unwrap_or(Rect::from_xywh(0.0, 0.0, 1.0, 1.0));

          let mut value: String = String::new();
          let _ = doc.mutate_dom(|dom| {
            value = crate::dom::find_node_mut_by_preorder_id(dom, input_node_id)
              .and_then(|node| crate::dom::input_color_value_string(node))
              .unwrap_or_default();
            false
          });

          let _ = self.ui_tx.send(WorkerToUi::ColorPickerOpened {
            tab_id,
            input_node_id,
            value,
            anchor_css,
          });

          if changed {
            tab.cancel.bump_paint();
            tab.needs_repaint = true;
          }
        }
        InteractionAction::OpenFilePicker {
          input_node_id,
          multiple,
          accept,
        } => {
          let anchor_css = doc
            .prepared()
            .and_then(|prepared| {
              let tree = prepared.fragment_tree_for_geometry(&tab.scroll_state);
              styled_node_anchor_css(prepared.box_tree(), &tree, &tab.scroll_state, input_node_id)
            })
            .filter(|rect| rect.width() > 0.0 && rect.height() > 0.0)
            .unwrap_or(Rect::from_xywh(0.0, 0.0, 1.0, 1.0));

          let _ = self.ui_tx.send(WorkerToUi::FilePickerOpened {
            tab_id,
            input_node_id,
            multiple,
            accept,
            anchor_css,
          });

          if changed {
            tab.cancel.bump_paint();
            tab.needs_repaint = true;
          }
        }
        _ => {
          // Basic keyboard scrolling: when scroll keys are pressed and the focused element is not a
          // form control that would normally consume them, treat the key as a viewport scrolling
          // shortcut (matching common browser behaviour like Space scrolling even when a link is
          // focused).
          if action_is_none {
            let focus_consumes_space =
              focused_is_input
                || focused_is_textarea
                || focused_is_select
                || focused_is_button
                || focused_is_video_controls;
            let focus_consumes_arrows =
              focused_is_input || focused_is_textarea || focused_is_select || focused_is_video_controls;
            let focus_consumes_home_end = focus_consumes_arrows;
            // PageUp/PageDown are not commonly consumed by media controls, so keep their behaviour
            // aligned with other non-button form controls.
            let focus_consumes_page = focused_is_input || focused_is_textarea || focused_is_select;

            let allow_scroll = match key {
              crate::interaction::KeyAction::Space | crate::interaction::KeyAction::ShiftSpace => {
                !focus_consumes_space
              }
              crate::interaction::KeyAction::ArrowDown
              | crate::interaction::KeyAction::ArrowUp
              | crate::interaction::KeyAction::ArrowLeft
              | crate::interaction::KeyAction::ArrowRight => !focus_consumes_arrows,
              crate::interaction::KeyAction::Home
              | crate::interaction::KeyAction::End
              | crate::interaction::KeyAction::ShiftHome
              | crate::interaction::KeyAction::ShiftEnd => !focus_consumes_home_end,
              crate::interaction::KeyAction::PageUp | crate::interaction::KeyAction::PageDown => {
                !focus_consumes_page
              }
              _ => false,
            };

            if allow_scroll {
              keyboard_scroll = match key {
                crate::interaction::KeyAction::Home | crate::interaction::KeyAction::ShiftHome => {
                  Some(UiToWorker::ScrollTo {
                    tab_id,
                    pos_css: (tab.scroll_state.viewport.x, 0.0),
                  })
                }
                crate::interaction::KeyAction::End | crate::interaction::KeyAction::ShiftEnd => {
                  Some(UiToWorker::ScrollTo {
                    tab_id,
                    pos_css: (tab.scroll_state.viewport.x, f32::MAX),
                  })
                }
                crate::interaction::KeyAction::ArrowDown => Some(UiToWorker::Scroll {
                  tab_id,
                  delta_css: (0.0, 40.0),
                  pointer_css: None,
                }),
                crate::interaction::KeyAction::ArrowUp => Some(UiToWorker::Scroll {
                  tab_id,
                  delta_css: (0.0, -40.0),
                  pointer_css: None,
                }),
                crate::interaction::KeyAction::ArrowRight => Some(UiToWorker::Scroll {
                  tab_id,
                  delta_css: (40.0, 0.0),
                  pointer_css: None,
                }),
                crate::interaction::KeyAction::ArrowLeft => Some(UiToWorker::Scroll {
                  tab_id,
                  delta_css: (-40.0, 0.0),
                  pointer_css: None,
                }),
                crate::interaction::KeyAction::Space => {
                  let h = tab.viewport_css.1.max(1) as f32;
                  let dy = (h * 0.9).max(1.0);
                  Some(UiToWorker::Scroll {
                    tab_id,
                    delta_css: (0.0, dy),
                    pointer_css: None,
                  })
                }
                crate::interaction::KeyAction::ShiftSpace => {
                  let h = tab.viewport_css.1.max(1) as f32;
                  let dy = -((h * 0.9).max(1.0));
                  Some(UiToWorker::Scroll {
                    tab_id,
                    delta_css: (0.0, dy),
                    pointer_css: None,
                  })
                }
                crate::interaction::KeyAction::PageDown => {
                  let h = tab.viewport_css.1.max(1) as f32;
                  let dy = (h * 0.9).max(1.0);
                  Some(UiToWorker::Scroll {
                    tab_id,
                    delta_css: (0.0, dy),
                    pointer_css: None,
                  })
                }
                crate::interaction::KeyAction::PageUp => {
                  let h = tab.viewport_css.1.max(1) as f32;
                  let dy = -((h * 0.9).max(1.0));
                  Some(UiToWorker::Scroll {
                    tab_id,
                    delta_css: (0.0, dy),
                    pointer_css: None,
                  })
                }
                _ => None,
              };
            }
          }
          if changed || scroll_changed {
            tab.cancel.bump_paint();
            tab.needs_repaint = true;
          }
        }
      }
    }

    if let Some((href, file_name)) = download_to_start {
      self.start_download(tab_id, href, file_name);
    }

    if let Some(href) = navigate_to {
      self.schedule_navigation(tab_id, href, NavigationReason::LinkClick);
    } else if let Some(request) = navigate_request {
      self.schedule_navigation_request(tab_id, request, NavigationReason::LinkClick);
    }

    if let Some(scroll_msg) = keyboard_scroll {
      self.handle_message(scroll_msg);
    }
  }

  fn next_job(&mut self) -> Option<Job> {
    if let Some(active) = self.active_tab {
      if let Some(tab) = self.tabs.get_mut(&active) {
        if let Some(req) = tab.pending_navigation.take() {
          return Some(Job::Navigate {
            tab_id: active,
            request: req,
          });
        }
      }
    }
    if let Some(active) = self.active_tab {
      if self.tabs.get(&active).is_some_and(|t| t.needs_repaint) {
        if let Some(tab) = self.tabs.get_mut(&active) {
          let force = std::mem::take(&mut tab.force_repaint);
          let is_scroll = std::mem::take(&mut tab.next_paint_is_scroll);
          tab.needs_repaint = false;
          tab.scroll_coalesce = false;
          return Some(Job::Paint {
            tab_id: active,
            force,
            is_scroll,
          });
        }
      }
    }

    // Any pending navigation.
    if let Some((tab_id, req)) = self
      .tabs
      .iter_mut()
      .find_map(|(id, tab)| tab.pending_navigation.take().map(|req| (*id, req)))
    {
      return Some(Job::Navigate {
        tab_id,
        request: req,
      });
    }

    // Paint any tab.
    if let Some(tab_id) = self
      .tabs
      .iter()
      .find_map(|(id, tab)| tab.needs_repaint.then_some(*id))
    {
      if let Some(tab) = self.tabs.get_mut(&tab_id) {
        let force = std::mem::take(&mut tab.force_repaint);
        let is_scroll = std::mem::take(&mut tab.next_paint_is_scroll);
        tab.needs_repaint = false;
        tab.scroll_coalesce = false;
        return Some(Job::Paint {
          tab_id,
          force,
          is_scroll,
        });
      }
    }

    None
  }

  fn is_output_still_current(&self, output: &JobOutput) -> bool {
    let Some(tab) = self.tabs.get(&output.tab_id) else {
      return false;
    };
    match output.snapshot_kind {
      SnapshotKind::Prepare => output.snapshot == tab.cancel.snapshot_prepare(),
      SnapshotKind::Paint => output.snapshot == tab.cancel.snapshot_paint(),
    }
  }

  fn run_job(&mut self, job: Job) -> Option<JobOutput> {
    match job {
      Job::Navigate { tab_id, request } => self.run_navigation(tab_id, request),
      Job::Paint {
        tab_id,
        force,
        is_scroll,
      } => self.run_paint(tab_id, force, is_scroll),
    }
  }

  // Intentionally a helper (no `&self`) so it can be called while holding `tab: &mut TabState`
  // borrowed from `self.tabs` without triggering borrow-checker errors (E0502).
  fn sync_js_tab_for_committed_navigation(
    runtime_toggles: &Arc<RuntimeToggles>,
    tab_id: TabId,
    tab: &mut TabState,
    committed_url: &str,
    viewport_css: (u32, u32),
    dpr: f32,
    timeout: Option<std::time::Duration>,
    cancel_callback: Option<Arc<crate::render_control::CancelCallback>>,
    debug_log_enabled: bool,
    msgs: &mut Vec<WorkerToUi>,
  ) {
    fn prewarm_js_tab_renderer_preorder_mapping(
      tab_id: TabId,
      js_tab: &mut BrowserTab,
      debug_log_enabled: bool,
      msgs: &mut Vec<WorkerToUi>,
    ) {
      // Pointer events can arrive immediately after a navigation commits. Ensure the JS tab's
      // renderer-preorder → dom2 NodeId mapping cache is populated so the first event can be routed
      // to the correct target even when elements lack `id=` attributes.
      //
      // This is a cheap, paint-free operation: it only traverses the dom2 tree to build the mapping.
      let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // Preorder id 1 corresponds to the document root in the renderer's traversal.
        js_tab.dom2_node_for_renderer_preorder(1)
      }));
      match res {
        Ok(Some(_)) => {}
        Ok(None) => {
          if debug_log_enabled {
            msgs.push(WorkerToUi::DebugLog {
              tab_id,
              line: "JS tab renderer preorder mapping prewarm returned None".to_string(),
            });
          }
        }
        Err(payload) => {
          if debug_log_enabled {
            let msg = payload
              .downcast_ref::<&str>()
              .map(|s| (*s).to_string())
              .or_else(|| payload.downcast_ref::<String>().cloned())
              .unwrap_or_else(|| "unknown panic".to_string());
            msgs.push(WorkerToUi::DebugLog {
              tab_id,
              line: format!("panic while prewarming JS tab renderer preorder mapping: {msg}"),
            });
          }
        }
      }
    }

    // `BrowserTab` navigations are powered by the resource fetcher (http/file/data); it does not
    // know how to fetch internal `about:` pages rendered by the UI worker.
    if about_pages::is_about_url(committed_url) {
      tab.js_tab = None;
      tab.js_dom_mapping_generation = 0;
      tab.js_dom_mapping = None;
      tab.js_dom_mapping_miss_log_last.clear();
      tab.js_dom_dirty = false;
      tab.js_dom_mutation_generation = 0;
      return;
    }
    let Some(doc) = tab.document.as_ref() else {
      tab.js_tab = None;
      tab.js_dom_mapping_generation = 0;
      tab.js_dom_mapping = None;
      tab.js_dom_mapping_miss_log_last.clear();
      tab.js_dom_dirty = false;
      tab.js_dom_mutation_generation = 0;
      return;
    };

    let cancel_check = cancel_callback.clone();
    // If the navigation has already been cancelled/preempted, avoid doing any JS work.
    if cancel_check.as_ref().is_some_and(|cb| cb()) {
      tab.js_tab = None;
      tab.js_dom_mapping_generation = 0;
      tab.js_dom_mapping = None;
      tab.js_dom_mapping_miss_log_last.clear();
      tab.js_dom_dirty = false;
      tab.js_dom_mutation_generation = 0;
      return;
    }

    let mut options = RenderOptions::default()
      .with_viewport(viewport_css.0, viewport_css.1)
      .with_device_pixel_ratio(dpr);
    options.runtime_toggles = Some(Arc::clone(runtime_toggles));
    options.timeout = timeout;
    options.cancel_callback = cancel_callback;

    let fetcher = doc.fetcher();
    let blank_html =
      "<!doctype html><html><head><meta charset=\"utf-8\"></head><body></body></html>";

    if let Some(js_tab) = tab.js_tab.as_mut() {
      if let Err(err) = js_tab.navigate_to_url(committed_url, options) {
        let cancelled = cancel_check.as_ref().is_some_and(|cb| cb());
        tab.js_tab = None;
        tab.js_dom_mapping_generation = 0;
        tab.js_dom_mapping = None;
        tab.js_dom_mapping_miss_log_last.clear();
        tab.js_dom_dirty = false;
        tab.js_dom_mutation_generation = 0;
        if debug_log_enabled && !cancelled {
          let kind = if err.is_timeout() { "timed out" } else { "failed" };
          msgs.push(WorkerToUi::DebugLog {
            tab_id,
            line: format!("js tab navigation to {committed_url} {kind}: {err}"),
          });
        }
      } else {
        tab.js_dom_dirty = false;
        // Keep the JS tab's view state (scroll) in sync with the UI worker so DOM APIs like
        // `document.elementFromPoint` reflect the same viewport as the rendered document.
        js_tab.set_scroll_state(tab.scroll_state.clone());
        tab.js_dom_mutation_generation = js_tab.dom().mutation_generation();
        prewarm_js_tab_renderer_preorder_mapping(tab_id, js_tab, debug_log_enabled, msgs);
      }
      // Navigation replaces the JS document, invalidating any preorder→NodeId mapping cache.
      tab.js_dom_mapping_generation = 0;
      tab.js_dom_mapping = None;
      tab.js_dom_mapping_miss_log_last.clear();
      return;
    }

    // We need to pass the (possibly deadline-bounded) `RenderOptions` into both:
    // - JS tab construction (which parses the initial HTML), and
    // - the subsequent navigation.
    //
    // This ensures JS-capable navigations are bounded by the same cooperative cancellation/timeout
    // mechanisms used for renderer navigations.
    let mut js_tab = match BrowserTab::from_html_with_document_url_and_fetcher(
      blank_html,
      about_pages::ABOUT_BLANK,
      options.clone(),
      VmJsBrowserTabExecutor::default(),
      fetcher,
    ) {
      Ok(tab) => tab,
      Err(err) => {
        let cancelled = cancel_check.as_ref().is_some_and(|cb| cb());
        if debug_log_enabled && !cancelled {
          let kind = if err.is_timeout() { "timed out" } else { "failed" };
          msgs.push(WorkerToUi::DebugLog {
            tab_id,
            line: format!("js tab init for {committed_url} {kind}: {err}"),
          });
        }
        return;
      }
    };

    if let Err(err) = js_tab.navigate_to_url(committed_url, options) {
      let cancelled = cancel_check.as_ref().is_some_and(|cb| cb());
      if debug_log_enabled && !cancelled {
        let kind = if err.is_timeout() { "timed out" } else { "failed" };
        msgs.push(WorkerToUi::DebugLog {
          tab_id,
          line: format!("js tab navigation to {committed_url} {kind}: {err}"),
        });
      }
      return;
    }
    js_tab.set_scroll_state(tab.scroll_state.clone());
    prewarm_js_tab_renderer_preorder_mapping(tab_id, &mut js_tab, debug_log_enabled, msgs);
    let generation = js_tab.dom().mutation_generation();
    tab.js_tab = Some(js_tab);
    tab.js_dom_mapping_generation = 0;
    tab.js_dom_mapping = None;
    tab.js_dom_mapping_miss_log_last.clear();
    tab.js_dom_dirty = false;
    tab.js_dom_mutation_generation = generation;
  }

  // Run a single bounded JS "pump" after a navigation commits, then (best-effort) sync the JS DOM
  // snapshot back into the renderer DOM so any script-driven UI changes become visible.
  //
  // Returns `true` when the renderer DOM was replaced and a repaint was scheduled.
  fn pump_js_once_and_sync_dom_after_committed_navigation(
    tab_id: TabId,
    tab: &mut TabState,
    msgs: &mut Vec<WorkerToUi>,
  ) -> bool {
    // Pump JS once (bounded) so any post-parse lifecycle tasks run.
    let cancel_snapshot = tab.cancel.snapshot_paint();
    let cancel_callback = cancel_snapshot.cancel_callback_for_paint(&tab.cancel);
    let deadline = deadline_for(cancel_callback.clone(), Some(POST_NAV_JS_PUMP_TIMEOUT));
    let _deadline_guard = DeadlineGuard::install(Some(&deadline));

    // Limit execution to a single task turn plus microtasks. This keeps the worker responsive while
    // still allowing initial DOMContentLoaded/defer-style work to run.
    let run_limits = RunLimits {
      max_tasks: POST_NAV_JS_PUMP_MAX_TASKS,
      max_microtasks: POST_NAV_JS_PUMP_MAX_MICROTASKS,
      max_wall_time: Some(POST_NAV_JS_PUMP_TIMEOUT),
    };

    let (js_dom_snapshot, js_dom_mapping, js_dom_generation) = {
      let Some(js_tab) = tab.js_tab.as_mut() else {
        return false;
      };
      if let Err(err) = js_tab.run_event_loop_until_idle(run_limits) {
        if !cancel_callback() {
          msgs.push(WorkerToUi::DebugLog {
            tab_id,
            line: format!("js post-navigation pump failed: {err}"),
          });
        }
      }
      // The post-navigation pump can resume streaming parsing and run lifecycle tasks, mutating the
      // JS DOM. Prewarm the JS tab's renderer-preorder → dom2 NodeId mapping cache *after* the pump
      // so the first user pointer event can be dispatched reliably without paying the mapping build
      // cost on the hot path.
      let prewarm = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // Preorder id 1 corresponds to the document root in the renderer's traversal.
        js_tab.dom2_node_for_renderer_preorder(1)
      }));
      match prewarm {
        Ok(Some(_)) => {}
        Ok(None) => {
          msgs.push(WorkerToUi::DebugLog {
            tab_id,
            line: "JS tab renderer preorder mapping prewarm returned None".to_string(),
          });
        }
        Err(payload) => {
          let msg = payload
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".to_string());
          msgs.push(WorkerToUi::DebugLog {
            tab_id,
            line: format!("panic while prewarming JS tab renderer preorder mapping: {msg}"),
          });
        }
      }
      // Snapshot the post-pump DOM so we can compare against the renderer DOM without holding a
      // borrow into `tab.js_tab` across the subsequent `tab.document` mutation.
      let dom2 = js_tab.dom();
      let generation = dom2.mutation_generation();
      let snapshot = dom2.to_renderer_dom_with_mapping();
      let mut dom_snapshot = snapshot.dom;
      let mapping = snapshot.mapping;
      dom2.project_form_control_state_into_renderer_dom_snapshot(&mut dom_snapshot, &mapping);
      (dom_snapshot, mapping, generation)
    };
    // Keep our cached generation in sync with whatever ran during the pump so subsequent ticks
    // don't treat the DOM as "dirty" purely due to this initial execution slice.
    tab.js_dom_mutation_generation = js_dom_generation;
    tab.js_dom_mapping_generation = js_dom_generation;
    tab.js_dom_mapping = Some(js_dom_mapping);
    tab.js_dom_mapping_miss_log_last.clear();

    let Some(doc) = tab.document.as_mut() else {
      return false;
    };

    // Sync dom2 → dom1 and schedule a repaint when the JS DOM snapshot differs from the renderer's
    // current DOM.
    if dom_tree_eq(doc.dom(), &js_dom_snapshot) {
      return false;
    }

    doc.mutate_dom(|dom| {
      *dom = js_dom_snapshot;
      true
    });
    if let Some(committed_url) = tab.last_committed_url.as_deref() {
      let new_base_url = crate::html::document_base_url(doc.dom(), Some(committed_url));
      if new_base_url != tab.last_base_url {
        tab.last_base_url = new_base_url.clone();
        doc.set_navigation_urls(tab.last_committed_url.clone(), new_base_url.clone());
      }
    }

    tab.cancel.bump_paint();
    tab.needs_repaint = true;
    true
  }

  fn run_navigation(&mut self, tab_id: TabId, request: NavigationRequest) -> Option<JobOutput> {
    let preempt_cancel_callback = self.preempt_cancel_callback_for_job(tab_id);
    let request_for_retry = request.clone();

    let NavigationRequest {
      request,
      apply_fragment_scroll,
    } = request;

    // Pull what we need out of `TabState` so we can release the borrow while running the expensive
    // prepare+paint pipeline (and so we can reinsert the document on all exit paths).
    let (snapshot, paint_snapshot, viewport_css, dpr, initial_scroll, cancel, doc, current_site_key) = {
      let tab = self.tabs.get_mut(&tab_id)?;
      let doc = tab.document.take();
      if doc.is_none() {
        // If we have to create a brand new long-lived `BrowserDocument` (e.g. first navigation, or a
        // recovered-from-crash tab), reset tick time so the new document's timeline starts at 0.
        tab.tick_time = Duration::ZERO;
      }
      (
        tab.cancel.snapshot_prepare(),
        tab.cancel.snapshot_paint(),
        tab.viewport_css,
        tab.dpr,
        tab.history.current().map(|e| (e.scroll_x, e.scroll_y)),
        tab.cancel.clone(),
        doc,
        tab.site_key.clone(),
      )
    };
    // Capture the original URL before any redirects/mutations for history bookkeeping.
    let original_url = request.url.clone();

    // Ensure we always put the document back into the tab state before returning.
    let mut doc = match doc {
      Some(doc) => doc,
      None => match self.build_initial_document(viewport_css, dpr) {
        Ok(doc) => doc,
        Err(err) => {
          let Some(tab) = self.tabs.get_mut(&tab_id) else {
            return None;
          };
          tab.loading = false;
          tab.pending_history_entry = false;
          tab.history.mark_committed();
          return Some(JobOutput {
            tab_id,
            snapshot,
            snapshot_kind: SnapshotKind::Prepare,
            msgs: vec![
              WorkerToUi::NavigationFailed {
                tab_id,
                url: original_url,
                error: format!("failed to create initial BrowserDocument: {err}"),
                can_go_back: tab.history.can_go_back(),
                can_go_forward: tab.history.can_go_forward(),
              },
              WorkerToUi::LoadingState {
                tab_id,
                loading: false,
              },
            ],
          });
        }
      },
    };

    // ---------------------------------------------------------------------------
    // Site isolation: process selection for this navigation
    // ---------------------------------------------------------------------------
    //
    // The UI worker models a site-isolated process boundary by rebuilding the per-tab renderer
    // when navigating across sites. We keep the previous committed `BrowserDocument` alive while a
    // cross-site navigation is in flight so cancellation (StopLoading / superseded navigation) can
    // restore the currently committed document state.
    let process_site_key = site_key_for_navigation(&original_url, None);
    let mut fallback_doc: Option<BrowserDocument> = None;

    // Fail fast for unsupported schemes before we allocate a new renderer for a site swap.
    if !about_pages::is_about_url(&original_url) {
      if let Err(err) = validate_user_navigation_url_scheme(&original_url) {
        let _ = self.reinsert_document(tab_id, doc);
        let Some(tab) = self.tabs.get_mut(&tab_id) else {
          return None;
        };
        tab.loading = false;
        tab.pending_history_entry = false;
        tab.history.mark_committed();
        return Some(JobOutput {
          tab_id,
          snapshot,
          snapshot_kind: SnapshotKind::Prepare,
          msgs: vec![
            WorkerToUi::NavigationFailed {
              tab_id,
              url: original_url,
              error: err,
              can_go_back: tab.history.can_go_back(),
              can_go_forward: tab.history.can_go_forward(),
            },
            WorkerToUi::LoadingState {
              tab_id,
              loading: false,
            },
          ],
        });
      }
    }

    // If this is a cross-site navigation, create a fresh renderer instance for the target site and
    // keep the previous committed document as a fallback for cancellation.
    if current_site_key
      .as_ref()
      .is_some_and(|current| current != &process_site_key)
    {
      fallback_doc = Some(doc);
      doc = match self.build_initial_document(viewport_css, dpr) {
        Ok(doc) => doc,
        Err(err) => {
          if let Some(fallback) = fallback_doc {
            let _ = self.reinsert_document(tab_id, fallback);
          }
          let Some(tab) = self.tabs.get_mut(&tab_id) else {
            return None;
          };
          tab.loading = false;
          tab.pending_history_entry = false;
          tab.history.mark_committed();
          return Some(JobOutput {
            tab_id,
            snapshot,
            snapshot_kind: SnapshotKind::Prepare,
            msgs: vec![
              WorkerToUi::NavigationFailed {
                tab_id,
                url: original_url,
                error: format!("failed to create renderer for site swap: {err}"),
                can_go_back: tab.history.can_go_back(),
                can_go_forward: tab.history.can_go_forward(),
              },
              WorkerToUi::LoadingState {
                tab_id,
                loading: false,
              },
            ],
          });
        }
      };
    }

    let prepare_cancel_callback = combine_cancel_callbacks(
      snapshot.cancel_callback_for_prepare(&cancel),
      preempt_cancel_callback.clone(),
    );
    let mut options = RenderOptions::default()
      .with_viewport(viewport_css.0, viewport_css.1)
      .with_device_pixel_ratio(dpr);
    options.runtime_toggles = Some(Arc::clone(&self.runtime_toggles));
    options.cancel_callback = Some(prepare_cancel_callback.clone());

    // -----------------------------
    // Prepare/navigation stage
    // -----------------------------

    let (reported_final_url, base_url) = if about_pages::is_about_url(&original_url) {
      let html = about_pages::html_for_about_url(&original_url).unwrap_or_else(|| {
        about_pages::error_page_html(
          "Unknown about page",
          &format!("Unknown URL: {original_url}"),
          None,
        )
      });

      let result = {
        let _guard = forward_stage_heartbeats(tab_id, self.ui_tx.clone());
        doc.navigate_html_with_options(
          &original_url,
          &html,
          Some(about_pages::ABOUT_BASE_URL),
          options.clone(),
        )
      };

      match result {
        Ok((committed_url, base_url)) => (Some(committed_url), Some(base_url)),
        Err(err) => {
          // Treat cancelled/preempted prepares as silent drops.
          if prepare_cancel_callback() {
            // New navigation superseded this attempt.
            if !snapshot.is_still_current_for_prepare(&cancel) {
              if let Some(fallback) = fallback_doc {
                let _ = self.reinsert_document(tab_id, fallback);
              } else {
                let _ = self.reinsert_document(tab_id, doc);
              }
              return None;
            }
            // Preempted by active-tab work: re-queue the navigation so it can resume later.
            if let Some(tab) = self.tabs.get_mut(&tab_id) {
              tab.pending_navigation = Some(request_for_retry);
            }
            if let Some(fallback) = fallback_doc {
              let _ = self.reinsert_document(tab_id, fallback);
            } else {
              let _ = self.reinsert_document(tab_id, doc);
            }
            return None;
          }
          if !snapshot.is_still_current_for_prepare(&cancel) {
            if let Some(fallback) = fallback_doc {
              let _ = self.reinsert_document(tab_id, fallback);
            } else {
              let _ = self.reinsert_document(tab_id, doc);
            }
            return None;
          }
          let _ = self.reinsert_document(tab_id, doc);
          return self.run_navigation_error(
            tab_id,
            &original_url,
            &format!("about page prepare failed: {err}"),
            snapshot,
          );
        }
      }
    } else {
      let report = {
        let _guard = forward_stage_heartbeats(tab_id, self.ui_tx.clone());
        if request.method == FormSubmissionMethod::Get
          && request.headers.is_empty()
          && request.body.is_none()
        {
          doc
            .navigate_url(&original_url, options.clone())
            .map(|report| (report.final_url, report.base_url))
        } else {
          doc
            .navigate_http_request_with_options(
              &original_url,
              request.method.as_http_method(),
              &request.headers,
              request.body.as_deref(),
              options.clone(),
            )
            .map(|(committed_url, base_url)| (Some(committed_url), Some(base_url)))
        }
      };
      match report {
        Ok((final_url, base_url)) => (final_url, base_url),
        Err(err) => {
          // If the navigation was cancelled/preempted, treat it as a silent drop.
          if prepare_cancel_callback() {
            if !snapshot.is_still_current_for_prepare(&cancel) {
              if let Some(fallback) = fallback_doc {
                let _ = self.reinsert_document(tab_id, fallback);
              } else {
                let _ = self.reinsert_document(tab_id, doc);
              }
              return None;
            }
            if let Some(tab) = self.tabs.get_mut(&tab_id) {
              tab.pending_navigation = Some(request_for_retry);
            }
            if let Some(fallback) = fallback_doc {
              let _ = self.reinsert_document(tab_id, fallback);
            } else {
              let _ = self.reinsert_document(tab_id, doc);
            }
            return None;
          }
          if !snapshot.is_still_current_for_prepare(&cancel) {
            if let Some(fallback) = fallback_doc {
              let _ = self.reinsert_document(tab_id, fallback);
            } else {
              let _ = self.reinsert_document(tab_id, doc);
            }
            return None;
          }

          // Restore the document before delegating to the navigation-error renderer.
          let _ = self.reinsert_document(tab_id, doc);
          return self.run_navigation_error(tab_id, &original_url, &err.to_string(), snapshot);
        }
      }
    };

    // If a new navigation was initiated while we were preparing, treat this result as cancelled.
    if !snapshot.is_still_current_for_prepare(&cancel) {
      if let Some(fallback) = fallback_doc {
        let _ = self.reinsert_document(tab_id, fallback);
      } else {
        let _ = self.reinsert_document(tab_id, doc);
      }
      return None;
    }

    // Preserve fragments across redirects so:
    // - history keeps the original `#fragment`
    // - `:target` / anchor scrolling still work
    let committed_url = match reported_final_url.as_deref() {
      Some(final_url) => apply_original_fragment_to_final_url(&original_url, final_url),
      None => original_url.clone(),
    };

    // ---------------------------------------------------------------------------
    // Site isolation: committed URL must match the process it ran in
    // ---------------------------------------------------------------------------
    //
    // A navigation may commit a different site than the URL it started with:
    // - Cross-site redirects (A -> B)
    // - A compromised/buggy renderer lying about `final_url`
    //
    // In either case, we must not allow the navigation to commit in the wrong renderer process.
    let committed_site_key = site_key_for_navigation(&committed_url, None);
    if committed_site_key != process_site_key {
      // Drop the untrusted renderer/document and restore the previously committed document (if any)
      // while we restart navigation in the correct process.
      if let Some(fallback) = fallback_doc {
        let _ = self.reinsert_document(tab_id, fallback);
      }

      let Some(tab) = self.tabs.get_mut(&tab_id) else {
        return None;
      };

      tab.site_mismatch_restarts = tab.site_mismatch_restarts.saturating_add(1);
      if tab.site_mismatch_restarts > MAX_SITE_MISMATCH_RESTARTS {
        // Give up after too many restarts to avoid infinite loops. Treat this as a navigation
        // failure and stop loading, leaving the committed history entry untouched.
        tab.loading = false;
        if tab.pending_history_entry {
          tab.history.cancel_pending_navigation_entry();
        } else {
          tab.history.revert_to_committed();
        }
        tab.pending_history_entry = false;
        return Some(JobOutput {
          tab_id,
          snapshot,
          snapshot_kind: SnapshotKind::Prepare,
          msgs: vec![
            WorkerToUi::NavigationFailed {
              tab_id,
              url: original_url,
              error: format!(
                "site isolation: navigation committed to {committed_url} but ran in wrong process; exceeded restart limit"
              ),
              can_go_back: tab.history.can_go_back(),
              can_go_forward: tab.history.can_go_forward(),
            },
            WorkerToUi::LoadingState {
              tab_id,
              loading: false,
            },
          ],
        });
      }

      // Keep the provisional history entry in sync with the final URL so the restarted navigation
      // can commit it in-place.
      tab.history.replace_current_url(committed_url.clone());

      let mut restart_request = request_for_retry;
      restart_request.request.url = committed_url;
      restart_request.apply_fragment_scroll = apply_fragment_scroll;
      tab.pending_navigation = Some(restart_request);

      // Keep loading state; do not emit NavigationCommitted/Failed until the site mismatch is
      // resolved.
      tab.loading = true;

      return None;
    }

    // Keep the document URL hint stable for `:target` evaluation and relative URL resolution.
    doc.set_navigation_urls(Some(committed_url.clone()), base_url.clone());
    doc.set_document_url_without_invalidation(Some(committed_url.clone()));

    // Compute initial scroll state (including fragment navigations like `#target`).
    let mut scroll_state = ScrollState::with_viewport(Point::new(
      initial_scroll.map(|(x, _)| x).unwrap_or(0.0),
      initial_scroll.map(|(_, y)| y).unwrap_or(0.0),
    ));
    if apply_fragment_scroll {
      if let Some(fragment) = url_fragment(&committed_url) {
        let offset = if fragment.is_empty() {
          Some(Point::ZERO)
        } else {
          // `scroll_offset_for_fragment_target` percent-decodes internally; do not pre-decode.
          doc.prepared().and_then(|prepared| {
            scroll_offset_for_fragment_target(
              prepared.dom(),
              prepared.box_tree(),
              prepared.fragment_tree(),
              fragment,
              prepared.layout_viewport(),
            )
          })
        };
        if let Some(offset) = offset {
          let prev_scroll = scroll_state.clone();
          scroll_state.viewport = offset;
          scroll_state.update_deltas_from(&prev_scroll);
        }
      }
    }
    doc.set_scroll_state(scroll_state.clone());

    // -----------------------------
    // Initial interaction state (autofocus)
    // -----------------------------
    //
    // The browser UI always provides an interaction state for rendering, so static-render
    // autofocus synthesis (which only runs when `interaction_state` is None) won't apply here.
    // Instead, we proactively focus the first eligible `[autofocus]` element before the first
    // paint so `:focus`/`:focus-visible` styles and caret/selection painting are visible
    // immediately after navigation commits.
    let mut interaction = InteractionEngine::new();
    let autofocus_target = crate::interaction::autofocus::autofocus_target_node_id(doc.dom());
    if let Some(target_id) = autofocus_target {
      // `InteractionEngine::focus_node_id` does not mutate the DOM; avoid invalidating the cached
      // layout from navigation preparation.
      doc.mutate_dom(|dom| {
        let _ = interaction.focus_node_id(dom, Some(target_id), true);
        false
      });

      // Scroll to reveal the autofocus target (best-effort).
      if let Some(prepared) = doc.prepared() {
        if let Some(next_scroll) = crate::interaction::focus_scroll::scroll_state_for_focus(
          prepared.box_tree(),
          prepared.fragment_tree(),
          &scroll_state,
          target_id,
        ) {
          scroll_state = next_scroll;
          doc.set_scroll_state(scroll_state.clone());
        }
      }
    }

    // -----------------------------
    // Initial visited-link state (`:visited`)
    // -----------------------------
    //
    // Populate `InteractionState.visited_links` for the newly loaded document by scanning all
    // `<a href>` / `<area href>` elements and matching their resolved URLs against the per-tab
    // visited URL store.
    //
    // This keeps visited styling internal (no DOM mutations) while allowing visited state to
    // persist across back/forward navigations within a tab.
    let base_for_links = base_url
      .as_deref()
      .unwrap_or_else(|| committed_url.as_str());
    if let Some(tab) = self.tabs.get(&tab_id) {
      let visited_links =
        visited_link_node_ids_for_dom(doc.dom(), base_for_links, &tab.visited_urls);
      interaction.set_visited_links(visited_links);
    }

    // -----------------------------
    // Initial paint stage
    // -----------------------------
    let paint_cancel_callback = combine_cancel_callbacks(
      paint_snapshot.cancel_callback_for_paint(&cancel),
      preempt_cancel_callback.clone(),
    );
    let paint_deadline = deadline_for(paint_cancel_callback.clone(), None);

    let painted = {
      let _guard = forward_stage_heartbeats(tab_id, self.ui_tx.clone());
      let interaction_state = (autofocus_target.is_some()
        || !interaction.interaction_state().visited_links.is_empty())
      .then(|| interaction.interaction_state());
      if let Some(interaction_state) = interaction_state {
        match doc.render_if_needed_with_deadlines_and_interaction_state(
          Some(&paint_deadline),
          Some(interaction_state),
        ) {
          Ok(Some(frame)) => Ok(Some(frame)),
          Ok(None) => doc
            .render_frame_with_deadlines_and_interaction_state(
              Some(&paint_deadline),
              Some(interaction_state),
            )
            .map(Some),
          Err(err) => Err(err),
        }
      } else {
        match doc.render_if_needed_with_deadlines(Some(&paint_deadline)) {
          Ok(Some(frame)) => Ok(Some(frame)),
          Ok(None) => doc
            .render_frame_with_deadlines(Some(&paint_deadline))
            .map(Some),
          Err(err) => Err(err),
        }
      }
    };

    let mut msgs = Vec::new();

    let painted = match painted {
      Ok(Some(frame)) => Some(frame),
      Ok(None) => None,
      Err(err) => {
        // If a new navigation was initiated while we were painting, drop this result silently.
        if !snapshot.is_still_current_for_prepare(&cancel) {
          if let Some(fallback) = fallback_doc {
            let _ = self.reinsert_document(tab_id, fallback);
          } else {
            let _ = self.reinsert_document(tab_id, doc);
          }
          return None;
        }

        // If only paint was bumped (e.g. scroll/viewport change) while the initial paint was
        // in-flight, treat this as a cancelled paint rather than a navigation failure.
        if paint_cancel_callback() || !paint_snapshot.is_still_current_for_paint(&cancel) {
          let runtime_toggles = Arc::clone(&self.runtime_toggles);
          let Some(tab) = self.tabs.get_mut(&tab_id) else {
            return None;
          };
          tab.scroll_state = scroll_state.clone();
          tab
            .history
            .update_scroll(tab.scroll_state.viewport.x, tab.scroll_state.viewport.y);
          tab.document = Some(doc);
          tab.interaction = interaction;
          tab.tick_time = Duration::ZERO;
          tab.last_committed_url = Some(committed_url.clone());
          tab.last_base_url = base_url.clone();
          tab.site_key = Some(site_key_for_navigation(&committed_url, None));
          tab.site_mismatch_restarts = 0;

          Self::sync_js_tab_for_committed_navigation(
            &runtime_toggles,
            tab_id,
            tab,
            &committed_url,
            viewport_css,
            dpr,
            options.timeout,
            options.cancel_callback.clone(),
            self.debug_log_enabled,
            &mut msgs,
          );

          let _ = Self::pump_js_once_and_sync_dom_after_committed_navigation(tab_id, tab, &mut msgs);

          let _ = tab
            .history
            .commit_navigation(&original_url, Some(committed_url.as_str()));
          let title = tab
            .document
            .as_ref()
            .and_then(|doc| find_document_title(doc.dom()));
          if let Some(title) = title.as_deref() {
            tab.history.set_title(title.to_string());
          }

          msgs.push(WorkerToUi::NavigationCommitted {
            tab_id,
            url: committed_url.clone(),
            title: title.clone(),
            can_go_back: tab.history.can_go_back(),
            can_go_forward: tab.history.can_go_forward(),
          });

          tab.loading = false;
          tab.pending_history_entry = false;
          if tab.scroll_state != tab.last_reported_scroll_state {
            msgs.push(WorkerToUi::ScrollStateUpdated {
              tab_id,
              scroll: tab.scroll_state.clone(),
            });
          }
          msgs.push(WorkerToUi::LoadingState {
            tab_id,
            loading: false,
          });

          // Ensure the next loop iteration paints with the latest `CancelGens` snapshot (and any
          // queued scroll/viewport updates).
          tab.needs_repaint = true;

          return Some(JobOutput {
            tab_id,
            snapshot,
            snapshot_kind: SnapshotKind::Prepare,
            msgs,
          });
        }

        let _ = self.reinsert_document(tab_id, doc);
        return self.run_navigation_error(
          tab_id,
          &original_url,
          &format!("paint failed: {err}"),
          snapshot,
        );
      }
    };

    // If a new navigation was initiated while we were painting, drop the result.
    if !snapshot.is_still_current_for_prepare(&cancel) {
      if let Some(fallback) = fallback_doc {
        let _ = self.reinsert_document(tab_id, fallback);
      } else {
        let _ = self.reinsert_document(tab_id, doc);
      }
      return None;
    }

    // -----------------------------
    // Favicon discovery/fetch
    // -----------------------------
    //
    // Do this before committing navigation state/history so a nav-bump during favicon fetch does
    // not leave behind a committed-but-never-reported history entry.
    let favicon = discover_favicon_url(&doc, &committed_url, base_url.as_deref()).and_then(|url| {
      let cancel_callback = snapshot.cancel_callback_for_prepare(&cancel);
      let deadline = deadline_for(cancel_callback, None);
      let _guard = DeadlineGuard::install(Some(&deadline));
      let loaded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        load_favicon_rgba_from_image_cache(doc.image_cache(), &url)
      }))
      .ok()
      .flatten();
      loaded.and_then(|(rgba, width, height)| {
        // Defensive: ensure the payload remains bounded.
        let expected_len = (width as usize)
          .saturating_mul(height as usize)
          .saturating_mul(4);
        if width == 0
          || height == 0
          || width > MAX_FAVICON_EDGE_PX
          || height > MAX_FAVICON_EDGE_PX
          || rgba.len() != expected_len
          || rgba.len() > MAX_FAVICON_BYTES
        {
          return None;
        }
        Some(WorkerToUi::Favicon {
          tab_id,
          rgba,
          width,
          height,
        })
      })
    });

    // If a new navigation was initiated while we were fetching the favicon, drop the result.
    if !snapshot.is_still_current_for_prepare(&cancel) {
      if let Some(fallback) = fallback_doc {
        let _ = self.reinsert_document(tab_id, fallback);
      } else {
        let _ = self.reinsert_document(tab_id, doc);
      }
      return None;
    }

    let runtime_toggles = Arc::clone(&self.runtime_toggles);
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return None;
    };

    // Commit navigation state.
    match &painted {
      Some(frame) => {
        tab.scroll_state = frame.scroll_state.clone();
      }
      None => {
        tab.scroll_state = scroll_state.clone();
      }
    }
    tab
      .history
      .update_scroll(tab.scroll_state.viewport.x, tab.scroll_state.viewport.y);
    tab.document = Some(doc);
    tab.interaction = interaction;
    tab.tick_time = Duration::ZERO;
    tab.last_committed_url = Some(committed_url.clone());
    tab.last_base_url = base_url.clone();
    tab.site_key = Some(site_key_for_navigation(&committed_url, None));
    tab.site_mismatch_restarts = 0;

    Self::sync_js_tab_for_committed_navigation(
      &runtime_toggles,
      tab_id,
      tab,
      &committed_url,
      viewport_css,
      dpr,
      options.timeout,
      options.cancel_callback.clone(),
      self.debug_log_enabled,
      &mut msgs,
    );

    let js_dom_changed =
      Self::pump_js_once_and_sync_dom_after_committed_navigation(tab_id, tab, &mut msgs);

    let _ = tab
      .history
      .commit_navigation(&original_url, Some(committed_url.as_str()));
    let title = tab
      .document
      .as_ref()
      .and_then(|doc| find_document_title(doc.dom()));
    if let Some(title) = title.as_deref() {
      tab.history.set_title(title.to_string());
    }

    msgs.push(WorkerToUi::NavigationCommitted {
      tab_id,
      url: committed_url.clone(),
      title: title.clone(),
      can_go_back: tab.history.can_go_back(),
      can_go_forward: tab.history.can_go_forward(),
    });

    if let Some(msg) = favicon {
      msgs.push(msg);
    }

    // Only emit FrameReady when the paint snapshot is still current. If the UI bumped paint while
    // we were rendering, skip this frame and let the subsequent repaint win.
    let mut emitted_frame = false;
    if let Some(frame) = painted {
      if paint_snapshot.is_still_current_for_paint(&cancel) && !js_dom_changed {
        let actual_dpr = tab
          .document
          .as_ref()
          .and_then(|d| d.prepared())
          .map(|p| p.device_pixel_ratio())
          .unwrap_or(dpr);

        let mut pixmap = frame.pixmap;

        if !tab.find.query.is_empty() {
          let prev_count = tab.find.matches.len();
          let prev_active = tab.find.active_match_index;
          if let Some(doc) = tab.document.as_ref() {
            Self::rebuild_find_matches(&mut tab.find, &tab.scroll_state, doc);
          } else {
            tab.find.matches.clear();
            tab.find.active_match_index = None;
          }
          if tab.find.matches.len() != prev_count || tab.find.active_match_index != prev_active {
            msgs.push(WorkerToUi::FindResult {
              tab_id,
              query: tab.find.query.clone(),
              case_sensitive: tab.find.case_sensitive,
              match_count: tab.find.matches.len(),
              active_match_index: tab.find.active_match_index,
            });
          }
          Self::apply_find_highlight(tab, actual_dpr, &mut pixmap);
        }

        msgs.push(WorkerToUi::FrameReady {
          tab_id,
          frame: RenderedFrame {
            pixmap,
            viewport_css,
            dpr: actual_dpr,
            scroll_state: tab.scroll_state.clone(),
            scroll_metrics: compute_scroll_metrics(
              tab.document.as_ref(),
              viewport_css,
              &tab.scroll_state,
            ),
            next_tick: (tab.document.as_ref().is_some_and(document_wants_ticks) || tab.js_tab.is_some())
              .then_some(DEFAULT_TICK_INTERVAL),
          },
        });
        if let Some(doc) = tab.document.as_ref() {
          if let Some((tree, bounds_css)) =
            compute_page_accessibility_snapshot(doc, &tab.interaction, &tab.scroll_state)
          {
            msgs.push(WorkerToUi::PageAccessibility {
              tab_id,
              tree,
              bounds_css,
            });
          }
        }
        emitted_frame = true;
      } else {
        tab.needs_repaint = true;
      }
    } else {
      tab.needs_repaint = true;
    }

    if !emitted_frame && tab.scroll_state != tab.last_reported_scroll_state {
      msgs.push(WorkerToUi::ScrollStateUpdated {
        tab_id,
        scroll: tab.scroll_state.clone(),
      });
    }

    tab.loading = false;
    tab.pending_history_entry = false;
    msgs.push(WorkerToUi::LoadingState {
      tab_id,
      loading: false,
    });

    Some(JobOutput {
      tab_id,
      snapshot,
      snapshot_kind: SnapshotKind::Prepare,
      msgs,
    })
  }

  fn run_navigation_error(
    &mut self,
    tab_id: TabId,
    original_url: &str,
    error: &str,
    snapshot: CancelSnapshot,
  ) -> Option<JobOutput> {
    let (viewport_css, dpr, cancel, can_go_back, can_go_forward) = match self.tabs.get(&tab_id) {
      Some(tab) => (
        tab.viewport_css,
        tab.dpr,
        tab.cancel.clone(),
        tab.history.can_go_back(),
        tab.history.can_go_forward(),
      ),
      None => return None,
    };

    // If the user initiated a new navigation while we were failing, treat this as cancelled.
    if !snapshot.is_still_current_for_prepare(&cancel) {
      return None;
    }

    let cancel_callback = snapshot.cancel_callback_for_prepare(&cancel);

    let html = about_pages::error_page_html("Navigation failed", error, Some(original_url));
    let mut options = RenderOptions::default()
      .with_viewport(viewport_css.0, viewport_css.1)
      .with_device_pixel_ratio(dpr);
    options.runtime_toggles = Some(Arc::clone(&self.runtime_toggles));
    options.cancel_callback = Some(cancel_callback.clone());

    // Lazily create the long-lived document/renderer if we don't have one yet.
    let needs_doc = self
      .tabs
      .get(&tab_id)
      .is_some_and(|tab| tab.document.is_none());
    if needs_doc {
      match self.build_initial_document(viewport_css, dpr) {
        Ok(doc) => {
          if let Some(tab) = self.tabs.get_mut(&tab_id) {
            tab.tick_time = Duration::ZERO;
            tab.document = Some(doc);
          }
        }
        Err(err) => {
          let Some(tab) = self.tabs.get_mut(&tab_id) else {
            return None;
          };
          tab.loading = false;
          tab.pending_history_entry = false;
          return Some(JobOutput {
            tab_id,
            snapshot,
            snapshot_kind: SnapshotKind::Prepare,
            msgs: vec![
              WorkerToUi::NavigationFailed {
                tab_id,
                url: original_url.to_string(),
                error: format!("{error} (and failed to create renderer: {err})"),
                can_go_back,
                can_go_forward,
              },
              WorkerToUi::LoadingState {
                tab_id,
                loading: false,
              },
            ],
          });
        }
      }
    }

    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return None;
    };

    if !snapshot.is_still_current_for_prepare(&tab.cancel) {
      return None;
    }

    let Some(doc) = tab.document.as_mut() else {
      return None;
    };

    let prepared = {
      let _guard = forward_stage_heartbeats(tab_id, self.ui_tx.clone());
      doc.navigate_html_with_options(
        about_pages::ABOUT_ERROR,
        &html,
        Some(about_pages::ABOUT_BASE_URL),
        options.clone(),
      )
    };
    if let Err(err) = prepared {
      if cancel_callback() {
        return None;
      }
      tab.loading = false;
      tab.pending_history_entry = false;
      tab.history.mark_committed();
      return Some(JobOutput {
        tab_id,
        snapshot,
        snapshot_kind: SnapshotKind::Prepare,
        msgs: vec![
          WorkerToUi::NavigationFailed {
            tab_id,
            url: original_url.to_string(),
            error: format!("{error} (and failed to prepare error page: {err})"),
            can_go_back,
            can_go_forward,
          },
          WorkerToUi::LoadingState {
            tab_id,
            loading: false,
          },
        ],
      });
    }

    // Only cancel the error page render when the navigation itself is superseded (nav bump). We
    // intentionally ignore paint bumps (e.g. scroll) so we still surface a deterministic error.
    doc.set_cancel_callback(Some(cancel_callback.clone()));

    let scroll_state = ScrollState::with_viewport(Point::ZERO);
    doc.set_scroll_state(scroll_state.clone());

    let painted = {
      let _guard = forward_stage_heartbeats(tab_id, self.ui_tx.clone());
      match doc.render_if_needed_with_scroll_state() {
        Ok(Some(frame)) => Ok(Some(frame)),
        Ok(None) => doc.render_frame_with_scroll_state().map(Some),
        Err(err) => Err(err),
      }
    };

    let painted = match painted {
      Ok(Some(frame)) => frame,
      Ok(None) => {
        if cancel_callback() {
          return None;
        }
        tab.loading = false;
        tab.pending_history_entry = false;
        tab.history.mark_committed();
        return Some(JobOutput {
          tab_id,
          snapshot,
          snapshot_kind: SnapshotKind::Prepare,
          msgs: vec![
            WorkerToUi::NavigationFailed {
              tab_id,
              url: original_url.to_string(),
              error: error.to_string(),
              can_go_back,
              can_go_forward,
            },
            WorkerToUi::LoadingState {
              tab_id,
              loading: false,
            },
          ],
        });
      }
      Err(err) => {
        if cancel_callback() {
          return None;
        }
        tab.loading = false;
        tab.pending_history_entry = false;
        tab.history.mark_committed();
        return Some(JobOutput {
          tab_id,
          snapshot,
          snapshot_kind: SnapshotKind::Prepare,
          msgs: vec![
            WorkerToUi::NavigationFailed {
              tab_id,
              url: original_url.to_string(),
              error: format!("{error} (and failed to render error page: {err})"),
              can_go_back,
              can_go_forward,
            },
            WorkerToUi::LoadingState {
              tab_id,
              loading: false,
            },
          ],
        });
      }
    };
    tab.interaction = InteractionEngine::new();
    tab.js_tab = None;
    tab.js_dom_mapping_generation = 0;
    tab.js_dom_mapping = None;
    tab.js_dom_mapping_miss_log_last.clear();
    tab.js_dom_dirty = false;
    tab.js_dom_mutation_generation = 0;
    tab.tick_time = Duration::ZERO;
    tab.scroll_state = painted.scroll_state.clone();
    tab.last_committed_url = Some(about_pages::ABOUT_ERROR.to_string());
    tab.last_base_url = Some(about_pages::ABOUT_BASE_URL.to_string());
    tab.site_key = Some(site_key_for_navigation(about_pages::ABOUT_ERROR, None));
    tab.site_mismatch_restarts = 0;

    tab.loading = false;
    tab.pending_history_entry = false;
    tab.history.mark_committed();

    let page_accessibility = tab
      .document
      .as_ref()
      .and_then(|doc| compute_page_accessibility_snapshot(doc, &tab.interaction, &tab.scroll_state));

    let mut msgs = Vec::new();
    msgs.push(WorkerToUi::NavigationFailed {
      tab_id,
      url: original_url.to_string(),
      error: error.to_string(),
      can_go_back: tab.history.can_go_back(),
      can_go_forward: tab.history.can_go_forward(),
    });
    msgs.push(WorkerToUi::FrameReady {
      tab_id,
      frame: RenderedFrame {
        pixmap: painted.pixmap,
        viewport_css: tab.viewport_css,
        dpr: tab
          .document
          .as_ref()
          .and_then(|d| d.prepared())
          .map(|p| p.device_pixel_ratio())
          .unwrap_or(tab.dpr),
        scroll_state: tab.scroll_state.clone(),
        scroll_metrics: compute_scroll_metrics(tab.document.as_ref(), tab.viewport_css, &tab.scroll_state),
        next_tick: (tab.document.as_ref().is_some_and(document_wants_ticks) || tab.js_tab.is_some())
          .then_some(DEFAULT_TICK_INTERVAL),
      },
    });
    if let Some((tree, bounds_css)) = page_accessibility {
      msgs.push(WorkerToUi::PageAccessibility {
        tab_id,
        tree,
        bounds_css,
      });
    }
    msgs.push(WorkerToUi::LoadingState {
      tab_id,
      loading: false,
    });

    Some(JobOutput {
      tab_id,
      snapshot,
      snapshot_kind: SnapshotKind::Prepare,
      msgs,
    })
  }

  fn run_paint(&mut self, tab_id: TabId, force: bool, is_scroll: bool) -> Option<JobOutput> {
    let preempt_cancel_callback = self.preempt_cancel_callback_for_job(tab_id);
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return None;
    };

    // Keep the renderer's DOM snapshot in sync with the live `dom2` document owned by the JS tab.
    // This lets JS-driven DOM mutations affect subsequent paints (while preserving the existing
    // BrowserDocument configuration like viewport/dpr/scroll).
    let js_dom_generation_changed = tab
      .js_tab
      .as_ref()
      .is_some_and(|js_tab| js_tab.dom().mutation_generation() != tab.js_dom_mutation_generation);
    if tab.js_dom_dirty || js_dom_generation_changed {
      sync_render_dom_from_js_tab(tab_id, tab, &self.ui_tx);
    }
    if tab.document.is_none() {
      return None;
    }

    let snapshot = tab.cancel.snapshot_paint();
    let cancel_callback = combine_cancel_callbacks(
      snapshot.cancel_callback_for_paint(&tab.cancel),
      preempt_cancel_callback.clone(),
    );

    // Forward render pipeline stage heartbeats during paint jobs (including scroll/hover repaints)
    // so UI callers and integration tests can observe progress and deterministically cancel
    // in-flight work.
    let scroll_deadline = if is_scroll {
      self
        .scroll_paint_budget
        .map(|budget| deadline_for(cancel_callback.clone(), Some(budget)))
    } else {
      None
    };
    let painted = {
      let Some(doc) = tab.document.as_mut() else {
        return None;
      };
      doc.set_cancel_callback(Some(cancel_callback.clone()));
      let _guard = forward_stage_heartbeats(tab_id, self.ui_tx.clone());
      let interaction_state = Some(tab.interaction.interaction_state());
      if force {
        if let Some(deadline) = scroll_deadline.as_ref() {
          doc
            .render_frame_with_deadlines_and_interaction_state(Some(deadline), interaction_state)
            .map(Some)
        } else {
          doc
            .render_frame_with_scroll_state_and_interaction_state(interaction_state)
            .map(Some)
        }
      } else {
        if let Some(deadline) = scroll_deadline.as_ref() {
          doc.render_if_needed_with_deadlines_and_interaction_state(
            Some(deadline),
            interaction_state,
          )
        } else {
          doc.render_if_needed_with_scroll_state_and_interaction_state(interaction_state)
        }
      }
    };

    let mut msgs = Vec::new();

    let mut should_retry = false;
    let painted = match painted {
      Ok(Some(frame)) => Some(frame),
      Ok(None) => None,
      Err(err) => {
        let scroll_timeout = scroll_deadline.is_some()
          && matches!(
            &err,
            crate::Error::Render(crate::error::RenderError::Timeout {
              stage: crate::error::RenderStage::Paint,
              ..
            })
          );

        if cancel_callback() || scroll_timeout {
          should_retry = true;
        } else {
          if self.debug_log_enabled {
            msgs.push(WorkerToUi::DebugLog {
              tab_id,
              line: format!("paint error: {err}"),
            });
          }
        }
        None
      }
    };

    if should_retry {
      tab.needs_repaint = true;
      if force {
        tab.force_repaint = true;
      }
    }

    if let Some(frame) = painted {
      tab.scroll_state = frame.scroll_state.clone();
      if let Some(js_tab) = tab.js_tab.as_mut() {
        js_tab.set_scroll_state(tab.scroll_state.clone());
      }
      tab
        .history
        .update_scroll(tab.scroll_state.viewport.x, tab.scroll_state.viewport.y);

      let actual_dpr = tab
        .document
        .as_ref()
        .and_then(|doc| doc.prepared())
        .map(|p| p.device_pixel_ratio())
        .unwrap_or(tab.dpr);

      let mut pixmap = frame.pixmap;

      if !tab.find.query.is_empty() {
        let prev_count = tab.find.matches.len();
        let prev_active = tab.find.active_match_index;
        if let Some(doc) = tab.document.as_ref() {
          Self::rebuild_find_matches(&mut tab.find, &tab.scroll_state, doc);
        } else {
          tab.find.matches.clear();
          tab.find.active_match_index = None;
        }
        if tab.find.matches.len() != prev_count || tab.find.active_match_index != prev_active {
          msgs.push(WorkerToUi::FindResult {
            tab_id,
            query: tab.find.query.clone(),
            case_sensitive: tab.find.case_sensitive,
            match_count: tab.find.matches.len(),
            active_match_index: tab.find.active_match_index,
          });
        }
        Self::apply_find_highlight(tab, actual_dpr, &mut pixmap);
      }

      msgs.push(WorkerToUi::FrameReady {
        tab_id,
        frame: RenderedFrame {
          pixmap,
          viewport_css: tab.viewport_css,
          dpr: actual_dpr,
          scroll_state: tab.scroll_state.clone(),
          scroll_metrics: compute_scroll_metrics(
            tab.document.as_ref(),
            tab.viewport_css,
            &tab.scroll_state,
          ),
          next_tick: (tab.document.as_ref().is_some_and(document_wants_ticks) || tab.js_tab.is_some())
            .then_some(DEFAULT_TICK_INTERVAL),
        },
      });
      if let Some(doc) = tab.document.as_ref() {
        if let Some((tree, bounds_css)) =
          compute_page_accessibility_snapshot(doc, &tab.interaction, &tab.scroll_state)
        {
          msgs.push(WorkerToUi::PageAccessibility {
            tab_id,
            tree,
            bounds_css,
          });
        }
      }
    }

    Some(JobOutput {
      tab_id,
      snapshot,
      snapshot_kind: SnapshotKind::Paint,
      msgs,
    })
  }

  fn build_initial_document(
    &self,
    viewport_css: (u32, u32),
    dpr: f32,
  ) -> crate::Result<BrowserDocument> {
    let mut renderer = self.factory.build_renderer()?;
    #[cfg(feature = "browser_ui")]
    UI_WORKER_RENDERER_BUILD_COUNT.fetch_add(1, Ordering::Relaxed);

    // Ensure a safe base URL hint from the start so subsequent `about:` renders don't accidentally
    // resolve relative URLs against whatever the renderer last navigated to.
    renderer.set_base_url(about_pages::ABOUT_BASE_URL);

    let html = about_pages::html_for_about_url(about_pages::ABOUT_BLANK).unwrap_or_else(|| {
      "<!doctype html><html><head><meta charset=\"utf-8\"></head><body></body></html>".to_string()
    });

    let mut options = RenderOptions::default()
      .with_viewport(viewport_css.0, viewport_css.1)
      .with_device_pixel_ratio(dpr);
    options.runtime_toggles = Some(Arc::clone(&self.runtime_toggles));

    let mut doc = BrowserDocument::new(renderer, &html, options)?;
    doc.set_navigation_urls(
      Some(about_pages::ABOUT_BLANK.to_string()),
      Some(about_pages::ABOUT_BASE_URL.to_string()),
    );
    doc.set_document_url_without_invalidation(Some(about_pages::ABOUT_BLANK.to_string()));
    Ok(doc)
  }

  fn reinsert_document(&mut self, tab_id: TabId, doc: BrowserDocument) -> Option<()> {
    let tab = self.tabs.get_mut(&tab_id)?;
    tab.document = Some(doc);
    Some(())
  }
}

fn default_ui_worker_factory() -> crate::Result<FastRenderFactory> {
  // The browser UI (and its integration tests) should not depend on system-installed fonts. Prefer
  // the bundled font set so navigation/scroll renders remain deterministic and avoid expensive
  // system font database scans under CI.
  let renderer_config = FastRenderConfig::default().with_font_sources(FontConfig::bundled_only());

  // `about:` pages are treated as trusted UI surfaces and are allowed to load shared chrome assets
  // via `chrome://...`. Untrusted pages (http/https/file/...) must not be able to load those
  // internal resources, so we install an origin-gated composite fetcher.
  let base_fetcher: Arc<dyn ResourceFetcher> = if let Some(cache) = renderer_config.resource_cache {
    let policy = renderer_config.resource_policy.clone();
    Arc::new(
      CachingFetcher::with_config(HttpFetcher::new().with_policy(policy.clone()), cache).with_policy(policy),
    )
  } else {
    Arc::new(HttpFetcher::new().with_policy(renderer_config.resource_policy.clone()))
  };
  let fetcher =
    Arc::new(crate::ui::about_pages_fetcher::AboutPagesCompositeFetcher::new(base_fetcher));

  FastRenderFactory::with_config(
    FastRenderPoolConfig::new()
      .with_renderer_config(renderer_config)
      .with_fetcher(fetcher),
  )
}

/// Spawn the headless UI render worker loop.
///
/// This worker consumes [`UiToWorker`] messages and emits [`WorkerToUi`] updates (frames,
/// navigation events, etc). It is intended to be driven by a UI thread/event loop, but it is also
/// usable from tests to exercise end-to-end interaction wiring.
pub fn spawn_ui_worker(name: impl Into<String>) -> crate::Result<UiThreadWorkerHandle> {
  spawn_worker_with_factory_inner(name.into(), None, default_ui_worker_factory()?)
}

/// Spawn a UI worker with an optional per-frame artificial delay (test-only).
pub fn spawn_ui_worker_for_test(
  name: impl Into<String>,
  test_render_delay_ms: Option<u64>,
) -> crate::Result<UiThreadWorkerHandle> {
  spawn_worker_with_factory_inner(
    name.into(),
    test_render_delay_ms,
    default_ui_worker_factory()?,
  )
}

/// Spawn a UI worker using a preconfigured [`FastRenderFactory`].
///
/// Useful for integration tests that need a custom fetcher.
pub fn spawn_ui_worker_with_factory(
  name: impl Into<String>,
  factory: FastRenderFactory,
) -> crate::Result<UiThreadWorkerHandle> {
  spawn_worker_with_factory_inner(name.into(), None, factory)
}

fn spawn_worker_with_factory_inner(
  name: String,
  test_render_delay_ms: Option<u64>,
  factory: FastRenderFactory,
) -> crate::Result<UiThreadWorkerHandle> {
  let (ui_to_worker_tx, ui_to_worker_rx) = std::sync::mpsc::channel::<UiToWorker>();
  let (worker_to_ui_tx, worker_to_ui_rx) = std::sync::mpsc::channel::<WorkerToUi>();

  let router_thread_name = format!("{name}-router");
  let join = std::thread::Builder::new()
    .name(name)
    .stack_size(crate::system::DEFAULT_RENDER_STACK_SIZE)
    .spawn(move || {
      struct TestRenderDelayGuard;

      impl Drop for TestRenderDelayGuard {
        fn drop(&mut self) {
          crate::render_control::set_test_render_delay_ms(None);
        }
      }

      // `set_test_render_delay_ms` is thread-local; ensure it is cleared when the worker exits so
      // integration tests cannot leak configuration across runs (and so the thread is reusable).
      let _delay_guard = test_render_delay_ms.map(|delay| {
        crate::render_control::set_test_render_delay_ms(Some(delay));
        TestRenderDelayGuard
      });

      // Route UI messages through a lightweight forwarder thread so time-sensitive operations
      // (e.g. download cancellation) can be observed even while the main worker is busy rendering.
      let downloads: Arc<Mutex<HashMap<DownloadId, ActiveDownload>>> =
        Arc::new(Mutex::new(HashMap::new()));
      let (runtime_tx, runtime_rx) = std::sync::mpsc::channel::<UiToWorker>();

      let router_downloads = Arc::clone(&downloads);
      let router_join = std::thread::Builder::new()
        .name(router_thread_name)
        .spawn(move || {
          use std::sync::mpsc::RecvTimeoutError;
          use std::time::{Duration, Instant};

          // Keep this short: the router thread should remain mostly blocking, and we don't want to
          // add noticeable latency to UI input when the runtime thread is idle. The goal is to
          // bound the number of messages enqueued into the unbounded runtime channel while the
          // runtime is busy.
          const COALESCE_WINDOW: Duration = Duration::from_millis(4);

          let mut coalescer = UiToWorkerRouterCoalescer::new();
          let mut deadline: Option<Instant> = None;

          loop {
            let recv = if coalescer.has_pending() {
              let now = Instant::now();
              let d = deadline.get_or_insert_with(|| now + COALESCE_WINDOW);
              if now >= *d {
                // Periodic flush: ensure liveness even under a continuous stream of coalescible
                // messages.
                let out = coalescer.flush();
                deadline = None;
                for msg in out {
                  if runtime_tx.send(msg).is_err() {
                    return;
                  }
                }
                continue;
              }
              ui_to_worker_rx.recv_timeout(d.saturating_duration_since(now))
            } else {
              deadline = None;
              match ui_to_worker_rx.recv() {
                Ok(msg) => Ok(msg),
                Err(_) => break,
              }
            };

            match recv {
              Ok(msg) => {
                // Apply cancellation immediately so it can interrupt long-running downloads even
                // while the render loop is busy with prepare/paint work.
                if let UiToWorker::CancelDownload { download_id, .. } = &msg {
                  let downloads = router_downloads.lock().unwrap_or_else(|err| err.into_inner());
                  if let Some(download) = downloads.get(download_id) {
                    download.cancel.store(true, Ordering::Release);
                  }
                }

                let out = coalescer.push(msg);
                if !out.is_empty() {
                  // A barrier (or a forced flush) resets the coalescing window.
                  deadline = None;
                  for msg in out {
                    if runtime_tx.send(msg).is_err() {
                      return;
                    }
                  }
                }
              }
              Err(RecvTimeoutError::Timeout) => {
                let out = coalescer.flush();
                deadline = None;
                for msg in out {
                  if runtime_tx.send(msg).is_err() {
                    return;
                  }
                }
              }
              Err(RecvTimeoutError::Disconnected) => {
                // UI sender was dropped. Flush any pending coalesced state best-effort, then
                // terminate so the runtime can observe channel closure.
                let out = coalescer.flush();
                for msg in out {
                  let _ = runtime_tx.send(msg);
                }
                break;
              }
            }
          }

          // Best-effort final flush (ignore errors: runtime might have exited).
          for msg in coalescer.flush() {
            let _ = runtime_tx.send(msg);
          }
        })
        .expect("spawn UI worker router thread");

      let _runtime_toggles_guard =
        crate::debug::runtime::set_thread_runtime_toggles(factory.runtime_toggles());
      let mut runtime = BrowserRuntime::new(runtime_rx, worker_to_ui_tx, factory, downloads);
      runtime.run();

      let _ = router_join.join();
    })?;

  Ok(UiThreadWorkerHandle {
    ui_tx: ui_to_worker_tx,
    ui_rx: worker_to_ui_rx,
    join,
  })
}

/// Spawn the browser worker thread.
///
/// The returned handle can be used from a headless caller (no winit/egui required).
pub fn spawn_browser_worker() -> crate::Result<BrowserWorkerHandle> {
  spawn_browser_worker_with_name("browser_worker")
}

/// Spawn the browser worker thread with an explicit thread name.
///
/// Keeping a named entrypoint allows the desktop `browser` binary to name its worker thread
/// (`fastr-browser-ui-worker`), while preserving a stable default name for tests that don't care.
pub fn spawn_browser_worker_with_name(
  name: impl Into<String>,
) -> crate::Result<BrowserWorkerHandle> {
  let handle = spawn_worker_with_factory_inner(name.into(), None, default_ui_worker_factory()?)?;
  Ok(BrowserWorkerHandle {
    tx: handle.ui_tx,
    rx: handle.ui_rx,
    join: handle.join,
  })
}

/// Like [`spawn_browser_worker`], but allows callers (tests) to provide a preconfigured renderer
/// factory.
pub fn spawn_browser_worker_with_factory(
  factory: FastRenderFactory,
) -> crate::Result<BrowserWorkerHandle> {
  let handle = spawn_worker_with_factory_inner("browser_worker".to_string(), None, factory)?;
  Ok(BrowserWorkerHandle {
    tx: handle.ui_tx,
    rx: handle.ui_rx,
    join: handle.join,
  })
}

#[cfg(any(test, feature = "browser_ui"))]
pub fn spawn_browser_worker_for_test(
  test_render_delay_ms: Option<u64>,
) -> crate::Result<BrowserWorkerHandle> {
  let handle = spawn_worker_with_factory_inner(
    "browser_worker".to_string(),
    test_render_delay_ms,
    default_ui_worker_factory()?,
  )?;
  Ok(BrowserWorkerHandle {
    tx: handle.ui_tx,
    rx: handle.ui_rx,
    join: handle.join,
  })
}

/// Spawn the production browser UI worker with a std::io-compatible API.
///
/// The desktop `browser` binary is written around `std::io::Result`, so this wrapper converts
/// FastRender's internal `Error` into an `io::Error` and returns the raw channel endpoints.
pub fn spawn_browser_ui_worker(
  name: impl Into<String>,
) -> std::io::Result<(
  std::sync::mpsc::Sender<UiToWorker>,
  std::sync::mpsc::Receiver<WorkerToUi>,
  std::thread::JoinHandle<()>,
)> {
  // Test hook used by the `browser --headless-smoke` integration harness to simulate the renderer
  // worker being unavailable. When this env var is set, callers that *require* the worker should
  // fail fast, while trusted in-process `about:` rendering paths should continue to work.
  if std::env::var_os("FASTR_TEST_BROWSER_HEADLESS_SMOKE_DISABLE_WORKER")
    .is_some_and(|v| !v.is_empty())
  {
    return Err(std::io::Error::new(
      std::io::ErrorKind::Other,
      "renderer worker disabled by FASTR_TEST_BROWSER_HEADLESS_SMOKE_DISABLE_WORKER",
    ));
  }

  let handle = spawn_browser_worker_with_name(name)
    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
  Ok((handle.tx, handle.rx, handle.join))
}

/// Convenience wrapper for browser integration tests.
///
/// Unlike [`spawn_ui_worker`], this returns a [`BrowserWorkerHandle`] (field names `tx`/`rx`) and
/// takes `&str` for the worker name.
pub fn spawn_test_browser_worker(name: &str) -> crate::Result<BrowserWorkerHandle> {
  spawn_browser_worker_with_name(name.to_string())
}

#[cfg(test)]
mod download_progress_tests {
  use super::*;

  #[test]
  fn download_progress_is_throttled_by_time() {
    let received = DOWNLOAD_PROGRESS_MIN_BYTES * 8;

    // Even if a lot of data arrives, we should not emit progress updates more frequently than the
    // time-based throttle.
    assert!(!should_emit_download_progress(
      received,
      0,
      DOWNLOAD_PROGRESS_MIN_INTERVAL - Duration::from_millis(1),
      false,
    ));

    assert!(should_emit_download_progress(
      received,
      0,
      DOWNLOAD_PROGRESS_MIN_INTERVAL,
      false,
    ));

    // After we "emit", the next update should again be suppressed until the interval elapses.
    let received2 = received + DOWNLOAD_PROGRESS_MIN_BYTES * 8;
    assert!(!should_emit_download_progress(
      received2,
      received,
      Duration::from_millis(1),
      false,
    ));
  }

  #[test]
  fn download_progress_forces_final_update() {
    // Final update must bypass throttling.
    assert!(should_emit_download_progress(123, 0, Duration::ZERO, true));
  }
}

#[cfg(test)]
mod base_url_tests {
  #[test]
  fn render_worker_does_not_to_string_base_url_for_links() {
    // Regression test: pointer-move / context-menu paths are hot and should not allocate an owned
    // `String` for the base URL. Keep `base_url_for_links(...)` borrowed and pass `&str` downstream.
    //
    // (We scan the source rather than counting allocations because these paths already perform
    // unrelated allocations during hit-testing and interaction bookkeeping.)
    let src = include_str!("render_worker.rs");
    let re = regex::Regex::new(r"(?s)base_url_for_links\(.*?\)\s*\.\s*to_string\(").expect("regex");
    assert!(
      !re.is_match(src),
      "render_worker.rs should not call `.to_string()` on base_url_for_links(...)"
    );

    let re = regex::Regex::new(r"(?s)base_url_for_links\(.*?\)\s*\.\s*to_owned\(").expect("regex");
    assert!(
      !re.is_match(src),
      "render_worker.rs should not call `.to_owned()` on base_url_for_links(...)"
    );
  }
}

#[cfg(test)]
mod drain_messages_viewport_coalescing_tests {
  use super::*;

  #[test]
  fn drain_messages_coalesces_viewport_changed_per_tab() -> crate::Result<()> {
    let (ui_tx, ui_rx) = std::sync::mpsc::channel::<UiToWorker>();
    let (worker_tx, _worker_rx) = std::sync::mpsc::channel::<WorkerToUi>();

    let factory = default_ui_worker_factory()?;
    let downloads: Arc<Mutex<HashMap<DownloadId, ActiveDownload>>> =
      Arc::new(Mutex::new(HashMap::new()));
    let mut runtime = BrowserRuntime::new(ui_rx, worker_tx, factory, downloads);

    let tab_id = TabId::new();
    ui_tx
      .send(UiToWorker::CreateTab {
        tab_id,
        initial_url: None,
        cancel: CancelGens::new(),
      })
      .unwrap();

    ui_tx
      .send(UiToWorker::ViewportChanged {
        tab_id,
        viewport_css: (100, 80),
        dpr: 1.0,
      })
      .unwrap();
    ui_tx
      .send(UiToWorker::ViewportChanged {
        tab_id,
        viewport_css: (200, 160),
        dpr: 1.5,
      })
      .unwrap();
    ui_tx
      .send(UiToWorker::ViewportChanged {
        tab_id,
        viewport_css: (300, 240),
        dpr: 2.0,
      })
      .unwrap();

    // A non-coalescable message should force pending viewport updates to be applied before it is
    // handled.
    ui_tx
      .send(UiToWorker::RequestRepaint {
        tab_id,
        reason: crate::ui::messages::RepaintReason::Explicit,
      })
      .unwrap();

    runtime.drain_messages();

    assert_eq!(
      runtime.viewport_changed_handled_for_test, 1,
      "expected ViewportChanged messages to be coalesced per tab"
    );

    let tab = runtime.tabs.get(&tab_id).expect("tab state");
    assert_eq!(tab.viewport_css, (300, 240));
    assert!((tab.dpr - 2.0).abs() < 1e-6);

    Ok(())
  }
}

#[cfg(test)]
mod js_tab_navigation_deadlines_tests {
  use super::*;

  #[test]
  fn js_tab_navigation_cancels_without_logging_when_cancel_callback_trips() {
    let runtime_toggles = crate::debug::runtime::runtime_toggles();
    let tab_id = TabId::new();

    let mut tab = TabState::new(CancelGens::new());
    tab.document = Some(
      BrowserDocument::from_html(
        "<!doctype html><html><body>ok</body></html>",
        RenderOptions::default()
          .with_viewport(1, 1)
          .with_device_pixel_ratio(1.0),
      )
      .expect("create BrowserDocument"),
    );

    let cancel_callback: Arc<crate::render_control::CancelCallback> = Arc::new(|| true);

    let mut msgs = Vec::new();
    BrowserRuntime::sync_js_tab_for_committed_navigation(
      &runtime_toggles,
      tab_id,
      &mut tab,
      "data:text/html,ok",
      (1, 1),
      1.0,
      None,
      Some(cancel_callback),
      true,
      &mut msgs,
    );

    assert!(
      tab.js_tab.is_none(),
      "expected js_tab to remain unset when cancellation triggers"
    );
    assert!(
      msgs.is_empty(),
      "expected cancellation to be silent (no DebugLog), got: {msgs:?}"
    );
  }

  #[test]
  fn js_tab_navigation_timeout_is_plumbed_and_logged() {
    let runtime_toggles = crate::debug::runtime::runtime_toggles();
    let tab_id = TabId::new();

    let mut tab = TabState::new(CancelGens::new());
    tab.document = Some(
      BrowserDocument::from_html(
        "<!doctype html><html><body>ok</body></html>",
        RenderOptions::default()
          .with_viewport(1, 1)
          .with_device_pixel_ratio(1.0),
      )
      .expect("create BrowserDocument"),
    );

    let mut msgs = Vec::new();
    BrowserRuntime::sync_js_tab_for_committed_navigation(
      &runtime_toggles,
      tab_id,
      &mut tab,
      "data:text/html,ok",
      (1, 1),
      1.0,
      Some(std::time::Duration::from_millis(0)),
      None,
      true,
      &mut msgs,
    );

    assert!(
      tab.js_tab.is_none(),
      "expected js_tab to remain unset when timeout triggers"
    );
    let saw_timeout_log = msgs.iter().any(|msg| match msg {
      WorkerToUi::DebugLog { tab_id: got, line } => {
        *got == tab_id
          && (line.contains("js tab init") || line.contains("js tab navigation"))
          && line.contains("timed out")
      }
      _ => false,
    });
    assert!(
      saw_timeout_log,
      "expected a DebugLog describing the timeout; got: {msgs:?}"
    );
  }
}
