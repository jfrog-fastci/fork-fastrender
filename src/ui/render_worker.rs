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
use crate::geometry::{Point, Rect, Size};
use crate::html::{find_document_favicon_url, find_document_title};
use crate::interaction::anchor_scroll::scroll_offset_for_fragment_target;
use crate::interaction::{
  fragment_tree_with_scroll, hit_test_dom, FormSubmission, FormSubmissionMethod, HitTestKind,
  InteractionAction, InteractionEngine,
};
use crate::paint::rasterize::fill_rect;
use crate::render_control::{
  push_stage_listener, DeadlineGuard, StageHeartbeat, StageListenerGuard,
};
use crate::scroll::ScrollState;
use crate::style::color::Rgba;
use crate::style::types::OrientationTransform;
use crate::text::font_db::FontConfig;
use crate::ui::about_pages;
use crate::ui::browser_limits::BrowserLimits;
use crate::ui::cancel::{deadline_for, CancelGens, CancelSnapshot};
use crate::ui::find_in_page::{FindIndex, FindMatch, FindOptions};
use crate::ui::history::TabHistory;
use crate::ui::messages::{
  CursorKind, NavigationReason, PointerButton, RenderedFrame, ScrollMetrics, TabId, UiToWorker,
  WorkerToUi,
};
use crate::ui::{resolve_link_url, validate_user_navigation_url_scheme};
use crate::web::events as web_events;
use image::imageops::FilterType;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
#[cfg(feature = "browser_ui")]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

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

// `UiToWorker::Tick` is the UI's periodic driver for time-based updates (CSS animations/transitions,
// and eventually JS timers). The UI does not provide a timestamp, so we advance a fixed amount of
// time per tick to keep behaviour deterministic for tests.
const TICK_ANIMATION_STEP_MS: f32 = 16.0;

// -----------------------------------------------------------------------------
// Favicon loading
// -----------------------------------------------------------------------------

/// Target maximum size for decoded favicons.
///
/// The UI renders favicons at a small logical size (e.g. 16 points). Keeping the decoded buffer
/// bounded avoids untrusted pages sending arbitrarily large payloads over the UI protocol.
const FAVICON_MAX_EDGE_PX: u32 = 32;

/// Maximum bytes allowed in a `WorkerToUi::Favicon` payload.
const MAX_FAVICON_BYTES: usize =
  (FAVICON_MAX_EDGE_PX as usize) * (FAVICON_MAX_EDGE_PX as usize) * 4;
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
  /// True when the next paint was triggered by a scroll message and we should coalesce any
  /// immediately-following scroll events before rendering.
  scroll_coalesce: bool,
  document: Option<BrowserDocument>,
  js_tab: Option<BrowserTab>,
  interaction: InteractionEngine,
  cancel: CancelGens,
  last_committed_url: Option<String>,
  last_base_url: Option<String>,

  last_pointer_pos_css: Option<(f32, f32)>,
  pointer_buttons: u16,
  last_hovered_dom_node_id: Option<usize>,
  last_hovered_dom_element_id: Option<String>,
  last_hovered_url: Option<String>,
  last_cursor: CursorKind,

  pending_navigation: Option<NavigationRequest>,
  needs_repaint: bool,
  force_repaint: bool,

  tick_animation_time_ms: f32,

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
      scroll_coalesce: false,
      document: None,
      js_tab: None,
      interaction: InteractionEngine::new(),
      cancel,
      last_committed_url: None,
      last_base_url: None,
      last_pointer_pos_css: None,
      pointer_buttons: 0,
      last_hovered_dom_node_id: None,
      last_hovered_dom_element_id: None,
      last_hovered_url: None,
      last_cursor: CursorKind::Default,
      pending_navigation: None,
      needs_repaint: false,
      force_repaint: false,
      tick_animation_time_ms: 0.0,
      find: FindInPageWorkerState::default(),
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

