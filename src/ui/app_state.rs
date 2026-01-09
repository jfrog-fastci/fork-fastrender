use crate::scroll::ScrollState;

use super::cancel::CancelGens;
use super::history::TabHistory;
use super::messages::{NavigationReason, TabId, UiToWorker, WorkerToUi};
use super::url::normalize_user_url;

#[derive(Debug)]
pub struct BrowserAppState {
  pub tabs: Vec<TabState>,
  pub active: TabId,
  /// Address-bar text for the active tab.
  ///
  /// This is intentionally modeled as a single string (not per-tab) so the UI layer can treat the
  /// chrome as "currently focused on the active tab".
  pub address_bar_text: String,
}

#[derive(Debug)]
pub struct TabState {
  pub id: TabId,
  pub cancel: CancelGens,
  pub history: TabHistory,
  pub loading: bool,
  pub viewport_css: (u32, u32),
  pub dpr: f32,
  pub last_scroll: ScrollState,

  /// Cached title for the tab UI (mirrors the current history entry's title).
  pub title: Option<String>,
  /// Cached navigation affordances for chrome buttons.
  pub can_go_back: bool,
  pub can_go_forward: bool,

  pending_navigation_original_url: Option<String>,
}

impl TabState {
  fn new(id: TabId, initial_url: Option<String>) -> Self {
    let history = match initial_url {
      Some(url) => TabHistory::with_initial(url),
      None => TabHistory::new(),
    };
    let mut tab = Self {
      id,
      cancel: CancelGens::new(),
      history,
      loading: false,
      viewport_css: (0, 0),
      dpr: 1.0,
      last_scroll: ScrollState::default(),
      title: None,
      can_go_back: false,
      can_go_forward: false,
      pending_navigation_original_url: None,
    };
    tab.refresh_nav_flags_from_history();
    tab.refresh_title_from_history();
    tab
  }

  fn refresh_nav_flags_from_history(&mut self) {
    self.can_go_back = self.history.can_go_back();
    self.can_go_forward = self.history.can_go_forward();
  }

  fn refresh_title_from_history(&mut self) {
    self.title = self.history.current().and_then(|entry| entry.title.clone());
  }

  fn current_url(&self) -> Option<&str> {
    self.history.current().map(|entry| entry.url.as_str())
  }
}

impl BrowserAppState {
  pub fn new() -> Self {
    let tab_id = TabId::new();
    Self {
      tabs: vec![TabState::new(tab_id, None)],
      active: tab_id,
      address_bar_text: "about:newtab".to_owned(),
    }
  }

  fn tab_index(&self, tab_id: TabId) -> Option<usize> {
    self.tabs.iter().position(|tab| tab.id == tab_id)
  }

  fn tab(&self, tab_id: TabId) -> Option<&TabState> {
    let idx = self.tab_index(tab_id)?;
    self.tabs.get(idx)
  }

  fn address_text_for_tab(tab: &TabState) -> String {
    tab
      .current_url()
      .map(str::to_owned)
      .unwrap_or_else(|| "about:newtab".to_owned())
  }

  pub fn create_tab(&mut self, initial_url: Option<String>) -> UiToWorker {
    let tab_id = TabId::new();
    let tab = TabState::new(tab_id, initial_url.clone());
    let cancel = tab.cancel.clone();
    self.tabs.push(tab);

    UiToWorker::CreateTab {
      tab_id,
      initial_url,
      cancel,
    }
  }

  pub fn close_tab(&mut self, tab_id: TabId) -> Option<UiToWorker> {
    let idx = self.tab_index(tab_id)?;

    // Keep the model in a valid state by refusing to close the last tab. The UI can still exit
    // the window through the OS close button.
    if self.tabs.len() == 1 {
      return None;
    }

    let was_active = self.active == tab_id;
    self.tabs.remove(idx);

    if was_active {
      let fallback_idx = idx.min(self.tabs.len() - 1);
      self.active = self.tabs[fallback_idx].id;
      self.address_bar_text = Self::address_text_for_tab(&self.tabs[fallback_idx]);
    }

    Some(UiToWorker::CloseTab { tab_id })
  }

