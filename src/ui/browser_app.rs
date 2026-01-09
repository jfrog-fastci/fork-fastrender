use crate::render_control::StageHeartbeat;
use crate::scroll::ScrollState;
use crate::ui::messages::{NavigationReason, TabId, UiToWorker};
use crate::ui::normalize_user_url;
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LatestFrameMeta {
  pub pixmap_px: (u32, u32),
  pub viewport_css: (u32, u32),
  pub dpr: f32,
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
  /// The URL most recently sent to the worker for navigation.
  ///
  /// Used to associate `NavigationCommitted`/`NavigationFailed` messages with the initiating URL.
  pub pending_nav_url: Option<String>,
}

impl BrowserTabState {
  pub fn new(tab_id: TabId, initial_url: String) -> Self {
    Self {
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
      pending_nav_url: None,
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


  /// Validate + normalize an address-bar navigation and produce a `UiToWorker::Navigate` message.
  ///
  /// This applies a scheme allowlist for typed URLs (http/https/file/about), rejecting
  /// `javascript:` and unknown schemes. On failure, the returned error is intended for
  /// user-facing display.
  ///
  /// On success, this marks the tab as loading, updates `current_url`, and sets `pending_nav_url`.
  pub fn navigate_typed(&mut self, raw: &str) -> Result<UiToWorker, String> {
    let normalized = normalize_user_url(raw)?;
    validate_typed_url_scheme(&normalized)?;

    self.current_url = Some(normalized.clone());
    self.loading = true;
    self.error = None;
    self.pending_nav_url = Some(normalized.clone());

    Ok(UiToWorker::Navigate {
      tab_id: self.id,
      url: normalized,
      reason: NavigationReason::TypedUrl,
    })
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

  pub fn push_tab(&mut self, tab: BrowserTabState, make_active: bool) {
    let tab_id = tab.id;
    self.tabs.push(tab);
    if make_active || self.active_tab.is_none() {
      self.active_tab = Some(tab_id);
      self.chrome.address_bar_editing = false;
      self.sync_address_bar_to_active();
    }
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
        BrowserTabState::new(new_tab_id, "about:newtab".to_string()),
        true,
      );
      return RemoveTabResult {
        new_active: Some(new_tab_id),
        created_tab: Some(new_tab_id),
      };
    }

    // Prefer the tab that shifted into the removed index, otherwise the new last tab.
    let new_active = self.tabs.get(idx).or_else(|| self.tabs.last()).unwrap().id;
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
}

#[cfg(test)]
mod browser_app_tests {
  use super::*;

  #[test]
  fn closing_last_tab_immediately_creates_newtab() {
    let _lock = crate::ui::messages::TAB_ID_TEST_LOCK
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());

    let mut app = BrowserAppState::new();

    let tab_id = TabId(1_000_000);
    app.push_tab(
      BrowserTabState::new(tab_id, "about:newtab".to_string()),
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
      Some("about:newtab")
    );
  }

  #[test]
  fn closing_active_tab_keeps_existing_tab_when_available() {
    let mut app = BrowserAppState::new();

    let a = TabId(1_000_000);
    let b = TabId(1_000_001);
    app.push_tab(BrowserTabState::new(a, "about:newtab".to_string()), true);
    app.push_tab(BrowserTabState::new(b, "about:newtab".to_string()), false);
    assert_eq!(app.active_tab_id(), Some(a));

    let result = app.remove_tab(a);
    assert_eq!(result.created_tab, None);
    assert_eq!(app.tabs.len(), 1);
    assert_eq!(app.active_tab_id(), Some(b));
    assert_eq!(result.new_active, Some(b));
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
}