fn trim_ascii_whitespace(value: &str) -> &str {
  // HTML attribute parsing ignores leading/trailing ASCII whitespace (TAB/LF/FF/CR/SPACE) but does
  // not treat all Unicode whitespace as ignorable (e.g. NBSP).
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
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
  js_tab: &BrowserTab,
  preorder_id: usize,
  element_id: Option<&str>,
) -> Option<crate::dom2::NodeId> {
  element_id
    .and_then(|id| js_tab.dom().get_element_by_id(id))
    .or_else(|| {
      js_tab
        .dom()
        .node_id_from_index(preorder_id.saturating_sub(1))
        .ok()
    })
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

fn cursor_for_form_control(dom: &mut crate::dom::DomNode, dom_node_id: usize) -> CursorKind {
  let Some(node) = crate::dom::find_node_mut_by_preorder_id(dom, dom_node_id) else {
    return CursorKind::Default;
  };
  let Some(tag) = node.tag_name() else {
    return CursorKind::Default;
  };

  if tag.eq_ignore_ascii_case("textarea") {
    return CursorKind::Text;
  }
  if tag.eq_ignore_ascii_case("input") {
    let ty = trim_ascii_whitespace(node.get_attribute_ref("type").unwrap_or(""));
    // Mirror the HTML spec: invalid/unknown `type` values fall back to `text`.
    //
    // Avoid showing the I-beam cursor for non-text-like controls (checkboxes, buttons, etc).
    if ty.is_empty()
      || !(ty.eq_ignore_ascii_case("hidden")
        || ty.eq_ignore_ascii_case("button")
        || ty.eq_ignore_ascii_case("submit")
        || ty.eq_ignore_ascii_case("reset")
        || ty.eq_ignore_ascii_case("checkbox")
        || ty.eq_ignore_ascii_case("radio")
        || ty.eq_ignore_ascii_case("range")
        || ty.eq_ignore_ascii_case("file")
        || ty.eq_ignore_ascii_case("image")
        || ty.eq_ignore_ascii_case("color"))
    {
      return CursorKind::Text;
    }
  }

  CursorKind::Default
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
        FAVICON_MAX_EDGE_PX,
        FAVICON_MAX_EDGE_PX,
        favicon_url,
        1.0,
      )
      .ok()?;
    let (w, h) = (pixmap.width(), pixmap.height());
    if w == 0 || h == 0 {
      return None;
    }
    if w > FAVICON_MAX_EDGE_PX || h > FAVICON_MAX_EDGE_PX {
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
  let target_w = src_w.min(FAVICON_MAX_EDGE_PX);
  let target_h = src_h.min(FAVICON_MAX_EDGE_PX);
  if target_w != src_w || target_h != src_h {
    rgba = image::imageops::resize(&rgba, target_w, target_h, FilterType::Triangle);
  }

  let (w, h) = rgba.dimensions();
  if w == 0 || h == 0 {
    return None;
  }
  if w > FAVICON_MAX_EDGE_PX || h > FAVICON_MAX_EDGE_PX {
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

struct BrowserRuntime {
  ui_rx: Receiver<UiToWorker>,
  ui_tx: Sender<WorkerToUi>,
  factory: FastRenderFactory,
  limits: BrowserLimits,
  download_dir: PathBuf,
  tabs: HashMap<TabId, TabState>,
  active_tab: Option<TabId>,
  /// Messages deferred during scroll coalescing that should be handled before blocking for the next
  /// message.
  deferred_msgs: VecDeque<UiToWorker>,
}

impl BrowserRuntime {
  fn new(
    ui_rx: Receiver<UiToWorker>,
    ui_tx: Sender<WorkerToUi>,
    factory: FastRenderFactory,
  ) -> Self {
    Self {
      ui_rx,
      ui_tx,
      factory,
      limits: BrowserLimits::from_env(),
      download_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
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
    let mut pending_pointer_moves: HashMap<
      TabId,
      ((f32, f32), PointerButton, crate::ui::PointerModifiers),
    > = HashMap::new();

    while let Some(msg) = self.try_recv_message() {
      match msg {
        UiToWorker::PointerMove {
          tab_id,
          pos_css,
          button,
          modifiers,
        } => {
          pending_pointer_moves.insert(tab_id, (pos_css, button, modifiers));
        }
        other => {
          for (tab_id, (pos_css, button, modifiers)) in pending_pointer_moves.drain() {
            self.handle_message(UiToWorker::PointerMove {
              tab_id,
              pos_css,
              button,
              modifiers,
            });
          }
          self.handle_message(other);
        }
      }
    }

    for (tab_id, (pos_css, button, modifiers)) in pending_pointer_moves.drain() {
      self.handle_message(UiToWorker::PointerMove {
        tab_id,
        pos_css,
        button,
        modifiers,
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
    let mut pending_pointer_moves: HashMap<
      TabId,
      ((f32, f32), PointerButton, crate::ui::PointerModifiers),
    > = HashMap::new();

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
          modifiers,
        } => {
          pending_pointer_moves.insert(tab_id, (pos_css, button, modifiers));
        }
        UiToWorker::Scroll { .. } | UiToWorker::ScrollTo { .. } => {
          for (tab_id, (pos_css, button, modifiers)) in pending_pointer_moves.drain() {
            self.handle_message(UiToWorker::PointerMove {
              tab_id,
              pos_css,
              button,
              modifiers,
            });
          }
          self.handle_message(msg);
        }
        other => {
          for (tab_id, (pos_css, button, modifiers)) in pending_pointer_moves.drain() {
            self.handle_message(UiToWorker::PointerMove {
              tab_id,
              pos_css,
              button,
              modifiers,
            });
          }
          // Defer non-coalescible messages (clicks, navigations, etc) until after we render the
          // coalesced scroll frame.
          self.deferred_msgs.push_front(other);
          break;
        }
      }
    }

    for (tab_id, (pos_css, button, modifiers)) in pending_pointer_moves.drain() {
      self.handle_message(UiToWorker::PointerMove {
        tab_id,
        pos_css,
        button,
        modifiers,
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
      UiToWorker::SetDownloadDirectory { path } => {
        self.download_dir = path;
      }
      UiToWorker::Navigate {
        tab_id,
        url,
        reason,
      } => {
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
          let pointer_pos_css = pointer_css.filter(|(x, y)| x.is_finite() && y.is_finite());

          let current_scroll = doc.scroll_state();
          let mut changed = false;
          let mut wheel_handled = false;

          if let Some(pointer_css) = pointer_pos_css {
            // Give a focused `<input type=number>` under the pointer a chance to consume the wheel
            // gesture for numeric stepping (instead of scrolling the page).
            let scroll_snapshot = tab.scroll_state.clone();
            let engine = &mut tab.interaction;
            if let Ok(step_result) = doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
              let scrolled = (!scroll_snapshot.elements.is_empty())
                .then(|| fragment_tree_with_scroll(fragment_tree, &scroll_snapshot));
              let hit_tree = scrolled.as_ref().unwrap_or(fragment_tree);
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
                    tab.scroll_state = doc.scroll_state();
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

            // Important: do not clamp to content bounds here. Paint-from-cache will clamp/snap the
            // scroll offset, and we still want to produce a new frame for "overscroll" deltas that
            // would otherwise appear as a no-op (e.g. scrolling past the end of the page while already
            // at max scroll).
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
            hover_update_pos_css = pointer_pos_css.or(tab.last_pointer_pos_css);
          }
        }

        if let Some(pos_css) = hover_update_pos_css {
          self.handle_pointer_move(
            tab_id,
            pos_css,
            PointerButton::None,
            crate::ui::PointerModifiers::NONE,
          );
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

          // Clamp to the root scroll bounds when layout artifacts are available.
          if let Some(prepared) = doc.prepared() {
            let viewport = Size::new(tab.viewport_css.0 as f32, tab.viewport_css.1 as f32);
            if let Some(root) =
              crate::scroll::build_scroll_chain(&prepared.fragment_tree().root, viewport, &[])
                .last()
            {
              next.viewport = root.bounds.clamp(next.viewport);
            }
          }

          if next != current {
            doc.set_scroll_state(next.clone());
            tab.scroll_state = next;
            tab.cancel.bump_paint();
            tab.needs_repaint = true;
            tab.scroll_coalesce = true;
          }
        } else {
          // No document yet; still record the scroll position for the first frame.
          let mut next = tab.scroll_state.clone();
          next.viewport = target;
          if next != tab.scroll_state {
            tab.scroll_state = next;
            if tab.loading {
              tab
                .history
                .update_scroll(tab.scroll_state.viewport.x, tab.scroll_state.viewport.y);
            }
          }
        }
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
      UiToWorker::ContextMenuRequest { tab_id, pos_css } => {
        self.handle_context_menu_request(tab_id, pos_css);
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
      UiToWorker::FindQuery { .. }
      | UiToWorker::FindNext { .. }
      | UiToWorker::FindPrev { .. }
      | UiToWorker::FindStop { .. } => {
        match msg {
          UiToWorker::FindQuery {
            tab_id,
            query,
            case_sensitive,
          } => self.handle_find_query(tab_id, &query, case_sensitive),
          UiToWorker::FindNext { tab_id } => self.handle_find_next(tab_id),
          UiToWorker::FindPrev { tab_id } => self.handle_find_prev(tab_id),
          UiToWorker::FindStop { tab_id } => self.handle_find_stop(tab_id),
          _ => {}
        }
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
          let _ = self.ui_tx.send(WorkerToUi::DebugLog {
            tab_id,
            line: format!("ignoring BackForward navigation to unknown URL: {requested_url}"),
          });
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

    // Navigations replace the document (or at least its URL/scroll state); clear any stale hover
    // metadata until the next pointer move re-establishes it.
    Self::maybe_emit_hover_changed(&self.ui_tx, tab_id, tab, None, CursorKind::Default);

    let had_pending_navigation = tab.loading;
    let had_pending_history_entry = tab.pending_history_entry;
    let url = request.url.clone();

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
            tab.history.mark_committed();
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

    let _ = self
      .ui_tx
      .send(WorkerToUi::NavigationStarted { tab_id, url });
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

    let tree = fragment_tree_with_scroll(prepared.fragment_tree(), scroll);
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
      let mut next = tab.scroll_state.clone();
      next.viewport = target;
      doc.set_scroll_state(next.clone());
      tab.scroll_state = next;
      tab
        .history
        .update_scroll(tab.scroll_state.viewport.x, tab.scroll_state.viewport.y);
      let _ = ui_tx.send(WorkerToUi::ScrollStateUpdated {
        tab_id,
        scroll: tab.scroll_state.clone(),
      });
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
    let pointer_in_page =
      pos_css.0.is_finite() && pos_css.1.is_finite() && pos_css.0 >= 0.0 && pos_css.1 >= 0.0;
    tab.last_pointer_pos_css = pointer_in_page.then_some(pos_css);
    let scroll = &tab.scroll_state;
    let viewport_point = viewport_point_for_pos_css(scroll, pos_css);
    let base_url = base_url_for_links(tab).to_string();

    let (changed, hovered_url, cursor, hovered_dom_node_id, hovered_dom_element_id) = {
      let Some(doc) = tab.document.as_mut() else {
        return;
      };
      let engine = &mut tab.interaction;
      match doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
        let scrolled =
          (!scroll.elements.is_empty()).then(|| fragment_tree_with_scroll(fragment_tree, scroll));
        let fragment_tree = scrolled.as_ref().unwrap_or(fragment_tree);
        let changed = engine.pointer_move(dom, box_tree, fragment_tree, scroll, viewport_point);
        let (hovered_url, cursor, hovered_dom_node_id, hovered_dom_element_id) = if !pointer_in_page
        {
          (None, CursorKind::Default, None, None)
        } else {
          let page_point = viewport_point.translate(scroll.viewport);
          match hit_test_dom(dom, box_tree, fragment_tree, page_point) {
            Some(hit) => {
              let element_id = crate::dom::find_node_mut_by_preorder_id(dom, hit.dom_node_id)
                .and_then(|node| node.get_attribute_ref("id"))
                .map(|id| id.to_string());
              let (hovered_url, cursor) = match hit.kind {
                HitTestKind::Link => {
                  let resolved = hit
                    .href
                    .as_deref()
                    .and_then(|href| resolve_link_url(&base_url, href));
                  // Keep showing the hand cursor over links even when we reject the URL scheme (e.g.
                  // `javascript:`).
                  (resolved, CursorKind::Pointer)
                }
                HitTestKind::FormControl => (None, cursor_for_form_control(dom, hit.dom_node_id)),
                _ => (None, CursorKind::Default),
              };
              (hovered_url, cursor, Some(hit.dom_node_id), element_id)
            }
            None => (None, CursorKind::Default, None, None),
          }
        };
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

    Self::maybe_emit_hover_changed(&self.ui_tx, tab_id, tab, hovered_url, cursor);

    // ---------------------------------------------------------------------------
    // DOM mouse events (`mousemove` + hover transitions)
    // ---------------------------------------------------------------------------
    let prev_hovered_dom_node_id = tab.last_hovered_dom_node_id;
    let prev_hovered_dom_element_id = tab.last_hovered_dom_element_id.clone();
    let hover_changed = prev_hovered_dom_node_id != hovered_dom_node_id;
    tab.last_hovered_dom_node_id = hovered_dom_node_id;
    tab.last_hovered_dom_element_id = hovered_dom_element_id.clone();

    let pointer_buttons = tab.pointer_buttons;
    let Some(js_tab) = tab.js_tab.as_mut() else {
      return;
    };

    let mouse_base = web_events::MouseEvent {
      client_x: mouse_client_coord(pos_css.0),
      client_y: mouse_client_coord(pos_css.1),
      button: mouse_event_button(button),
      buttons: pointer_buttons,
      ctrl_key: modifiers.ctrl(),
      shift_key: modifiers.shift(),
      alt_key: modifiers.alt(),
      meta_key: modifiers.meta(),
      related_target: None,
    };

    let current_target = hovered_dom_node_id.and_then(|preorder_id| {
      js_dom_node_for_preorder_id(js_tab, preorder_id, hovered_dom_element_id.as_deref())
    });

    let should_mousemove = current_target.is_some_and(|target_node_id| {
      let dom = js_tab.dom();
      dom.events().has_listeners_for_dispatch(
        web_events::EventTargetId::Node(target_node_id),
        "mousemove",
        dom,
        /* bubbles */ true,
        /* composed */ false,
      )
    });
    if should_mousemove {
      if let Some(target_node_id) = current_target {
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

    if !hover_changed {
      return;
    }

    let prev_target = prev_hovered_dom_node_id.and_then(|preorder_id| {
      js_dom_node_for_preorder_id(js_tab, preorder_id, prev_hovered_dom_element_id.as_deref())
    });

    let should_mouseout = prev_target.is_some_and(|prev_node_id| {
      let dom = js_tab.dom();
      dom.events().has_listeners_for_dispatch(
        web_events::EventTargetId::Node(prev_node_id),
        "mouseout",
        dom,
        /* bubbles */ true,
        /* composed */ false,
      )
    });
    let should_mouseleave = prev_target.is_some_and(|prev_node_id| {
      let dom = js_tab.dom();
      dom.events().has_listeners_for_dispatch(
        web_events::EventTargetId::Node(prev_node_id),
        "mouseleave",
        dom,
        /* bubbles */ false,
        /* composed */ false,
      )
    });

    // out/leave on previous target.
    if let Some(prev_node_id) = prev_target {
      let related = current_target.map(|id| web_events::EventTargetId::Node(id).normalize());

      let mut mouse = mouse_base;
      mouse.related_target = related;

      if should_mouseout {
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

      if should_mouseleave {
        let _ = js_tab.dispatch_mouse_event(
          prev_node_id,
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
      let dom = js_tab.dom();
      dom.events().has_listeners_for_dispatch(
        web_events::EventTargetId::Node(new_node_id),
        "mouseover",
        dom,
        /* bubbles */ true,
        /* composed */ false,
      )
    });
    let should_mouseenter = current_target.is_some_and(|new_node_id| {
      let dom = js_tab.dom();
      dom.events().has_listeners_for_dispatch(
        web_events::EventTargetId::Node(new_node_id),
        "mouseenter",
        dom,
        /* bubbles */ false,
        /* composed */ false,
      )
    });

    // over/enter on new target.
    if let Some(new_node_id) = current_target {
      let related = prev_target.map(|id| web_events::EventTargetId::Node(id).normalize());

      let mut mouse = mouse_base;
      mouse.related_target = related;

      if should_mouseover {
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

      if should_mouseenter {
        let _ = js_tab.dispatch_mouse_event(
          new_node_id,
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
    tab.pointer_buttons |= mouse_buttons_mask_for_button(button);
    let Some(doc) = tab.document.as_mut() else {
      return;
    };
    let scroll = &tab.scroll_state;
    let viewport_point = viewport_point_for_pos_css(scroll, pos_css);
    let engine = &mut tab.interaction;

    let (changed, target_id, target_element_id) =
      match doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
        let scrolled =
          (!scroll.elements.is_empty()).then(|| fragment_tree_with_scroll(fragment_tree, scroll));
        let fragment_tree = scrolled.as_ref().unwrap_or(fragment_tree);

        let changed = if matches!(button, PointerButton::Primary | PointerButton::Middle) {
          engine.pointer_down_with_click_count(
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
          false
        };

        let page_point = viewport_point.translate(scroll.viewport);
        let hit = hit_test_dom(dom, box_tree, fragment_tree, page_point);
        let target_id = hit.as_ref().map(|hit| hit.dom_node_id);
        let target_element_id = target_id.and_then(|target_id| {
          crate::dom::find_node_mut_by_preorder_id(dom, target_id)
            .and_then(|node| node.get_attribute_ref("id"))
            .map(|id| id.to_string())
        });

        (changed, (changed, target_id, target_element_id))
      }) {
        Ok(changed) => changed,
        Err(_) => return,
      };

    if let Some(target_id) = target_id {
      let pointer_buttons = tab.pointer_buttons;
      if let Some(js_tab) = tab.js_tab.as_mut() {
        let target = js_dom_node_for_preorder_id(js_tab, target_id, target_element_id.as_deref());
        if let Some(node_id) = target {
          let mouse = web_events::MouseEvent {
            client_x: mouse_client_coord(pos_css.0),
            client_y: mouse_client_coord(pos_css.1),
            button: mouse_event_button(button),
            buttons: pointer_buttons,
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
            let _ = self.ui_tx.send(WorkerToUi::DebugLog {
              tab_id,
              line: format!("js mousedown event dispatch failed: {err}"),
            });
          }
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

    if !matches!(button, PointerButton::Primary | PointerButton::Middle) {
      // Right-click/etc: no default interaction engine actions, but still dispatch a DOM `mouseup`
      // event so JS can observe non-primary buttons.
      let Some(doc) = tab.document.as_mut() else {
        return;
      };
      let scroll = &tab.scroll_state;
      let viewport_point = viewport_point_for_pos_css(scroll, pos_css);
      let pointer_buttons = tab.pointer_buttons;

      let (target_id, target_element_id) =
        match doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
          let scrolled =
            (!scroll.elements.is_empty()).then(|| fragment_tree_with_scroll(fragment_tree, scroll));
          let fragment_tree = scrolled.as_ref().unwrap_or(fragment_tree);

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

      if let Some(target_id) = target_id {
        if let Some(js_tab) = tab.js_tab.as_mut() {
          let target = js_dom_node_for_preorder_id(js_tab, target_id, target_element_id.as_deref());
          if let Some(node_id) = target {
            let mouse = web_events::MouseEvent {
              client_x: mouse_client_coord(pos_css.0),
              client_y: mouse_client_coord(pos_css.1),
              button: mouse_event_button(button),
              buttons: pointer_buttons,
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
              let _ = self.ui_tx.send(WorkerToUi::DebugLog {
                tab_id,
                line: format!("js mouseup event dispatch failed: {err}"),
              });
            }
          }
        }
      }
      return;
    }

    let pointer_buttons = tab.pointer_buttons;

    let base_url = base_url_for_links(tab).to_string();
    let document_url = tab
      .last_committed_url
      .as_deref()
      .unwrap_or(about_pages::ABOUT_BASE_URL)
      .to_string();
    let scroll_snapshot = tab.scroll_state.clone();
    let viewport_point = viewport_point_for_pos_css(&scroll_snapshot, pos_css);
    let (
      dom_changed,
      action,
      anchor_css,
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
      let (
        dom_changed,
        action,
        anchor_css,
        focus_scroll,
        mouseup_target,
        mouseup_target_element_id,
        click_target,
        click_target_element_id,
        form_submitter,
        form_submitter_element_id,
      ) = match doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
        let scrolled = (!scroll_snapshot.elements.is_empty())
          .then(|| fragment_tree_with_scroll(fragment_tree, &scroll_snapshot));
        let hit_tree = scrolled.as_ref().unwrap_or(fragment_tree);
        let (dom_changed, action) = engine.pointer_up_with_scroll(
          dom,
          box_tree,
          hit_tree,
          &scroll_snapshot,
          viewport_point,
          button,
          modifiers,
          &document_url,
          &base_url,
        );

        let page_point = viewport_point.translate(scroll_snapshot.viewport);
        let mouseup_target =
          hit_test_dom(dom, box_tree, hit_tree, page_point).map(|hit| hit.dom_node_id);
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
            // `select_anchor_css` expects an unscrolled fragment tree and applies element scroll
            // offsets internally.
            select_anchor_css(box_tree, fragment_tree, &scroll_snapshot, *select_node_id)
          }
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
            let page_point = viewport_point.translate(scroll_snapshot.viewport);
            crate::interaction::hit_test::hit_test_dom(dom, box_tree, hit_tree, page_point)
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
        scroll_changed = true;
        let _ = self.ui_tx.send(WorkerToUi::ScrollStateUpdated {
          tab_id,
          scroll: tab.scroll_state.clone(),
        });
      }

      (
        dom_changed,
        action,
        anchor_css,
        scroll_changed,
        mouseup_target,
        mouseup_target_element_id,
        click_target,
        click_target_element_id,
        form_submitter,
        form_submitter_element_id,
      )
    };

    if let Some(target_id) = mouseup_target {
      if let Some(js_tab) = tab.js_tab.as_mut() {
        let target =
          js_dom_node_for_preorder_id(js_tab, target_id, mouseup_target_element_id.as_deref());
        if let Some(node_id) = target {
          let mouse = web_events::MouseEvent {
            client_x: mouse_client_coord(pos_css.0),
            client_y: mouse_client_coord(pos_css.1),
            button: mouse_event_button(button),
            buttons: pointer_buttons,
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
            let _ = self.ui_tx.send(WorkerToUi::DebugLog {
              tab_id,
              line: format!("js mouseup event dispatch failed: {err}"),
            });
          }
        }
      }
    }

    let mut default_allowed = true;
    if let Some(target_id) = click_target {
      if let Some(js_tab) = tab.js_tab.as_mut() {
        let target =
          js_dom_node_for_preorder_id(js_tab, target_id, click_target_element_id.as_deref());

        if let Some(node_id) = target {
          match js_tab.dispatch_click_event(node_id) {
            Ok(allowed) => default_allowed = allowed,
            Err(err) => {
              let _ = self.ui_tx.send(WorkerToUi::DebugLog {
                tab_id,
                line: format!("js click event dispatch failed: {err}"),
              });
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
          let submitter_node =
            js_dom_node_for_preorder_id(js_tab, submitter_id, form_submitter_element_id.as_deref());
          if let Some(submitter_node) = submitter_node {
            if let Some(form_node) = js_find_form_owner_for_submitter(js_tab.dom(), submitter_node)
            {
              match js_tab.dispatch_submit_event(form_node) {
                Ok(allowed) => default_allowed = allowed,
                Err(err) => {
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

    match action {
      InteractionAction::Navigate { href } => {
        if default_allowed {
          self.schedule_navigation(tab_id, href, NavigationReason::LinkClick);
        } else if dom_changed || scroll_changed {
          tab.needs_repaint = true;
        }
      }
      InteractionAction::OpenInNewTab { href } => {
        if default_allowed {
          let _ = self
            .ui_tx
            .send(WorkerToUi::RequestOpenInNewTab { tab_id, url: href });
        }
        if dom_changed || scroll_changed {
          tab.needs_repaint = true;
        }
      }
      InteractionAction::NavigateRequest { request } => {
        if default_allowed {
          self.schedule_navigation_request(tab_id, request, NavigationReason::LinkClick);
        } else if dom_changed || scroll_changed {
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
      _ => {
        if dom_changed || scroll_changed {
          tab.needs_repaint = true;
        }
      }
    }
  }

  fn handle_context_menu_request(&mut self, tab_id: TabId, pos_css: (f32, f32)) {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };

    let base_url = base_url_for_links(tab).to_string();
    let scroll = &tab.scroll_state;
    let viewport_point = viewport_point_for_pos_css(scroll, pos_css);
    let page_point = viewport_point.translate(scroll.viewport);

    let Some(doc) = tab.document.as_mut() else {
      let _ = self.ui_tx.send(WorkerToUi::ContextMenu {
        tab_id,
        pos_css,
        link_url: None,
      });
      return;
    };

    let href = match doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
      let scrolled =
        (!scroll.elements.is_empty()).then(|| fragment_tree_with_scroll(fragment_tree, scroll));
      let hit_tree = scrolled.as_ref().unwrap_or(fragment_tree);
      let href = hit_test_dom(dom, box_tree, hit_tree, page_point).and_then(|hit| {
        if hit.kind == HitTestKind::Link {
          hit.href
        } else {
          None
        }
      });
      (false, href)
    }) {
      Ok(href) => href,
      Err(_) => None,
    };

    let link_url = href.and_then(|href| resolve_link_url(&base_url, &href));

    let _ = self.ui_tx.send(WorkerToUi::ContextMenu {
      tab_id,
      pos_css,
      link_url,
    });
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
    let engine = &mut tab.interaction;
    let dom_changed = doc.mutate_dom(|dom| {
      let index = crate::interaction::dom_index::DomIndex::build(dom);
      let rows = collect_select_rows(&index, select_node_id);
      let row = rows.get(item_index).copied();
      match row {
        Some(SelectRow::Option { node_id, disabled }) if !disabled => {
          should_close = true;
          engine.activate_select_option(dom, select_node_id, node_id, false)
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

    if let Some(text) = copied {
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

    if let Some(text) = cut_text {
      let _ = self
        .ui_tx
        .send(WorkerToUi::SetClipboardText { tab_id, text });
    }

    if changed {
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

    let changed = doc.mutate_dom(|dom| tab.interaction.clipboard_paste(dom, text));
    if changed {
      tab.cancel.bump_paint();
      tab.needs_repaint = true;
    }
  }

  fn handle_key_action(&mut self, tab_id: TabId, key: crate::interaction::KeyAction) {
    let mut navigate_to: Option<String> = None;
    let mut navigate_request: Option<FormSubmission> = None;
    let mut keyboard_scroll: Option<UiToWorker> = None;

    {
      let Some(tab) = self.tabs.get_mut(&tab_id) else {
        return;
      };
      let focus_none = tab.interaction.focused_node_id().is_none();
      let base_url = base_url_for_links(tab).to_string();
      let document_url = tab
        .last_committed_url
        .as_deref()
        .unwrap_or(about_pages::ABOUT_BASE_URL)
        .to_string();

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
          &document_url,
          &base_url,
        );
        let submitter = tab.interaction.take_last_form_submitter();
        let submitter_element_id = submitter.and_then(|submitter_id| {
          crate::dom::find_node_mut_by_preorder_id(dom, submitter_id)
            .and_then(|node| node.get_attribute_ref("id"))
            .map(|id| id.to_string())
        });
        let focused = tab.interaction.focused_node_id();
        let (focused_element_id, focused_is_text_input) = focused
          .and_then(|focused_id| {
            crate::dom::find_node_mut_by_preorder_id(dom, focused_id).map(|node| {
              (
                node.get_attribute_ref("id").map(|id| id.to_string()),
                dom_is_text_input(node),
              )
            })
          })
          .unwrap_or((None, false));
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
      ) = match result {
        Ok(result) => result,
        Err(_) => {
          let mut action = InteractionAction::None;
          let mut submitter: Option<usize> = None;
          let mut submitter_element_id: Option<String> = None;
          let mut focused: Option<usize> = None;
          let mut focused_element_id: Option<String> = None;
          let mut focused_is_text_input = false;
          let changed = doc.mutate_dom(|dom| {
            let (dom_changed, next_action) =
              tab
                .interaction
                .key_activate(dom, key, &document_url, &base_url);
            action = next_action;
            submitter = tab.interaction.take_last_form_submitter();
            submitter_element_id = submitter.and_then(|submitter_id| {
              crate::dom::find_node_mut_by_preorder_id(dom, submitter_id)
                .and_then(|node| node.get_attribute_ref("id"))
                .map(|id| id.to_string())
            });
            focused = tab.interaction.focused_node_id();
            let (id, is_text_input) = focused
              .and_then(|focused_id| {
                crate::dom::find_node_mut_by_preorder_id(dom, focused_id).map(|node| {
                  (
                    node.get_attribute_ref("id").map(|id| id.to_string()),
                    dom_is_text_input(node),
                  )
                })
              })
              .unwrap_or((None, false));
            focused_element_id = id;
            focused_is_text_input = is_text_input;
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
          )
        }
      };

      let mut scroll_changed = false;
      if let Some(next_scroll) = focus_scroll {
        tab.scroll_state = next_scroll;
        doc.set_scroll_state(tab.scroll_state.clone());
        scroll_changed = true;
        let _ = self.ui_tx.send(WorkerToUi::ScrollStateUpdated {
          tab_id,
          scroll: tab.scroll_state.clone(),
        });
      }

      let mut default_allowed = true;

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

      if let Some(target_id) = click_target_id {
        if let Some(js_tab) = tab.js_tab.as_mut() {
          let target = js_dom_node_for_preorder_id(js_tab, target_id, click_target_element_id);
          if let Some(node_id) = target {
            match js_tab.dispatch_click_event(node_id) {
              Ok(allowed) => default_allowed = allowed,
              Err(err) => {
                let _ = self.ui_tx.send(WorkerToUi::DebugLog {
                  tab_id,
                  line: format!("js click event dispatch failed: {err}"),
                });
              }
            }
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
          InteractionAction::Navigate { .. } | InteractionAction::NavigateRequest { .. }
        )
      {
        submit_source_id = focused;
        submit_source_element_id = focused_element_id.as_deref();
      }

      if default_allowed {
        if let Some(source_id) = submit_source_id {
          if let Some(js_tab) = tab.js_tab.as_mut() {
            let source_node =
              js_dom_node_for_preorder_id(js_tab, source_id, submit_source_element_id);
            if let Some(source_node) = source_node {
              if let Some(form_node) = js_find_form_owner_for_submitter(js_tab.dom(), source_node) {
                match js_tab.dispatch_submit_event(form_node) {
                  Ok(allowed) => default_allowed = allowed,
                  Err(err) => {
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
          // Basic keyboard scrolling: when nothing is focused, treat Home/End/Space as viewport
          // scrolling shortcuts (matching common browser behaviour). Focused form controls should
          // keep receiving these keys for caret/selection/option navigation.
          if focus_none && !changed && !scroll_changed && action_is_none {
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
              _ => None,
            };
          }
          if changed || scroll_changed {
            tab.cancel.bump_paint();
            tab.needs_repaint = true;
          }
        }
      }
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
          tab.needs_repaint = false;
          tab.scroll_coalesce = false;
          return Some(Job::Paint {
            tab_id: active,
            force,
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

  fn sync_js_tab_for_committed_navigation(
    tab_id: TabId,
    tab: &mut TabState,
    committed_url: &str,
    viewport_css: (u32, u32),
    dpr: f32,
    msgs: &mut Vec<WorkerToUi>,
  ) {
    // `BrowserTab` navigations are powered by the resource fetcher (http/file/data); it does not
    // know how to fetch internal `about:` pages rendered by the UI worker.
    if about_pages::is_about_url(committed_url) {
      tab.js_tab = None;
      return;
    }
    let Some(doc) = tab.document.as_ref() else {
      tab.js_tab = None;
      return;
    };

    let options = RenderOptions::default()
      .with_viewport(viewport_css.0, viewport_css.1)
      .with_device_pixel_ratio(dpr);
    let fetcher = doc.fetcher();
    let blank_html =
      "<!doctype html><html><head><meta charset=\"utf-8\"></head><body></body></html>";

    if let Some(js_tab) = tab.js_tab.as_mut() {
      if let Err(err) = js_tab.navigate_to_url(committed_url, options) {
        tab.js_tab = None;
        msgs.push(WorkerToUi::DebugLog {
          tab_id,
          line: format!("js tab navigation failed: {err}"),
        });
      }
      return;
    }

    let mut js_tab = match BrowserTab::from_html_with_document_url_and_fetcher(
      blank_html,
      about_pages::ABOUT_BLANK,
      options.clone(),
      VmJsBrowserTabExecutor::default(),
      fetcher,
    ) {
      Ok(tab) => tab,
      Err(err) => {
        msgs.push(WorkerToUi::DebugLog {
          tab_id,
          line: format!("failed to create JS tab: {err}"),
        });
        return;
      }
    };

    if let Err(err) = js_tab.navigate_to_url(committed_url, options) {
      msgs.push(WorkerToUi::DebugLog {
        tab_id,
        line: format!("js tab navigation failed: {err}"),
      });
      return;
    }
    tab.js_tab = Some(js_tab);
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
    let (snapshot, paint_snapshot, viewport_css, dpr, initial_scroll, cancel, doc) = {
      let tab = self.tabs.get_mut(&tab_id)?;
      (
        tab.cancel.snapshot_prepare(),
        tab.cancel.snapshot_paint(),
        tab.viewport_css,
        tab.dpr,
        tab.history.current().map(|e| (e.scroll_x, e.scroll_y)),
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

    let prepare_cancel_callback = combine_cancel_callbacks(
      snapshot.cancel_callback_for_prepare(&cancel),
      preempt_cancel_callback.clone(),
    );
    let mut options = RenderOptions::default()
      .with_viewport(viewport_css.0, viewport_css.1)
      .with_device_pixel_ratio(dpr);
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
          let _ = self.reinsert_document(tab_id, doc);
          // Treat cancelled/preempted prepares as silent drops.
          if prepare_cancel_callback() {
            // New navigation superseded this attempt.
            if !snapshot.is_still_current_for_prepare(&cancel) {
              return None;
            }
            // Preempted by active-tab work: re-queue the navigation so it can resume later.
            if let Some(tab) = self.tabs.get_mut(&tab_id) {
              tab.pending_navigation = Some(request_for_retry);
            }
            return None;
          }
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
              // Restore the document before delegating to the navigation-error renderer.
              let _ = self.reinsert_document(tab_id, doc);

              // If the navigation was cancelled/preempted, treat it as a silent drop.
              if prepare_cancel_callback() {
                if !snapshot.is_still_current_for_prepare(&cancel) {
                  return None;
                }
                if let Some(tab) = self.tabs.get_mut(&tab_id) {
                  tab.pending_navigation = Some(request_for_retry);
                }
                return None;
              }
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
    // Initial paint stage
    // -----------------------------
    let paint_cancel_callback = combine_cancel_callbacks(
      paint_snapshot.cancel_callback_for_paint(&cancel),
      preempt_cancel_callback.clone(),
    );
    let paint_deadline = deadline_for(paint_cancel_callback.clone(), None);

    let painted = {
      let _guard = forward_stage_heartbeats(tab_id, self.ui_tx.clone());
      let interaction_state = autofocus_target.map(|_| interaction.interaction_state());
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
          let _ = self.reinsert_document(tab_id, doc);
          return None;
        }

        // If only paint was bumped (e.g. scroll/viewport change) while the initial paint was
        // in-flight, treat this as a cancelled paint rather than a navigation failure.
        if paint_cancel_callback() || !paint_snapshot.is_still_current_for_paint(&cancel) {
          let Some(tab) = self.tabs.get_mut(&tab_id) else {
            return None;
          };
          tab.scroll_state = scroll_state.clone();
          tab
            .history
            .update_scroll(tab.scroll_state.viewport.x, tab.scroll_state.viewport.y);
          tab.document = Some(doc);
          tab.interaction = interaction;
          tab.tick_animation_time_ms = 0.0;
          tab.last_committed_url = Some(committed_url.clone());
          tab.last_base_url = base_url.clone();

          Self::sync_js_tab_for_committed_navigation(
            tab_id,
            tab,
            &committed_url,
            viewport_css,
            dpr,
            &mut msgs,
          );

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
          || width > FAVICON_MAX_EDGE_PX
          || height > FAVICON_MAX_EDGE_PX
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
    tab.interaction = interaction;
    tab.tick_animation_time_ms = 0.0;
    tab.last_committed_url = Some(committed_url.clone());
    tab.last_base_url = base_url.clone();

    Self::sync_js_tab_for_committed_navigation(
      tab_id,
      tab,
      &committed_url,
      viewport_css,
      dpr,
      &mut msgs,
    );

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
    if let Some(frame) = painted {
      if paint_snapshot.is_still_current_for_paint(&cancel) {
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

    let html = about_pages::error_page_html("Navigation failed", error, Some(original_url));
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
    tab.tick_animation_time_ms = 0.0;
    tab.scroll_state = painted.scroll_state.clone();
    tab.last_committed_url = Some(about_pages::ABOUT_ERROR.to_string());
    tab.last_base_url = Some(about_pages::ABOUT_BASE_URL.to_string());

    tab.loading = false;
    tab.pending_history_entry = false;
    tab.history.mark_committed();

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
            scroll_metrics: compute_scroll_metrics(
              tab.document.as_ref(),
              tab.viewport_css,
              &tab.scroll_state,
            ),
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
    let preempt_cancel_callback = self.preempt_cancel_callback_for_job(tab_id);
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return None;
    };
    let Some(doc) = tab.document.as_mut() else {
      return None;
    };

    let snapshot = tab.cancel.snapshot_paint();
    let cancel_callback = combine_cancel_callbacks(
      snapshot.cancel_callback_for_paint(&tab.cancel),
      preempt_cancel_callback.clone(),
    );
    doc.set_cancel_callback(Some(cancel_callback.clone()));

    // Forward render pipeline stage heartbeats during paint jobs (including scroll/hover repaints)
    // so UI callers and integration tests can observe progress and deterministically cancel
    // in-flight work.
    let painted = {
      let _guard = forward_stage_heartbeats(tab_id, self.ui_tx.clone());
      let interaction_state = Some(tab.interaction.interaction_state());
      if force {
        doc
          .render_frame_with_scroll_state_and_interaction_state(interaction_state)
          .map(Some)
      } else {
        doc.render_if_needed_with_scroll_state_and_interaction_state(interaction_state)
      }
    };

    let mut msgs = Vec::new();

    let mut should_retry = false;
    let painted = match painted {
      Ok(Some(frame)) => Some(frame),
      Ok(None) => None,
      Err(err) => {
        if cancel_callback() {
          should_retry = true;
        } else {
          msgs.push(WorkerToUi::DebugLog {
            tab_id,
            line: format!("paint error: {err}"),
          });
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
      tab
        .history
        .update_scroll(tab.scroll_state.viewport.x, tab.scroll_state.viewport.y);

      let actual_dpr = doc
        .prepared()
        .map(|p| p.device_pixel_ratio())
        .unwrap_or(tab.dpr);

      let mut pixmap = frame.pixmap;

      if !tab.find.query.is_empty() {
        let prev_count = tab.find.matches.len();
        let prev_active = tab.find.active_match_index;
        Self::rebuild_find_matches(&mut tab.find, &tab.scroll_state, &*doc);
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