  pub fn set_active_tab(&mut self, tab_id: TabId) -> Option<UiToWorker> {
    let idx = self.tab_index(tab_id)?;
    let address_text = Self::address_text_for_tab(&self.tabs[idx]);
    self.active = tab_id;
    self.address_bar_text = address_text;
    Some(UiToWorker::SetActiveTab { tab_id })
  }

  pub fn navigate_typed(&mut self, tab_id: TabId, user_input: &str) -> Result<UiToWorker, String> {
    let url = normalize_user_url(user_input)?;
    let idx = self
      .tab_index(tab_id)
      .ok_or_else(|| format!("unknown tab: {tab_id:?}"))?;
    let is_active = self.active == tab_id;

    {
      let tab = &mut self.tabs[idx];
      tab.history.push(url.clone());
      tab.pending_navigation_original_url = Some(url.clone());
      tab.title = None;
      tab.refresh_nav_flags_from_history();
    }

    if is_active {
      self.address_bar_text = url.clone();
    }

    Ok(UiToWorker::Navigate {
      tab_id,
      url,
      reason: NavigationReason::TypedUrl,
    })
  }

  pub fn go_back(&mut self, tab_id: TabId) -> Option<UiToWorker> {
    let idx = self.tab_index(tab_id)?;
    let is_active = self.active == tab_id;

    let url = {
      let tab = &mut self.tabs[idx];
      let entry = tab.history.go_back()?;
      let url = entry.url.clone();

      tab.pending_navigation_original_url = Some(url.clone());
      tab.refresh_nav_flags_from_history();
      tab.refresh_title_from_history();
      url
    };

    if is_active {
      self.address_bar_text = url.clone();
    }

    Some(UiToWorker::Navigate {
      tab_id,
      url,
      reason: NavigationReason::BackForward,
    })
  }

  pub fn go_forward(&mut self, tab_id: TabId) -> Option<UiToWorker> {
    let idx = self.tab_index(tab_id)?;
    let is_active = self.active == tab_id;

    let url = {
      let tab = &mut self.tabs[idx];
      let entry = tab.history.go_forward()?;
      let url = entry.url.clone();

      tab.pending_navigation_original_url = Some(url.clone());
      tab.refresh_nav_flags_from_history();
      tab.refresh_title_from_history();
      url
    };

    if is_active {
      self.address_bar_text = url.clone();
    }

    Some(UiToWorker::Navigate {
      tab_id,
      url,
      reason: NavigationReason::BackForward,
    })
  }

  pub fn reload(&mut self, tab_id: TabId) -> Option<UiToWorker> {
    let idx = self.tab_index(tab_id)?;
    let url = {
      let tab = &mut self.tabs[idx];
      let entry = tab.history.reload_target()?;
      let url = entry.url.clone();
      tab.pending_navigation_original_url = Some(url.clone());
      url
    };

    Some(UiToWorker::Navigate {
      tab_id,
      url,
      reason: NavigationReason::Reload,
    })
  }

