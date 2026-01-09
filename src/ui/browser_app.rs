use crate::render_control::StageHeartbeat;
use crate::scroll::ScrollState;
use crate::ui::about_pages;
use crate::ui::messages::{NavigationReason, RenderedFrame, TabId, UiToWorker, WorkerToUi};
use crate::ui::history::TabHistory;
use crate::ui::normalize_user_url;
use std::collections::VecDeque;
use url::Url;

const DEBUG_LOG_CAPACITY: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LatestFrameMeta {
  pub pixmap_px: (u32, u32),
  pub viewport_css: (u32, u32),
  pub dpr: f32,
}

#[derive(Debug, Default)]
pub struct AppUpdate {
  /// Whether the front-end should schedule a repaint/redraw.
  pub request_redraw: bool,
  /// Recommended full window title for the host window.
  pub set_window_title: Option<String>,
  /// A new pixmap is ready for upload; the state model does not store pixel buffers.
  pub frame_ready: Option<FrameReadyUpdate>,
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
  pub title: Option<String>,
  pub current_url: Option<String>,
  pub loading: bool,
  pub error: Option<String>,
  pub stage: Option<StageHeartbeat>,
  pub can_go_back: bool,
  pub can_go_forward: bool,
  pub scroll_state: ScrollState,
  pub latest_frame_meta: Option<LatestFrameMeta>,
  pub history: TabHistory,
  /// When navigating via Back/Forward/Reload, the UI restores the scroll offset stored in history
  /// by sending a follow-up `UiToWorker::Scroll` after navigation has committed.
  ///
  /// The restore is intentionally tracked in the UI (rather than in the worker protocol) so that
  /// history remains UI-owned.
  pub pending_restore_scroll: Option<(f32, f32)>,
  // Internal bookkeeping so we restore at the *later* of:
  // - the first `FrameReady` for the new page (ensures we don't clobber history scroll with a
  //   pre-restore scroll=0 frame), and
  // - `NavigationCommitted` (ensures we don't restore before navigation has committed).
  pub pending_restore_nav_committed: bool,
  pub pending_restore_frame_ready: bool,
  /// The URL most recently sent to the worker for navigation.
  ///
  /// Used to associate `NavigationCommitted`/`NavigationFailed` messages with the initiating URL.
  pub pending_nav_url: Option<String>,
  debug_log: VecDeque<String>,
}

impl BrowserTabState {
  pub fn new(tab_id: TabId, initial_url: String) -> Self {
    let history = TabHistory::with_initial(initial_url.clone());
    let mut tab = Self {
      id: tab_id,
      title: None,
      current_url: Some(initial_url),
      loading: false,
      error: None,
      stage: None,
      can_go_back: false,
      can_go_forward: false,
      scroll_state: ScrollState::default(),
      latest_frame_meta: None,
      history,
      pending_restore_scroll: None,
      pending_restore_nav_committed: false,
      pending_restore_frame_ready: false,
      pending_nav_url: None,
      debug_log: VecDeque::new(),
    };
    tab.sync_nav_flags_from_history();
    tab
  }

  pub fn current_url(&self) -> Option<&str> {
    self.current_url.as_deref()
  }

  fn sanitize_scroll_restore_target(value: f32) -> f32 {
    if value.is_finite() { value.max(0.0) } else { 0.0 }
  }

  pub fn begin_scroll_restore(&mut self, scroll_x: f32, scroll_y: f32) {
    self.pending_restore_scroll = Some((
      Self::sanitize_scroll_restore_target(scroll_x),
      Self::sanitize_scroll_restore_target(scroll_y),
    ));
    self.pending_restore_nav_committed = false;
    self.pending_restore_frame_ready = false;
  }

  pub fn clear_scroll_restore(&mut self) {
    self.pending_restore_scroll = None;
    self.pending_restore_nav_committed = false;
    self.pending_restore_frame_ready = false;
  }

  pub fn note_scroll_restore_nav_committed(&mut self) {
    if self.pending_restore_scroll.is_some() {
      self.pending_restore_nav_committed = true;
    }
  }

