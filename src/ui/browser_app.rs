use crate::render_control::StageHeartbeat;
use crate::scroll::ScrollState;
use crate::ui::history::TabHistory;
use crate::ui::messages::TabId;

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
  pub loading: bool,
  pub error: Option<String>,
  pub stage: Option<StageHeartbeat>,
  pub can_go_back: bool,
  pub can_go_forward: bool,
  pub scroll_state: ScrollState,
  pub latest_frame_meta: Option<LatestFrameMeta>,
  pub history: TabHistory,
  /// The URL most recently sent to the worker for navigation.
  ///
  /// Used to associate `NavigationCommitted`/`NavigationFailed` messages with the initiating URL.
  pub pending_nav_url: Option<String>,
}

impl BrowserTabState {
  pub fn new(tab_id: TabId, initial_url: String) -> Self {
    let history = TabHistory::with_initial(initial_url);
    let can_go_back = history.can_go_back();
    let can_go_forward = history.can_go_forward();
    Self {
      id: tab_id,
      title: None,
      loading: false,
      error: None,
      stage: None,
      can_go_back,
      can_go_forward,
      scroll_state: ScrollState::default(),
      latest_frame_meta: None,
      history,
      pending_nav_url: None,
    }
  }

  pub fn current_url(&self) -> Option<&str> {
    self.history.current().map(|entry| entry.url.as_str())
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

  pub fn sync_nav_flags_from_history(&mut self) {
    self.can_go_back = self.history.can_go_back();
    self.can_go_forward = self.history.can_go_forward();
  }
}

#[derive(Debug, Default)]
pub struct ChromeState {
  pub address_bar_text: String,
  pub address_bar_has_focus: bool,
}

#[derive(Debug)]
pub struct BrowserAppState {
  pub tabs: Vec<BrowserTabState>,
  pub active_tab: Option<TabId>,
  pub chrome: ChromeState,
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
    self.sync_address_bar_to_active();
    true
  }

  pub fn push_tab(&mut self, tab: BrowserTabState, make_active: bool) {
    let tab_id = tab.id;
    self.tabs.push(tab);
    if make_active || self.active_tab.is_none() {
      self.active_tab = Some(tab_id);
      self.sync_address_bar_to_active();
    }
  }

  /// Removes a tab, returning the new active tab if the active tab changed.
  pub fn remove_tab(&mut self, tab_id: TabId) -> Option<TabId> {
    let Some(idx) = self.tabs.iter().position(|t| t.id == tab_id) else {
      return None;
    };

    self.tabs.remove(idx);

    let was_active = self.active_tab == Some(tab_id);
    if !was_active {
      return None;
    }

    let new_active = if self.tabs.is_empty() {
      None
    } else {
      // Prefer the tab that shifted into the removed index, otherwise the new last tab.
      Some(self.tabs.get(idx).or_else(|| self.tabs.last()).unwrap().id)
    };
    self.active_tab = new_active;
    self.sync_address_bar_to_active();
    new_active
  }

  pub fn sync_address_bar_to_active(&mut self) {
    let Some(active) = self.active_tab() else {
      self.chrome.address_bar_text.clear();
      return;
    };
    self.chrome.address_bar_text = active
      .current_url()
      .map(str::to_string)
      .unwrap_or_default();
  }
}