  pub fn on_worker_msg(&mut self, msg: WorkerToUi) {
    match msg {
      WorkerToUi::NavigationStarted { tab_id, url } => {
        let Some(idx) = self.tab_index(tab_id) else {
          return;
        };
        let is_active = self.active == tab_id;
        {
          let tab = &mut self.tabs[idx];
          tab.loading = true;
          tab.pending_navigation_original_url = Some(url.clone());
        }

        if is_active {
          self.address_bar_text = url;
        }
      }
      WorkerToUi::NavigationCommitted {
        tab_id,
        url,
        title,
        ..
      } => {
        let Some(idx) = self.tab_index(tab_id) else {
          return;
        };
        let is_active = self.active == tab_id;

        let address_text = {
          let tab = &mut self.tabs[idx];

          let original = tab
            .pending_navigation_original_url
            .clone()
            .or_else(|| tab.current_url().map(str::to_owned))
            .unwrap_or_else(|| url.clone());
          tab.history.commit_navigation(&original, Some(&url));

          if let Some(title) = title.clone() {
            tab.history.set_title(title);
          }

          tab.refresh_nav_flags_from_history();
          tab.refresh_title_from_history();

          Self::address_text_for_tab(tab)
        };

        if is_active {
          self.address_bar_text = address_text;
        }
      }
      WorkerToUi::NavigationFailed { tab_id, url, .. } => {
        let Some(idx) = self.tab_index(tab_id) else {
          return;
        };
        let is_active = self.active == tab_id;
        self.tabs[idx].loading = false;

        if is_active {
          self.address_bar_text = url;
        }
      }
      WorkerToUi::LoadingState { tab_id, loading } => {
        let Some(idx) = self.tab_index(tab_id) else {
          return;
        };
        self.tabs[idx].loading = loading;
      }
      WorkerToUi::ScrollStateUpdated { tab_id, scroll } => {
        let Some(idx) = self.tab_index(tab_id) else {
          return;
        };
        let tab = &mut self.tabs[idx];
        tab.history.update_scroll(scroll.viewport.x, scroll.viewport.y);
        tab.last_scroll = scroll;
      }
      _ => {}
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn typed_navigation_pushes_history_and_emits_normalized_url() {
    let mut app = BrowserAppState::new();
    let tab_id = app.active;
    let before_len = app.tab(tab_id).unwrap().history.len();

    let msg = app.navigate_typed(tab_id, "example.com").unwrap();

    assert!(matches!(
      msg,
      UiToWorker::Navigate {
        tab_id: got_tab,
        ref url,
        reason: NavigationReason::TypedUrl,
      } if got_tab == tab_id && url == "https://example.com/"
    ));

    let tab = app.tab(tab_id).unwrap();
    assert_eq!(tab.history.len(), before_len + 1);
    assert_eq!(tab.history.current().unwrap().url, "https://example.com/");
  }

  #[test]
  fn back_forward_do_not_create_new_history_entries() {
    let mut app = BrowserAppState::new();
    let tab_id = app.active;

    let url_a = match app.navigate_typed(tab_id, "a.com").unwrap() {
      UiToWorker::Navigate { url, .. } => url,
      _ => unreachable!(),
    };
    let url_b = match app.navigate_typed(tab_id, "b.com").unwrap() {
      UiToWorker::Navigate { url, .. } => url,
      _ => unreachable!(),
    };

    assert_eq!(app.tab(tab_id).unwrap().history.len(), 2);

    let back = app.go_back(tab_id).unwrap();
    assert!(matches!(
      back,
      UiToWorker::Navigate {
        tab_id: got_tab,
        ref url,
        reason: NavigationReason::BackForward,
      } if got_tab == tab_id && url == &url_a
    ));
    assert_eq!(app.tab(tab_id).unwrap().history.len(), 2);

    let forward = app.go_forward(tab_id).unwrap();
    assert!(matches!(
      forward,
      UiToWorker::Navigate {
        tab_id: got_tab,
        ref url,
        reason: NavigationReason::BackForward,
      } if got_tab == tab_id && url == &url_b
    ));
    assert_eq!(app.tab(tab_id).unwrap().history.len(), 2);
  }

  #[test]
  fn reload_emits_navigate_without_mutating_history() {
    let mut app = BrowserAppState::new();
    let tab_id = app.active;

    let expected_url = match app.navigate_typed(tab_id, "example.com").unwrap() {
      UiToWorker::Navigate { url, .. } => url,
      _ => unreachable!(),
    };

    let before_len = app.tab(tab_id).unwrap().history.len();
    let reload = app.reload(tab_id).unwrap();

    assert!(matches!(
      reload,
      UiToWorker::Navigate {
        tab_id: got_tab,
        ref url,
        reason: NavigationReason::Reload,
      } if got_tab == tab_id && url == &expected_url
    ));

    let tab = app.tab(tab_id).unwrap();
    assert_eq!(tab.history.len(), before_len);
    assert_eq!(tab.history.current().unwrap().url, expected_url);
  }

  #[test]
  fn active_tab_switch_updates_address_bar_text() {
    let mut app = BrowserAppState::new();
    let tab_a = app.active;
    let url_a = match app.navigate_typed(tab_a, "a.com").unwrap() {
      UiToWorker::Navigate { url, .. } => url,
      _ => unreachable!(),
    };

    let tab_b = match app.create_tab(Some("https://b.com/".to_string())) {
      UiToWorker::CreateTab { tab_id, .. } => tab_id,
      _ => unreachable!(),
    };

    app.set_active_tab(tab_b).unwrap();
    assert_eq!(app.address_bar_text, "https://b.com/");

    app.set_active_tab(tab_a).unwrap();
    assert_eq!(app.address_bar_text, url_a);
  }
}
