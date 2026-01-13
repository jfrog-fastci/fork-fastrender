use crate::render_control::StageHeartbeat;
use crate::scroll::ScrollState;
use crate::multiprocess::SiteKey;
use crate::ui::about_pages;
use crate::ui::appearance::AppearanceSettings;
use crate::ui::browser_limits::BrowserLimits;
use crate::ui::cancel::CancelGens;
use crate::ui::messages::{
  CursorKind, DatalistSuggestion, DownloadId, DownloadOutcome, NavigationReason, RenderedFrame,
  ScrollMetrics, TabId, UiToWorker, WorkerToUi,
};
use crate::ui::protocol_limits::{
  MAX_DEBUG_LOG_BYTES, MAX_DOWNLOAD_FILE_NAME_BYTES, MAX_ERROR_BYTES, MAX_FIND_QUERY_BYTES,
  MAX_TITLE_BYTES, MAX_URL_BYTES, MAX_WARNING_BYTES,
};
use crate::ui::untrusted::{
  sanitize_untrusted_select_control, sanitize_untrusted_text, validate_untrusted_favicon_rgba,
  validate_untrusted_navigation_url,
};
use crate::ui::{
  resolve_omnibox_input, validate_user_navigation_url_scheme, GlobalHistoryStore,
  OmniboxSuggestion, VisitedUrlStore,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;
use url::Url;

const DEBUG_LOG_CAPACITY: usize = 256;
const CLOSED_TAB_STACK_CAPACITY: usize = 20;
// Keep in sync with `src/ui/render_worker.rs`'s `FAVICON_MAX_EDGE_PX`.
const FAVICON_MAX_EDGE_PX: u32 = 32;

static NEXT_TAB_GROUP_ID: AtomicU64 = AtomicU64::new(1);

/// Identifier for a chrome tab group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TabGroupId(pub u64);

/// Identifier for a renderer process assigned by the browser process manager.
///
/// This is browser-side state used for compositor/IPC routing; it is not persisted as part of the
/// browsing session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RendererProcessId(pub u64);

/// Debug-friendly key representing the "site" a tab is currently assigned to.
///
/// This is intended to support future site isolation (process-per-site) policies. The exact
/// semantics are currently opaque to the UI layer and are owned by the browser process manager.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SiteKey(pub String);

impl TabGroupId {
  /// Generate a new process-unique tab group id.
  pub fn new() -> Self {
    // Keep `0` as a reserved "invalid" value, mirroring `TabId`.
    loop {
      let id = NEXT_TAB_GROUP_ID.fetch_add(1, Ordering::Relaxed);
      if id != 0 {
        return Self(id);
      }
    }
  }
}

/// Opaque focus token used to restore focus to a UI element after dismissing a popup/context menu.
///
/// This is intentionally UI-backend-agnostic: egui-based front-ends can convert from/to egui ids,
/// while other UIs can use whatever stable widget/focus identifiers they prefer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UiFocusToken(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TabGroupColor {
  #[default]
  Blue,
  Gray,
  Red,
  Orange,
  Yellow,
  Green,
  Purple,
  Pink,
}

impl TabGroupColor {
  pub const ALL: [Self; 8] = [
    Self::Blue,
    Self::Gray,
    Self::Red,
    Self::Orange,
    Self::Yellow,
    Self::Green,
    Self::Purple,
    Self::Pink,
  ];

  pub fn as_str(self) -> &'static str {
    match self {
      Self::Blue => "Blue",
      Self::Gray => "Gray",
      Self::Red => "Red",
      Self::Orange => "Orange",
      Self::Yellow => "Yellow",
      Self::Green => "Green",
      Self::Purple => "Purple",
      Self::Pink => "Pink",
    }
  }

  fn as_session_str(self) -> &'static str {
    match self {
      Self::Blue => "blue",
      Self::Gray => "gray",
      Self::Red => "red",
      Self::Orange => "orange",
      Self::Yellow => "yellow",
      Self::Green => "green",
      Self::Purple => "purple",
      Self::Pink => "pink",
    }
  }

  fn parse_session_str(raw: &str) -> Option<Self> {
    let v = raw.trim().to_ascii_lowercase();
    if v.is_empty() {
      return None;
    }
    match v.as_str() {
      "blue" => Some(Self::Blue),
      "gray" | "grey" => Some(Self::Gray),
      "red" => Some(Self::Red),
      "orange" => Some(Self::Orange),
      "yellow" => Some(Self::Yellow),
      "green" => Some(Self::Green),
      "purple" => Some(Self::Purple),
      "pink" => Some(Self::Pink),
      _ => None,
    }
  }
  pub fn rgb(self) -> (u8, u8, u8) {
    match self {
      // Roughly matches Chrome's tab group palette.
      Self::Blue => (66, 133, 244),
      Self::Gray => (95, 99, 104),
      Self::Red => (234, 67, 53),
      Self::Orange => (251, 188, 4),
      Self::Yellow => (255, 214, 0),
      Self::Green => (52, 168, 83),
      Self::Purple => (138, 74, 218),
      Self::Pink => (233, 30, 99),
    }
  }
}

impl serde::Serialize for TabGroupColor {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: serde::Serializer,
  {
    serializer.serialize_str(self.as_session_str())
  }
}

impl<'de> serde::Deserialize<'de> for TabGroupColor {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: serde::Deserializer<'de>,
  {
    let raw = <String as serde::Deserialize<'de>>::deserialize(deserializer)?;
    // Be permissive so hand-edited session files don't hard-fail on unknown values. Unknown/empty
    // strings fall back to the default color.
    Ok(Self::parse_session_str(&raw).unwrap_or_default())
  }
}

