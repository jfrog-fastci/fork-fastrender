use crate::ui::messages::TabId;
use crate::ui::ChromeAction;

/// Parsed representation of a `chrome-action:` URL.
///
/// This enum is intentionally "URL-shaped" rather than UI-frontend-shaped: it represents actions
/// encoded into internal chrome HTML documents (e.g. link/button `href` attributes), and can be
/// translated into the higher-level [`ChromeAction`] pipeline used by the rest of the browser UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChromeActionUrl {
  Back,
  Forward,
  Reload,
  StopLoading,
  Home,
  NewTab,
  ReopenClosedTab,
  OpenTabSearch,
  CloseTabSearch,
  ToggleBookmarksBar,
  ToggleHistoryPanel,
  ToggleBookmarksManager,
  OpenClearBrowsingDataDialog,
  ToggleDownloadsPanel,
  ToggleBookmarkForActiveTab,
  FocusAddressBar,
  NewWindow,
  ToggleFullScreen,
  OpenFindInPage,
  SavePage,
  PrintPage,
  SetShowMenuBar { show: bool },
  AddressBarFocusChanged { has_focus: bool },
  Navigate { url: String },
  OpenUrlInNewTab { url: String },
  CloseTab { tab_id: TabId },
  DetachTab { tab_id: TabId },
  ReloadTab { tab_id: TabId },
  DuplicateTab { tab_id: TabId },
  CloseOtherTabs { tab_id: TabId },
  CloseTabsToRight { tab_id: TabId },
  ActivateTab { tab_id: TabId },
  TogglePinTab { tab_id: TabId },
}

fn validate_tab_id(tab_id: TabId) -> Result<TabId, String> {
  if tab_id.0 == 0 {
    return Err("invalid tab id: 0".to_string());
  }
  Ok(tab_id)
}

fn trim_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
}

