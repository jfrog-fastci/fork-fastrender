//! Canonical message-driven browser UI worker.
//!
//! This module contains the single production implementation of the UI↔worker protocol used by the
//! windowed `browser` app (`src/bin/browser.rs`) and the browser UI integration tests. It owns
//! per-tab state (document, interaction engine, history, cancellation) and renders on a dedicated
//! large-stack thread.

use crate::api::{BrowserDocument, FastRenderConfig, FastRenderFactory, FastRenderPoolConfig, RenderOptions};
use crate::geometry::{Point, Rect};
use crate::html::find_document_title;
use crate::interaction::anchor_scroll::scroll_offset_for_fragment_target;
use crate::interaction::{dom_mutation, fragment_tree_with_scroll, InteractionAction, InteractionEngine};
use crate::render_control::{push_stage_listener, StageHeartbeat, StageListenerGuard};
use crate::scroll::ScrollState;
use crate::text::font_db::FontConfig;
use crate::ui::about_pages;
use crate::ui::cancel::{deadline_for, CancelGens, CancelSnapshot};
use crate::ui::history::TabHistory;
use crate::ui::messages::{
  NavigationReason, PointerButton, RenderedFrame, TabId, UiToWorker, WorkerToUi,
};
use crate::ui::validate_user_navigation_url_scheme;
use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
#[cfg(feature = "browser_ui")]
use std::sync::atomic::{AtomicUsize, Ordering};

// -----------------------------------------------------------------------------
// Test hooks
// -----------------------------------------------------------------------------

/// Global counter for how many `FastRender` instances were built by the UI worker.
///
/// This is a lightweight integration-test hook used to assert that tabs reuse a single renderer
/// across navigations (instead of rebuilding one per navigation).
#[cfg(feature = "browser_ui")]
static UI_WORKER_RENDERER_BUILD_COUNT: AtomicUsize = AtomicUsize::new(0);

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
  pub fn split(self) -> (Sender<UiToWorker>, Receiver<WorkerToUi>, std::thread::JoinHandle<()>) {
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
  url: String,
  apply_fragment_scroll: bool,
}

// `UiToWorker::Tick` is the UI's periodic driver for time-based updates (CSS animations/transitions,
// and eventually JS timers). The UI does not provide a timestamp, so we advance a fixed amount of
// time per tick to keep behaviour deterministic for tests.
const TICK_ANIMATION_STEP_MS: f32 = 16.0;

struct TabState {
  history: TabHistory,
  loading: bool,
  pending_history_entry: bool,
  viewport_css: (u32, u32),
  dpr: f32,
  scroll_state: ScrollState,
  /// True when the next paint was triggered by a scroll message and we should coalesce any
  /// immediately-following scroll events before rendering.
  scroll_coalesce: bool,
  document: Option<BrowserDocument>,
  interaction: InteractionEngine,
  cancel: CancelGens,
  last_committed_url: Option<String>,
  last_base_url: Option<String>,

  pending_navigation: Option<NavigationRequest>,
  needs_repaint: bool,
  force_repaint: bool,

  tick_animation_time_ms: f32,
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
      scroll_coalesce: false,
      document: None,
      interaction: InteractionEngine::new(),
      cancel,
      last_committed_url: None,
      last_base_url: None,
      pending_navigation: None,
      needs_repaint: false,
      force_repaint: false,
      tick_animation_time_ms: 0.0,
    }
  }
}

fn forward_stage_heartbeats(tab_id: TabId, sender: Sender<WorkerToUi>) -> StageListenerGuard {
  let listener = Arc::new(move |stage: StageHeartbeat| {
    // Best-effort: UI might have dropped its receiver.
    let _ = sender.send(WorkerToUi::Stage { tab_id, stage });
  });
  push_stage_listener(Some(listener))
}

fn clamp_viewport((w, h): (u32, u32)) -> (u32, u32) {
  (w.max(1), h.max(1))
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
    let sx = if scroll.viewport.x.is_finite() { scroll.viewport.x } else { 0.0 };
    let sy = if scroll.viewport.y.is_finite() { scroll.viewport.y } else { 0.0 };
    Point::new(-sx - 1.0, -sy - 1.0)
  }
}

fn base_url_for_links(tab: &TabState) -> &str {
  tab
    .last_base_url
    .as_deref()
    .or(tab.last_committed_url.as_deref())
    .unwrap_or(about_pages::ABOUT_BASE_URL)
}

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