  pub fn note_scroll_restore_frame_ready(&mut self) {
    // Ignore `FrameReady` signals until we've seen `NavigationCommitted` for the navigation we're
    // restoring. This prevents a late frame from the *previous* page (e.g. from a recent scroll)
    // from being mistaken as the first frame of the new navigation and causing us to compute the
    // restore delta against the wrong baseline.
    if self.pending_restore_scroll.is_some() && self.pending_restore_nav_committed {
      self.pending_restore_frame_ready = true;
    }
  }

  /// Returns a scroll delta needed to restore the pending target, clearing the pending restore.
  ///
  /// Restoration is ready once we've seen both:
  /// - `NavigationCommitted`, and
  /// - a `FrameReady` for the new page (so pre-restore frames don't overwrite history scroll).
  pub fn take_scroll_restore_delta_if_ready(&mut self) -> Option<(f32, f32)> {
    let Some((target_x, target_y)) = self.pending_restore_scroll else {
      return None;
    };
    if !(self.pending_restore_nav_committed && self.pending_restore_frame_ready) {
      return None;
    }

    let current = self.scroll_state.viewport;
    let current_x = if current.x.is_finite() { current.x } else { 0.0 };
    let current_y = if current.y.is_finite() { current.y } else { 0.0 };
    let mut delta_x = target_x - current_x;
    let mut delta_y = target_y - current_y;
    if !delta_x.is_finite() {
      delta_x = 0.0;
    }
    if !delta_y.is_finite() {
      delta_y = 0.0;
    }

    self.clear_scroll_restore();
    Some((delta_x, delta_y))
  }

  pub fn sync_nav_flags_from_history(&mut self) {
    self.can_go_back = self.history.can_go_back();
    self.can_go_forward = self.history.can_go_forward();
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


  /// Validate + normalize an address-bar navigation and produce a `UiToWorker::Navigate` message.
  ///
  /// This applies a scheme allowlist for typed URLs (http/https/file/about), rejecting
  /// `javascript:` and unknown schemes. On failure, the returned error is intended for
  /// user-facing display.
  ///
  /// On success, this marks the tab as loading, updates `current_url`, and sets `pending_nav_url`.
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
    validate_typed_url_scheme(&normalized)?;

    self.history.push(normalized.clone());
    self.sync_nav_flags_from_history();

    self.current_url = Some(normalized.clone());
    self.loading = true;
    self.error = None;
    self.title = None;
    self.pending_nav_url = Some(normalized.clone());

    Ok(UiToWorker::Navigate {
      tab_id: self.id,
      url: normalized,
      reason: NavigationReason::TypedUrl,
    })
  }

  pub fn debug_log(&self) -> impl Iterator<Item = &str> {
    self.debug_log.iter().map(String::as_str)
  }

  fn push_debug_log(&mut self, line: String) {
    if self.debug_log.len() >= DEBUG_LOG_CAPACITY {
      self.debug_log.pop_front();
    }
    self.debug_log.push_back(line);
  }
}