impl ChromeActionUrl {
  pub fn into_chrome_action(self) -> Result<ChromeAction, String> {
    Ok(match self {
      Self::Back => ChromeAction::Back,
      Self::Forward => ChromeAction::Forward,
      Self::Reload => ChromeAction::Reload,
      Self::StopLoading => ChromeAction::StopLoading,
      Self::Home => ChromeAction::Home,
      Self::NewTab => ChromeAction::NewTab,
      Self::ReopenClosedTab => ChromeAction::ReopenClosedTab,
      Self::OpenTabSearch => ChromeAction::OpenTabSearch,
      Self::CloseTabSearch => ChromeAction::CloseTabSearch,
      Self::ToggleBookmarksBar => ChromeAction::ToggleBookmarksBar,
      Self::ToggleHistoryPanel => ChromeAction::ToggleHistoryPanel,
      Self::ToggleBookmarksManager => ChromeAction::ToggleBookmarksManager,
      Self::OpenClearBrowsingDataDialog => ChromeAction::OpenClearBrowsingDataDialog,
      Self::ToggleDownloadsPanel => ChromeAction::ToggleDownloadsPanel,
      Self::ToggleBookmarkForActiveTab => ChromeAction::ToggleBookmarkForActiveTab,
      Self::FocusAddressBar => ChromeAction::FocusAddressBar,
      Self::NewWindow => ChromeAction::NewWindow,
      Self::ToggleFullScreen => ChromeAction::ToggleFullScreen,
      Self::OpenFindInPage => ChromeAction::OpenFindInPage,
      Self::SavePage => ChromeAction::SavePage,
      Self::PrintPage => ChromeAction::PrintPage,
      Self::SetShowMenuBar { show } => ChromeAction::SetShowMenuBar(show),
      Self::AddressBarFocusChanged { has_focus } => ChromeAction::AddressBarFocusChanged(has_focus),
      Self::Navigate { url } => {
        let url = trim_ascii_whitespace(&url);
        if url.is_empty() {
          return Err("Navigate action requires a non-empty url".to_string());
        }
        ChromeAction::NavigateTo(url.to_string())
      }
      Self::OpenUrlInNewTab { url } => {
        let url = trim_ascii_whitespace(&url);
        if url.is_empty() {
          return Err("OpenUrlInNewTab action requires a non-empty url".to_string());
        }
        ChromeAction::OpenUrlInNewTab(url.to_string())
      }
      Self::CloseTab { tab_id } => ChromeAction::CloseTab(validate_tab_id(tab_id)?),
      Self::DetachTab { tab_id } => ChromeAction::DetachTab(validate_tab_id(tab_id)?),
      Self::ReloadTab { tab_id } => ChromeAction::ReloadTab(validate_tab_id(tab_id)?),
      Self::DuplicateTab { tab_id } => ChromeAction::DuplicateTab(validate_tab_id(tab_id)?),
      Self::CloseOtherTabs { tab_id } => ChromeAction::CloseOtherTabs(validate_tab_id(tab_id)?),
      Self::CloseTabsToRight { tab_id } => ChromeAction::CloseTabsToRight(validate_tab_id(tab_id)?),
      Self::ActivateTab { tab_id } => ChromeAction::ActivateTab(validate_tab_id(tab_id)?),
      Self::TogglePinTab { tab_id } => ChromeAction::TogglePinTab(validate_tab_id(tab_id)?),
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn maps_simple_actions() {
    let cases = vec![
      (ChromeActionUrl::Back, ChromeAction::Back),
      (ChromeActionUrl::Forward, ChromeAction::Forward),
      (ChromeActionUrl::Reload, ChromeAction::Reload),
      (ChromeActionUrl::StopLoading, ChromeAction::StopLoading),
      (ChromeActionUrl::Home, ChromeAction::Home),
      (ChromeActionUrl::NewTab, ChromeAction::NewTab),
      (
        ChromeActionUrl::SetShowMenuBar { show: true },
        ChromeAction::SetShowMenuBar(true),
      ),
      (
        ChromeActionUrl::AddressBarFocusChanged { has_focus: false },
        ChromeAction::AddressBarFocusChanged(false),
      ),
      (
        ChromeActionUrl::CloseTab { tab_id: TabId(1) },
        ChromeAction::CloseTab(TabId(1)),
      ),
      (
        ChromeActionUrl::ActivateTab { tab_id: TabId(2) },
        ChromeAction::ActivateTab(TabId(2)),
      ),
      (
        ChromeActionUrl::TogglePinTab { tab_id: TabId(3) },
        ChromeAction::TogglePinTab(TabId(3)),
      ),
    ];

    for (input, expected) in cases {
      assert_eq!(input.into_chrome_action().unwrap(), expected);
    }
  }

  #[test]
  fn maps_url_payload_actions() {
    assert_eq!(
      ChromeActionUrl::Navigate {
        url: "https://example.com/".to_string()
      }
      .into_chrome_action()
      .unwrap(),
      ChromeAction::NavigateTo("https://example.com/".to_string())
    );

    assert_eq!(
      ChromeActionUrl::OpenUrlInNewTab {
        url: "about:blank".to_string()
      }
      .into_chrome_action()
      .unwrap(),
      ChromeAction::OpenUrlInNewTab("about:blank".to_string())
    );
  }

  #[test]
  fn errors_on_empty_url() {
    assert!(
      ChromeActionUrl::Navigate { url: "   ".to_string() }
        .into_chrome_action()
        .is_err()
    );
    assert!(
      ChromeActionUrl::OpenUrlInNewTab { url: "".to_string() }
        .into_chrome_action()
        .is_err()
    );
  }

  #[test]
  fn errors_on_invalid_tab_id() {
    let err = ChromeActionUrl::CloseTab { tab_id: TabId(0) }
      .into_chrome_action()
      .unwrap_err();
    assert!(err.contains("invalid tab id"), "unexpected error: {err}");
  }
}