fn select_anchor_css(
  box_tree: &crate::BoxTree,
  fragment_tree: &crate::FragmentTree,
  scroll_state: &ScrollState,
  select_node_id: usize,
) -> Option<Rect> {
  // BoxTree: find the first box produced by the `<select>` element.
  let select_box_id = {
    let mut stack: Vec<&crate::BoxNode> = vec![&box_tree.root];
    let mut found = None;
    while let Some(node) = stack.pop() {
      if node.styled_node_id == Some(select_node_id) {
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

  // FragmentTree: compute absolute page-space bounds for the select's box.
  let mut fragment_tree_scrolled = fragment_tree.clone();
  crate::scroll::apply_scroll_offsets(&mut fragment_tree_scrolled, scroll_state);
  let page_rect =
    crate::interaction::absolute_bounds_for_box_id(&fragment_tree_scrolled, select_box_id)?;

  // Convert page-space bounds to viewport-local coords for UI positioning.
  Some(page_rect.translate(Point::new(
    -scroll_state.viewport.x,
    -scroll_state.viewport.y,
  )))
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

struct BrowserRuntime {
  ui_rx: Receiver<UiToWorker>,
  ui_tx: Sender<WorkerToUi>,
  factory: FastRenderFactory,
  tabs: HashMap<TabId, TabState>,
  active_tab: Option<TabId>,
  /// Messages deferred during scroll coalescing that should be handled before blocking for the next
  /// message.
  deferred_msgs: VecDeque<UiToWorker>,
}

impl BrowserRuntime {
  fn new(ui_rx: Receiver<UiToWorker>, ui_tx: Sender<WorkerToUi>, factory: FastRenderFactory) -> Self {
    Self {
      ui_rx,
      ui_tx,
      factory,
      tabs: HashMap::new(),
      active_tab: None,
      deferred_msgs: VecDeque::new(),
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
      // produces a frame (see `worker_runtime::cancellation_rapid_scroll_coalesces_to_last_frame`).
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
        continue;
      }

      for msg in output.msgs {
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
    // Coalesce pointer-move bursts so we only do one hit-test per tab before the next paint job.
    //
    // Pointer-move can arrive at a very high frequency (especially with high polling-rate mice).
    // The renderer only needs the *latest* pointer position before repainting, so collapsing
    // back-to-back moves avoids redundant DOM hit-testing work.
    let mut pending_pointer_moves: HashMap<TabId, ((f32, f32), PointerButton)> = HashMap::new();

    while let Some(msg) = self.try_recv_message() {
      match msg {
        UiToWorker::PointerMove {
          tab_id,
          pos_css,
          button,
        } => {
          pending_pointer_moves.insert(tab_id, (pos_css, button));
        }
        other => {
          for (tab_id, (pos_css, button)) in pending_pointer_moves.drain() {
            self.handle_message(UiToWorker::PointerMove {
              tab_id,
              pos_css,
              button,
            });
          }
          self.handle_message(other);
        }
      }
    }

    for (tab_id, (pos_css, button)) in pending_pointer_moves.drain() {
      self.handle_message(UiToWorker::PointerMove {
        tab_id,
        pos_css,
        button,
      });
    }
  }

  fn drain_scroll_burst(&mut self) {
    use std::time::{Duration, Instant};

    // Keep this short: we only want to capture back-to-back scroll wheel events that happen within
    // the same UI input burst.
    const COALESCE_WINDOW: Duration = Duration::from_millis(1);

    let deadline = Instant::now() + COALESCE_WINDOW;

    // Reuse the existing pointer-move coalescing logic during scroll bursts so we don't do
    // redundant hit-testing work while the user is scrolling.
    let mut pending_pointer_moves: HashMap<TabId, ((f32, f32), PointerButton)> = HashMap::new();

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
        UiToWorker::PointerMove {
          tab_id,
          pos_css,
          button,
        } => {
          pending_pointer_moves.insert(tab_id, (pos_css, button));
        }
        UiToWorker::Scroll { .. } => {
          for (tab_id, (pos_css, button)) in pending_pointer_moves.drain() {
            self.handle_message(UiToWorker::PointerMove {
              tab_id,
              pos_css,
              button,
            });
          }
          self.handle_message(msg);
        }
        other => {
          for (tab_id, (pos_css, button)) in pending_pointer_moves.drain() {
            self.handle_message(UiToWorker::PointerMove {
              tab_id,
              pos_css,
              button,
            });
          }
          // Defer non-coalescible messages (clicks, navigations, etc) until after we render the
          // coalesced scroll frame.
          self.deferred_msgs.push_front(other);
          break;
        }
      }
    }

    for (tab_id, (pos_css, button)) in pending_pointer_moves.drain() {
      self.handle_message(UiToWorker::PointerMove {
        tab_id,
        pos_css,
        button,
      });
    }
  }

  fn handle_message(&mut self, msg: UiToWorker) {
    match msg {
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
      UiToWorker::NewTab { tab_id, initial_url } => {
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
      }
      UiToWorker::Navigate { tab_id, url, reason } => {
        self.schedule_navigation(tab_id, url, reason);
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
          self.begin_navigation(tab_id, url, NavigationReason::BackForward, false);
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
          self.begin_navigation(tab_id, url, NavigationReason::BackForward, false);
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
          self.begin_navigation(tab_id, url, NavigationReason::Reload, false);
        }
      }
      UiToWorker::Tick { tab_id } => {
        self.handle_tick(tab_id);
      }
      UiToWorker::ViewportChanged {
        tab_id,
        viewport_css,
        dpr,
      } => {
        let Some(tab) = self.tabs.get_mut(&tab_id) else {
          return;
        };
        tab.viewport_css = clamp_viewport(viewport_css);
        tab.dpr = if dpr.is_finite() { dpr.max(f32::EPSILON) } else { 1.0 };
        // Viewport changes should cancel any in-flight paints, but do not attempt to paint before
        // the first navigation completes (no document/layout cache yet).
        tab.cancel.bump_paint();

        if let Some(doc) = tab.document.as_mut() {
          tab.needs_repaint = true;
          tab.force_repaint = true;
          doc.set_viewport(tab.viewport_css.0, tab.viewport_css.1);
          doc.set_device_pixel_ratio(tab.dpr);
        }
      }
      UiToWorker::Scroll {
        tab_id,
        delta_css,
        pointer_css,
      } => {
        let mut hover_update_pos_css: Option<(f32, f32)> = None;
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
            let mut next = tab.scroll_state.clone();
            next.viewport.x = (next.viewport.x + delta_x).max(0.0);
            next.viewport.y = (next.viewport.y + delta_y).max(0.0);
            if next != tab.scroll_state {
              tab.scroll_state = next;
              if tab.loading {
                tab.history.update_scroll(tab.scroll_state.viewport.x, tab.scroll_state.viewport.y);
              }
            }
            return;
          };

          // When scrolling with a stationary pointer, the hovered element can change as content
          // moves under the cursor. Track the latest pointer position so we can re-run hover
          // hit-testing after applying scroll offsets.
          let pointer_pos_css = pointer_css.filter(|(x, y)| x.is_finite() && y.is_finite());

          let current_scroll = doc.scroll_state();
          let mut changed = false;
          let mut wheel_handled = false;

          if let Some(pointer_css) = pointer_pos_css {
            // Apply scroll wheel deltas to the scroll container under the pointer (including element
            // scroll offsets like `<select size>` listboxes).
            match doc.wheel_scroll_at_viewport_point(
              Point::new(pointer_css.0, pointer_css.1),
              (delta_x, delta_y),
            ) {
              Ok(scrolled) => {
                wheel_handled = true;
                if scrolled {
                  tab.scroll_state = doc.scroll_state();
                  changed = true;
                }
              }
              Err(_) => {
                // No cached layout yet; fall back to basic viewport scrolling below.
              }
            }
          }

          // If no pointer position was provided (or we couldn't apply wheel scrolling at all), treat
          // this as a basic viewport scroll and clamp to the content bounds when possible.
          if !wheel_handled {
            let mut next = current_scroll.clone();

            // Important: do not clamp to content bounds here. Paint-from-cache will clamp/snap the
            // scroll offset, and we still want to produce a new frame for "overscroll" deltas that
            // would otherwise appear as a no-op (e.g. scrolling past the end of the page while already
            // at max scroll).
            let apply_axis = |current: f32, delta: f32| {
              if delta == 0.0 || !delta.is_finite() {
                return current;
              }
              let value = current + delta;
              if value.is_finite() { value.max(0.0) } else { current }
            };

            // Force evaluation of `doc.prepared()` so we keep layout alive and let paint apply
            // scroll-snap/clamp logic, but do not use it for clamping here.
            let _ = doc.prepared();

            next.viewport.x = apply_axis(next.viewport.x, delta_x);
            next.viewport.y = apply_axis(next.viewport.y, delta_y);

            if next != current_scroll {
              doc.set_scroll_state(next.clone());
              tab.scroll_state = next;
              changed = true;
            }
          }

          if changed {
            tab.cancel.bump_paint();
            tab.needs_repaint = true;
            tab.scroll_coalesce = true;
            hover_update_pos_css = pointer_pos_css;
          }
        }

        if let Some(pos_css) = hover_update_pos_css {
          self.handle_pointer_move(tab_id, pos_css);
        }
      }
      UiToWorker::PointerMove {
        tab_id,
        pos_css,
        button: _,
      } => {
        self.handle_pointer_move(tab_id, pos_css);
      }
      UiToWorker::PointerDown {
        tab_id,
        pos_css,
        button,
      } => {
        self.handle_pointer_down(tab_id, pos_css, button);
      }
      UiToWorker::PointerUp {
        tab_id,
        pos_css,
        button,
      } => {
        self.handle_pointer_up(tab_id, pos_css, button);
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
      UiToWorker::TextInput { tab_id, text } => {
        self.handle_text_input(tab_id, &text);
      }
      UiToWorker::KeyAction { tab_id, key } => {
        self.handle_key_action(tab_id, key);
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
        self.begin_navigation(tab_id, url, NavigationReason::TypedUrl, true);
      }
      NavigationReason::LinkClick => {
        // Link clicks are resolved by the interaction engine against the current document base
        // URL, so we treat them as already-canonical.
        self.begin_navigation(tab_id, requested_url, NavigationReason::LinkClick, true);
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
        self.begin_navigation(tab_id, nav_url, NavigationReason::Reload, push_history);
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
          let _ = self.ui_tx.send(WorkerToUi::DebugLog {
            tab_id,
            line: format!("ignoring BackForward navigation to unknown URL: {requested_url}"),
          });
          return;
        };

        self.begin_navigation(tab_id, nav_url, NavigationReason::BackForward, false);
      }
    }
  }

  fn begin_navigation(
    &mut self,
    tab_id: TabId,
    url: String,
    reason: NavigationReason,
    push_history: bool,
  ) {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };
    let had_pending_navigation = tab.loading;
    let had_pending_history_entry = tab.pending_history_entry;

    // Fragment-only navigation within the same document: update URL + scroll state in-place.
    //
    // Avoid a full reload/reprepare; we reuse the cached layout artifacts for hit-testing and
    // compute a new viewport offset for the fragment target.
    //
    // `Reload` must not take this path because callers expect a full reload.
    if reason != NavigationReason::Reload {
      if !tab.loading {
        if let (Some(current), Some(doc)) = (tab.last_committed_url.as_deref(), tab.document.as_mut()) {
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
                let offset =
                  scroll_offset_for_fragment_target(dom, box_tree, fragment_tree, fragment, viewport);
                (false, offset)
              }) {
                Ok(Some(offset)) => offset,
                Ok(None) => Point::ZERO,
                Err(err) => {
                  let _ = self.ui_tx.send(WorkerToUi::DebugLog {
                    tab_id,
                    line: format!("fragment navigation scroll failed: {err}"),
                  });
                  tab.scroll_state.viewport
                }
              }
            };

            tab.scroll_state.viewport = offset;
            doc.set_scroll_state(tab.scroll_state.clone());

            let title = find_document_title(doc.dom());
            if let Some(title) = title.as_deref() {
              tab.history.set_title(title.to_string());
            }
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

    tab.cancel.bump_nav();
    tab.loading = true;
    tab.needs_repaint = false;
    tab.pending_navigation = Some(NavigationRequest {
      url: url.clone(),
      apply_fragment_scroll: matches!(reason, NavigationReason::TypedUrl | NavigationReason::LinkClick),
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

    let _ = self.ui_tx.send(WorkerToUi::NavigationStarted { tab_id, url });
    let _ = self.ui_tx.send(WorkerToUi::LoadingState {
      tab_id,
      loading: true,
    });
  }

  fn handle_tick(&mut self, tab_id: TabId) {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };
    let Some(doc) = tab.document.as_mut() else {
      return;
    };

    // Only schedule animation sampling when the document contains time-dependent primitives.
    //
    // `BrowserDocument` resolves time-based CSS animations/transitions to a deterministic settled
    // state unless `RenderOptions.animation_time` is set. Use ticks to advance that time (and mark
    // paint dirty) so animated pages can produce multiple frames without explicit UI interaction.
    if !document_wants_ticks(doc) {
      return;
    }

    let next_time = tab.tick_animation_time_ms + TICK_ANIMATION_STEP_MS;
    tab.tick_animation_time_ms = if next_time.is_finite() {
      next_time
    } else {
      f32::MAX
    };
    doc.set_animation_time_ms(tab.tick_animation_time_ms);
    tab.needs_repaint = true;
  }

  fn handle_pointer_move(&mut self, tab_id: TabId, pos_css: (f32, f32)) {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };
    let Some(doc) = tab.document.as_mut() else {
      return;
    };
    let scroll = &tab.scroll_state;
    let viewport_point = viewport_point_for_pos_css(scroll, pos_css);
    let engine = &mut tab.interaction;

    let changed = match doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
      let scrolled =
        (!scroll.elements.is_empty()).then(|| fragment_tree_with_scroll(fragment_tree, scroll));
      let fragment_tree = scrolled.as_ref().unwrap_or(fragment_tree);
      let changed = engine.pointer_move(dom, box_tree, fragment_tree, scroll, viewport_point);
      (changed, changed)
    }) {
      Ok(changed) => changed,
      Err(_) => return,
    };
    if changed {
      tab.cancel.bump_paint();
      tab.needs_repaint = true;
    }
  }

  fn handle_pointer_down(&mut self, tab_id: TabId, pos_css: (f32, f32), button: PointerButton) {
    if !matches!(button, PointerButton::Primary) {
      return;
    }
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };
    let Some(doc) = tab.document.as_mut() else {
      return;
    };
    let scroll = &tab.scroll_state;
    let viewport_point = viewport_point_for_pos_css(scroll, pos_css);
    let engine = &mut tab.interaction;

    let changed = match doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
      let scrolled =
        (!scroll.elements.is_empty()).then(|| fragment_tree_with_scroll(fragment_tree, scroll));
      let fragment_tree = scrolled.as_ref().unwrap_or(fragment_tree);
      let changed = engine.pointer_down(dom, box_tree, fragment_tree, scroll, viewport_point);
      (changed, changed)
    }) {
      Ok(changed) => changed,
      Err(_) => return,
    };
    if changed {
      tab.cancel.bump_paint();
      tab.needs_repaint = true;
    }
  }

  fn handle_pointer_up(&mut self, tab_id: TabId, pos_css: (f32, f32), button: PointerButton) {
    if !matches!(button, PointerButton::Primary) {
      return;
    }
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };
    let base_url = base_url_for_links(tab).to_string();
    let document_url = tab
      .last_committed_url
      .as_deref()
      .unwrap_or(about_pages::ABOUT_BASE_URL)
      .to_string();
    let scroll = &tab.scroll_state;
    let viewport_point = viewport_point_for_pos_css(scroll, pos_css);
    let engine = &mut tab.interaction;
    let Some(doc) = tab.document.as_mut() else {
      return;
    };
    let (dom_changed, action, anchor_css) =
      match doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
        let scrolled =
          (!scroll.elements.is_empty()).then(|| fragment_tree_with_scroll(fragment_tree, scroll));
        let hit_tree = scrolled.as_ref().unwrap_or(fragment_tree);
        let (dom_changed, action) = engine.pointer_up_with_scroll(
          dom,
          box_tree,
          hit_tree,
          scroll,
          viewport_point,
          &document_url,
          &base_url,
        );

        let anchor_css = match &action {
          InteractionAction::OpenSelectDropdown { select_node_id, .. } => {
            // `select_anchor_css` expects an unscrolled fragment tree and applies element scroll
            // offsets internally.
            select_anchor_css(box_tree, fragment_tree, scroll, *select_node_id)
          }
          _ => None,
        };

        (dom_changed, (dom_changed, action, anchor_css))
      }) {
      Ok(result) => result,
      Err(_) => return,
    };

    match action {
      InteractionAction::Navigate { href } => {
        self.schedule_navigation(tab_id, href, NavigationReason::LinkClick);
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
        if dom_changed {
          tab.needs_repaint = true;
        }
      }
      _ => {
        if dom_changed {
          tab.needs_repaint = true;
        }
      }
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

    let dom_changed = doc.mutate_dom(|dom| {
      dom_mutation::activate_select_option(dom, select_node_id, option_node_id, false)
    });
    if dom_changed {
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
    let dom_changed = doc.mutate_dom(|dom| {
      let index = crate::interaction::dom_index::DomIndex::build(dom);
      let rows = collect_select_rows(&index, select_node_id);
      let row = rows.get(item_index).copied();
      match row {
        Some(SelectRow::Option { node_id, disabled }) if !disabled => {
          should_close = true;
          dom_mutation::activate_select_option(dom, select_node_id, node_id, false)
        }
        _ => false,
      }
    });

    if should_close {
      let _ = self.ui_tx.send(WorkerToUi::SelectDropdownClosed { tab_id });
    }

    if dom_changed {
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

    let changed = doc.mutate_dom(|dom| tab.interaction.text_input(dom, text));
    if changed {
      tab.cancel.bump_paint();
      tab.needs_repaint = true;
    }
  }

  fn handle_key_action(&mut self, tab_id: TabId, key: crate::interaction::KeyAction) {
    let mut navigate_to: Option<String> = None;

    {
      let Some(tab) = self.tabs.get_mut(&tab_id) else {
        return;
      };
      let base_url = base_url_for_links(tab).to_string();
      let document_url = tab
        .last_committed_url
        .as_deref()
        .unwrap_or(about_pages::ABOUT_BASE_URL)
        .to_string();

      let Some(doc) = tab.document.as_mut() else {
        return;
      };

      let result = doc.mutate_dom_with_layout_artifacts(|dom, box_tree, _fragment_tree| {
        let (dom_changed, action) = tab.interaction.key_activate_with_box_tree(
          dom,
          Some(box_tree),
          key,
          &document_url,
          &base_url,
        );
        (dom_changed, (dom_changed, action))
      });
      let (changed, action) = match result {
        Ok(result) => result,
        Err(_) => {
          let mut action = InteractionAction::None;
          let changed = doc.mutate_dom(|dom| {
            let (dom_changed, next_action) =
              tab.interaction.key_activate(dom, key, &document_url, &base_url);
            action = next_action;
            dom_changed
          });
          (changed, action)
        }
      };

      match action {
        InteractionAction::Navigate { href } => {
          navigate_to = Some(href);
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
              select_anchor_css(
                prepared.box_tree(),
                prepared.fragment_tree(),
                &tab.scroll_state,
                select_node_id,
              )
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
        _ => {
          if changed {
            tab.cancel.bump_paint();
            tab.needs_repaint = true;
          }
        }
      }
    }

    if let Some(href) = navigate_to {
      self.schedule_navigation(tab_id, href, NavigationReason::LinkClick);
    }
  }

  fn next_job(&mut self) -> Option<Job> {
    // Navigation takes priority over repaint.
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
    // Any pending navigation.
    if let Some((tab_id, req)) = self
      .tabs
      .iter_mut()
      .find_map(|(id, tab)| tab.pending_navigation.take().map(|req| (*id, req)))
    {
      return Some(Job::Navigate { tab_id, request: req });
    }

    // Paint active tab first.
    if let Some(active) = self.active_tab {
      if self.tabs.get(&active).is_some_and(|t| t.needs_repaint) {
        if let Some(tab) = self.tabs.get_mut(&active) {
          let force = std::mem::take(&mut tab.force_repaint);
          tab.needs_repaint = false;
          tab.scroll_coalesce = false;
          return Some(Job::Paint {
            tab_id: active,
            force,
          });
        }
      }
    }

    // Paint any tab.
    if let Some(tab_id) = self
      .tabs
      .iter()
      .find_map(|(id, tab)| tab.needs_repaint.then_some(*id))
    {
      if let Some(tab) = self.tabs.get_mut(&tab_id) {
        let force = std::mem::take(&mut tab.force_repaint);
        tab.needs_repaint = false;
        tab.scroll_coalesce = false;
        return Some(Job::Paint { tab_id, force });
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
      Job::Paint { tab_id, force } => self.run_paint(tab_id, force),
    }
  }

  fn run_navigation(&mut self, tab_id: TabId, request: NavigationRequest) -> Option<JobOutput> {
    // Pull what we need out of `TabState` so we can release the borrow while running the expensive
    // prepare+paint pipeline (and so we can reinsert the document on all exit paths).
    let (snapshot, paint_snapshot, viewport_css, dpr, initial_scroll, apply_fragment_scroll, cancel, doc) =
      {
        let tab = self.tabs.get_mut(&tab_id)?;
        (
          tab.cancel.snapshot_prepare(),
          tab.cancel.snapshot_paint(),
          tab.viewport_css,
          tab.dpr,
          tab.history.current().map(|e| (e.scroll_x, e.scroll_y)),
          request.apply_fragment_scroll,
          tab.cancel.clone(),
          tab.document.take(),
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

    let prepare_cancel_callback = snapshot.cancel_callback_for_prepare(&cancel);
    let mut options = RenderOptions::default()
      .with_viewport(viewport_css.0, viewport_css.1)
      .with_device_pixel_ratio(dpr);
    options.cancel_callback = Some(prepare_cancel_callback.clone());

    // -----------------------------
    // Prepare/navigation stage
    // -----------------------------

    let (reported_final_url, base_url) = if about_pages::is_about_url(&original_url) {
      let html = about_pages::html_for_about_url(&original_url).unwrap_or_else(|| {
        about_pages::error_page_html("Unknown about page", &format!("Unknown URL: {original_url}"))
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
          let _ = self.reinsert_document(tab_id, doc);
          // Treat cancelled prepares as silent drops.
          if !snapshot.is_still_current_for_prepare(&cancel) {
            return None;
          }
          return self.run_navigation_error(
            tab_id,
            &original_url,
            &format!("about page prepare failed: {err}"),
            snapshot,
          );
        }
      }
    } else {
      match validate_user_navigation_url_scheme(&original_url) {
        Ok(()) => {
          let report = {
            let _guard = forward_stage_heartbeats(tab_id, self.ui_tx.clone());
            doc.navigate_url(&original_url, options.clone())
          };
          match report {
            Ok(report) => (report.final_url, report.base_url),
            Err(err) => {
              // Restore the document before delegating to the navigation-error renderer.
              let _ = self.reinsert_document(tab_id, doc);

              // If the navigation was cancelled via nav bump, treat it as a silent drop.
              if !snapshot.is_still_current_for_prepare(&cancel) {
                return None;
              }

              return self.run_navigation_error(tab_id, &original_url, &err.to_string(), snapshot);
            }
          }
        }
        Err(err) => {
          let _ = self.reinsert_document(tab_id, doc);

          // Unsupported URL schemes should fail fast without rendering an error page.
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
    };

    // If a new navigation was initiated while we were preparing, treat this result as cancelled.
    if !snapshot.is_still_current_for_prepare(&cancel) {
      let _ = self.reinsert_document(tab_id, doc);
      return None;
    }

    // Preserve fragments across redirects so:
    // - history keeps the original `#fragment`
    // - `:target` / anchor scrolling still work
    let committed_url = match reported_final_url.as_deref() {
      Some(final_url) => apply_original_fragment_to_final_url(&original_url, final_url),
      None => original_url.clone(),
    };

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
          scroll_state.viewport = offset;
        }
      }
    }
    doc.set_scroll_state(scroll_state.clone());

    // -----------------------------
    // Initial paint stage
    // -----------------------------
    let paint_cancel_callback = paint_snapshot.cancel_callback_for_paint(&cancel);
    let paint_deadline = deadline_for(paint_cancel_callback.clone(), None);

    let painted = {
      let _guard = forward_stage_heartbeats(tab_id, self.ui_tx.clone());
      match doc.render_if_needed_with_deadlines(Some(&paint_deadline)) {
        Ok(Some(frame)) => Ok(Some(frame)),
        Ok(None) => doc.render_frame_with_deadlines(Some(&paint_deadline)).map(Some),
        Err(err) => Err(err),
      }
    };

    let mut msgs = Vec::new();

    let painted = match painted {
      Ok(Some(frame)) => Some(frame),
      Ok(None) => None,
      Err(err) => {
        // If a new navigation was initiated while we were painting, drop this result silently.
        if !snapshot.is_still_current_for_prepare(&cancel) {
          let _ = self.reinsert_document(tab_id, doc);
          return None;
        }

        // If only paint was bumped (e.g. scroll/viewport change) while the initial paint was
        // in-flight, treat this as a cancelled paint rather than a navigation failure.
        if !paint_snapshot.is_still_current_for_paint(&cancel) {
          let Some(tab) = self.tabs.get_mut(&tab_id) else {
            return None;
          };
          tab.scroll_state = scroll_state.clone();
          tab
            .history
            .update_scroll(tab.scroll_state.viewport.x, tab.scroll_state.viewport.y);
          tab.document = Some(doc);
          tab.interaction = InteractionEngine::new();
          tab.tick_animation_time_ms = 0.0;
          tab.last_committed_url = Some(committed_url.clone());
          tab.last_base_url = base_url.clone();

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
      let _ = self.reinsert_document(tab_id, doc);
      return None;
    }

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
    tab.interaction = InteractionEngine::new();
    tab.tick_animation_time_ms = 0.0;
    tab.last_committed_url = Some(committed_url.clone());
    tab.last_base_url = base_url.clone();

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

    // Only emit FrameReady when the paint snapshot is still current. If the UI bumped paint while
    // we were rendering, skip this frame and let the subsequent repaint win.
    if let Some(frame) = painted {
      if paint_snapshot.is_still_current_for_paint(&cancel) {
        msgs.push(WorkerToUi::FrameReady {
          tab_id,
          frame: RenderedFrame {
            pixmap: frame.pixmap,
            viewport_css,
            dpr: tab
              .document
              .as_ref()
              .and_then(|d| d.prepared())
              .map(|p| p.device_pixel_ratio())
              .unwrap_or(dpr),
            scroll_state: tab.scroll_state.clone(),
            wants_ticks: tab.document.as_ref().is_some_and(document_wants_ticks),
          },
        });
        msgs.push(WorkerToUi::ScrollStateUpdated {
          tab_id,
          scroll: tab.scroll_state.clone(),
        });
      } else {
        tab.needs_repaint = true;
      }
    } else {
      tab.needs_repaint = true;
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

    let html = about_pages::error_page_html("Navigation failed", error);
    let mut options = RenderOptions::default()
      .with_viewport(viewport_css.0, viewport_css.1)
      .with_device_pixel_ratio(dpr);
    options.cancel_callback = Some(cancel_callback.clone());

    // Lazily create the long-lived document/renderer if we don't have one yet.
    let needs_doc = self
      .tabs
      .get(&tab_id)
      .is_some_and(|tab| tab.document.is_none());
    if needs_doc {
      match self.build_initial_document(viewport_css, dpr) {
        Ok(doc) => {
          let _ = self.reinsert_document(tab_id, doc);
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
    tab.tick_animation_time_ms = 0.0;
    tab.scroll_state = painted.scroll_state.clone();
    tab.last_committed_url = Some(about_pages::ABOUT_ERROR.to_string());
    tab.last_base_url = Some(about_pages::ABOUT_BASE_URL.to_string());

    tab.loading = false;
    tab.pending_history_entry = false;

    Some(JobOutput {
      tab_id,
      snapshot,
      snapshot_kind: SnapshotKind::Prepare,
      msgs: vec![
        WorkerToUi::NavigationFailed {
          tab_id,
          url: original_url.to_string(),
          error: error.to_string(),
          can_go_back: tab.history.can_go_back(),
          can_go_forward: tab.history.can_go_forward(),
        },
        WorkerToUi::FrameReady {
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
            wants_ticks: tab.document.as_ref().is_some_and(document_wants_ticks),
          },
        },
        WorkerToUi::ScrollStateUpdated {
          tab_id,
          scroll: tab.scroll_state.clone(),
        },
        WorkerToUi::LoadingState {
          tab_id,
          loading: false,
        },
      ],
    })
  }

  fn run_paint(&mut self, tab_id: TabId, force: bool) -> Option<JobOutput> {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return None;
    };
    let Some(doc) = tab.document.as_mut() else {
      return None;
    };

    let snapshot = tab.cancel.snapshot_paint();
    let cancel_callback = snapshot.cancel_callback_for_paint(&tab.cancel);
    doc.set_cancel_callback(Some(cancel_callback.clone()));

    // Forward render pipeline stage heartbeats during paint jobs (including scroll/hover repaints)
    // so UI callers and integration tests can observe progress and deterministically cancel
    // in-flight work.
    let painted = {
      let _guard = forward_stage_heartbeats(tab_id, self.ui_tx.clone());
      if force {
        doc.render_frame_with_scroll_state().map(Some)
      } else {
        doc.render_if_needed_with_scroll_state()
      }
    };

    let mut msgs = Vec::new();

    let painted = match painted {
      Ok(Some(frame)) => Some(frame),
      Ok(None) => None,
      Err(err) => {
        if !cancel_callback() {
          msgs.push(WorkerToUi::DebugLog {
            tab_id,
            line: format!("paint error: {err}"),
          });
        }
        None
      }
    };

    if let Some(frame) = painted {
      tab.scroll_state = frame.scroll_state.clone();
      tab.history.update_scroll(tab.scroll_state.viewport.x, tab.scroll_state.viewport.y);

      msgs.push(WorkerToUi::FrameReady {
        tab_id,
        frame: RenderedFrame {
          pixmap: frame.pixmap,
          viewport_css: tab.viewport_css,
          dpr: tab
            .document
            .as_ref()
            .and_then(|d| d.prepared())
            .map(|p| p.device_pixel_ratio())
            .unwrap_or(tab.dpr),
          scroll_state: tab.scroll_state.clone(),
          wants_ticks: tab.document.as_ref().is_some_and(document_wants_ticks),
        },
      });
      msgs.push(WorkerToUi::ScrollStateUpdated {
        tab_id,
        scroll: tab.scroll_state.clone(),
      });
    }

    Some(JobOutput {
      tab_id,
      snapshot,
      snapshot_kind: SnapshotKind::Paint,
      msgs,
    })
  }

  fn build_initial_document(&self, viewport_css: (u32, u32), dpr: f32) -> crate::Result<BrowserDocument> {
    let mut renderer = self.factory.build_renderer()?;
    #[cfg(feature = "browser_ui")]
    UI_WORKER_RENDERER_BUILD_COUNT.fetch_add(1, Ordering::Relaxed);

    // Ensure a safe base URL hint from the start so subsequent `about:` renders don't accidentally
    // resolve relative URLs against whatever the renderer last navigated to.
    renderer.set_base_url(about_pages::ABOUT_BASE_URL);

    let html = about_pages::html_for_about_url(about_pages::ABOUT_BLANK)
      .unwrap_or_else(|| "<!doctype html><html><head><meta charset=\"utf-8\"></head><body></body></html>".to_string());

    let options = RenderOptions::default()
      .with_viewport(viewport_css.0, viewport_css.1)
      .with_device_pixel_ratio(dpr);

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
  FastRenderFactory::with_config(FastRenderPoolConfig::new().with_renderer_config(renderer_config))
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
  spawn_worker_with_factory_inner(name.into(), test_render_delay_ms, default_ui_worker_factory()?)
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

      // `set_test_render_delay_ms` is process-global; ensure it is cleared when the worker exits so
      // integration tests cannot leak configuration across runs.
      let _delay_guard = test_render_delay_ms.map(|delay| {
        crate::render_control::set_test_render_delay_ms(Some(delay));
        TestRenderDelayGuard
      });

      let mut runtime = BrowserRuntime::new(ui_to_worker_rx, worker_to_ui_tx, factory);
      runtime.run();
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
pub fn spawn_browser_worker_with_factory(factory: FastRenderFactory) -> crate::Result<BrowserWorkerHandle> {
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