#[derive(Debug, Clone)]
pub struct TabGroupState {
  pub id: TabGroupId,
  pub title: String,
  pub color: TabGroupColor,
  pub collapsed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LatestFrameMeta {
  pub pixmap_px: (u32, u32),
  pub viewport_css: (u32, u32),
  pub dpr: f32,
  pub wants_ticks: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadStatus {
  InProgress {
    received_bytes: u64,
    total_bytes: Option<u64>,
  },
  Completed,
  Failed {
    error: String,
  },
  Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadEntry {
  pub download_id: DownloadId,
  pub tab_id: TabId,
  pub url: String,
  pub file_name: String,
  pub path: PathBuf,
  pub status: DownloadStatus,
}

#[derive(Debug, Default)]
pub struct DownloadsState {
  pub downloads: Vec<DownloadEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DownloadProgressSummary {
  pub active_count: usize,
  pub received_bytes: u64,
  pub total_bytes: Option<u64>,
}

impl DownloadsState {
  pub fn active_count(&self) -> usize {
    self
      .downloads
      .iter()
      .filter(|d| matches!(d.status, DownloadStatus::InProgress { .. }))
      .count()
  }

  pub fn aggregate_progress(&self) -> DownloadProgressSummary {
    let mut active_count = 0usize;
    let mut received_bytes: u64 = 0;
    let mut total_bytes: u64 = 0;
    let mut total_known_for_all = true;

    for d in &self.downloads {
      let DownloadStatus::InProgress {
        received_bytes: received,
        total_bytes: total,
      } = d.status
      else {
        continue;
      };

      active_count += 1;
      received_bytes = received_bytes.saturating_add(received);

      match total {
        Some(total) => total_bytes = total_bytes.saturating_add(total),
        None => total_known_for_all = false,
      }
    }

    DownloadProgressSummary {
      active_count,
      received_bytes,
      total_bytes: if total_known_for_all && active_count > 0 {
        Some(total_bytes)
      } else {
        None
      },
    }
  }

  fn get_mut(&mut self, download_id: DownloadId) -> Option<&mut DownloadEntry> {
    self
      .downloads
      .iter_mut()
      .find(|d| d.download_id == download_id)
  }

  fn insert_or_update(&mut self, entry: DownloadEntry) {
    if let Some(existing) = self.get_mut(entry.download_id) {
      *existing = entry;
    } else {
      self.downloads.push(entry);
    }
  }
}

#[derive(Debug, Default)]
pub struct AppUpdate {
  /// Whether the front-end should schedule a repaint/redraw.
  pub request_redraw: bool,
  /// Whether the browser's global history store was mutated.
  ///
  /// Front-ends that persist history to disk can use this to decide when to flush new snapshots.
  pub history_changed: bool,
  /// Recommended full window title for the host window.
  pub set_window_title: Option<String>,
  /// A new pixmap is ready for upload; the state model does not store pixel buffers.
  pub frame_ready: Option<FrameReadyUpdate>,
  /// A new favicon is ready for upload.
  pub favicon_ready: Option<FaviconReadyUpdate>,
  /// The worker requested opening a `<select>` dropdown for a specific tab.
  ///
  /// Front-ends are expected to pick an anchor position (typically current pointer position or the
  /// control's screen-space rect if known).
  pub open_select_dropdown: Option<OpenSelectDropdownUpdate>,
  /// The worker requested opening a `<datalist>` suggestions popup for a specific tab.
  pub open_datalist: Option<OpenDatalistUpdate>,
}

pub struct FrameReadyUpdate {
  pub tab_id: TabId,
  pub pixmap: tiny_skia::Pixmap,
  pub viewport_css: (u32, u32),
  pub dpr: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FaviconMeta {
  pub size_px: (u32, u32),
}

pub struct FaviconReadyUpdate {
  pub tab_id: TabId,
  pub rgba: Vec<u8>,
  pub width: u32,
  pub height: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FindInPageState {
  pub open: bool,
  pub query: String,
  pub case_sensitive: bool,
  pub match_count: usize,
  pub active_match_index: Option<usize>,
}

impl Default for FindInPageState {
  fn default() -> Self {
    Self {
      open: false,
      query: String::new(),
      case_sensitive: false,
      match_count: 0,
      active_match_index: None,
    }
  }
}

impl std::fmt::Debug for FaviconReadyUpdate {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("FaviconReadyUpdate")
      .field("tab_id", &self.tab_id)
      .field("size_px", &(self.width, self.height))
      .field("rgba_len", &self.rgba.len())
      .finish()
  }
}

#[derive(Debug, Clone)]
pub struct OpenSelectDropdownUpdate {
  pub tab_id: TabId,
  pub select_node_id: usize,
  pub control: crate::tree::box_tree::SelectControl,
  /// Optional viewport-local CSS-pixel rect for positioning a dropdown popup.
  ///
  /// Some worker implementations only send cursor-anchored dropdown requests; for those, this will
  /// be `None` and front-ends should pick a reasonable anchor (e.g. current pointer position).
  pub anchor_css: Option<crate::geometry::Rect>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OpenDatalistUpdate {
  pub tab_id: TabId,
  pub input_node_id: usize,
  pub options: Vec<DatalistSuggestion>,
  /// Optional viewport-local CSS-pixel rect for positioning a datalist popup.
  pub anchor_css: Option<crate::geometry::Rect>,
}

impl std::fmt::Debug for FrameReadyUpdate {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("FrameReadyUpdate")
      .field("tab_id", &self.tab_id)
      .field("pixmap_px", &(self.pixmap.width(), self.pixmap.height()))
      .field("viewport_css", &self.viewport_css)
      .field("dpr", &self.dpr)
      .finish()
  }
}

#[derive(Debug)]
pub struct BrowserTabState {
  pub id: TabId,
  /// Which renderer process (if any) is currently responsible for this tab.
  ///
  /// Renderer process assignment is owned by the browser process manager; newly created tabs start
  /// unassigned and are expected to be assigned later.
  pub renderer_process: Option<RendererProcessId>,
  /// Optional site key recorded alongside renderer process assignment.
  ///
  /// This is primarily for debugging and future site isolation policies.
  pub site_key: Option<SiteKey>,
  /// Whether this tab is pinned in the tab strip.
  ///
  /// Invariant (enforced by [`BrowserAppState`]): all pinned tabs are stored contiguously at the
  /// start of [`BrowserAppState::tabs`].
  pub pinned: bool,
  pub group: Option<TabGroupId>,
  /// Shared cancellation generations for this tab.
  ///
  /// The UI thread can bump these counters (without blocking on the worker) to cancel in-flight
  /// navigation/paint work.
  pub cancel: CancelGens,
  /// Best-effort site isolation key snapshot derived from the latest navigation URL.
  pub site_key: Option<SiteKey>,
  /// URL shown in the address bar when this tab is active.
  ///
  /// This is driven by worker navigation events (e.g. [`WorkerToUi::NavigationCommitted`]), along
  /// with optimistic UI updates for typed navigations, and is *not* an authoritative history stack.
  pub current_url: Option<String>,
  /// The last URL reported by the worker as committed (after redirects).
  ///
  /// Unlike `current_url`, this is not affected by optimistic UI updates for typed navigations.
  pub committed_url: Option<String>,
  pub title: Option<String>,
  /// The last title reported by the worker as committed.
  ///
  /// Unlike `title`, this is preserved across optimistic UI updates that clear `title` during a
  /// pending navigation.
  pub committed_title: Option<String>,
  pub loading: bool,
  /// Whether the tab is currently considered unresponsive by the browser UI watchdog.
  ///
  /// This is set when `loading == true` but the UI has not observed any `WorkerToUi` messages for
  /// the tab within a configured timeout. It is cleared when new worker messages arrive or when
  /// the user dismisses the watchdog UI (e.g. "Wait" or "Reload").
  pub unresponsive: bool,
  /// Last time the UI observed activity from the renderer for this tab.
  ///
  /// This is updated when the browser UI receives any `WorkerToUi` message for the tab, and is
  /// also updated when the UI initiates a new navigation so the watchdog timeout starts from the
  /// user action even if the worker never responds.
  pub last_worker_msg_at: SystemTime,
  pub error: Option<String>,
  /// Optional non-fatal warning for this tab (e.g. viewport clamping).
  pub warning: Option<String>,
  /// Last stage heartbeat received from the worker (debug; may regress if heartbeats arrive
  /// out-of-order).
  pub stage: Option<StageHeartbeat>,
  /// Highest stage heartbeat observed during the current load (monotonic; user-facing).
  pub load_stage: Option<StageHeartbeat>,
  /// Monotonic progress fraction derived from the highest observed stage.
  pub load_progress: Option<f32>,
  pub can_go_back: bool,
  pub can_go_forward: bool,
  /// Per-tab page zoom factor.
  ///
  /// This affects how the windowed UI computes `viewport_css` + `dpr` for rendering:
  /// - higher zoom → fewer CSS pixels in the viewport + higher DPR
  /// - lower zoom → more CSS pixels in the viewport + lower DPR
  pub zoom: f32,
  pub hovered_url: Option<String>,
  pub cursor: CursorKind,
  pub find: FindInPageState,
  pub scroll_state: ScrollState,
  pub scroll_metrics: Option<ScrollMetrics>,
  pub latest_frame_meta: Option<LatestFrameMeta>,
  pub favicon_meta: Option<FaviconMeta>,
  debug_log: VecDeque<String>,
}

impl BrowserTabState {
  pub fn new(tab_id: TabId, initial_url: String) -> Self {
    let committed_url = initial_url.clone();
    let site_key = Url::parse(&committed_url)
      .ok()
      .map(|url| SiteKey::from_url(&url));
    Self {
      id: tab_id,
      renderer_process: None,
      site_key: None,
      pinned: false,
      group: None,
      cancel: CancelGens::new(),
      site_key,
      current_url: Some(initial_url),
      committed_url: Some(committed_url),
      title: None,
      committed_title: None,
      loading: false,
      unresponsive: false,
      last_worker_msg_at: SystemTime::UNIX_EPOCH,
      error: None,
      warning: None,
      stage: None,
      load_stage: None,
      load_progress: None,
      can_go_back: false,
      can_go_forward: false,
      zoom: crate::ui::zoom::DEFAULT_ZOOM,
      hovered_url: None,
      cursor: CursorKind::Default,
      find: FindInPageState::default(),
      scroll_state: ScrollState::default(),
      scroll_metrics: None,
      latest_frame_meta: None,
      favicon_meta: None,
      debug_log: VecDeque::new(),
    }
  }

  pub fn current_url(&self) -> Option<&str> {
    self.current_url.as_deref()
  }

  pub fn display_title(&self) -> &str {
    if let Some(title) = self.title.as_deref().filter(|t| !t.trim().is_empty()) {
      return title;
    }
    self.current_url().unwrap_or("New Tab")
  }

  /// Returns a deterministic monotonic progress fraction for a chrome loading indicator.
  ///
  /// - `None` when this tab is not loading.
  /// - `Some(0.0)` when loading but no stage heartbeat has been observed yet.
  pub fn chrome_loading_progress(&self) -> Option<f32> {
    crate::ui::chrome_loading_progress::chrome_loading_progress(self.loading, self.load_progress)
  }

  /// Validate + normalize an address-bar navigation and produce a `UiToWorker::Navigate` message.
  ///
  /// This applies a scheme allowlist for typed URLs (http/https/file/about), rejecting
  /// `javascript:` and unknown schemes. On failure, the returned error is intended for
  /// user-facing display.
  ///
  /// On success, this marks the tab as loading and updates `current_url` for immediate UI display.
  ///
  /// The worker remains the source of truth for the ultimately committed URL (e.g. after
  /// redirects) and navigation history/back-forward state.
  pub fn navigate_typed(&mut self, raw: &str) -> Result<UiToWorker, String> {
    let raw_trimmed = crate::ui::url::trim_ascii_whitespace(raw);

    let normalized = if raw_trimmed.starts_with('#') {
      let current = self
        .current_url
        .as_deref()
        .ok_or_else(|| "cannot navigate to a fragment without an active document".to_string())?;
      let current = Url::parse(current).map_err(|err| err.to_string())?;
      current
        .join(raw_trimmed)
        .map_err(|err| err.to_string())?
        .to_string()
    } else {
      match resolve_omnibox_input(raw_trimmed)? {
        crate::ui::OmniboxInputResolution::Url { url } => url,
        crate::ui::OmniboxInputResolution::Search { url, .. } => url,
      }
    };
    validate_user_navigation_url_scheme(&normalized)?;

    self.current_url = Some(normalized.clone());
    self.loading = true;
    self.unresponsive = false;
    self.last_worker_msg_at = SystemTime::now();
    self.error = None;
    self.title = None;
    self.stage = None;
    self.reset_load_progress();

    Ok(UiToWorker::Navigate {
      tab_id: self.id,
      url: normalized,
      reason: NavigationReason::TypedUrl,
    })
  }

  pub fn debug_log(&self) -> impl Iterator<Item = &str> {
    self.debug_log.iter().map(String::as_str)
  }

  fn reset_load_progress(&mut self) {
    self.load_stage = None;
    self.load_progress = Some(0.0);
  }

  fn clear_load_progress(&mut self) {
    self.load_stage = None;
    self.load_progress = None;
  }

  fn update_load_progress_for_stage(&mut self, stage: StageHeartbeat) {
    if !self.loading {
      return;
    }

    let prev = self
      .load_progress
      .filter(|p| p.is_finite())
      .map(|p| p.clamp(0.0, 1.0))
      .unwrap_or(0.0);

    let stage_progress = stage.loading_progress().clamp(0.0, 1.0);
    let next = prev.max(stage_progress);

    if stage_progress >= prev {
      self.load_stage = Some(stage);
    }
    self.load_progress = Some(next);
  }

  fn push_debug_log(&mut self, line: String) {
    if self.debug_log.len() >= DEBUG_LOG_CAPACITY {
      self.debug_log.pop_front();
    }
    self.debug_log.push_back(line);
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosedTabState {
  pub url: String,
  pub title: Option<String>,
  pub pinned: bool,
}

#[cfg(test)]
mod tab_tests {
  use super::BrowserTabState;
  use crate::ui::messages::{NavigationReason, UiToWorker};
  use crate::ui::TabId;

  #[test]
  fn typed_javascript_url_is_rejected() {
    let mut tab = BrowserTabState::new(TabId(1), "about:newtab".to_string());
    let before = tab.current_url.clone();
    assert!(tab.navigate_typed("javascript:alert(1)").is_err());
    assert!(!tab.loading);
    assert_eq!(tab.current_url, before);
  }

  #[test]
  fn typed_unknown_scheme_is_rejected() {
    let mut tab = BrowserTabState::new(TabId(1), "about:newtab".to_string());
    let before = tab.current_url.clone();
    assert!(tab.navigate_typed("foo:bar").is_err());
    assert!(!tab.loading);
    assert_eq!(tab.current_url, before);
  }

  #[test]
  fn typed_about_url_is_allowed() {
    let mut tab = BrowserTabState::new(TabId(1), "about:newtab".to_string());
    let msg = tab
      .navigate_typed("about:blank")
      .expect("about URL should be allowed");
    match msg {
      UiToWorker::Navigate {
        tab_id,
        url,
        reason,
      } => {
        assert_eq!(tab_id, TabId(1));
        assert_eq!(url, "about:blank");
        assert_eq!(reason, NavigationReason::TypedUrl);
      }
      other => panic!("expected Navigate, got {other:?}"),
    }

    assert_eq!(tab.current_url(), Some("about:blank"));
    assert_eq!(tab.error, None);
    assert!(tab.loading);
  }

  #[test]
  fn typed_bare_word_navigates_to_search() {
    let mut tab = BrowserTabState::new(TabId(1), "about:newtab".to_string());
    let msg = tab
      .navigate_typed("cats")
      .expect("bare words should be treated as search queries");
    match msg {
      UiToWorker::Navigate {
        tab_id,
        url,
        reason,
      } => {
        assert_eq!(tab_id, TabId(1));
        assert_eq!(url, "https://duckduckgo.com/?q=cats");
        assert_eq!(reason, NavigationReason::TypedUrl);
      }
      other => panic!("expected Navigate, got {other:?}"),
    }

    assert_eq!(tab.current_url(), Some("https://duckduckgo.com/?q=cats"));
    assert_eq!(tab.error, None);
    assert!(tab.loading);
  }

  #[test]
  fn typed_fragment_is_resolved_against_current_url() {
    let mut tab = BrowserTabState::new(TabId(1), "https://example.com/page.html".to_string());
    let msg = tab
      .navigate_typed("#target")
      .expect("fragment-only URL should resolve against current URL");
    match msg {
      UiToWorker::Navigate {
        tab_id,
        url,
        reason,
      } => {
        assert_eq!(tab_id, TabId(1));
        assert_eq!(url, "https://example.com/page.html#target");
        assert_eq!(reason, NavigationReason::TypedUrl);
      }
      other => panic!("expected Navigate, got {other:?}"),
    }

    assert_eq!(
      tab.current_url(),
      Some("https://example.com/page.html#target")
    );
    assert_eq!(tab.error, None);
    assert!(tab.loading);
  }
}

#[cfg(test)]
mod worker_message_validation_tests {
  use super::{BrowserAppState, BrowserTabState, FAVICON_MAX_EDGE_PX};
  use crate::scroll::{ScrollBounds, ScrollState};
  use crate::ui::browser_limits::BrowserLimits;
  use crate::ui::messages::{RenderedFrame, ScrollMetrics, TabId, WorkerToUi};

  fn app_with_single_tab(tab_id: TabId) -> BrowserAppState {
    let mut app = BrowserAppState::new();
    app.push_tab(
      BrowserTabState::new(tab_id, "about:newtab".to_string()),
      true,
    );
    app
  }

  #[test]
  fn invalid_favicon_byte_length_is_rejected() {
    let tab_id = TabId(1);
    let mut app = app_with_single_tab(tab_id);

    let update = app.apply_worker_msg(WorkerToUi::Favicon {
      tab_id,
      rgba: vec![0u8; 15],
      width: 2,
      height: 2,
    });

    assert!(update.favicon_ready.is_none());
    assert!(app.tab(tab_id).unwrap().favicon_meta.is_none());
    assert!(!update.request_redraw);
  }

  #[test]
  fn oversized_favicon_dimensions_are_rejected() {
    let tab_id = TabId(1);
    let mut app = app_with_single_tab(tab_id);

    let width = FAVICON_MAX_EDGE_PX + 1;
    let height = 1;
    let rgba_len = (width as usize) * (height as usize) * 4;

    let update = app.apply_worker_msg(WorkerToUi::Favicon {
      tab_id,
      rgba: vec![0u8; rgba_len],
      width,
      height,
    });

    assert!(update.favicon_ready.is_none());
    assert!(app.tab(tab_id).unwrap().favicon_meta.is_none());
    assert!(!update.request_redraw);
  }

  #[test]
  fn absurd_pixmap_size_is_rejected_and_does_not_update_latest_frame_meta() {
    let tab_id = TabId(1);
    let mut app = app_with_single_tab(tab_id);

    let limits = BrowserLimits::default();
    // Exceed the dimension limit without allocating a huge buffer.
    let pix_w = limits.max_dim_px + 1;
    let pix_h = 1;
    let pixmap = tiny_skia::Pixmap::new(pix_w, pix_h).expect("pixmap");
    let viewport_css = (pix_w, pix_h);

    let frame = RenderedFrame {
      pixmap,
      viewport_css,
      dpr: 1.0,
      scroll_state: ScrollState::default(),
      scroll_metrics: ScrollMetrics {
        viewport_css,
        scroll_css: (0.0, 0.0),
        bounds_css: ScrollBounds {
          min_x: 0.0,
          min_y: 0.0,
          max_x: 0.0,
          max_y: 0.0,
        },
        content_css: (0.0, 0.0),
      },
      wants_ticks: false,
    };

    let update = app.apply_worker_msg(WorkerToUi::FrameReady { tab_id, frame });

    assert!(update.frame_ready.is_none());
    assert!(app.tab(tab_id).unwrap().latest_frame_meta.is_none());
    assert!(!update.request_redraw);
  }
}

#[derive(Debug, Default)]
pub struct ChromeState {
  pub address_bar_text: String,
  /// True while the user is actively editing the address bar.
  ///
  /// While this is true, we avoid auto-syncing the address bar text from navigation events so
  /// in-progress input is not clobbered.
  pub address_bar_editing: bool,
  pub address_bar_has_focus: bool,
  pub omnibox: OmniboxUiState,
  /// One-frame request flag consumed by `chrome_ui` to focus the address bar.
  pub request_focus_address_bar: bool,
  /// One-frame request flag consumed by `chrome_ui` to select all text in the address bar.
  pub request_select_all_address_bar: bool,
  /// Cached remote search query suggestions (typeahead) for the current omnibox query.
  ///
  /// This is egui-agnostic state: the windowed front-end owns the background fetch worker and
  /// stores the latest results here for consumption by the omnibox suggestion engine.
  pub remote_search_cache: RemoteSearchSuggestCache,
  /// Whether the chrome "Appearance" popup is currently open.
  pub appearance_popup_open: bool,
  /// Whether the chrome History side panel is currently visible.
  pub history_panel_open: bool,
  /// Search/filter query for the History panel.
  pub history_search_text: String,
  /// Whether the chrome Bookmarks Manager side panel is currently visible.
  pub bookmarks_manager_open: bool,
  /// Search/filter query for the Bookmarks Manager panel.
  pub bookmarks_manager_search_text: String,
  /// Whether the bookmarks bar is visible.
  pub bookmarks_bar_visible: bool,
  /// Whether the browser-style in-window menu bar is visible.
  ///
  /// This is a UI-only preference; on macOS it defaults to hidden so the app feels closer to a
  /// native "unified toolbar" browser.
  pub show_menu_bar: bool,
  pub tab_search: TabSearchState,
  /// The currently open tab-strip context menu (right-click on a tab label/icon), if any.
  ///
  /// This is chrome-only UI state (not part of the worker protocol).
  pub open_tab_context_menu: Option<OpenTabContextMenuState>,
  /// Last known tab context menu rect (in egui points), used for click-outside dismissal.
  pub tab_context_menu_rect: Option<(f32, f32, f32, f32)>,
  /// Transient tab-strip drag state (used by the optional egui chrome).
  ///
  /// Kept behind the `browser_ui` feature gate so the core renderer does not depend on egui types.
  #[cfg(feature = "browser_ui")]
  pub dragging_tab_id: Option<TabId>,
  #[cfg(feature = "browser_ui")]
  pub drag_start_pointer_pos: Option<(f32, f32)>,
  /// Screen-space rect of the tab when the drag started (used for rendering a floating drag ghost).
  #[cfg(feature = "browser_ui")]
  pub drag_start_tab_rect: Option<(f32, f32, f32, f32)>,
  /// Monotonic counter incremented each time a tab drag starts.
  ///
  /// This is used to namespace egui animation ids so drag animations (lift/indicator/pulse) reset
  /// cleanly between separate drag sessions (even when dragging the same tab from the same
  /// position).
  #[cfg(feature = "browser_ui")]
  pub tab_drag_session: u64,
  #[cfg(feature = "browser_ui")]
  pub drag_target_index: Option<usize>,
  /// Transient "drag a hovered link to the address bar" state.
  ///
  /// When the user begins a primary-button drag while the active tab reports a `hovered_url`, the
  /// chrome layer can treat it as a link drag candidate and, on drop over the address bar, trigger
  /// a navigation.
  ///
  /// This is kept behind the `browser_ui` feature gate so the core renderer does not depend on egui
  /// types.
  #[cfg(feature = "browser_ui")]
  pub link_drag_url: Option<String>,
  #[cfg(feature = "browser_ui")]
  pub link_drag_start_pos: Option<(f32, f32)>,
  #[cfg(feature = "browser_ui")]
  pub link_drag_active: bool,
  /// Per-tab close animation state, keyed by tab id.
  ///
  /// This lives in UI-only state so tab closes can be animated consistently regardless of how they
  /// were initiated (tab strip, keyboard shortcut, menu bar, etc).
  #[cfg(feature = "browser_ui")]
  pub closing_tabs: HashMap<TabId, ClosingTabState>,
}

/// Close animation state for a tab.
#[cfg(feature = "browser_ui")]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClosingTabState {
  pub start_time: f64,
  pub duration: f32,
}

#[cfg(feature = "browser_ui")]
impl ClosingTabState {
  pub fn progress(&self, now: f64) -> f32 {
    if self.duration <= 0.0 {
      return 1.0;
    }
    let dt = (now - self.start_time).max(0.0);
    (dt as f32 / self.duration).clamp(0.0, 1.0)
  }
}

impl ChromeState {
  #[cfg(feature = "browser_ui")]
  pub fn clear_tab_drag(&mut self) {
    self.dragging_tab_id = None;
    self.drag_start_pointer_pos = None;
    self.drag_start_tab_rect = None;
    self.drag_target_index = None;
  }

  #[cfg(feature = "browser_ui")]
  pub fn clear_link_drag(&mut self) {
    self.link_drag_url = None;
    self.link_drag_start_pos = None;
    self.link_drag_active = false;
  }

  /// Clear any close-animation state for `tab_id`.
  #[cfg(feature = "browser_ui")]
  pub fn clear_tab_close(&mut self, tab_id: TabId) {
    self.closing_tabs.remove(&tab_id);
  }

  /// Returns the close animation progress for `tab_id` (0.0..=1.0), if the tab is currently
  /// closing.
  #[cfg(feature = "browser_ui")]
  pub fn tab_close_progress(&self, tab_id: TabId, now: f64) -> Option<f32> {
    self.closing_tabs.get(&tab_id).map(|s| s.progress(now))
  }

  /// Handle a close-tab request, returning `true` if the caller should perform the actual close.
  ///
  /// When motion is enabled, the first request starts an animation and returns `false`; a later
  /// request (after the animation duration has elapsed) returns `true`.
  ///
  /// When motion is disabled (reduced motion or egui animations disabled), this returns `true`
  /// immediately.
  #[cfg(feature = "browser_ui")]
  pub fn request_close_tab(
    &mut self,
    tab_id: TabId,
    now: f64,
    motion: crate::ui::motion::UiMotion,
    animations_enabled: bool,
  ) -> bool {
    let duration = motion.durations.tab_close;
    let motion_enabled = motion.enabled && animations_enabled && duration > 0.0;
    if !motion_enabled {
      return true;
    }

    match self.closing_tabs.get(&tab_id).copied() {
      None => {
        self.closing_tabs.insert(
          tab_id,
          ClosingTabState {
            start_time: now,
            duration,
          },
        );
        // If the user is currently dragging this tab, cancel the drag so the tab strip doesn't try
        // to reorder a tab that is disappearing.
        if self.dragging_tab_id == Some(tab_id) {
          self.clear_tab_drag();
        }
        false
      }
      Some(state) => state.progress(now) >= 1.0,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OpenTabContextMenuState {
  pub tab_id: TabId,
  pub anchor_points: (f32, f32),
  /// Focus token for the invoking tab control, used to restore focus when the context menu closes.
  pub opener_focus: Option<UiFocusToken>,
}

#[derive(Debug, Clone)]
pub struct RemoteSearchSuggestCache {
  pub query: String,
  pub suggestions: Vec<String>,
  pub fetched_at: SystemTime,
}

impl Default for RemoteSearchSuggestCache {
  fn default() -> Self {
    Self {
      query: String::new(),
      suggestions: Vec::new(),
      fetched_at: SystemTime::UNIX_EPOCH,
    }
  }
}

#[derive(Debug, Default)]
pub struct TabSearchState {
  pub open: bool,
  pub query: String,
  pub selected: usize,
}

/// Egui-agnostic UI state for the address bar omnibox dropdown.
#[derive(Debug, Clone)]
pub struct OmniboxUiState {
  pub open: bool,
  pub selected: Option<usize>,
  /// The address bar contents before the user started navigating suggestions.
  ///
  /// This is captured when the selection first moves away from "no selection", so Escape can
  /// restore the original typed input after previewing a suggestion.
  pub original_input: Option<String>,
  /// The raw address bar input that `suggestions` were last built for.
  pub last_built_for_input: String,
  /// `RemoteSearchSuggestCache::fetched_at` value observed when building `suggestions`.
  ///
  /// This is used to refresh the omnibox dropdown when remote suggestions arrive for the current
  /// query, even if the user has paused typing.
  pub last_built_remote_fetched_at: SystemTime,
  pub suggestions: Vec<OmniboxSuggestion>,
}

impl Default for OmniboxUiState {
  fn default() -> Self {
    Self {
      open: false,
      selected: None,
      original_input: None,
      last_built_for_input: String::new(),
      last_built_remote_fetched_at: SystemTime::UNIX_EPOCH,
      suggestions: Vec::new(),
    }
  }
}

impl OmniboxUiState {
  pub fn reset(&mut self) {
    // Keep allocations around: omnibox open/close is frequent, and rebuilding the suggestion list
    // already does enough work without also thrashing heap capacity.
    self.open = false;
    self.selected = None;
    self.original_input = None;
    self.last_built_for_input.clear();
    self.last_built_remote_fetched_at = SystemTime::UNIX_EPOCH;
    self.suggestions.clear();
  }
}

#[derive(Debug)]
pub struct BrowserAppState {
  pub tabs: Vec<BrowserTabState>,
  pub active_tab: Option<TabId>,
  pub closed_tabs: Vec<ClosedTabState>,
  pub history: GlobalHistoryStore,
  pub visited: VisitedUrlStore,
  pub tab_groups: HashMap<TabGroupId, TabGroupState>,
  pub chrome: ChromeState,
  pub downloads: DownloadsState,
  pub appearance: AppearanceSettings,
  session_revision: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoveTabResult {
  /// New active tab id (only set when the closed tab was the active tab).
  pub new_active: Option<TabId>,
  /// New tab created as a recovery path when the tab list becomes unexpectedly empty.
  pub created_tab: Option<TabId>,
}

impl BrowserAppState {
  pub fn new() -> Self {
    Self {
      tabs: Vec::new(),
      active_tab: None,
      closed_tabs: Vec::new(),
      history: GlobalHistoryStore::default(),
      visited: VisitedUrlStore::new(),
      tab_groups: HashMap::new(),
      chrome: ChromeState::default(),
      downloads: DownloadsState::default(),
      appearance: AppearanceSettings::default(),
      session_revision: 0,
    }
  }

  pub fn new_with_initial_tab(initial_url: String) -> Self {
    let url = if initial_url.trim().is_empty() {
      about_pages::ABOUT_NEWTAB.to_string()
    } else {
      initial_url
    };
    let tab_id = TabId::new();
    let mut app = Self::new();
    app.push_tab(BrowserTabState::new(tab_id, url), true);
    app
  }

  pub fn active_tab_id(&self) -> Option<TabId> {
    self.active_tab
  }

  /// Monotonic revision counter for state that affects the persisted session snapshot.
  ///
  /// Front-ends can compare this value before/after a frame to detect whether autosave should run.
  pub fn session_revision(&self) -> u64 {
    self.session_revision
  }

  fn bump_session_revision(&mut self) {
    self.session_revision = self.session_revision.wrapping_add(1);
  }

  pub fn tab(&self, tab_id: TabId) -> Option<&BrowserTabState> {
    self.tabs.iter().find(|t| t.id == tab_id)
  }

  pub fn tab_mut(&mut self, tab_id: TabId) -> Option<&mut BrowserTabState> {
    self.tabs.iter_mut().find(|t| t.id == tab_id)
  }

  /// Record the renderer process assignment for `tab_id`.
  ///
  /// This is intentionally not part of the persisted browser session; it is owned by the browser
  /// process manager and can change across runs.
  ///
  /// Returns `true` if `tab_id` existed and was updated.
  pub fn set_tab_renderer(
    &mut self,
    tab_id: TabId,
    process_id: RendererProcessId,
    site_key: Option<SiteKey>,
  ) -> bool {
    let Some(tab) = self.tab_mut(tab_id) else {
      return false;
    };
    tab.renderer_process = Some(process_id);
    tab.site_key = site_key;
    true
  }

  /// Get the renderer process currently assigned to `tab_id`, if any.
  pub fn tab_renderer(&self, tab_id: TabId) -> Option<RendererProcessId> {
    self.tab(tab_id).and_then(|t| t.renderer_process)
  }

  pub fn active_tab(&self) -> Option<&BrowserTabState> {
    self.active_tab.and_then(|id| self.tab(id))
  }

  pub fn active_tab_mut(&mut self) -> Option<&mut BrowserTabState> {
    let id = self.active_tab?;
    self.tab_mut(id)
  }

  pub fn set_active_tab(&mut self, tab_id: TabId) -> bool {
    if self.active_tab == Some(tab_id) {
      return false;
    }
    if self.tab(tab_id).is_none() {
      return false;
    }
    self.active_tab = Some(tab_id);
    if let Some(group_id) = self.tab(tab_id).and_then(|t| t.group) {
      if let Some(group) = self.tab_groups.get_mut(&group_id) {
        group.collapsed = false;
      }
    }
    // Switching tabs should always reflect the newly active tab URL in the address bar. If the
    // user was typing, cancel that edit rather than carrying the partially typed URL across tabs.
    self.chrome.address_bar_editing = false;
    self.chrome.omnibox.reset();
    self.sync_address_bar_to_active();
    if let Some(tab) = self.tab_mut(tab_id) {
      tab.hovered_url = None;
      tab.cursor = CursorKind::Default;
    }
    self.bump_session_revision();
    true
  }

  pub fn set_active(&mut self, tab_id: TabId) {
    let _ = self.set_active_tab(tab_id);
  }

  fn pinned_len(&self) -> usize {
    let pinned = self.tabs.iter().take_while(|t| t.pinned).count();
    debug_assert!(
      self.tabs[pinned..].iter().all(|t| !t.pinned),
      "pinned tabs must be contiguous at the start of BrowserAppState::tabs"
    );
    pinned
  }

  pub fn pin_tab(&mut self, tab_id: TabId) -> bool {
    let Some(idx) = self.tabs.iter().position(|t| t.id == tab_id) else {
      return false;
    };
    if self.tabs[idx].pinned {
      return false;
    }
    let pinned_end = self.pinned_len();
    let mut tab = self.tabs.remove(idx);
    tab.pinned = true;
    tab.group = None;
    let insert_at = pinned_end.min(self.tabs.len());
    self.tabs.insert(insert_at, tab);
    self.prune_empty_tab_groups();
    self.bump_session_revision();
    true
  }

  pub fn unpin_tab(&mut self, tab_id: TabId) -> bool {
    let Some(idx) = self.tabs.iter().position(|t| t.id == tab_id) else {
      return false;
    };
    if !self.tabs[idx].pinned {
      return false;
    }
    let pinned_end = self.pinned_len();
    let mut tab = self.tabs.remove(idx);
    tab.pinned = false;
    tab.group = None;
    let insert_at = pinned_end.saturating_sub(1).min(self.tabs.len());
    self.tabs.insert(insert_at, tab);
    self.prune_empty_tab_groups();
    self.bump_session_revision();
    true
  }

  pub fn toggle_pin_tab(&mut self, tab_id: TabId) -> bool {
    match self.tab(tab_id).map(|t| t.pinned) {
      Some(true) => self.unpin_tab(tab_id),
      Some(false) => self.pin_tab(tab_id),
      None => false,
    }
  }

  pub fn push_tab(&mut self, tab: BrowserTabState, make_active: bool) {
    let tab_id = tab.id;
    let mut tab = tab;
    if tab.pinned {
      tab.group = None;
      let idx = self.pinned_len();
      self.tabs.insert(idx, tab);
    } else {
      self.tabs.push(tab);
    }
    if make_active || self.active_tab.is_none() {
      self.active_tab = Some(tab_id);
      self.chrome.address_bar_editing = false;
      self.chrome.omnibox.reset();
      self.sync_address_bar_to_active();
    }
    self.bump_session_revision();
  }

  pub fn create_tab(&mut self, initial_url: Option<String>) -> TabId {
    let url = initial_url.unwrap_or_else(|| about_pages::ABOUT_NEWTAB.to_string());
    let tab_id = TabId::new();
    self.push_tab(BrowserTabState::new(tab_id, url), true);
    tab_id
  }

  /// Reorder a tab in-place within the tab strip.
  ///
  /// This is used by the egui chrome tab-strip drag-to-reorder implementation.
  ///
  /// Returns `true` if a reorder was applied.
  pub fn reorder_tab(&mut self, tab_id: TabId, target_index: usize) -> bool {
    let len = self.tabs.len();
    if len == 0 {
      return false;
    }
    let Some(from_idx) = self.tabs.iter().position(|t| t.id == tab_id) else {
      return false;
    };

    // Preserve the invariant that pinned tabs are stored contiguously at the front of the tab
    // list: pinned tabs can only be reordered within the pinned segment, and unpinned tabs can only
    // be reordered within the unpinned segment.
    let pinned_end = self.pinned_len();
    let is_pinned = self.tabs[from_idx].pinned;
    let mut target_index = target_index.min(len - 1);
    if is_pinned {
      target_index = target_index.min(pinned_end.saturating_sub(1));
    } else {
      target_index = target_index.max(pinned_end);
    }
    if from_idx == target_index {
      return false;
    }

    let tab = self.tabs.remove(from_idx);
    self.tabs.insert(target_index, tab);
    self.bump_session_revision();
    true
  }

  pub fn close_tab(&mut self, tab_id: TabId) {
    let _ = self.remove_tab(tab_id);
  }

  pub fn clear_history(&mut self) {
    self.history.clear();
    self.visited.clear();
  }

  /// Populate [`BrowserAppState::visited`] from the persisted [`BrowserAppState::history`].
  ///
  /// This is intended for browser startup so omnibox suggestions are immediately useful after a
  /// restart, before any new navigation commits are observed.
  pub fn seed_visited_from_history(&mut self) {
    self.visited.seed_from_global_history(&self.history);
  }

  fn tab_group_range(&self, group_id: TabGroupId) -> Option<std::ops::Range<usize>> {
    let mut start: Option<usize> = None;
    let mut end: usize = 0;
    for (idx, tab) in self.tabs.iter().enumerate() {
      if tab.group == Some(group_id) {
        if start.is_none() {
          start = Some(idx);
        }
        end = idx + 1;
      } else if start.is_some() {
        break;
      }
    }
    start.map(|s| s..end)
  }

  fn prune_empty_tab_groups(&mut self) {
    let mut in_use = HashSet::new();
    for tab in &self.tabs {
      if let Some(group_id) = tab.group {
        in_use.insert(group_id);
      }
    }
    self.tab_groups.retain(|id, _| in_use.contains(id));
  }

  fn adjust_insertion_index_to_avoid_splitting_groups(&self, mut idx: usize) -> usize {
    if idx == 0 || idx >= self.tabs.len() {
      return idx;
    }
    let left = self.tabs[idx - 1].group;
    let right = self.tabs[idx].group;
    if left.is_some() && left == right {
      let group_id = left.expect("checked is_some above");
      // Insert after the group block to preserve contiguity.
      if let Some(range) = self.tab_group_range(group_id) {
        idx = range.end;
      }
    }
    idx
  }

  pub fn create_group_with_tabs(&mut self, tab_ids: &[TabId]) -> TabGroupId {
    if tab_ids.is_empty() {
      return TabGroupId(0);
    }

    let selected: HashSet<TabId> = tab_ids
      .iter()
      .copied()
      .filter(|tab_id| self.tab(*tab_id).is_some_and(|t| !t.pinned))
      .collect();
    if selected.is_empty() {
      return TabGroupId(0);
    }

    let mut remaining = Vec::with_capacity(self.tabs.len());
    let mut extracted = Vec::new();
    let mut insert_idx: Option<usize> = None;

    for tab in self.tabs.drain(..) {
      if selected.contains(&tab.id) {
        if insert_idx.is_none() {
          insert_idx = Some(remaining.len());
        }
        extracted.push(tab);
      } else {
        remaining.push(tab);
      }
    }
    self.tabs = remaining;

    let Some(insert_idx) = insert_idx else {
      // None of the requested tabs exist.
      return TabGroupId(0);
    };

    for tab in &mut extracted {
      tab.group = None;
    }

    let group_id = TabGroupId::new();
    self.tab_groups.insert(
      group_id,
      TabGroupState {
        id: group_id,
        title: "Group".to_string(),
        color: TabGroupColor::default(),
        collapsed: false,
      },
    );

    let insert_idx = self.adjust_insertion_index_to_avoid_splitting_groups(insert_idx);

    for tab in &mut extracted {
      tab.group = Some(group_id);
    }

    self.tabs.splice(insert_idx..insert_idx, extracted);
    self.prune_empty_tab_groups();
    self.bump_session_revision();
    group_id
  }

  pub fn add_tab_to_group(&mut self, tab_id: TabId, group_id: TabGroupId) {
    if !self.tab_groups.contains_key(&group_id) {
      return;
    }

    if self.tab(tab_id).is_some_and(|t| t.pinned) {
      // Pinned tabs cannot be grouped.
      return;
    }

    // If the tab is currently grouped elsewhere, ungroup it first (this may move it).
    if self
      .tabs
      .iter()
      .find(|t| t.id == tab_id)
      .and_then(|t| t.group)
      .is_some_and(|existing| existing != group_id)
    {
      self.remove_tab_from_group(tab_id);
    }

    let Some(src_idx) = self.tabs.iter().position(|t| t.id == tab_id) else {
      return;
    };
    if self.tabs[src_idx].group == Some(group_id) {
      return;
    }

    let Some(range) = self.tab_group_range(group_id) else {
      // The group exists, but has no tabs; repair by removing it.
      self.tab_groups.remove(&group_id);
      self.bump_session_revision();
      return;
    };

    let mut insert_idx = range.end;
    if src_idx < insert_idx {
      insert_idx = insert_idx.saturating_sub(1);
    }

    let mut tab = self.tabs.remove(src_idx);
    tab.group = Some(group_id);
    self.tabs.insert(insert_idx, tab);

    if self.active_tab == Some(tab_id) {
      if let Some(group) = self.tab_groups.get_mut(&group_id) {
        group.collapsed = false;
      }
    }

    self.prune_empty_tab_groups();
    self.bump_session_revision();
  }

  pub fn remove_tab_from_group(&mut self, tab_id: TabId) {
    let Some(idx) = self.tabs.iter().position(|t| t.id == tab_id) else {
      return;
    };
    let Some(group_id) = self.tabs[idx].group else {
      return;
    };

    let Some(range) = self.tab_group_range(group_id) else {
      // The tab claims to be grouped, but the group isn't contiguous; treat as ungrouped.
      self.tabs[idx].group = None;
      self.prune_empty_tab_groups();
      self.bump_session_revision();
      return;
    };

    let start = range.start;
    let end = range.end;

    if idx == start || idx + 1 == end {
      self.tabs[idx].group = None;
    } else {
      let mut tab = self.tabs.remove(idx);
      tab.group = None;
      let insert_idx = end.saturating_sub(1);
      self.tabs.insert(insert_idx, tab);
    }

    self.prune_empty_tab_groups();
    self.bump_session_revision();
  }

  pub fn set_group_title(&mut self, group_id: TabGroupId, title: String) {
    let mut changed = false;
    if let Some(group) = self.tab_groups.get_mut(&group_id) {
      if group.title != title {
        group.title = title;
        changed = true;
      }
    }
    if changed {
      self.bump_session_revision();
    }
  }

  pub fn set_group_color(&mut self, group_id: TabGroupId, color: TabGroupColor) {
    let mut changed = false;
    if let Some(group) = self.tab_groups.get_mut(&group_id) {
      if group.color != color {
        group.color = color;
        changed = true;
      }
    }
    if changed {
      self.bump_session_revision();
    }
  }

  pub fn toggle_group_collapsed(&mut self, group_id: TabGroupId) {
    let active_in_group = self
      .active_tab
      .and_then(|id| self.tab(id))
      .is_some_and(|t| t.group == Some(group_id));
    let mut changed = false;
    if let Some(group) = self.tab_groups.get_mut(&group_id) {
      let before = group.collapsed;
      if active_in_group {
        group.collapsed = false;
      } else {
        group.collapsed = !group.collapsed;
      }
      changed = group.collapsed != before;
    }
    if changed {
      self.bump_session_revision();
    }
  }

  pub fn ungroup(&mut self, group_id: TabGroupId) {
    let mut changed = false;
    for tab in &mut self.tabs {
      if tab.group == Some(group_id) {
        tab.group = None;
        changed = true;
      }
    }
    if self.tab_groups.remove(&group_id).is_some() {
      changed = true;
    }
    if changed {
      self.bump_session_revision();
    }
  }

  /// Drag-style reordering helper that preserves tab group invariants.
  ///
  /// This is intended to be used by the chrome tab strip drag/reorder UX.
  ///
  /// Rules:
  /// - Dragging a grouped tab outside its group removes it from the group.
  /// - Dragging an ungrouped tab into a group region adds it to that group.
  /// - Dragging within a group preserves membership and reorders within the group.
  pub fn drag_reorder_tab(&mut self, tab_id: TabId, dst_index: usize) -> bool {
    let Some(src_idx) = self.tabs.iter().position(|t| t.id == tab_id) else {
      return false;
    };

    let mut tab = self.tabs.remove(src_idx);
    let old_group = tab.group;

    let mut insert_idx = dst_index.min(self.tabs.len());

    let pinned_end = self.pinned_len();
    if tab.pinned {
      // Pinned tabs are kept in a fixed leading segment and cannot be grouped.
      tab.group = None;
      insert_idx = insert_idx.min(pinned_end);
      let changed = insert_idx != src_idx || tab.group != old_group;
      self.tabs.insert(insert_idx, tab);
      self.prune_empty_tab_groups();
      if changed {
        self.bump_session_revision();
      }
      return changed;
    }
    // Ungrouped tabs cannot be inserted into the pinned segment.
    insert_idx = insert_idx.max(pinned_end);

    let left_group = insert_idx
      .checked_sub(1)
      .and_then(|idx| self.tabs.get(idx))
      .and_then(|t| t.group);
    let right_group = self.tabs.get(insert_idx).and_then(|t| t.group);

    let inferred_group = if left_group.is_some() && left_group == right_group {
      left_group
    } else if let Some(left) = left_group {
      if right_group.is_none() {
        Some(left)
      } else {
        None
      }
    } else {
      right_group
    };

    let new_group = match old_group {
      Some(group_id) => {
        // After removing this tab, check whether the destination is still within the group block.
        if let Some(range) = self.tab_group_range(group_id) {
          if insert_idx >= range.start && insert_idx <= range.end {
            Some(group_id)
          } else {
            inferred_group
          }
        } else {
          // The tab was the last in its group; treat it as an ungrouped tab being inserted.
          inferred_group
        }
      }
      None => inferred_group,
    }
    .filter(|group_id| self.tab_groups.contains_key(group_id));

    tab.group = new_group;

    if self.active_tab == Some(tab_id) {
      if let Some(group_id) = tab.group {
        if let Some(group) = self.tab_groups.get_mut(&group_id) {
          group.collapsed = false;
        }
      }
    }

    if tab.group.is_none() {
      insert_idx = self.adjust_insertion_index_to_avoid_splitting_groups(insert_idx);
    }

    let changed = insert_idx != src_idx || tab.group != old_group;
    self.tabs.insert(insert_idx, tab);
    self.prune_empty_tab_groups();
    if changed {
      self.bump_session_revision();
    }
    changed
  }

  /// Removes a tab, returning the new active tab if the active tab changed.
  ///
  /// Invariant: closing the last remaining tab is a no-op.
  pub fn remove_tab(&mut self, tab_id: TabId) -> RemoveTabResult {
    let Some(idx) = self.tabs.iter().position(|t| t.id == tab_id) else {
      return RemoveTabResult {
        new_active: None,
        created_tab: None,
      };
    };

    if self.tabs.len() == 1 {
      return RemoveTabResult {
        new_active: None,
        created_tab: None,
      };
    }

    #[cfg(feature = "browser_ui")]
    {
      self.chrome.clear_tab_close(tab_id);
      if self.chrome.dragging_tab_id == Some(tab_id) {
        self.chrome.clear_tab_drag();
      }
    }

    let closed = self.tabs.remove(idx);
    self.prune_empty_tab_groups();
    self.push_closed_tab_state(ClosedTabState {
      url: closed
        .committed_url
        .clone()
        .or_else(|| closed.current_url.clone())
        .unwrap_or_else(|| about_pages::ABOUT_NEWTAB.to_string()),
      title: closed
        .committed_title
        .clone()
        .or_else(|| closed.title.clone()),
      pinned: closed.pinned,
    });
    self.bump_session_revision();

    let was_active = self.active_tab == Some(tab_id);
    if !was_active {
      return RemoveTabResult {
        new_active: None,
        created_tab: None,
      };
    }

    // Prefer the tab that shifted into the removed index, otherwise the new last tab.
    let new_active = self
      .tabs
      .get(idx)
      .or_else(|| self.tabs.last())
      .map(|tab| tab.id);
    let Some(new_active) = new_active else {
      // This should be unreachable because we already handled the empty-tabs case above. Recover
      // by creating a new tab so we never panic in production code.
      let new_tab_id = TabId::new();
      self.push_tab(
        BrowserTabState::new(new_tab_id, "about:newtab".to_string()),
        true,
      );
      return RemoveTabResult {
        new_active: Some(new_tab_id),
        created_tab: Some(new_tab_id),
      };
    };
    let _ = self.set_active_tab(new_active);
    RemoveTabResult {
      new_active: Some(new_active),
      created_tab: None,
    }
  }

  /// Close all tabs except `tab_id`, returning the ids of closed tabs.
  ///
  /// Invariant: if `tab_id` exists, at least one tab remains (the kept tab).
  pub fn close_other_tabs(&mut self, tab_id: TabId) -> Vec<TabId> {
    if self.tabs.len() <= 1 {
      return Vec::new();
    }
    let Some(keep_idx) = self.tabs.iter().position(|t| t.id == tab_id) else {
      return Vec::new();
    };

    let mut tabs = std::mem::take(&mut self.tabs);
    let kept = tabs.remove(keep_idx);

    let mut closed_ids = Vec::new();
    for closed in tabs {
      #[cfg(feature = "browser_ui")]
      self.chrome.clear_tab_close(closed.id);
      closed_ids.push(closed.id);
      self.push_closed_tab_state(ClosedTabState {
        url: closed
          .committed_url
          .clone()
          .or_else(|| closed.current_url.clone())
          .unwrap_or_else(|| about_pages::ABOUT_NEWTAB.to_string()),
        title: closed
          .committed_title
          .clone()
          .or_else(|| closed.title.clone()),
        pinned: closed.pinned,
      });
    }

    self.tabs = vec![kept];
    let _ = self.set_active_tab(tab_id);
    self.prune_empty_tab_groups();
    self.bump_session_revision();
    closed_ids
  }

  /// Close all tabs to the right of `tab_id`, returning the ids of closed tabs.
  ///
  /// Invariant: closing tabs to the right is a no-op if `tab_id` is the last tab (or doesn't
  /// exist), and never makes the tab list empty.
  pub fn close_tabs_to_right(&mut self, tab_id: TabId) -> Vec<TabId> {
    if self.tabs.len() <= 1 {
      return Vec::new();
    }
    let Some(idx) = self.tabs.iter().position(|t| t.id == tab_id) else {
      return Vec::new();
    };
    if idx + 1 >= self.tabs.len() {
      return Vec::new();
    }

    let active_id = self.active_tab_id();
    let active_idx = active_id.and_then(|id| self.tabs.iter().position(|t| t.id == id));

    let drained = self.tabs.drain((idx + 1)..).collect::<Vec<_>>();
    let mut closed_ids = Vec::new();
    for closed in drained {
      #[cfg(feature = "browser_ui")]
      self.chrome.clear_tab_close(closed.id);
      closed_ids.push(closed.id);
      self.push_closed_tab_state(ClosedTabState {
        url: closed
          .committed_url
          .clone()
          .or_else(|| closed.current_url.clone())
          .unwrap_or_else(|| about_pages::ABOUT_NEWTAB.to_string()),
        title: closed
          .committed_title
          .clone()
          .or_else(|| closed.title.clone()),
        pinned: closed.pinned,
      });
    }

    if active_idx.is_some_and(|active_idx| active_idx > idx) {
      let _ = self.set_active_tab(tab_id);
    }

    // Defensive fallback: ensure `active_tab` stays valid.
    let active_is_valid = self
      .active_tab_id()
      .is_some_and(|id| self.tabs.iter().any(|t| t.id == id));
    if !active_is_valid {
      let _ = self.set_active_tab(tab_id);
      if self.active_tab.is_none() {
        self.active_tab = self.tabs.first().map(|t| t.id);
      }
    }

    self.prune_empty_tab_groups();
    self.bump_session_revision();
    closed_ids
  }

  fn push_closed_tab_state(&mut self, closed: ClosedTabState) {
    if CLOSED_TAB_STACK_CAPACITY == 0 {
      return;
    }
    if self.closed_tabs.len() >= CLOSED_TAB_STACK_CAPACITY {
      // Drop the oldest entries first so `pop_closed_tab` behaves like a typical "reopen last
      // closed tab" stack.
      let overflow = self.closed_tabs.len() + 1 - CLOSED_TAB_STACK_CAPACITY;
      self.closed_tabs.drain(0..overflow);
    }
    self.closed_tabs.push(closed);
  }

  pub fn pop_closed_tab(&mut self) -> Option<ClosedTabState> {
    self.closed_tabs.pop()
  }

  pub fn sync_address_bar_to_active(&mut self) {
    if self.chrome.address_bar_editing {
      return;
    }
    let Some(active) = self.active_tab() else {
      self.chrome.address_bar_text.clear();
      return;
    };
    self.chrome.address_bar_text = active.current_url().map(str::to_string).unwrap_or_default();
  }

  pub fn set_address_bar_editing(&mut self, editing: bool) {
    self.chrome.address_bar_editing = editing;
    self.chrome.address_bar_has_focus = editing;
    if !editing {
      self.chrome.omnibox.reset();
      self.sync_address_bar_to_active();
    }
  }

  pub fn set_address_bar_text(&mut self, text: String) {
    self.chrome.address_bar_text = text;
  }

  pub fn commit_address_bar(&mut self) -> Result<String, String> {
    let tab_id = self.active_tab.ok_or_else(|| "no active tab".to_string())?;

    let raw = crate::ui::url::trim_ascii_whitespace(&self.chrome.address_bar_text);
    if raw.is_empty() {
      return Err("empty URL".to_string());
    }

    let normalized = if raw.starts_with('#') {
      let current = self
        .tab(tab_id)
        .and_then(|t| t.current_url.as_deref())
        .ok_or_else(|| "cannot navigate to a fragment without an active document".to_string())?;
      let current = Url::parse(current).map_err(|err| err.to_string())?;
      current
        .join(raw)
        .map_err(|err| err.to_string())?
        .to_string()
    } else {
      match resolve_omnibox_input(raw)? {
        crate::ui::OmniboxInputResolution::Url { url } => url,
        crate::ui::OmniboxInputResolution::Search { url, .. } => url,
      }
    };
    validate_user_navigation_url_scheme(&normalized)?;

    self.chrome.address_bar_editing = false;
    self.chrome.address_bar_has_focus = false;
    self.chrome.omnibox.reset();
    self.chrome.address_bar_text = normalized.clone();

    if let Some(tab) = self.tab_mut(tab_id) {
      tab.current_url = Some(normalized.clone());
      tab.loading = true;
      tab.unresponsive = false;
      tab.last_worker_msg_at = SystemTime::now();
      tab.error = None;
      tab.stage = None;
      tab.reset_load_progress();
    }

    Ok(normalized)
  }

  pub fn update_unresponsive_tabs(
    &mut self,
    now: SystemTime,
    timeout: std::time::Duration,
  ) -> bool {
    let mut changed = false;
    for tab in &mut self.tabs {
      if !tab.loading {
        if tab.unresponsive {
          tab.unresponsive = false;
          changed = true;
        }
        continue;
      }
      if tab.unresponsive {
        continue;
      }

      let elapsed = now
        .duration_since(tab.last_worker_msg_at)
        .unwrap_or(std::time::Duration::ZERO);
      if elapsed >= timeout {
        tab.unresponsive = true;
        changed = true;
      }
    }
    changed
  }

  /// Returns the minimum duration after which `update_unresponsive_tabs` may change state for any
  /// currently-loading tab.
  ///
  /// Front-ends can use this to schedule a future repaint so the watchdog triggers even when the UI
  /// is otherwise idle (e.g. reduced-motion disables animated spinners).
  pub fn next_unresponsive_check_in(
    &self,
    now: SystemTime,
    timeout: std::time::Duration,
  ) -> Option<std::time::Duration> {
    let mut next: Option<std::time::Duration> = None;
    for tab in &self.tabs {
      if !tab.loading || tab.unresponsive {
        continue;
      }
      let elapsed = now
        .duration_since(tab.last_worker_msg_at)
        .unwrap_or(std::time::Duration::ZERO);
      let remaining = timeout.saturating_sub(elapsed);
      next = Some(next.map_or(remaining, |prev| prev.min(remaining)));
    }
    next
  }

  /// Dismiss the "page unresponsive" UI for a tab and reset the watchdog timer.
  pub fn dismiss_tab_unresponsive(&mut self, tab_id: TabId, now: SystemTime) -> bool {
    let Some(tab) = self.tab_mut(tab_id) else {
      return false;
    };
    tab.unresponsive = false;
    tab.last_worker_msg_at = now;
    true
  }

  pub fn apply_worker_msg(&mut self, msg: WorkerToUi) -> AppUpdate {
    self.apply_worker_msg_at(msg, SystemTime::now())
  }

  pub fn apply_worker_msg_at(&mut self, msg: WorkerToUi, now: SystemTime) -> AppUpdate {
    let mut update = AppUpdate::default();
    let tab_id = msg.tab_id();
    if let Some(tab) = self.tab_mut(tab_id) {
      tab.last_worker_msg_at = now;
      tab.unresponsive = false;
    }

    match msg {
      WorkerToUi::FrameReady { tab_id, frame } => {
        let RenderedFrame {
          pixmap,
          viewport_css,
          dpr,
          scroll_state,
          scroll_metrics,
          wants_ticks,
        } = frame;
        let pixmap_px = (pixmap.width(), pixmap.height());

        // The renderer process is treated as untrusted in multiprocess builds. Validate payload
        // invariants defensively before storing metadata or asking the UI to upload a GPU texture.
        let limits = BrowserLimits::default();
        let (pix_w, pix_h) = pixmap_px;
        let (vp_w, vp_h) = viewport_css;

        let is_active_tab = self.active_tab_id() == Some(tab_id);

        let pixmap_nonzero = pix_w != 0 && pix_h != 0;
        let pixmap_within_limits = pix_w <= limits.max_dim_px
          && pix_h <= limits.max_dim_px
          && (pix_w as u64).saturating_mul(pix_h as u64) <= limits.max_pixels;
        let viewport_nonzero = vp_w != 0 && vp_h != 0;

        let mut dpr = if dpr.is_finite() && dpr > 0.0 { dpr } else { 1.0 };
        // Keep in sync with `src/ui/browser_limits.rs`'s renderer DPR clamp range.
        dpr = dpr.clamp(0.1, limits.max_dpr);

        let expected_w = ((vp_w as f64) * (dpr as f64)).round();
        let expected_h = ((vp_h as f64) * (dpr as f64)).round();
        let expected_w = expected_w.max(1.0).min(u32::MAX as f64) as u32;
        let expected_h = expected_h.max(1.0).min(u32::MAX as f64) as u32;

        let tolerance_px = 1u32;
        let dims_match = pix_w.abs_diff(expected_w) <= tolerance_px
          && pix_h.abs_diff(expected_h) <= tolerance_px;

        if pixmap_nonzero && pixmap_within_limits && viewport_nonzero && dims_match {
          if let Some(tab) = self.tab_mut(tab_id) {
            tab.scroll_state = scroll_state;
            tab.scroll_metrics = Some(scroll_metrics);
            tab.latest_frame_meta = Some(LatestFrameMeta {
              pixmap_px,
              viewport_css,
              dpr,
              wants_ticks,
            });

            // Only the active tab's page content is visible, so only its frames should trigger a
            // UI redraw. Background tabs still emit `frame_ready` so front-ends can coalesce/defer
            // texture uploads until a later repaint (e.g. when switching tabs).
            update.request_redraw = is_active_tab;
            update.frame_ready = Some(FrameReadyUpdate {
              tab_id,
              pixmap,
              viewport_css,
              dpr,
            });
          }
        }
      }
      WorkerToUi::RequestWakeAfter { .. } => {
        // Wakeup scheduling is handled by the host UI event loop (e.g. `src/bin/browser.rs`).
      }
      WorkerToUi::OpenSelectDropdown {
        tab_id,
        select_node_id,
        control,
      } => {
        update.request_redraw = true;
        update.open_select_dropdown = Some(OpenSelectDropdownUpdate {
          tab_id,
          select_node_id,
          control: sanitize_untrusted_select_control(control),
          anchor_css: None,
        });
      }
      WorkerToUi::SelectDropdownOpened {
        tab_id,
        select_node_id,
        control,
        anchor_css,
      } => {
        update.request_redraw = true;
        update.open_select_dropdown = Some(OpenSelectDropdownUpdate {
          tab_id,
          select_node_id,
          control: sanitize_untrusted_select_control(control),
          anchor_css: Some(anchor_css),
        });
      }
      WorkerToUi::SelectDropdownClosed { .. } => {
        // Front-ends that show a `<select>` overlay should dismiss it.
        update.request_redraw = true;
      }
      WorkerToUi::DatalistOpened {
        tab_id,
        input_node_id,
        options,
        anchor_css,
      } => {
        update.request_redraw = true;
        update.open_datalist = Some(OpenDatalistUpdate {
          tab_id,
          input_node_id,
          options,
          anchor_css: Some(anchor_css),
        });
      }
      WorkerToUi::DatalistClosed { .. } => {
        // Front-ends that show a `<datalist>` overlay should dismiss it.
        update.request_redraw = true;
      }
      WorkerToUi::DateTimePickerOpened { .. } => {
        // Front-ends may show an overlay picker for date/time-like inputs.
        update.request_redraw = true;
      }
      WorkerToUi::DateTimePickerClosed { .. } => {
        // Front-ends that show a picker overlay should dismiss it.
        update.request_redraw = true;
      }
      WorkerToUi::ColorPickerOpened { .. } => {
        // Front-ends may show an overlay picker for color inputs.
        update.request_redraw = true;
      }
      WorkerToUi::ColorPickerClosed { .. } => {
        // Front-ends that show a picker overlay should dismiss it.
        update.request_redraw = true;
      }
      WorkerToUi::FilePickerOpened { .. } => {
        // Front-ends may show a file picker for file inputs.
        update.request_redraw = true;
      }
      WorkerToUi::FilePickerClosed { .. } => {
        // Front-ends that show a file picker should dismiss it.
        update.request_redraw = true;
      }
      WorkerToUi::Stage { tab_id, stage } => {
        if let Some(tab) = self.tab_mut(tab_id) {
          tab.stage = Some(stage);
          tab.update_load_progress_for_stage(stage);
          update.request_redraw = true;
        }
      }
      WorkerToUi::NavigationStarted { tab_id, url } => {
        let safe_url = validate_untrusted_navigation_url(&url).ok();
        let site_key = if url.len() > MAX_URL_BYTES {
          None
        } else {
          safe_url
            .as_deref()
            .and_then(|url| Url::parse(url).ok())
            .map(|url| SiteKey::from_url(&url))
        };
        if let Some(tab) = self.tab_mut(tab_id) {
          tab.site_key = site_key;
          if let Some(url) = safe_url.as_ref() {
            tab.current_url = Some(url.clone());
          }
          tab.loading = true;
          tab.error = None;
          tab.stage = None;
          tab.reset_load_progress();
          tab.favicon_meta = None;
          tab.hovered_url = None;
          tab.cursor = CursorKind::Default;
        }
        if let Some(url) = safe_url {
          if self.active_tab_id() == Some(tab_id) && !self.chrome.address_bar_editing {
            self.chrome.address_bar_text = url;
          }
        }
        update.request_redraw = true;
      }
      WorkerToUi::NavigationCommitted {
        tab_id,
        url,
        title,
        can_go_back,
        can_go_forward,
      } => {
        let safe_url = validate_untrusted_navigation_url(&url).ok();
        let safe_title = title
          .as_deref()
          .map(|t| sanitize_untrusted_text(t, MAX_TITLE_BYTES))
          .filter(|t| !t.is_empty());
        let site_key = if url.len() > MAX_URL_BYTES {
          None
        } else {
          safe_url
            .as_deref()
            .and_then(|url| Url::parse(url).ok())
            .map(|url| SiteKey::from_url(&url))
        };

        if let Some(url) = safe_url.as_ref() {
          // Record global history. This is the single canonical source of truth for what counts as a
          // "visit" (scheme allowlist, fragment stripping, `about:` filtering, etc).
          update.history_changed = self.history.record(url.clone(), safe_title.clone());
        }

        // Keep the omnibox visited store consistent with the global history store by recording the
        // normalized URL (e.g. fragment stripped).
        //
        // When global history rejects a navigation (notably internal `about:` pages), we still
        // record a small allowlist of useful `about:` pages so they remain discoverable via omnibox
        // autocomplete. The visited store enforces the policy (see
        // `ui::visited::should_record_visit_in_history`).
        if update.history_changed && safe_url.is_some() {
          let url = safe_url.as_ref().unwrap();
          let normalized_url = self
            .history
            .get(url)
            .map(|entry| entry.url.clone())
            // `record` returned true, so this should be unreachable, but keep a safe fallback to
            // avoid losing visited entries in release builds if invariants change.
            .unwrap_or_else(|| url.clone());
          self.visited.record_visit(normalized_url, safe_title.clone());
        } else if let Some(url) = safe_url.as_ref().filter(|u| about_pages::is_about_url(u)) {
          // Normalize internal pages by stripping any query/fragment so e.g. `about:help#foo` and
          // `about:history?q=rust` do not create separate visited entries.
          let normalized_about = url
            .split(|c| matches!(c, '?' | '#'))
            .next()
            .unwrap_or(url.as_str())
            .to_string();
          self.visited.record_visit(normalized_about, safe_title.clone());
        }
        if let Some(tab) = self.tab_mut(tab_id) {
          tab.site_key = site_key;
          if let Some(url) = safe_url.as_ref() {
            tab.current_url = Some(url.clone());
            tab.committed_url = Some(url.clone());
          }
          if safe_url.is_some() {
            tab.title = safe_title.clone();
            tab.committed_title = safe_title.clone();
          }
          tab.loading = false;
          tab.error = None;
          tab.stage = None;
          tab.clear_load_progress();
          tab.can_go_back = can_go_back;
          tab.can_go_forward = can_go_forward;
          tab.hovered_url = None;
          tab.cursor = CursorKind::Default;
        }
        if let Some(url) = safe_url {
          if self.active_tab_id() == Some(tab_id) && !self.chrome.address_bar_editing {
            self.chrome.address_bar_text = url;
          }
        }
        update.request_redraw = true;
      }
      WorkerToUi::NavigationFailed {
        tab_id,
        url,
        error,
        can_go_back,
        can_go_forward,
      } => {
        let safe_url = validate_untrusted_navigation_url(&url).ok();
        let safe_error = sanitize_untrusted_text(&error, MAX_ERROR_BYTES);
        let site_key = if url.len() > MAX_URL_BYTES {
          None
        } else {
          safe_url
            .as_deref()
            .and_then(|url| Url::parse(url).ok())
            .map(|url| SiteKey::from_url(&url))
        };
        // Do not record failed navigations in global omnibox history.
        if let Some(tab) = self.tab_mut(tab_id) {
          tab.site_key = site_key;
          if let Some(url) = safe_url.as_ref() {
            tab.current_url = Some(url.clone());
          }
          tab.loading = false;
          if safe_url.is_some() {
            tab.error = Some(safe_error);
          }
          tab.stage = None;
          tab.clear_load_progress();
          tab.can_go_back = can_go_back;
          tab.can_go_forward = can_go_forward;
          tab.title = None;
          tab.favicon_meta = None;
          tab.hovered_url = None;
          tab.cursor = CursorKind::Default;
        }
        if let Some(url) = safe_url {
          if self.active_tab_id() == Some(tab_id) && !self.chrome.address_bar_editing {
            self.chrome.address_bar_text = url;
          }
        }
        update.request_redraw = true;
      }
      WorkerToUi::Favicon {
        tab_id,
        rgba,
        width,
        height,
      } => {
        // Validate favicon payload invariants before storing metadata or asking the UI to upload a
        // GPU texture. In multiprocess builds, the renderer process is treated as untrusted.
        //
        // Note: enforce both total byte length and per-axis dimension limits (see
        // `FAVICON_MAX_EDGE_PX`).
        if validate_untrusted_favicon_rgba(rgba.len(), width, height)
          && width <= FAVICON_MAX_EDGE_PX
          && height <= FAVICON_MAX_EDGE_PX
        {
          if let Some(tab) = self.tab_mut(tab_id) {
            tab.favicon_meta = Some(FaviconMeta {
              size_px: (width, height),
            });
            update.request_redraw = true;
            update.favicon_ready = Some(FaviconReadyUpdate {
              tab_id,
              rgba,
              width,
              height,
            });
          }
        }
      }
      WorkerToUi::RequestOpenInNewTab { .. } | WorkerToUi::RequestOpenInNewTabRequest { .. } => {
        // The UI owns tab identifiers; front-ends are expected to handle this message directly by
        // allocating a new tab id and issuing `CreateTab`/`Navigate`. The shared state model does
        // not automatically create tabs.
        update.request_redraw = true;
      }
      WorkerToUi::ScrollStateUpdated { tab_id, scroll } => {
        if let Some(tab) = self.tab_mut(tab_id) {
          if tab.scroll_state != scroll {
            tab.scroll_state = scroll;
            update.request_redraw = true;
          }
        }
      }
      WorkerToUi::LoadingState { tab_id, loading } => {
        if let Some(tab) = self.tab_mut(tab_id) {
          tab.loading = loading;
          if loading {
            if tab.load_progress.is_none() {
              tab.reset_load_progress();
            }
          } else {
            tab.clear_load_progress();
          }
        }
        update.request_redraw = true;
      }
      WorkerToUi::Warning { tab_id, text } => {
        let safe = sanitize_untrusted_text(&text, MAX_WARNING_BYTES);
        if let Some(tab) = self.tab_mut(tab_id) {
          tab.warning = (!safe.is_empty()).then_some(safe);
        }
        update.request_redraw = self.active_tab_id() == Some(tab_id);
      }
      WorkerToUi::DebugLog { tab_id, line } => {
        let safe = sanitize_untrusted_text(&line, MAX_DEBUG_LOG_BYTES);
        if let Some(tab) = self.tab_mut(tab_id) {
          if !safe.is_empty() {
            tab.push_debug_log(safe);
          }
        }
        update.request_redraw = self.active_tab_id() == Some(tab_id);
      }
      WorkerToUi::ContextMenu { .. } => {
        // Front-ends may use this message to open a page context menu; it does not directly mutate
        // the shared tab model, but it should trigger a redraw so UIs can react immediately.
        update.request_redraw = true;
      }
      WorkerToUi::HoverChanged {
        tab_id,
        hovered_url,
        cursor,
      } => {
        let safe_hovered =
          hovered_url.and_then(|url| crate::ui::url::sanitize_worker_url_for_ui(&url));
        if let Some(tab) = self.tab_mut(tab_id) {
          tab.hovered_url = safe_hovered;
          tab.cursor = cursor;
        }
        update.request_redraw = self.active_tab_id() == Some(tab_id);
      }
      WorkerToUi::FindResult {
        tab_id,
        query,
        case_sensitive,
        match_count,
        active_match_index,
      } => {
        let safe_query = sanitize_untrusted_text(&query, MAX_FIND_QUERY_BYTES);
        if let Some(tab) = self.tab_mut(tab_id) {
          tab.find.query = safe_query;
          tab.find.case_sensitive = case_sensitive;
          tab.find.match_count = match_count;
          tab.find.active_match_index = if match_count == 0 {
            None
          } else {
            active_match_index
          };
        }
        update.request_redraw = self.active_tab_id() == Some(tab_id);
      }
      WorkerToUi::SetClipboardText { .. } => {
        // Clipboard is handled by the front-end (e.g. `src/bin/browser.rs`); the shared app state
        // model does not store clipboard contents.
        update.request_redraw = true;
      }
      WorkerToUi::DownloadStarted {
        tab_id,
        download_id,
        url,
        file_name,
        path,
        total_bytes,
      } => {
        let safe_url = sanitize_untrusted_text(&url, MAX_URL_BYTES);
        let safe_file_name = sanitize_untrusted_text(&file_name, MAX_DOWNLOAD_FILE_NAME_BYTES);
        self.downloads.insert_or_update(DownloadEntry {
          download_id,
          tab_id,
          url: safe_url,
          file_name: safe_file_name,
          path,
          status: DownloadStatus::InProgress {
            received_bytes: 0,
            total_bytes,
          },
        });
        update.request_redraw = true;
      }
      WorkerToUi::DownloadProgress {
        tab_id: _,
        download_id,
        received_bytes,
        total_bytes,
      } => {
        if let Some(entry) = self.downloads.get_mut(download_id) {
          if let DownloadStatus::InProgress { .. } = &mut entry.status {
            entry.status = DownloadStatus::InProgress {
              received_bytes,
              total_bytes,
            };
            update.request_redraw = true;
          }
        }
      }
      WorkerToUi::DownloadFinished {
        tab_id: _,
        download_id,
        outcome,
      } => {
        if let Some(entry) = self.downloads.get_mut(download_id) {
          entry.status = match outcome {
            DownloadOutcome::Completed => DownloadStatus::Completed,
            DownloadOutcome::Cancelled => DownloadStatus::Cancelled,
            DownloadOutcome::Failed { error } => DownloadStatus::Failed {
              error: sanitize_untrusted_text(&error, MAX_ERROR_BYTES),
            },
          };
          update.request_redraw = true;
        }
      }
    }

    update
  }
}

#[cfg(test)]
mod browser_tab_tests {
  use super::BrowserTabState;
  use crate::ui::messages::{NavigationReason, UiToWorker};
  use crate::ui::TabId;

  #[test]
  fn typed_javascript_url_is_rejected() {
    let mut tab = BrowserTabState::new(TabId(1), "about:newtab".to_string());
    assert!(tab.navigate_typed("javascript:alert(1)").is_err());
    assert_eq!(tab.current_url(), Some("about:newtab"));
    assert!(!tab.loading);
  }

  #[test]
  fn typed_unknown_scheme_is_rejected() {
    let mut tab = BrowserTabState::new(TabId(1), "about:newtab".to_string());
    assert!(tab.navigate_typed("foo:bar").is_err());
    assert_eq!(tab.current_url(), Some("about:newtab"));
    assert!(!tab.loading);
  }

  #[test]
  fn typed_about_url_is_allowed() {
    let mut tab = BrowserTabState::new(TabId(1), "about:newtab".to_string());
    let msg = tab
      .navigate_typed("about:blank")
      .expect("about URL should be allowed");
    assert!(matches!(
      msg,
      UiToWorker::Navigate {
        tab_id: TabId(1),
        ref url,
        reason: NavigationReason::TypedUrl,
      } if url == "about:blank"
    ));
    assert!(tab.loading);
  }
}

#[cfg(test)]
mod browser_app_tests {
  use super::*;
  use crate::geometry::Point;

  fn assert_active_is_valid(app: &BrowserAppState) {
    let active = app.active_tab_id();
    assert!(active.is_some(), "active tab must exist");
    assert!(
      app.tabs.iter().any(|t| Some(t.id) == active),
      "active tab must exist (active={active:?}, tabs={:?})",
      app.tabs.iter().map(|t| t.id).collect::<Vec<_>>()
    );
  }

  #[test]
  fn newly_created_tabs_have_no_renderer_process_until_assigned() {
    let tab = BrowserTabState::new(TabId(1_000_000), about_pages::ABOUT_NEWTAB.to_string());
    assert_eq!(tab.renderer_process, None);
    assert_eq!(tab.site_key, None);
  }

  #[test]
  fn set_tab_renderer_updates_target_tab_and_is_noop_for_missing_tab() {
    let mut app = BrowserAppState::new();

    let a = TabId(1_000_000);
    let b = TabId(1_000_001);

    app.push_tab(
      BrowserTabState::new(a, about_pages::ABOUT_NEWTAB.to_string()),
      true,
    );
    app.push_tab(
      BrowserTabState::new(b, about_pages::ABOUT_NEWTAB.to_string()),
      false,
    );

    assert_eq!(app.tab_renderer(a), None);
    assert_eq!(app.tab_renderer(b), None);

    let pid_a = RendererProcessId(10);
    assert!(app.set_tab_renderer(
      a,
      pid_a,
      Some(SiteKey("https://a.test".to_string()))
    ));
    assert_eq!(app.tab_renderer(a), Some(pid_a));
    assert_eq!(app.tab(a).unwrap().site_key, Some(SiteKey("https://a.test".to_string())));
    assert_eq!(app.tab_renderer(b), None);

    // Missing tab id should not mutate existing tabs.
    let pid_missing = RendererProcessId(11);
    assert!(!app.set_tab_renderer(
      TabId(9_999_999),
      pid_missing,
      Some(SiteKey("https://missing.test".to_string()))
    ));
    assert_eq!(app.tab_renderer(a), Some(pid_a));
    assert_eq!(app.tab(a).unwrap().site_key, Some(SiteKey("https://a.test".to_string())));
    assert_eq!(app.tab_renderer(b), None);
    assert_eq!(app.tab(b).unwrap().site_key, None);
  }

  #[test]
  fn closing_last_tab_is_noop() {
    let _lock = crate::ui::messages::TAB_ID_TEST_LOCK
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());

    let mut app = BrowserAppState::new();

    let tab_id = TabId(1_000_000);
    app.push_tab(
      BrowserTabState::new(tab_id, about_pages::ABOUT_NEWTAB.to_string()),
      true,
    );
    assert_eq!(app.tabs.len(), 1);
    assert_eq!(app.active_tab_id(), Some(tab_id));

    let result = app.remove_tab(tab_id);

    assert_eq!(app.tabs.len(), 1);
    assert_eq!(app.active_tab_id(), Some(tab_id));
    assert_eq!(result.new_active, None);
    assert_eq!(result.created_tab, None);
  }

  #[test]
  fn closing_active_tab_keeps_existing_tab_when_available() {
    let mut app = BrowserAppState::new();

    let a = TabId(1_000_000);
    let b = TabId(1_000_001);
    app.push_tab(
      BrowserTabState::new(a, about_pages::ABOUT_NEWTAB.to_string()),
      true,
    );
    app.push_tab(
      BrowserTabState::new(b, about_pages::ABOUT_NEWTAB.to_string()),
      false,
    );
    assert_eq!(app.active_tab_id(), Some(a));

    let result = app.remove_tab(a);
    assert_eq!(result.created_tab, None);
    assert_eq!(app.tabs.len(), 1);
    assert_eq!(app.active_tab_id(), Some(b));
    assert_eq!(result.new_active, Some(b));
  }

  #[test]
  fn tab_create_close_invariants() {
    let mut app = BrowserAppState::new_with_initial_tab("about:newtab".to_string());
    assert_eq!(app.tabs.len(), 1);
    assert_active_is_valid(&app);

    let t2 = app.create_tab(Some("https://example.com".to_string()));
    assert_eq!(app.tabs.len(), 2);
    assert_eq!(app.active_tab_id(), Some(t2));
    assert_active_is_valid(&app);

    app.close_tab(t2);
    assert_eq!(app.tabs.len(), 1);
    assert_active_is_valid(&app);

    // Closing the last remaining tab should be a no-op.
    let last = app.active_tab_id().unwrap();
    app.close_tab(last);
    assert_eq!(app.tabs.len(), 1);
    assert_active_is_valid(&app);
    assert_eq!(app.active_tab_id(), Some(last));
  }

  #[test]
  fn close_other_tabs_keeps_requested_tab_and_active_is_valid() {
    let mut app = BrowserAppState::new();
    let tab_a = TabId(1);
    let tab_b = TabId(2);
    let tab_c = TabId(3);
    app.push_tab(
      BrowserTabState::new(tab_a, about_pages::ABOUT_NEWTAB.to_string()),
      true,
    );
    app.push_tab(
      BrowserTabState::new(tab_b, about_pages::ABOUT_NEWTAB.to_string()),
      false,
    );
    app.push_tab(
      BrowserTabState::new(tab_c, about_pages::ABOUT_NEWTAB.to_string()),
      false,
    );
    app.set_active_tab(tab_b);

    let closed = app.close_other_tabs(tab_c);

    assert_eq!(closed, vec![tab_a, tab_b]);
    assert_eq!(
      app.tabs.iter().map(|t| t.id).collect::<Vec<_>>(),
      vec![tab_c]
    );
    assert_eq!(app.active_tab_id(), Some(tab_c));
    assert_active_is_valid(&app);
  }

  #[test]
  fn close_tabs_to_right_closes_expected_tabs_and_preserves_invariants() {
    let mut app = BrowserAppState::new();
    let tab_a = TabId(1);
    let tab_b = TabId(2);
    let tab_c = TabId(3);
    let tab_d = TabId(4);
    app.push_tab(
      BrowserTabState::new(tab_a, about_pages::ABOUT_NEWTAB.to_string()),
      false,
    );
    app.push_tab(
      BrowserTabState::new(tab_b, about_pages::ABOUT_NEWTAB.to_string()),
      false,
    );
    app.push_tab(
      BrowserTabState::new(tab_c, about_pages::ABOUT_NEWTAB.to_string()),
      false,
    );
    app.push_tab(
      BrowserTabState::new(tab_d, about_pages::ABOUT_NEWTAB.to_string()),
      true,
    );
    assert_eq!(app.active_tab_id(), Some(tab_d));

    let closed = app.close_tabs_to_right(tab_b);

    assert_eq!(closed, vec![tab_c, tab_d]);
    assert_eq!(
      app.tabs.iter().map(|t| t.id).collect::<Vec<_>>(),
      vec![tab_a, tab_b]
    );
    assert_eq!(app.active_tab_id(), Some(tab_b));
    assert_active_is_valid(&app);
  }

  #[test]
  fn navigation_committed_updates_title_url_and_nav_flags() {
    let mut app = BrowserAppState::new_with_initial_tab("about:newtab".to_string());
    let tab_id = app.active_tab_id().unwrap();

    let _update = app.apply_worker_msg(WorkerToUi::NavigationCommitted {
      tab_id,
      url: "https://example.com/".to_string(),
      title: Some("Example Domain".to_string()),
      can_go_back: true,
      can_go_forward: false,
    });

    let tab = app.active_tab().unwrap();
    assert_eq!(tab.current_url(), Some("https://example.com/"));
    assert_eq!(tab.title.as_deref(), Some("Example Domain"));
    assert!(tab.can_go_back);
    assert!(!tab.can_go_forward);
    assert_eq!(app.chrome.address_bar_text, "https://example.com/");
  }

  #[test]
  fn worker_disallowed_scheme_does_not_update_tab_url_or_address_bar() {
    let mut app = BrowserAppState::new_with_initial_tab("https://example.com/".to_string());
    let tab_id = app.active_tab_id().unwrap();

    let before_tab_url = app.active_tab().and_then(|t| t.current_url()).map(str::to_string);
    let before_address_bar = app.chrome.address_bar_text.clone();

    let update = app.apply_worker_msg(WorkerToUi::NavigationCommitted {
      tab_id,
      url: "javascript:alert(1)".to_string(),
      title: Some("Bad".to_string()),
      can_go_back: false,
      can_go_forward: false,
    });

    assert!(
      !update.history_changed,
      "disallowed worker URL should not be recorded in history"
    );
    assert_eq!(
      app.active_tab().and_then(|t| t.current_url()).map(str::to_string),
      before_tab_url,
      "current_url must not be clobbered by disallowed scheme"
    );
    assert_eq!(
      app.chrome.address_bar_text, before_address_bar,
      "address bar must not be clobbered by disallowed scheme"
    );
  }

  #[test]
  fn worker_strings_are_sanitized_and_truncated() {
    use crate::ui::protocol_limits::{MAX_DEBUG_LOG_BYTES, MAX_TITLE_BYTES, MAX_URL_BYTES};

    let mut app = BrowserAppState::new_with_initial_tab("about:newtab".to_string());
    let tab_id = app.active_tab_id().unwrap();

    let long_url = format!("https://example.com/{}", "a".repeat(MAX_URL_BYTES * 2));
    let long_title = "t".repeat(MAX_TITLE_BYTES * 2);

    app.apply_worker_msg(WorkerToUi::NavigationCommitted {
      tab_id,
      url: long_url,
      title: Some(long_title),
      can_go_back: false,
      can_go_forward: false,
    });

    let tab = app.active_tab().unwrap();
    let stored_url = tab.current_url().unwrap();
    assert!(
      stored_url.len() <= MAX_URL_BYTES,
      "expected URL to be clamped (len={}, max={})",
      stored_url.len(),
      MAX_URL_BYTES
    );
    assert!(
      tab.title.as_deref().is_some_and(|t| t.len() <= MAX_TITLE_BYTES),
      "expected title to be clamped"
    );

    let long_log = "x".repeat(MAX_DEBUG_LOG_BYTES * 2);
    app.apply_worker_msg(WorkerToUi::DebugLog {
      tab_id,
      line: long_log,
    });
    let log_line = app
      .active_tab()
      .unwrap()
      .debug_log()
      .last()
      .expect("expected a debug log line");
    assert!(
      log_line.len() <= MAX_DEBUG_LOG_BYTES,
      "expected debug log line to be clamped"
    );
  }

  #[test]
  fn worker_control_chars_are_stripped() {
    let mut app = BrowserAppState::new_with_initial_tab("about:newtab".to_string());
    let tab_id = app.active_tab_id().unwrap();

    app.apply_worker_msg(WorkerToUi::Warning {
      tab_id,
      text: "a\u{0000}b\u{001f}c\u{007f}d".to_string(),
    });

    let warning = app
      .active_tab()
      .and_then(|t| t.warning.as_deref())
      .expect("expected warning to be set");
    assert_eq!(warning, "abcd");
    assert!(
      !warning.chars().any(|c| c.is_ascii_control()),
      "sanitized warning must not contain ASCII control characters"
    );
  }

  #[test]
  fn background_tab_frame_ready_does_not_request_redraw() {
    let mut app = BrowserAppState::new();
    let tab_a = TabId(1);
    let tab_b = TabId(2);

    app.push_tab(BrowserTabState::new(tab_a, "about:blank".to_string()), true);
    app.push_tab(BrowserTabState::new(tab_b, "about:newtab".to_string()), false);
    assert_eq!(app.active_tab_id(), Some(tab_a));

    let viewport_css = (1, 1);
    let frame = RenderedFrame {
      pixmap: tiny_skia::Pixmap::new(1, 1).expect("pixmap"),
      viewport_css,
      dpr: 1.0,
      scroll_state: ScrollState::default(),
      scroll_metrics: ScrollMetrics {
        viewport_css,
        scroll_css: (0.0, 0.0),
        bounds_css: crate::scroll::ScrollBounds {
          min_x: 0.0,
          min_y: 0.0,
          max_x: 0.0,
          max_y: 0.0,
        },
        content_css: (1.0, 1.0),
      },
      wants_ticks: false,
    };

    let update = app.apply_worker_msg(WorkerToUi::FrameReady { tab_id: tab_b, frame });

    assert!(
      !update.request_redraw,
      "expected FrameReady for inactive tab to avoid scheduling a redraw"
    );
    assert_eq!(update.frame_ready.as_ref().map(|f| f.tab_id), Some(tab_b));

    let meta = app
      .tab(tab_b)
      .and_then(|t| t.latest_frame_meta.as_ref())
      .expect("expected latest_frame_meta to be updated for inactive tab");
    assert_eq!(meta.viewport_css, viewport_css);
    assert_eq!(meta.pixmap_px, (1, 1));
  }

  #[test]
  fn hover_changed_sanitizes_hovered_url() {
    let mut app = BrowserAppState::new_with_initial_tab("about:newtab".to_string());
    let tab_id = app.active_tab_id().unwrap();

    app.apply_worker_msg(WorkerToUi::HoverChanged {
      tab_id,
      hovered_url: Some("javascript:alert(1)".to_string()),
      cursor: CursorKind::Pointer,
    });

    let tab = app.tab(tab_id).expect("tab should exist");
    assert_eq!(
      tab.hovered_url, None,
      "expected disallowed hovered_url to be dropped"
    );
  }

  #[test]
  fn navigation_committed_is_recorded_in_history_and_visited_stores() {
    let mut app = BrowserAppState::new_with_initial_tab("about:newtab".to_string());
    let tab_id = app.active_tab_id().unwrap();

    let update = app.apply_worker_msg(WorkerToUi::NavigationCommitted {
      tab_id,
      url: "https://example.com/".to_string(),
      title: Some("Example Domain".to_string()),
      can_go_back: false,
      can_go_forward: false,
    });

    assert!(update.history_changed);
    assert_eq!(app.visited.len(), 1);
    let record = app.visited.iter_recent().next().expect("expected visit");
    assert_eq!(record.url, "https://example.com/");
    assert_eq!(record.title.as_deref(), Some("Example Domain"));

    assert_eq!(app.history.entries.len(), 1);
    let entry = app.history.entries.last().expect("expected history entry");
    assert_eq!(entry.url, "https://example.com/");
    assert_eq!(entry.title.as_deref(), Some("Example Domain"));
    assert_eq!(entry.visit_count, 1);
    assert_ne!(
      entry.visited_at_ms, 0,
      "expected committed navigations to have a visit timestamp"
    );
  }

  #[test]
  fn navigation_committed_updates_site_key_and_invalid_clears_it() {
    let mut app = BrowserAppState::new_with_initial_tab("about:newtab".to_string());
    let tab_id = app.active_tab_id().unwrap();

    app.apply_worker_msg(WorkerToUi::NavigationCommitted {
      tab_id,
      url: "https://example.com/".to_string(),
      title: None,
      can_go_back: false,
      can_go_forward: false,
    });

    let tab = app.active_tab().unwrap();
    let expected = SiteKey::from_url(&Url::parse("https://example.com/").unwrap());
    assert_eq!(tab.site_key, Some(expected));

    app.apply_worker_msg(WorkerToUi::NavigationCommitted {
      tab_id,
      url: "not a url".to_string(),
      title: None,
      can_go_back: false,
      can_go_forward: false,
    });

    let tab = app.active_tab().unwrap();
    assert_eq!(tab.site_key, None);
  }

  #[test]
  fn about_pages_are_not_recorded_in_history_or_visited_stores() {
    let mut app = BrowserAppState::new_with_initial_tab("about:newtab".to_string());
    let tab_id = app.active_tab_id().unwrap();

    let update = app.apply_worker_msg(WorkerToUi::NavigationCommitted {
      tab_id,
      url: "about:blank".to_string(),
      title: Some("Blank".to_string()),
      can_go_back: false,
      can_go_forward: false,
    });

    assert!(!update.history_changed);
    assert!(app.visited.is_empty());
    assert!(app.history.entries.is_empty());
  }

  #[test]
  fn visited_urls_strip_fragments_and_dedupe_by_normalized_url() {
    let mut app = BrowserAppState::new_with_initial_tab("about:newtab".to_string());
    let tab_id = app.active_tab_id().unwrap();

    app.apply_worker_msg(WorkerToUi::NavigationCommitted {
      tab_id,
      url: "https://example.test/y#one".to_string(),
      title: Some("One".to_string()),
      can_go_back: false,
      can_go_forward: false,
    });
    app.apply_worker_msg(WorkerToUi::NavigationCommitted {
      tab_id,
      url: "https://example.test/y#two".to_string(),
      title: Some("Two".to_string()),
      can_go_back: false,
      can_go_forward: false,
    });

    assert_eq!(app.visited.len(), 1);
    let record = app.visited.iter_recent().next().expect("expected visit");
    assert_eq!(record.url, "https://example.test/y");

    assert_eq!(app.history.entries.len(), 1);
    let entry = app.history.entries.last().expect("expected history entry");
    assert_eq!(entry.url, "https://example.test/y");
    assert_eq!(entry.visit_count, 2);
  }

  #[test]
  fn visited_urls_ignore_unsupported_schemes() {
    let mut app = BrowserAppState::new_with_initial_tab("about:newtab".to_string());
    let tab_id = app.active_tab_id().unwrap();

    let update = app.apply_worker_msg(WorkerToUi::NavigationCommitted {
      tab_id,
      url: "mailto:test@example.com".to_string(),
      title: Some("Email".to_string()),
      can_go_back: false,
      can_go_forward: false,
    });

    assert!(!update.history_changed);
    assert!(app.visited.is_empty());
    assert!(app.history.entries.is_empty());
  }

  #[test]
  fn clear_history_empties_history_and_visited_stores() {
    let mut app = BrowserAppState::new_with_initial_tab("about:newtab".to_string());
    let tab_id = app.active_tab_id().unwrap();

    app.apply_worker_msg(WorkerToUi::NavigationCommitted {
      tab_id,
      url: "https://example.com/".to_string(),
      title: Some("Example Domain".to_string()),
      can_go_back: false,
      can_go_forward: false,
    });
    assert!(!app.visited.is_empty());
    assert!(!app.history.entries.is_empty());

    app.clear_history();
    assert!(app.visited.is_empty());
    assert!(app.history.entries.is_empty());
  }

  #[test]
  fn history_records_only_committed_navigations_with_normalization() {
    let mut app = BrowserAppState::new_with_initial_tab("about:newtab".to_string());
    let tab_id = app.active_tab_id().unwrap();

    // Started navigations should not be recorded.
    app.apply_worker_msg(WorkerToUi::NavigationStarted {
      tab_id,
      url: "https://example.com/redirect".to_string(),
    });
    assert!(app.history.entries.is_empty());

    // Committed navigations record a visit. For redirects, the worker reports the final committed
    // URL in `NavigationCommitted`, and the history store strips fragments.
    let update = app.apply_worker_msg(WorkerToUi::NavigationCommitted {
      tab_id,
      url: "https://example.com/final#frag".to_string(),
      title: Some("Example".to_string()),
      can_go_back: false,
      can_go_forward: false,
    });
    assert!(update.history_changed);
    assert_eq!(app.history.entries.len(), 1);
    let entry = app.history.entries.last().expect("expected history entry");
    assert_eq!(entry.url, "https://example.com/final");
    assert_eq!(entry.visit_count, 1);
    assert_eq!(app.visited.len(), 1);
    assert_eq!(
      app.visited.iter_recent().next().unwrap().url,
      "https://example.com/final"
    );

    // `about:` pages are ignored by global history.
    let update = app.apply_worker_msg(WorkerToUi::NavigationCommitted {
      tab_id,
      url: "about:help".to_string(),
      title: Some("Help".to_string()),
      can_go_back: false,
      can_go_forward: false,
    });
    assert!(!update.history_changed);
    assert_eq!(app.history.entries.len(), 1);
  }

  #[test]
  fn navigation_committed_records_useful_about_pages_but_not_transient_ones() {
    let mut app = BrowserAppState::new_with_initial_tab("about:newtab".to_string());
    let tab_id = app.active_tab_id().unwrap();

    assert_eq!(app.visited.len(), 0);

    // `about:newtab` is a transient internal page and should not pollute visited history.
    app.apply_worker_msg(WorkerToUi::NavigationCommitted {
      tab_id,
      url: about_pages::ABOUT_NEWTAB.to_string(),
      title: Some("New Tab".to_string()),
      can_go_back: false,
      can_go_forward: false,
    });
    assert_eq!(app.visited.len(), 0);

    // User-facing `about:` pages should still be recorded so they can autocomplete.
    app.apply_worker_msg(WorkerToUi::NavigationCommitted {
      tab_id,
      url: about_pages::ABOUT_HELP.to_string(),
      title: Some("Help".to_string()),
      can_go_back: false,
      can_go_forward: false,
    });

    assert_eq!(app.visited.len(), 1);
    let record = app.visited.iter_recent().next().expect("expected visit");
    assert_eq!(record.url, about_pages::ABOUT_HELP);
    assert_eq!(record.title.as_deref(), Some("Help"));
  }

  #[test]
  fn scroll_state_updated_requests_redraw_only_when_changed() {
    let mut app = BrowserAppState::new_with_initial_tab("about:newtab".to_string());
    let tab_id = app.active_tab_id().unwrap();

    let scroll = ScrollState::with_viewport(Point::new(0.0, 10.0));

    let first = app.apply_worker_msg(WorkerToUi::ScrollStateUpdated {
      tab_id,
      scroll: scroll.clone(),
    });
    assert!(first.request_redraw);
    assert_eq!(app.active_tab().unwrap().scroll_state, scroll);

    let second = app.apply_worker_msg(WorkerToUi::ScrollStateUpdated {
      tab_id,
      scroll: scroll.clone(),
    });
    assert!(
      !second.request_redraw,
      "expected identical scroll updates to not request redraw"
    );
    assert_eq!(app.active_tab().unwrap().scroll_state, scroll);
  }

  #[test]
  fn frame_ready_updates_scroll_and_meta() {
    let mut app = BrowserAppState::new_with_initial_tab("about:newtab".to_string());
    let tab_id = app.active_tab_id().unwrap();

    let expected_scroll = ScrollState::with_viewport(Point::new(10.0, 20.0));
    let pixmap = tiny_skia::Pixmap::new(2, 3).unwrap();
    let viewport_css = (800, 600);
    let dpr = 2.0;
    let scroll_metrics = ScrollMetrics {
      viewport_css,
      scroll_css: (10.0, 20.0),
      bounds_css: crate::scroll::ScrollBounds {
        min_x: 0.0,
        min_y: 0.0,
        max_x: 0.0,
        max_y: 0.0,
      },
      content_css: (viewport_css.0 as f32, viewport_css.1 as f32),
    };

    let update = app.apply_worker_msg(WorkerToUi::FrameReady {
      tab_id,
      frame: RenderedFrame {
        pixmap,
        viewport_css,
        dpr,
        scroll_state: expected_scroll.clone(),
        scroll_metrics,
        wants_ticks: false,
      },
    });

    let tab = app.active_tab().unwrap();
    assert_eq!(tab.scroll_state, expected_scroll);
    assert_eq!(
      tab.latest_frame_meta,
      Some(LatestFrameMeta {
        pixmap_px: (2, 3),
        viewport_css,
        dpr,
        wants_ticks: false,
      })
    );

    let ready = update.frame_ready.expect("expected FrameReadyUpdate");
    assert_eq!(ready.tab_id, tab_id);
    assert_eq!(ready.viewport_css, viewport_css);
    assert!((ready.dpr - dpr).abs() < f32::EPSILON);
    assert_eq!((ready.pixmap.width(), ready.pixmap.height()), (2, 3));
  }

  #[test]
  fn closed_tabs_stack_push_pop_and_noop_when_empty() {
    let mut app = BrowserAppState::new();
    let tab_a = TabId(1);
    let tab_b = TabId(2);

    let mut a = BrowserTabState::new(tab_a, "https://committed.example/".to_string());
    a.committed_title = Some("Committed".to_string());
    // Simulate an in-flight typed navigation where the optimistic UI state differs from the last
    // committed navigation.
    a.current_url = Some("https://typed.example/".to_string());
    a.title = None;
    let mut b = BrowserTabState::new(tab_b, "https://b.example/".to_string());
    b.title = Some("B".to_string());

    app.push_tab(a, true);
    app.push_tab(b, false);
    assert!(app.closed_tabs.is_empty());

    let _ = app.remove_tab(tab_a);
    assert_eq!(
      app.closed_tabs,
      vec![ClosedTabState {
        url: "https://committed.example/".to_string(),
        title: Some("Committed".to_string()),
        pinned: false,
      }]
    );

    let popped = app.pop_closed_tab().expect("expected closed tab state");
    assert_eq!(
      popped,
      ClosedTabState {
        url: "https://committed.example/".to_string(),
        title: Some("Committed".to_string()),
        pinned: false,
      }
    );
    assert!(app.closed_tabs.is_empty());

    // Pop on empty is a no-op.
    assert_eq!(app.pop_closed_tab(), None);
  }

  #[test]
  fn closed_tabs_stack_is_capped() {
    let mut app = BrowserAppState::new();

    // Create cap+2 tabs so we can close cap+1 of them (closing the last remaining tab is a no-op).
    let total_tabs = CLOSED_TAB_STACK_CAPACITY + 2;
    for i in 0..total_tabs {
      let tab_id = TabId((i + 1) as u64);
      let mut tab = BrowserTabState::new(tab_id, format!("https://example.com/{i}"));
      tab.title = Some(format!("T{i}"));
      app.push_tab(tab, i == 0);
    }

    for i in 0..(CLOSED_TAB_STACK_CAPACITY + 1) {
      let tab_id = TabId((i + 1) as u64);
      let _ = app.remove_tab(tab_id);
    }

    assert_eq!(app.closed_tabs.len(), CLOSED_TAB_STACK_CAPACITY);
    assert_eq!(
      app.closed_tabs.first().map(|t| t.url.as_str()),
      Some("https://example.com/1"),
      "expected the oldest entry to be dropped when exceeding the cap"
    );
    let expected_last = format!("https://example.com/{CLOSED_TAB_STACK_CAPACITY}");
    assert_eq!(
      app.closed_tabs.last().map(|t| t.url.as_str()),
      Some(expected_last.as_str())
    );
  }

  #[test]
  fn stage_loading_progress_is_monotonic() {
    assert!(
      StageHeartbeat::ReadCache.loading_progress() > 0.0,
      "expected ReadCache to map to a progress value > 0.0 so 0.0 can represent \"no stage yet\""
    );
    assert_eq!(
      StageHeartbeat::Done.loading_progress(),
      1.0,
      "expected Done to map to exactly 1.0"
    );

    let mut prev = 0.0_f32;

    for stage in StageHeartbeat::all() {
      let progress = stage.loading_progress();
      assert!(
        progress.is_finite(),
        "expected StageHeartbeat::{stage:?}.loading_progress() to be finite, got {progress}"
      );
      assert!(
        (0.0..=1.0).contains(&progress),
        "expected progress in [0,1], got {progress} for StageHeartbeat::{stage:?}"
      );
      assert!(
        progress > prev,
        "expected strictly increasing progress: StageHeartbeat::{stage:?} ({progress}) <= previous ({prev})"
      );
      prev = progress;
    }

    assert!(
      (prev - 1.0).abs() <= f32::EPSILON,
      "expected final stage to map to 1.0, got {prev}"
    );
  }

  #[test]
  fn stage_ordering_produces_monotonic_load_progress() {
    let mut app = BrowserAppState::new_with_initial_tab("about:newtab".to_string());
    let tab_id = app.active_tab_id().unwrap();

    app.apply_worker_msg(WorkerToUi::NavigationStarted {
      tab_id,
      url: "https://example.com/".to_string(),
    });

    let mut prev = app.active_tab().unwrap().load_progress.unwrap();
    assert!((prev - 0.0).abs() < 1e-6);

    for &stage in StageHeartbeat::all() {
      app.apply_worker_msg(WorkerToUi::Stage { tab_id, stage });
      let p = app.active_tab().unwrap().load_progress.unwrap();
      assert!(
        p + 1e-6 >= prev,
        "expected load progress to be monotonic (prev={prev}, next={p}, stage={stage:?})"
      );
      prev = p;
    }
  }

  #[test]
  fn duplicate_stages_do_not_change_load_progress() {
    let mut app = BrowserAppState::new_with_initial_tab("about:newtab".to_string());
    let tab_id = app.active_tab_id().unwrap();
    app.apply_worker_msg(WorkerToUi::NavigationStarted {
      tab_id,
      url: "https://example.com/".to_string(),
    });

    app.apply_worker_msg(WorkerToUi::Stage {
      tab_id,
      stage: StageHeartbeat::DomParse,
    });
    let p1 = app.active_tab().unwrap().load_progress.unwrap();

    app.apply_worker_msg(WorkerToUi::Stage {
      tab_id,
      stage: StageHeartbeat::DomParse,
    });
    let p2 = app.active_tab().unwrap().load_progress.unwrap();

    assert!((p2 - p1).abs() < 1e-6);
  }

  #[test]
  fn chrome_loading_progress_is_monotonic_across_out_of_order_stage_events() {
    let mut app = BrowserAppState::new_with_initial_tab("about:newtab".to_string());
    let tab_id = app.active_tab_id().unwrap();

    app.apply_worker_msg(WorkerToUi::NavigationStarted {
      tab_id,
      url: "https://example.com/".to_string(),
    });

    let mut prev = app
      .active_tab()
      .expect("tab exists")
      .chrome_loading_progress()
      .expect("tab should be loading");
    assert!(
      (prev - 0.0).abs() <= f32::EPSILON,
      "expected initial progress to be 0.0 after NavigationStarted, got {prev}"
    );

    for stage in [
      StageHeartbeat::Layout,
      // Regressing stage heartbeat must not reduce chrome progress.
      StageHeartbeat::ReadCache,
      StageHeartbeat::PaintRasterize,
      StageHeartbeat::DomParse,
      StageHeartbeat::Done,
    ] {
      app.apply_worker_msg(WorkerToUi::Stage { tab_id, stage });
      let next = app
        .active_tab()
        .expect("tab exists")
        .chrome_loading_progress()
        .expect("tab should be loading");
      assert!(
        next + f32::EPSILON >= prev,
        "expected chrome loading progress to be monotonic (prev={prev}, next={next}, stage={stage:?})"
      );
      prev = next;
    }
  }

  #[test]
  fn chrome_loading_progress_resets_across_navigations() {
    let mut app = BrowserAppState::new_with_initial_tab("about:newtab".to_string());
    let tab_id = app.active_tab_id().unwrap();

    // Navigation 1: start → observe stage progress.
    app.apply_worker_msg(WorkerToUi::NavigationStarted {
      tab_id,
      url: "https://example.com/".to_string(),
    });
    app.apply_worker_msg(WorkerToUi::Stage {
      tab_id,
      stage: StageHeartbeat::Layout,
    });

    let progress_before = app
      .active_tab()
      .expect("tab exists")
      .chrome_loading_progress()
      .expect("tab should be loading");
    assert!(
      progress_before > 0.0,
      "expected non-zero progress after a stage heartbeat, got {progress_before}"
    );

    // Navigation 2: should clear stage/progress.
    app.apply_worker_msg(WorkerToUi::NavigationStarted {
      tab_id,
      url: "https://second.example/".to_string(),
    });

    {
      let tab = app.active_tab().unwrap();
      assert_eq!(tab.load_stage, None);
      assert_eq!(tab.load_progress, Some(0.0));
      assert_eq!(
        tab.chrome_loading_progress(),
        Some(0.0),
        "expected progress to reset to 0.0 on navigation start"
      );
    }

    // Navigation commit should stop showing progress entirely.
    app.apply_worker_msg(WorkerToUi::NavigationCommitted {
      tab_id,
      url: "https://second.example/".to_string(),
      title: None,
      can_go_back: false,
      can_go_forward: false,
    });
    assert_eq!(
      app
        .active_tab()
        .expect("tab exists")
        .chrome_loading_progress(),
      None,
      "expected progress to be hidden once loading=false"
    );
  }

  #[test]
  fn find_result_updates_only_target_tab_and_does_not_mutate_open() {
    let mut app = BrowserAppState::new();
    let tab_a = TabId(1);
    let tab_b = TabId(2);
    app.push_tab(
      BrowserTabState::new(tab_a, about_pages::ABOUT_NEWTAB.to_string()),
      true,
    );
    app.push_tab(
      BrowserTabState::new(tab_b, about_pages::ABOUT_NEWTAB.to_string()),
      false,
    );

    // UI controls find visibility; the worker must not mutate it.
    app.tab_mut(tab_a).unwrap().find.open = true;
    app.tab_mut(tab_b).unwrap().find.open = false;

    app.apply_worker_msg(WorkerToUi::FindResult {
      tab_id: tab_a,
      query: "needle".to_string(),
      case_sensitive: true,
      match_count: 5,
      active_match_index: Some(2),
    });

    let a = app.tab(tab_a).unwrap();
    assert!(a.find.open, "open should be UI-owned and preserved");
    assert_eq!(a.find.query, "needle");
    assert!(a.find.case_sensitive);
    assert_eq!(a.find.match_count, 5);
    assert_eq!(a.find.active_match_index, Some(2));

    let b = app.tab(tab_b).unwrap();
    assert!(!b.find.open);
    assert_eq!(b.find, FindInPageState::default());
  }

  #[test]
  fn reorder_tab_moves_tab_and_clamps_target_index() {
    let mut app = BrowserAppState::new();
    let a = TabId(1);
    let b = TabId(2);
    let c = TabId(3);

    app.push_tab(
      BrowserTabState::new(a, about_pages::ABOUT_NEWTAB.to_string()),
      true,
    );
    app.push_tab(
      BrowserTabState::new(b, about_pages::ABOUT_NEWTAB.to_string()),
      false,
    );
    app.push_tab(
      BrowserTabState::new(c, about_pages::ABOUT_NEWTAB.to_string()),
      false,
    );

    assert_eq!(
      app.tabs.iter().map(|t| t.id).collect::<Vec<_>>(),
      vec![a, b, c]
    );

    // Moving the first tab to an out-of-bounds index clamps to the last position.
    assert!(app.reorder_tab(a, 999));
    assert_eq!(
      app.tabs.iter().map(|t| t.id).collect::<Vec<_>>(),
      vec![b, c, a]
    );
  }

  #[test]
  fn reorder_tab_is_noop_when_tab_not_found() {
    let mut app = BrowserAppState::new();
    let a = TabId(1);
    let b = TabId(2);
    app.push_tab(
      BrowserTabState::new(a, about_pages::ABOUT_NEWTAB.to_string()),
      true,
    );
    app.push_tab(
      BrowserTabState::new(b, about_pages::ABOUT_NEWTAB.to_string()),
      false,
    );

    assert!(!app.reorder_tab(TabId(999), 0));
    assert_eq!(
      app.tabs.iter().map(|t| t.id).collect::<Vec<_>>(),
      vec![a, b]
    );
  }

  #[test]
  fn reorder_tab_does_not_change_active_tab_id() {
    let mut app = BrowserAppState::new();
    let a = TabId(1);
    let b = TabId(2);
    let c = TabId(3);
    app.push_tab(
      BrowserTabState::new(a, about_pages::ABOUT_NEWTAB.to_string()),
      false,
    );
    app.push_tab(
      BrowserTabState::new(b, about_pages::ABOUT_NEWTAB.to_string()),
      true,
    );
    app.push_tab(
      BrowserTabState::new(c, about_pages::ABOUT_NEWTAB.to_string()),
      false,
    );

    let active_before = app.active_tab_id();
    assert_eq!(active_before, Some(b));

    assert!(app.reorder_tab(a, 2));
    assert_eq!(app.active_tab_id(), active_before);
  }

  fn assert_pinned_invariant(app: &BrowserAppState) {
    let pinned = app.tabs.iter().take_while(|t| t.pinned).count();
    assert!(
      app.tabs[..pinned].iter().all(|t| t.pinned),
      "expected pinned tabs at start"
    );
    assert!(
      app.tabs[pinned..].iter().all(|t| !t.pinned),
      "expected unpinned tabs after pinned segment"
    );
  }

  #[test]
  fn pin_and_unpin_move_tabs_and_preserve_contiguous_pinned_segment() {
    let mut app = BrowserAppState::new();
    let a = TabId(1);
    let b = TabId(2);
    let c = TabId(3);
    let d = TabId(4);
    app.push_tab(BrowserTabState::new(a, "about:newtab".to_string()), true);
    app.push_tab(BrowserTabState::new(b, "about:newtab".to_string()), false);
    app.push_tab(BrowserTabState::new(c, "about:newtab".to_string()), false);
    app.push_tab(BrowserTabState::new(d, "about:newtab".to_string()), false);

    // Pin an unpinned tab moves it into the pinned segment at the far left.
    assert!(app.pin_tab(c));
    assert_pinned_invariant(&app);
    assert_eq!(
      app.tabs.iter().map(|t| t.id).collect::<Vec<_>>(),
      vec![c, a, b, d]
    );
    assert!(app.tab(c).unwrap().pinned);

    // Pinning another tab appends it to the pinned segment (preserve order among pinned).
    assert!(app.pin_tab(a));
    assert_pinned_invariant(&app);
    assert_eq!(
      app.tabs.iter().map(|t| t.id).collect::<Vec<_>>(),
      vec![c, a, b, d]
    );
    // After pinning `a`, the pinned segment should be [c, a].
    assert!(app.tabs[0].pinned);
    assert!(app.tabs[1].pinned);

    // Unpinning a tab moves it to the start of the unpinned segment.
    assert!(app.unpin_tab(c));
    assert_pinned_invariant(&app);
    assert_eq!(
      app.tabs.iter().map(|t| t.id).collect::<Vec<_>>(),
      vec![a, c, b, d]
    );
    assert!(!app.tab(c).unwrap().pinned);
  }

  #[test]
  fn active_tab_id_survives_pin_and_unpin_reordering() {
    let mut app = BrowserAppState::new();
    let a = TabId(1);
    let b = TabId(2);
    let c = TabId(3);
    app.push_tab(BrowserTabState::new(a, "about:newtab".to_string()), true);
    app.push_tab(BrowserTabState::new(b, "about:newtab".to_string()), false);
    app.push_tab(BrowserTabState::new(c, "about:newtab".to_string()), false);

    app.set_active(b);
    assert_eq!(app.active_tab_id(), Some(b));

    assert!(app.pin_tab(c));
    assert_eq!(app.active_tab_id(), Some(b));
    assert_active_is_valid(&app);

    assert!(app.pin_tab(b));
    assert_eq!(app.active_tab_id(), Some(b));
    assert_active_is_valid(&app);

    assert!(app.unpin_tab(b));
    assert_eq!(app.active_tab_id(), Some(b));
    assert_active_is_valid(&app);
  }

  #[test]
  fn closed_tab_state_preserves_pinned_and_reopen_can_restore_it() {
    let mut app = BrowserAppState::new();
    let a = TabId(1);
    let b = TabId(2);
    app.push_tab(BrowserTabState::new(a, "about:newtab".to_string()), true);
    app.push_tab(BrowserTabState::new(b, "about:newtab".to_string()), false);

    assert!(app.pin_tab(a));
    assert!(app.tab(a).unwrap().pinned);
    assert_pinned_invariant(&app);

    let _ = app.remove_tab(a);
    let closed = app.pop_closed_tab().expect("expected closed tab state");
    assert!(closed.pinned);

    // Simulate "reopen closed tab": create a new tab from the closed state and preserve `pinned`.
    let reopened = TabId(3);
    let mut tab = BrowserTabState::new(reopened, closed.url);
    tab.title = closed.title.clone();
    tab.committed_title = closed.title;
    tab.pinned = closed.pinned;
    app.push_tab(tab, true);

    assert!(app.tab(reopened).unwrap().pinned);
    assert_pinned_invariant(&app);
    assert_eq!(app.tabs.first().map(|t| t.id), Some(reopened));
  }

  // Note: scroll restoration is worker-owned (see `ui::render_worker`), so the windowed UI state
  // model has no pending scroll restore bookkeeping to unit test here.
}

#[cfg(test)]
mod address_bar_tests {
  use super::*;

  #[test]
  fn sync_address_bar_to_active_does_not_clobber_while_editing() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "https://example.com/".to_string()),
      true,
    );

    app.set_address_bar_editing(true);
    app.set_address_bar_text("typed text".to_string());
    app.sync_address_bar_to_active();
    assert_eq!(app.chrome.address_bar_text, "typed text");
    assert!(app.chrome.address_bar_editing);
    assert!(app.chrome.address_bar_has_focus);

    app.set_address_bar_editing(false);
    assert!(!app.chrome.address_bar_editing);
    assert!(!app.chrome.address_bar_has_focus);
    assert_eq!(app.chrome.address_bar_text, "https://example.com/");
  }

  #[test]
  fn switching_tabs_cancels_address_bar_editing() {
    let mut app = BrowserAppState::new();
    let tab_a = TabId(1);
    let tab_b = TabId(2);
    app.push_tab(
      BrowserTabState::new(tab_a, "https://a.example/".to_string()),
      true,
    );
    app.push_tab(
      BrowserTabState::new(tab_b, "https://b.example/".to_string()),
      false,
    );

    app.chrome.address_bar_text = "partially typed".to_string();
    app.chrome.address_bar_editing = true;

    assert!(app.set_active_tab(tab_b));
    assert!(!app.chrome.address_bar_editing);
    assert_eq!(app.chrome.address_bar_text, "https://b.example/");
  }

  #[test]
  fn address_bar_editing_prevents_overwrite_until_commit() {
    let mut app = BrowserAppState::new_with_initial_tab("about:newtab".to_string());
    let tab_id = app.active_tab_id().unwrap();

    app.set_address_bar_editing(true);
    app.set_address_bar_text("https://typed.example".to_string());

    app.apply_worker_msg(WorkerToUi::NavigationStarted {
      tab_id,
      url: "https://started.example/".to_string(),
    });
    assert_eq!(
      app.chrome.address_bar_text, "https://typed.example",
      "worker updates should not clobber user typing"
    );

    app.apply_worker_msg(WorkerToUi::NavigationCommitted {
      tab_id,
      url: "https://committed.example/".to_string(),
      title: Some("Committed".to_string()),
      can_go_back: false,
      can_go_forward: false,
    });

    assert_eq!(
      app.chrome.address_bar_text, "https://typed.example",
      "worker updates should not clobber user typing"
    );
    assert_eq!(
      app.active_tab().and_then(|t| t.current_url()),
      Some("https://committed.example/")
    );

    let committed = app.commit_address_bar().unwrap();
    assert_eq!(committed, "https://typed.example/");
    assert!(!app.chrome.address_bar_editing);

    app.apply_worker_msg(WorkerToUi::NavigationCommitted {
      tab_id,
      url: "https://after.example/".to_string(),
      title: None,
      can_go_back: false,
      can_go_forward: false,
    });
    assert_eq!(
      app.chrome.address_bar_text, "https://after.example/",
      "after commit, address bar should follow tab display_url again"
    );
  }
}

#[cfg(test)]
mod tab_group_tests {
  use super::*;

  fn assert_group_contiguous(app: &BrowserAppState, group_id: TabGroupId) {
    let indices: Vec<usize> = app
      .tabs
      .iter()
      .enumerate()
      .filter_map(|(idx, tab)| (tab.group == Some(group_id)).then_some(idx))
      .collect();

    if indices.is_empty() {
      assert!(
        !app.tab_groups.contains_key(&group_id),
        "group state should not exist without member tabs"
      );
      return;
    }

    assert!(
      app.tab_groups.contains_key(&group_id),
      "group state must exist when tabs reference it"
    );
    for window in indices.windows(2) {
      assert_eq!(
        window[1],
        window[0] + 1,
        "group tabs must remain contiguous (indices={indices:?}, tabs={:?})",
        app.tabs.iter().map(|t| (t.id, t.group)).collect::<Vec<_>>()
      );
    }
  }

  #[test]
  fn create_group_makes_tabs_contiguous_and_moves_block() {
    let mut app = BrowserAppState::new();
    let a = TabId(1);
    let b = TabId(2);
    let c = TabId(3);
    let d = TabId(4);
    app.push_tab(
      BrowserTabState::new(a, about_pages::ABOUT_NEWTAB.to_string()),
      true,
    );
    app.push_tab(
      BrowserTabState::new(b, about_pages::ABOUT_NEWTAB.to_string()),
      false,
    );
    app.push_tab(
      BrowserTabState::new(c, about_pages::ABOUT_NEWTAB.to_string()),
      false,
    );
    app.push_tab(
      BrowserTabState::new(d, about_pages::ABOUT_NEWTAB.to_string()),
      false,
    );

    let group = app.create_group_with_tabs(&[b, d]);
    assert_ne!(group, TabGroupId(0));
    assert!(app.tab_groups.contains_key(&group));

    assert_eq!(
      app.tabs.iter().map(|t| t.id).collect::<Vec<_>>(),
      vec![a, b, d, c],
      "expected non-contiguous tabs to be moved adjacent to the first selected tab"
    );
    assert_eq!(app.tab(b).and_then(|t| t.group), Some(group));
    assert_eq!(app.tab(d).and_then(|t| t.group), Some(group));
    assert_eq!(app.tab(a).and_then(|t| t.group), None);
    assert_eq!(app.tab(c).and_then(|t| t.group), None);
    assert_group_contiguous(&app, group);
  }

  #[test]
  fn removing_last_tab_deletes_group() {
    let mut app = BrowserAppState::new();
    let a = TabId(1);
    let b = TabId(2);
    let c = TabId(3);
    app.push_tab(
      BrowserTabState::new(a, about_pages::ABOUT_NEWTAB.to_string()),
      true,
    );
    app.push_tab(
      BrowserTabState::new(b, about_pages::ABOUT_NEWTAB.to_string()),
      false,
    );
    app.push_tab(
      BrowserTabState::new(c, about_pages::ABOUT_NEWTAB.to_string()),
      false,
    );

    let group = app.create_group_with_tabs(&[a, b]);
    assert!(app.tab_groups.contains_key(&group));

    app.remove_tab_from_group(a);
    assert!(app.tab_groups.contains_key(&group));
    assert_eq!(app.tab(a).and_then(|t| t.group), None);
    assert_group_contiguous(&app, group);

    app.remove_tab_from_group(b);
    assert!(!app.tab_groups.contains_key(&group));
    assert_eq!(app.tab(b).and_then(|t| t.group), None);
    assert_group_contiguous(&app, group);
  }

  #[test]
  fn collapsing_group_hides_tabs_but_active_tab_expands_it() {
    let mut app = BrowserAppState::new();
    let a = TabId(1);
    let b = TabId(2);
    let c = TabId(3);
    app.push_tab(
      BrowserTabState::new(a, about_pages::ABOUT_NEWTAB.to_string()),
      true,
    );
    app.push_tab(
      BrowserTabState::new(b, about_pages::ABOUT_NEWTAB.to_string()),
      false,
    );
    app.push_tab(
      BrowserTabState::new(c, about_pages::ABOUT_NEWTAB.to_string()),
      false,
    );

    let group = app.create_group_with_tabs(&[a, b]);
    assert!(app.set_active_tab(c));

    app.toggle_group_collapsed(group);
    assert!(
      app.tab_groups.get(&group).is_some_and(|g| g.collapsed),
      "expected group to collapse when active tab is outside the group"
    );

    assert!(app.set_active_tab(a));
    assert!(
      app.tab_groups.get(&group).is_some_and(|g| !g.collapsed),
      "expected activating a tab in a collapsed group to expand it"
    );

    // Prevent collapsing the group while it contains the active tab.
    app.toggle_group_collapsed(group);
    assert!(
      app.tab_groups.get(&group).is_some_and(|g| !g.collapsed),
      "expected group not to collapse while it contains the active tab"
    );
  }

  #[test]
  fn drag_reorder_adds_and_removes_group_membership() {
    let mut app = BrowserAppState::new();
    let a = TabId(1);
    let b = TabId(2);
    let c = TabId(3);
    let d = TabId(4);
    let e = TabId(5);
    app.push_tab(
      BrowserTabState::new(a, about_pages::ABOUT_NEWTAB.to_string()),
      true,
    );
    app.push_tab(
      BrowserTabState::new(b, about_pages::ABOUT_NEWTAB.to_string()),
      false,
    );
    app.push_tab(
      BrowserTabState::new(c, about_pages::ABOUT_NEWTAB.to_string()),
      false,
    );
    app.push_tab(
      BrowserTabState::new(d, about_pages::ABOUT_NEWTAB.to_string()),
      false,
    );
    app.push_tab(
      BrowserTabState::new(e, about_pages::ABOUT_NEWTAB.to_string()),
      false,
    );

    let group = app.create_group_with_tabs(&[b, c, d]);
    assert_group_contiguous(&app, group);

    // Drag a grouped tab outside the group: it should become ungrouped.
    app.drag_reorder_tab(b, 0);
    assert_eq!(app.tab(b).and_then(|t| t.group), None);
    assert_group_contiguous(&app, group);

    // Drag an ungrouped tab into the group region: it should join the group.
    app.drag_reorder_tab(e, 3);
    assert_eq!(app.tab(e).and_then(|t| t.group), Some(group));
    assert_group_contiguous(&app, group);

    // Drag within the group should reorder while staying grouped.
    app.drag_reorder_tab(d, 2);
    assert_eq!(app.tab(d).and_then(|t| t.group), Some(group));
    assert_group_contiguous(&app, group);
  }

  #[test]
  fn session_revision_bumps_for_pin_and_unpin() {
    let mut app = BrowserAppState::new();
    let a = TabId(1);
    let b = TabId(2);
    app.push_tab(
      BrowserTabState::new(a, about_pages::ABOUT_NEWTAB.to_string()),
      true,
    );
    app.push_tab(
      BrowserTabState::new(b, about_pages::ABOUT_NEWTAB.to_string()),
      false,
    );

    let rev0 = app.session_revision();
    assert!(app.pin_tab(b));
    let rev1 = app.session_revision();
    assert!(rev1 > rev0, "expected pin to bump session revision");

    assert!(app.unpin_tab(b));
    let rev2 = app.session_revision();
    assert!(rev2 > rev1, "expected unpin to bump session revision");
  }

  #[test]
  fn session_revision_bumps_for_group_title_color_and_collapse_changes() {
    let mut app = BrowserAppState::new();
    let a = TabId(1);
    let b = TabId(2);
    let c = TabId(3);
    app.push_tab(
      BrowserTabState::new(a, about_pages::ABOUT_NEWTAB.to_string()),
      true,
    );
    app.push_tab(
      BrowserTabState::new(b, about_pages::ABOUT_NEWTAB.to_string()),
      false,
    );
    app.push_tab(
      BrowserTabState::new(c, about_pages::ABOUT_NEWTAB.to_string()),
      false,
    );

    let group = app.create_group_with_tabs(&[a, b]);

    // Ensure the active tab is outside the group so we can collapse it.
    assert!(app.set_active_tab(c));
    let rev0 = app.session_revision();

    app.set_group_title(group, "Renamed".to_string());
    let rev1 = app.session_revision();
    assert!(
      rev1 > rev0,
      "expected set_group_title to bump session revision"
    );

    app.set_group_color(group, TabGroupColor::Orange);
    let rev2 = app.session_revision();
    assert!(
      rev2 > rev1,
      "expected set_group_color to bump session revision"
    );

    app.toggle_group_collapsed(group);
    let rev3 = app.session_revision();
    assert!(
      rev3 > rev2,
      "expected toggle_group_collapsed to bump session revision"
    );
  }

  #[test]
  fn watchdog_marks_loading_tab_unresponsive_after_timeout() {
    use std::time::Duration;

    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    let mut tab = BrowserTabState::new(tab_id, "https://example.com/".to_string());
    tab.loading = true;
    tab.last_worker_msg_at = SystemTime::UNIX_EPOCH + Duration::from_secs(10);
    app.push_tab(tab, true);

    let timeout = Duration::from_secs(5);
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(16);
    assert!(
      app.update_unresponsive_tabs(now, timeout),
      "expected watchdog to mark the tab as unresponsive"
    );
    assert!(app.tab(tab_id).unwrap().unresponsive);
  }

  #[test]
  fn watchdog_does_not_mark_tabs_when_not_loading() {
    use std::time::Duration;

    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    let mut tab = BrowserTabState::new(tab_id, "https://example.com/".to_string());
    tab.loading = false;
    tab.last_worker_msg_at = SystemTime::UNIX_EPOCH;
    app.push_tab(tab, true);

    let timeout = Duration::from_secs(5);
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
    assert!(!app.update_unresponsive_tabs(now, timeout));
    assert!(!app.tab(tab_id).unwrap().unresponsive);
  }

  #[test]
  fn worker_messages_clear_unresponsive_and_refresh_timestamp() {
    use std::time::Duration;

    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    let mut tab = BrowserTabState::new(tab_id, "https://example.com/".to_string());
    tab.loading = true;
    tab.unresponsive = true;
    tab.last_worker_msg_at = SystemTime::UNIX_EPOCH;
    app.push_tab(tab, true);

    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(42);
    let _update = app.apply_worker_msg_at(
      WorkerToUi::Stage {
        tab_id,
        stage: StageHeartbeat::DomParse,
      },
      now,
    );

    let tab = app.tab(tab_id).unwrap();
    assert!(!tab.unresponsive);
    assert_eq!(tab.last_worker_msg_at, now);
  }

  #[test]
  fn dismiss_tab_unresponsive_resets_watchdog_timer() {
    use std::time::Duration;

    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    let mut tab = BrowserTabState::new(tab_id, "https://example.com/".to_string());
    tab.loading = true;
    tab.last_worker_msg_at = SystemTime::UNIX_EPOCH;
    app.push_tab(tab, true);

    let timeout = Duration::from_secs(5);
    let t1 = SystemTime::UNIX_EPOCH + Duration::from_secs(6);
    app.update_unresponsive_tabs(t1, timeout);
    assert!(app.tab(tab_id).unwrap().unresponsive);

    let t2 = SystemTime::UNIX_EPOCH + Duration::from_secs(7);
    assert!(app.dismiss_tab_unresponsive(tab_id, t2));
    assert!(!app.tab(tab_id).unwrap().unresponsive);

    // Not enough time has elapsed since dismissal.
    app.update_unresponsive_tabs(t2 + Duration::from_secs(4), timeout);
    assert!(!app.tab(tab_id).unwrap().unresponsive);

    // Timeout elapsed since dismissal.
    app.update_unresponsive_tabs(t2 + Duration::from_secs(6), timeout);
    assert!(app.tab(tab_id).unwrap().unresponsive);
  }
}
