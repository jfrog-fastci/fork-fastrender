use crate::render_control::StageHeartbeat;
use crate::scroll::ScrollState;
use crate::ui::about_pages;
use crate::ui::cancel::CancelGens;
use crate::ui::messages::{
  CursorKind, NavigationReason, RenderedFrame, ScrollMetrics, TabId, UiToWorker, WorkerToUi,
};
use crate::ui::{normalize_user_url, validate_user_navigation_url_scheme};
use std::collections::VecDeque;
use url::Url;

const DEBUG_LOG_CAPACITY: usize = 256;
const CLOSED_TAB_STACK_CAPACITY: usize = 20;

fn progress_for_stage(stage: StageHeartbeat) -> f32 {
  match stage {
    StageHeartbeat::ReadCache => 1.0 / 12.0,
    StageHeartbeat::FollowRedirects => 2.0 / 12.0,
    StageHeartbeat::CssInline => 3.0 / 12.0,
    StageHeartbeat::DomParse => 4.0 / 12.0,
    StageHeartbeat::Script => 5.0 / 12.0,
    StageHeartbeat::CssParse => 6.0 / 12.0,
    StageHeartbeat::Cascade => 7.0 / 12.0,
    StageHeartbeat::BoxTree => 8.0 / 12.0,
    StageHeartbeat::Layout => 9.0 / 12.0,
    StageHeartbeat::PaintBuild => 10.0 / 12.0,
    StageHeartbeat::PaintRasterize => 11.0 / 12.0,
    StageHeartbeat::Done => 1.0,
  }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LatestFrameMeta {
  pub pixmap_px: (u32, u32),
  pub viewport_css: (u32, u32),
  pub dpr: f32,
  pub wants_ticks: bool,
}

#[derive(Debug, Default)]
pub struct AppUpdate {
  /// Whether the front-end should schedule a repaint/redraw.
  pub request_redraw: bool,
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
  /// Shared cancellation generations for this tab.
  ///
  /// The UI thread can bump these counters (without blocking on the worker) to cancel in-flight
  /// navigation/paint work.
  pub cancel: CancelGens,
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
  pub error: Option<String>,
  /// Optional non-fatal warning for this tab (e.g. viewport clamping).
  pub warning: Option<String>,
  pub stage: Option<StageHeartbeat>,
  pub load_stage: Option<StageHeartbeat>,
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
  pub scroll_state: ScrollState,
  pub scroll_metrics: Option<ScrollMetrics>,
  pub latest_frame_meta: Option<LatestFrameMeta>,
  pub favicon_meta: Option<FaviconMeta>,
  debug_log: VecDeque<String>,
}

impl BrowserTabState {
  pub fn new(tab_id: TabId, initial_url: String) -> Self {
    let committed_url = initial_url.clone();
    Self {
      id: tab_id,
      cancel: CancelGens::new(),
      current_url: Some(initial_url),
      committed_url: Some(committed_url),
      title: None,
      committed_title: None,
      loading: false,
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

  pub fn display_title(&self) -> String {
    if let Some(title) = self.title.as_ref().filter(|t| !t.trim().is_empty()) {
      return title.clone();
    }
    self
      .current_url()
      .map(str::to_string)
      .unwrap_or_else(|| "New Tab".to_string())
  }

  /// Returns a deterministic monotonic progress fraction for a chrome loading indicator.
  ///
  /// - `None` when this tab is not loading.
  /// - `Some(0.0)` when loading but no stage heartbeat has been observed yet.
  pub fn chrome_loading_progress(&self) -> Option<f32> {
    crate::ui::chrome_loading_progress::chrome_loading_progress(self.loading, self.stage)
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
    let raw_trimmed = raw.trim();

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
      normalize_user_url(raw_trimmed)?
    };
    validate_user_navigation_url_scheme(&normalized)?;

    self.current_url = Some(normalized.clone());
    self.loading = true;
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

    let stage_progress = progress_for_stage(stage).clamp(0.0, 1.0);
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
#[derive(Debug, Default)]
pub struct ChromeState {
  pub address_bar_text: String,
  /// True while the user is actively editing the address bar.
  ///
  /// While this is true, we avoid auto-syncing the address bar text from navigation events so
  /// in-progress input is not clobbered.
  pub address_bar_editing: bool,
  pub address_bar_has_focus: bool,
  /// One-frame request flag consumed by `chrome_ui` to focus the address bar.
  pub request_focus_address_bar: bool,
  /// One-frame request flag consumed by `chrome_ui` to select all text in the address bar.
  pub request_select_all_address_bar: bool,
}

#[derive(Debug)]
pub struct BrowserAppState {
  pub tabs: Vec<BrowserTabState>,
  pub active_tab: Option<TabId>,
  pub closed_tabs: Vec<ClosedTabState>,
  pub chrome: ChromeState,
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
      chrome: ChromeState::default(),
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

  pub fn tab(&self, tab_id: TabId) -> Option<&BrowserTabState> {
    self.tabs.iter().find(|t| t.id == tab_id)
  }

  pub fn tab_mut(&mut self, tab_id: TabId) -> Option<&mut BrowserTabState> {
    self.tabs.iter_mut().find(|t| t.id == tab_id)
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
    // Switching tabs should always reflect the newly active tab URL in the address bar. If the
    // user was typing, cancel that edit rather than carrying the partially typed URL across tabs.
    self.chrome.address_bar_editing = false;
    self.sync_address_bar_to_active();
    if let Some(tab) = self.tab_mut(tab_id) {
      tab.hovered_url = None;
      tab.cursor = CursorKind::Default;
    }
    true
  }

  pub fn set_active(&mut self, tab_id: TabId) {
    let _ = self.set_active_tab(tab_id);
  }

  pub fn push_tab(&mut self, tab: BrowserTabState, make_active: bool) {
    let tab_id = tab.id;
    self.tabs.push(tab);
    if make_active || self.active_tab.is_none() {
      self.active_tab = Some(tab_id);
      self.chrome.address_bar_editing = false;
      self.sync_address_bar_to_active();
    }
  }

  pub fn create_tab(&mut self, initial_url: Option<String>) -> TabId {
    let url = initial_url.unwrap_or_else(|| about_pages::ABOUT_NEWTAB.to_string());
    let tab_id = TabId::new();
    self.push_tab(BrowserTabState::new(tab_id, url), true);
    tab_id
  }

  pub fn close_tab(&mut self, tab_id: TabId) {
    let _ = self.remove_tab(tab_id);
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

    let closed = self.tabs.remove(idx);
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
    });

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
    self.active_tab = Some(new_active);
    self.chrome.address_bar_editing = false;
    self.sync_address_bar_to_active();
    RemoveTabResult {
      new_active: Some(new_active),
      created_tab: None,
    }
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
      self.sync_address_bar_to_active();
    }
  }

  pub fn set_address_bar_text(&mut self, text: String) {
    self.chrome.address_bar_text = text;
  }

  pub fn commit_address_bar(&mut self) -> Result<String, String> {
    let tab_id = self.active_tab.ok_or_else(|| "no active tab".to_string())?;

    let normalized = normalize_user_url(&self.chrome.address_bar_text)?;
    validate_user_navigation_url_scheme(&normalized)?;

    self.chrome.address_bar_editing = false;
    self.chrome.address_bar_has_focus = false;
    self.chrome.address_bar_text = normalized.clone();

    if let Some(tab) = self.tab_mut(tab_id) {
      tab.current_url = Some(normalized.clone());
      tab.loading = true;
      tab.error = None;
      tab.stage = None;
      tab.reset_load_progress();
    }

    Ok(normalized)
  }

  pub fn apply_worker_msg(&mut self, msg: WorkerToUi) -> AppUpdate {
    let mut update = AppUpdate::default();

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

        if let Some(tab) = self.tab_mut(tab_id) {
          tab.scroll_state = scroll_state;
          tab.scroll_metrics = Some(scroll_metrics);
          tab.latest_frame_meta = Some(LatestFrameMeta {
            pixmap_px,
            viewport_css,
            dpr,
            wants_ticks,
          });
        }

        update.request_redraw = true;
        update.frame_ready = Some(FrameReadyUpdate {
          tab_id,
          pixmap,
          viewport_css,
          dpr,
        });
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
          control,
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
          control,
          anchor_css: Some(anchor_css),
        });
      }
      WorkerToUi::SelectDropdownClosed { .. } => {
        // Front-ends that show a `<select>` overlay should dismiss it.
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
        if let Some(tab) = self.tab_mut(tab_id) {
          tab.current_url = Some(url.clone());
          tab.loading = true;
          tab.error = None;
          tab.stage = None;
          tab.reset_load_progress();
          tab.favicon_meta = None;
          tab.hovered_url = None;
          tab.cursor = CursorKind::Default;
        }
        if self.active_tab_id() == Some(tab_id) && !self.chrome.address_bar_editing {
          self.chrome.address_bar_text = url;
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
        if let Some(tab) = self.tab_mut(tab_id) {
          tab.current_url = Some(url.clone());
          tab.committed_url = Some(url.clone());
          let committed_title = title.clone();
          tab.title = title;
          tab.committed_title = committed_title;
          tab.loading = false;
          tab.error = None;
          tab.stage = None;
          tab.clear_load_progress();
          tab.can_go_back = can_go_back;
          tab.can_go_forward = can_go_forward;
          tab.hovered_url = None;
          tab.cursor = CursorKind::Default;
        }
        if self.active_tab_id() == Some(tab_id) && !self.chrome.address_bar_editing {
          self.chrome.address_bar_text = url;
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
        if let Some(tab) = self.tab_mut(tab_id) {
          tab.current_url = Some(url.clone());
          tab.loading = false;
          tab.error = Some(error);
          tab.stage = None;
          tab.clear_load_progress();
          tab.can_go_back = can_go_back;
          tab.can_go_forward = can_go_forward;
          tab.title = None;
          tab.favicon_meta = None;
          tab.hovered_url = None;
          tab.cursor = CursorKind::Default;
        }
        if self.active_tab_id() == Some(tab_id) && !self.chrome.address_bar_editing {
          self.chrome.address_bar_text = url;
        }
        update.request_redraw = true;
      }
      WorkerToUi::Favicon {
        tab_id,
        rgba,
        width,
        height,
      } => {
        if let Some(tab) = self.tab_mut(tab_id) {
          tab.favicon_meta = Some(FaviconMeta {
            size_px: (width, height),
          });
        }
        update.request_redraw = true;
        update.favicon_ready = Some(FaviconReadyUpdate {
          tab_id,
          rgba,
          width,
          height,
        });
      }
      WorkerToUi::RequestOpenInNewTab { .. } => {
        // The UI owns tab identifiers; front-ends are expected to handle this message directly by
        // allocating a new tab id and issuing `CreateTab`/`Navigate`. The shared state model does
        // not automatically create tabs.
        update.request_redraw = true;
      }
      WorkerToUi::ScrollStateUpdated { tab_id, scroll } => {
        if let Some(tab) = self.tab_mut(tab_id) {
          tab.scroll_state = scroll;
        }
        update.request_redraw = true;
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
        if let Some(tab) = self.tab_mut(tab_id) {
          tab.warning = Some(text);
        }
        update.request_redraw = self.active_tab_id() == Some(tab_id);
      }
      WorkerToUi::DebugLog { tab_id, line } => {
        if let Some(tab) = self.tab_mut(tab_id) {
          tab.push_debug_log(line);
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
        if let Some(tab) = self.tab_mut(tab_id) {
          tab.hovered_url = hovered_url;
          tab.cursor = cursor;
        }
        update.request_redraw = self.active_tab_id() == Some(tab_id);
      }
      WorkerToUi::SetClipboardText { .. } => {
        // Clipboard is handled by the front-end (e.g. `src/bin/browser.rs`); the shared app state
        // model does not store clipboard contents.
        update.request_redraw = true;
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
      }]
    );

    let popped = app.pop_closed_tab().expect("expected closed tab state");
    assert_eq!(
      popped,
      ClosedTabState {
        url: "https://committed.example/".to_string(),
        title: Some("Committed".to_string()),
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

  fn ordered_stage_heartbeats() -> [StageHeartbeat; 12] {
    [
      StageHeartbeat::ReadCache,
      StageHeartbeat::FollowRedirects,
      StageHeartbeat::CssInline,
      StageHeartbeat::DomParse,
      StageHeartbeat::Script,
      StageHeartbeat::CssParse,
      StageHeartbeat::Cascade,
      StageHeartbeat::BoxTree,
      StageHeartbeat::Layout,
      StageHeartbeat::PaintBuild,
      StageHeartbeat::PaintRasterize,
      StageHeartbeat::Done,
    ]
  }

  #[test]
  fn progress_for_stage_is_monotonic_in_stage_order() {
    let mut prev = 0.0;
    for stage in ordered_stage_heartbeats() {
      let p = progress_for_stage(stage);
      assert!(p.is_finite());
      assert!(p > prev, "expected progress to increase (prev={prev}, next={p})");
      assert!(p >= 0.0);
      assert!(p <= 1.0);
      prev = p;
    }
    assert!((prev - 1.0).abs() < 1e-6);
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

    for stage in ordered_stage_heartbeats() {
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
  fn out_of_order_stages_do_not_reduce_load_progress() {
    let mut app = BrowserAppState::new_with_initial_tab("about:newtab".to_string());
    let tab_id = app.active_tab_id().unwrap();
    app.apply_worker_msg(WorkerToUi::NavigationStarted {
      tab_id,
      url: "https://example.com/".to_string(),
    });

    app.apply_worker_msg(WorkerToUi::Stage {
      tab_id,
      stage: StageHeartbeat::Layout,
    });
    let p1 = app.active_tab().unwrap().load_progress.unwrap();

    // Regressing heartbeat must not reduce progress.
    app.apply_worker_msg(WorkerToUi::Stage {
      tab_id,
      stage: StageHeartbeat::ReadCache,
    });
    let p2 = app.active_tab().unwrap().load_progress.unwrap();

    assert!((p2 - p1).abs() < 1e-6);
  }

  #[test]
  fn load_progress_resets_on_navigation_started() {
    let mut app = BrowserAppState::new_with_initial_tab("about:newtab".to_string());
    let tab_id = app.active_tab_id().unwrap();

    app.apply_worker_msg(WorkerToUi::NavigationStarted {
      tab_id,
      url: "https://example.com/".to_string(),
    });
    app.apply_worker_msg(WorkerToUi::Stage {
      tab_id,
      stage: StageHeartbeat::Layout,
    });

    let before = app.active_tab().unwrap().load_progress.unwrap();
    assert!(before > 0.0);

    app.apply_worker_msg(WorkerToUi::NavigationStarted {
      tab_id,
      url: "https://second.example/".to_string(),
    });

    let tab = app.active_tab().unwrap();
    assert_eq!(tab.load_stage, None);
    assert_eq!(tab.load_progress, Some(0.0));
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