fn validate_typed_url_scheme(url: &str) -> Result<(), String> {
  let parsed = Url::parse(url).map_err(|err| err.to_string())?;
  let scheme = parsed.scheme().to_ascii_lowercase();
  match scheme.as_str() {
    "http" | "https" | "file" | "about" => Ok(()),
    "javascript" => Err("typed navigation to javascript: URLs is not supported".to_string()),
    _ => Err(format!("unsupported URL scheme for typed navigation: {scheme}")),
  }
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
    assert_eq!(tab.pending_nav_url, None);
    assert!(!tab.loading);
    assert_eq!(tab.current_url, before);
  }

  #[test]
  fn typed_unknown_scheme_is_rejected() {
    let mut tab = BrowserTabState::new(TabId(1), "about:newtab".to_string());
    let before = tab.current_url.clone();
    assert!(tab.navigate_typed("foo:bar").is_err());
    assert_eq!(tab.pending_nav_url, None);
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
    assert_eq!(tab.pending_nav_url.as_deref(), Some("about:blank"));
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

    assert_eq!(tab.current_url(), Some("https://example.com/page.html#target"));
    assert_eq!(
      tab.pending_nav_url.as_deref(),
      Some("https://example.com/page.html#target")
    );
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
  pub chrome: ChromeState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoveTabResult {
  /// New active tab id (only set when the closed tab was the active tab).
  pub new_active: Option<TabId>,
  /// New tab created to maintain the "at least one tab" invariant.
  pub created_tab: Option<TabId>,
}

impl BrowserAppState {
  pub fn new() -> Self {
    Self {
      tabs: Vec::new(),
      active_tab: None,
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
  /// Invariant: closing the last tab will immediately create a new `about:newtab` tab and make it
  /// active.
  pub fn remove_tab(&mut self, tab_id: TabId) -> RemoveTabResult {
    let Some(idx) = self.tabs.iter().position(|t| t.id == tab_id) else {
      return RemoveTabResult {
        new_active: None,
        created_tab: None,
      };
    };

    self.tabs.remove(idx);

    let was_active = self.active_tab == Some(tab_id);
    if !was_active {
      return RemoveTabResult {
        new_active: None,
        created_tab: None,
      };
    }

    if self.tabs.is_empty() {
      let new_tab_id = TabId::new();
      self.push_tab(
        BrowserTabState::new(new_tab_id, about_pages::ABOUT_NEWTAB.to_string()),
        true,
      );
      return RemoveTabResult {
        new_active: Some(new_tab_id),
        created_tab: Some(new_tab_id),
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
    let tab_id = self
      .active_tab
      .ok_or_else(|| "no active tab".to_string())?;

    let normalized = normalize_user_url(&self.chrome.address_bar_text)?;
    validate_typed_url_scheme(&normalized)?;

    self.chrome.address_bar_editing = false;
    self.chrome.address_bar_has_focus = false;
    self.chrome.address_bar_text = normalized.clone();

    if let Some(tab) = self.tab_mut(tab_id) {
      tab.current_url = Some(normalized.clone());
      tab.loading = true;
      tab.error = None;
      tab.stage = None;
      tab.pending_nav_url = Some(normalized.clone());
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
        } = frame;
        let pixmap_px = (pixmap.width(), pixmap.height());

        if let Some(tab) = self.tab_mut(tab_id) {
          tab.scroll_state = scroll_state;
          tab.latest_frame_meta = Some(LatestFrameMeta {
            pixmap_px,
            viewport_css,
            dpr,
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
      WorkerToUi::Stage { tab_id, stage } => {
        if let Some(tab) = self.tab_mut(tab_id) {
          tab.stage = Some(stage);
          update.request_redraw = true;
        }
      }
      WorkerToUi::NavigationStarted { tab_id, url } => {
        if let Some(tab) = self.tab_mut(tab_id) {
          tab.current_url = Some(url.clone());
          tab.loading = true;
          tab.error = None;
          tab.stage = None;
          tab.pending_nav_url = Some(url.clone());
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
          tab.title = title;
          tab.loading = false;
          tab.error = None;
          tab.stage = None;
          tab.pending_nav_url = None;
          tab.can_go_back = can_go_back;
          tab.can_go_forward = can_go_forward;
        }
        if self.active_tab_id() == Some(tab_id) && !self.chrome.address_bar_editing {
          self.chrome.address_bar_text = url;
        }
        update.request_redraw = true;
      }
      WorkerToUi::NavigationFailed { tab_id, url, error } => {
        if let Some(tab) = self.tab_mut(tab_id) {
          tab.loading = false;
          tab.error = Some(error);
          tab.stage = None;
          tab.pending_nav_url = None;
        }
        if self.active_tab_id() == Some(tab_id) && !self.chrome.address_bar_editing {
          self.chrome.address_bar_text = url;
        }
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
        }
        update.request_redraw = true;
      }
      WorkerToUi::DebugLog { tab_id, line } => {
        if let Some(tab) = self.tab_mut(tab_id) {
          tab.push_debug_log(line);
        }
        update.request_redraw = self.active_tab_id() == Some(tab_id);
      }
    }

    update
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
  fn closing_last_tab_immediately_creates_newtab() {
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
    let new_tab_id = app.active_tab_id().expect("must have active tab after close");
    assert_ne!(new_tab_id, tab_id);
    assert_eq!(result.new_active, Some(new_tab_id));
    assert_eq!(result.created_tab, Some(new_tab_id));
    assert_eq!(
      app.tab(new_tab_id).and_then(|t| t.current_url()),
      Some(about_pages::ABOUT_NEWTAB)
    );
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

    // Closing the last tab should auto-create a new one.
    let last = app.active_tab_id().unwrap();
    app.close_tab(last);
    assert_eq!(app.tabs.len(), 1);
    assert_active_is_valid(&app);
    assert_eq!(
      app.active_tab().and_then(|t| t.current_url()),
      Some(about_pages::ABOUT_NEWTAB)
    );
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

    let update = app.apply_worker_msg(WorkerToUi::FrameReady {
      tab_id,
      frame: RenderedFrame {
        pixmap,
        viewport_css,
        dpr,
        scroll_state: expected_scroll.clone(),
      },
    });

    let tab = app.active_tab().unwrap();
    assert_eq!(tab.scroll_state, expected_scroll);
    assert_eq!(
      tab.latest_frame_meta,
      Some(LatestFrameMeta {
        pixmap_px: (2, 3),
        viewport_css,
        dpr
      })
    );

    let ready = update.frame_ready.expect("expected FrameReadyUpdate");
    assert_eq!(ready.tab_id, tab_id);
    assert_eq!(ready.viewport_css, viewport_css);
    assert!((ready.dpr - dpr).abs() < f32::EPSILON);
    assert_eq!((ready.pixmap.width(), ready.pixmap.height()), (2, 3));
  }

  #[test]
  fn browser_tab_state_starts_without_pending_scroll_restore() {
    let tab = BrowserTabState::new(TabId(1), "about:blank".to_string());
    assert_eq!(tab.pending_restore_scroll, None);
    assert!(!tab.pending_restore_nav_committed);
    assert!(!tab.pending_restore_frame_ready);
  }

  #[test]
  fn browser_tab_state_pending_scroll_restore_can_be_set_and_cleared() {
    let mut tab = BrowserTabState::new(TabId(1), "about:blank".to_string());
    tab.begin_scroll_restore(10.0, 20.0);
    assert_eq!(tab.pending_restore_scroll, Some((10.0, 20.0)));
    assert!(!tab.pending_restore_nav_committed);
    assert!(!tab.pending_restore_frame_ready);

    tab.clear_scroll_restore();
    assert_eq!(tab.pending_restore_scroll, None);
    assert!(!tab.pending_restore_nav_committed);
    assert!(!tab.pending_restore_frame_ready);
  }

  #[test]
  fn browser_tab_state_scroll_restore_does_not_use_old_frames() {
    let mut tab = BrowserTabState::new(TabId(1), "about:blank".to_string());
    tab.begin_scroll_restore(10.0, 20.0);

    // A FrameReady from before the navigation commits should not be used as the restore trigger.
    tab.note_scroll_restore_frame_ready();
    assert!(!tab.pending_restore_frame_ready);

    tab.note_scroll_restore_nav_committed();
    tab.note_scroll_restore_frame_ready();
    assert!(tab.pending_restore_frame_ready);
  }
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

    app.chrome.address_bar_text = "typed text".to_string();
    app.chrome.address_bar_editing = true;
    app.sync_address_bar_to_active();
    assert_eq!(app.chrome.address_bar_text, "typed text");

    app.chrome.address_bar_editing = false;
    app.sync_address_bar_to_active();
    assert_eq!(app.chrome.address_bar_text, "https://example.com/");
  }

  #[test]
  fn switching_tabs_cancels_address_bar_editing() {
    let mut app = BrowserAppState::new();
    let tab_a = TabId(1);
    let tab_b = TabId(2);
    app.push_tab(BrowserTabState::new(tab_a, "https://a.example/".to_string()), true);
    app.push_tab(BrowserTabState::new(tab_b, "https://b.example/".to_string()), false);

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

    app.apply_worker_msg(WorkerToUi::NavigationCommitted {
      tab_id,
      url: "https://committed.example/".to_string(),
      title: Some("Committed".to_string()),
      can_go_back: false,
      can_go_forward: false,
    });

    assert_eq!(
      app.chrome.address_bar_text,
      "https://typed.example",
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
      app.chrome.address_bar_text,
      "https://after.example/",
      "after commit, address bar should follow tab display_url again"
    );
  }
}
