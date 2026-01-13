use crate::ui::messages::TabId;
use crate::ui::ChromeAction;
use url::Url;

pub const CHROME_ACTION_SCHEME: &str = "chrome-action";

/// Parsed representation of a `chrome-action:` URL.
///
/// This enum is intentionally "URL-shaped" rather than UI-frontend-shaped: it represents actions
/// encoded into internal chrome HTML documents (e.g. link/button `href` attributes or form
/// submissions), and can be translated into the higher-level [`ChromeAction`] pipeline used by the
/// rest of the browser UI.
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
  /// Raw omnibox/address-bar input string (URL or search query).
  Navigate { url: String },
  /// Raw omnibox/address-bar input string opened in a new foreground tab.
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
  /// Parse a `chrome-action:` URL string.
  pub fn parse(raw: &str) -> Result<Self, String> {
    let url = Url::parse(raw).map_err(|err| err.to_string())?;
    Self::parse_url(&url)
  }

  /// Parse a `chrome-action:` action from an already-parsed URL.
  pub fn parse_url(url: &Url) -> Result<Self, String> {
    if !url.scheme().eq_ignore_ascii_case(CHROME_ACTION_SCHEME) {
      return Err(format!(
        "expected {CHROME_ACTION_SCHEME}: URL, got scheme={:?}",
        url.scheme()
      ));
    }

    let action = chrome_action_name(url)
      .map(str::to_ascii_lowercase)
      .ok_or_else(|| "missing chrome-action name".to_string())?;

    let mut tab_id: Option<TabId> = None;
    let mut url_value: Option<String> = None;
    let mut show: Option<bool> = None;
    let mut has_focus: Option<bool> = None;

    for (k, v) in url.query_pairs() {
      match k.as_ref() {
        "tab" | "tab_id" => tab_id = Some(parse_tab_id(&v)?),
        // For compatibility with older docs/examples, accept both `url` and `input`.
        "url" | "input" => url_value = Some(v.to_string()),
        "show" => show = Some(parse_bool(&v)?),
        "has_focus" | "focus" => has_focus = Some(parse_bool(&v)?),
        _ => {}
      }
    }

    let require_tab = || tab_id.ok_or_else(|| "missing required query param `tab`".to_string());
    let require_url = || url_value.clone().ok_or_else(|| "missing required query param `url`".to_string());

    match action.as_str() {
      "back" => Ok(Self::Back),
      "forward" => Ok(Self::Forward),
      "reload" => Ok(Self::Reload),
      "stop-loading" => Ok(Self::StopLoading),
      "home" => Ok(Self::Home),
      "new-tab" => Ok(Self::NewTab),
      "reopen-closed-tab" => Ok(Self::ReopenClosedTab),
      "open-tab-search" => Ok(Self::OpenTabSearch),
      "close-tab-search" => Ok(Self::CloseTabSearch),
      "toggle-bookmarks-bar" => Ok(Self::ToggleBookmarksBar),
      "toggle-history-panel" => Ok(Self::ToggleHistoryPanel),
      "toggle-bookmarks-manager" => Ok(Self::ToggleBookmarksManager),
      "open-clear-browsing-data-dialog" => Ok(Self::OpenClearBrowsingDataDialog),
      "toggle-downloads-panel" => Ok(Self::ToggleDownloadsPanel),
      "toggle-bookmark-for-active-tab" => Ok(Self::ToggleBookmarkForActiveTab),
      "focus-address-bar" => Ok(Self::FocusAddressBar),
      "new-window" => Ok(Self::NewWindow),
      "toggle-full-screen" => Ok(Self::ToggleFullScreen),
      "open-find-in-page" => Ok(Self::OpenFindInPage),
      "save-page" => Ok(Self::SavePage),
      "print-page" => Ok(Self::PrintPage),
      "set-show-menu-bar" => Ok(Self::SetShowMenuBar {
        show: show.ok_or_else(|| "missing required query param `show`".to_string())?,
      }),
      "address-bar-focus-changed" => Ok(Self::AddressBarFocusChanged {
        has_focus: has_focus.ok_or_else(|| "missing required query param `has_focus`".to_string())?,
      }),
      "navigate" => Ok(Self::Navigate { url: require_url()? }),
      "open-url-in-new-tab" => Ok(Self::OpenUrlInNewTab { url: require_url()? }),
      "close-tab" => Ok(Self::CloseTab { tab_id: require_tab()? }),
      "detach-tab" => Ok(Self::DetachTab { tab_id: require_tab()? }),
      "reload-tab" => Ok(Self::ReloadTab { tab_id: require_tab()? }),
      "duplicate-tab" => Ok(Self::DuplicateTab { tab_id: require_tab()? }),
      "close-other-tabs" => Ok(Self::CloseOtherTabs { tab_id: require_tab()? }),
      "close-tabs-to-right" => Ok(Self::CloseTabsToRight { tab_id: require_tab()? }),
      "activate-tab" => Ok(Self::ActivateTab { tab_id: require_tab()? }),
      "toggle-pin-tab" => Ok(Self::TogglePinTab { tab_id: require_tab()? }),
      _ => Err(format!("unknown chrome-action name: {action:?}")),
    }
  }

  /// Convert into the browser UI's higher-level action enum.
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

  /// Format as a canonical `chrome-action:` URL string.
  pub fn to_url_string(&self) -> String {
    match self {
      Self::Back => format!("{CHROME_ACTION_SCHEME}:back"),
      Self::Forward => format!("{CHROME_ACTION_SCHEME}:forward"),
      Self::Reload => format!("{CHROME_ACTION_SCHEME}:reload"),
      Self::StopLoading => format!("{CHROME_ACTION_SCHEME}:stop-loading"),
      Self::Home => format!("{CHROME_ACTION_SCHEME}:home"),
      Self::NewTab => format!("{CHROME_ACTION_SCHEME}:new-tab"),
      Self::ReopenClosedTab => format!("{CHROME_ACTION_SCHEME}:reopen-closed-tab"),
      Self::OpenTabSearch => format!("{CHROME_ACTION_SCHEME}:open-tab-search"),
      Self::CloseTabSearch => format!("{CHROME_ACTION_SCHEME}:close-tab-search"),
      Self::ToggleBookmarksBar => format!("{CHROME_ACTION_SCHEME}:toggle-bookmarks-bar"),
      Self::ToggleHistoryPanel => format!("{CHROME_ACTION_SCHEME}:toggle-history-panel"),
      Self::ToggleBookmarksManager => format!("{CHROME_ACTION_SCHEME}:toggle-bookmarks-manager"),
      Self::OpenClearBrowsingDataDialog => {
        format!("{CHROME_ACTION_SCHEME}:open-clear-browsing-data-dialog")
      }
      Self::ToggleDownloadsPanel => format!("{CHROME_ACTION_SCHEME}:toggle-downloads-panel"),
      Self::ToggleBookmarkForActiveTab => {
        format!("{CHROME_ACTION_SCHEME}:toggle-bookmark-for-active-tab")
      }
      Self::FocusAddressBar => format!("{CHROME_ACTION_SCHEME}:focus-address-bar"),
      Self::NewWindow => format!("{CHROME_ACTION_SCHEME}:new-window"),
      Self::ToggleFullScreen => format!("{CHROME_ACTION_SCHEME}:toggle-full-screen"),
      Self::OpenFindInPage => format!("{CHROME_ACTION_SCHEME}:open-find-in-page"),
      Self::SavePage => format!("{CHROME_ACTION_SCHEME}:save-page"),
      Self::PrintPage => format!("{CHROME_ACTION_SCHEME}:print-page"),
      Self::SetShowMenuBar { show } => format!(
        "{CHROME_ACTION_SCHEME}:set-show-menu-bar?{}",
        url::form_urlencoded::Serializer::new(String::new())
          .append_pair("show", if *show { "1" } else { "0" })
          .finish()
      ),
      Self::AddressBarFocusChanged { has_focus } => format!(
        "{CHROME_ACTION_SCHEME}:address-bar-focus-changed?{}",
        url::form_urlencoded::Serializer::new(String::new())
          .append_pair("has_focus", if *has_focus { "1" } else { "0" })
          .finish()
      ),
      Self::Navigate { url } => format!(
        "{CHROME_ACTION_SCHEME}:navigate?{}",
        url::form_urlencoded::Serializer::new(String::new())
          .append_pair("url", url)
          .finish()
      ),
      Self::OpenUrlInNewTab { url } => format!(
        "{CHROME_ACTION_SCHEME}:open-url-in-new-tab?{}",
        url::form_urlencoded::Serializer::new(String::new())
          .append_pair("url", url)
          .finish()
      ),
      Self::CloseTab { tab_id } => format!("{CHROME_ACTION_SCHEME}:close-tab?tab={}", tab_id.0),
      Self::DetachTab { tab_id } => {
        format!("{CHROME_ACTION_SCHEME}:detach-tab?tab={}", tab_id.0)
      }
      Self::ReloadTab { tab_id } => {
        format!("{CHROME_ACTION_SCHEME}:reload-tab?tab={}", tab_id.0)
      }
      Self::DuplicateTab { tab_id } => {
        format!("{CHROME_ACTION_SCHEME}:duplicate-tab?tab={}", tab_id.0)
      }
      Self::CloseOtherTabs { tab_id } => {
        format!("{CHROME_ACTION_SCHEME}:close-other-tabs?tab={}", tab_id.0)
      }
      Self::CloseTabsToRight { tab_id } => {
        format!("{CHROME_ACTION_SCHEME}:close-tabs-to-right?tab={}", tab_id.0)
      }
      Self::ActivateTab { tab_id } => {
        format!("{CHROME_ACTION_SCHEME}:activate-tab?tab={}", tab_id.0)
      }
      Self::TogglePinTab { tab_id } => {
        format!("{CHROME_ACTION_SCHEME}:toggle-pin-tab?tab={}", tab_id.0)
      }
    }
  }
}

fn chrome_action_name(url: &Url) -> Option<&str> {
  // Canonical form: `chrome-action:<action>` (opaque/cannot-be-a-base URLs).
  let path = url.path().trim_start_matches('/');
  if !path.is_empty() {
    return Some(path);
  }
  // Be permissive and accept `chrome-action://<action>` if a caller produced it.
  url.host_str()
}

fn parse_tab_id(raw: &str) -> Result<TabId, String> {
  let id: u64 = raw
    .trim()
    .parse()
    .map_err(|_| format!("invalid tab id: {raw:?}"))?;
  if id == 0 {
    return Err("tab id must be non-zero".to_string());
  }
  Ok(TabId(id))
}

fn parse_bool(raw: &str) -> Result<bool, String> {
  let v = raw.trim().to_ascii_lowercase();
  match v.as_str() {
    "1" | "true" | "yes" | "on" => Ok(true),
    "0" | "false" | "no" | "off" => Ok(false),
    _ => Err(format!("invalid boolean value: {raw:?}")),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn maps_all_variants() {
    let tab_id = TabId(123);

    let cases: Vec<(ChromeActionUrl, ChromeAction)> = vec![
      (ChromeActionUrl::Back, ChromeAction::Back),
      (ChromeActionUrl::Forward, ChromeAction::Forward),
      (ChromeActionUrl::Reload, ChromeAction::Reload),
      (ChromeActionUrl::StopLoading, ChromeAction::StopLoading),
      (ChromeActionUrl::Home, ChromeAction::Home),
      (ChromeActionUrl::NewTab, ChromeAction::NewTab),
      (ChromeActionUrl::ReopenClosedTab, ChromeAction::ReopenClosedTab),
      (ChromeActionUrl::OpenTabSearch, ChromeAction::OpenTabSearch),
      (ChromeActionUrl::CloseTabSearch, ChromeAction::CloseTabSearch),
      (ChromeActionUrl::ToggleBookmarksBar, ChromeAction::ToggleBookmarksBar),
      (ChromeActionUrl::ToggleHistoryPanel, ChromeAction::ToggleHistoryPanel),
      (
        ChromeActionUrl::ToggleBookmarksManager,
        ChromeAction::ToggleBookmarksManager,
      ),
      (
        ChromeActionUrl::OpenClearBrowsingDataDialog,
        ChromeAction::OpenClearBrowsingDataDialog,
      ),
      (
        ChromeActionUrl::ToggleDownloadsPanel,
        ChromeAction::ToggleDownloadsPanel,
      ),
      (
        ChromeActionUrl::ToggleBookmarkForActiveTab,
        ChromeAction::ToggleBookmarkForActiveTab,
      ),
      (ChromeActionUrl::FocusAddressBar, ChromeAction::FocusAddressBar),
      (ChromeActionUrl::NewWindow, ChromeAction::NewWindow),
      (ChromeActionUrl::ToggleFullScreen, ChromeAction::ToggleFullScreen),
      (ChromeActionUrl::OpenFindInPage, ChromeAction::OpenFindInPage),
      (ChromeActionUrl::SavePage, ChromeAction::SavePage),
      (ChromeActionUrl::PrintPage, ChromeAction::PrintPage),
      (
        ChromeActionUrl::SetShowMenuBar { show: true },
        ChromeAction::SetShowMenuBar(true),
      ),
      (
        ChromeActionUrl::AddressBarFocusChanged { has_focus: false },
        ChromeAction::AddressBarFocusChanged(false),
      ),
      (
        ChromeActionUrl::Navigate {
          url: "https://example.com/".to_string(),
        },
        ChromeAction::NavigateTo("https://example.com/".to_string()),
      ),
      (
        ChromeActionUrl::OpenUrlInNewTab {
          url: "about:blank".to_string(),
        },
        ChromeAction::OpenUrlInNewTab("about:blank".to_string()),
      ),
      (
        ChromeActionUrl::CloseTab { tab_id },
        ChromeAction::CloseTab(tab_id),
      ),
      (
        ChromeActionUrl::DetachTab { tab_id },
        ChromeAction::DetachTab(tab_id),
      ),
      (
        ChromeActionUrl::ReloadTab { tab_id },
        ChromeAction::ReloadTab(tab_id),
      ),
      (
        ChromeActionUrl::DuplicateTab { tab_id },
        ChromeAction::DuplicateTab(tab_id),
      ),
      (
        ChromeActionUrl::CloseOtherTabs { tab_id },
        ChromeAction::CloseOtherTabs(tab_id),
      ),
      (
        ChromeActionUrl::CloseTabsToRight { tab_id },
        ChromeAction::CloseTabsToRight(tab_id),
      ),
      (
        ChromeActionUrl::ActivateTab { tab_id },
        ChromeAction::ActivateTab(tab_id),
      ),
      (
        ChromeActionUrl::TogglePinTab { tab_id },
        ChromeAction::TogglePinTab(tab_id),
      ),
    ];

    for (input, expected) in cases {
      assert_eq!(input.into_chrome_action().unwrap(), expected);
    }
  }

  #[test]
  fn url_payload_trims_ascii_whitespace() {
    assert_eq!(
      ChromeActionUrl::Navigate {
        url: "  https://example.com/ \n".to_string(),
      }
      .into_chrome_action()
      .unwrap(),
      ChromeAction::NavigateTo("https://example.com/".to_string())
    );

    assert_eq!(
      ChromeActionUrl::OpenUrlInNewTab {
        url: "\tabout:blank\r".to_string(),
      }
      .into_chrome_action()
      .unwrap(),
      ChromeAction::OpenUrlInNewTab("about:blank".to_string())
    );
  }

  #[test]
  fn errors_on_empty_url() {
    assert!(
      ChromeActionUrl::Navigate {
        url: "   ".to_string(),
      }
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
    let cases = vec![
      ChromeActionUrl::CloseTab { tab_id: TabId(0) },
      ChromeActionUrl::DetachTab { tab_id: TabId(0) },
      ChromeActionUrl::ReloadTab { tab_id: TabId(0) },
      ChromeActionUrl::DuplicateTab { tab_id: TabId(0) },
      ChromeActionUrl::CloseOtherTabs { tab_id: TabId(0) },
      ChromeActionUrl::CloseTabsToRight { tab_id: TabId(0) },
      ChromeActionUrl::ActivateTab { tab_id: TabId(0) },
      ChromeActionUrl::TogglePinTab { tab_id: TabId(0) },
    ];

    for case in cases {
      let err = case.into_chrome_action().unwrap_err();
      assert!(err.contains("invalid tab id"), "unexpected error: {err}");
    }
  }

  #[test]
  fn chrome_action_url_round_trip_examples() {
    let cases = vec![
      ChromeActionUrl::Back,
      ChromeActionUrl::Forward,
      ChromeActionUrl::Reload,
      ChromeActionUrl::StopLoading,
      ChromeActionUrl::Home,
      ChromeActionUrl::NewTab,
      ChromeActionUrl::ActivateTab { tab_id: TabId(7) },
      ChromeActionUrl::CloseTab { tab_id: TabId(42) },
      ChromeActionUrl::DetachTab { tab_id: TabId(123) },
      ChromeActionUrl::Navigate {
        url: "https://example.com/?q=cats & dogs".to_string(),
      },
      ChromeActionUrl::OpenUrlInNewTab {
        url: "cats".to_string(),
      },
      ChromeActionUrl::ToggleHistoryPanel,
      ChromeActionUrl::ToggleDownloadsPanel,
    ];

    for action in cases {
      let url = action.to_url_string();
      let parsed = ChromeActionUrl::parse(&url).unwrap_or_else(|err| {
        panic!("failed to parse {url:?}: {err}");
      });
      assert_eq!(parsed, action, "round-trip mismatch for {url:?}");
    }
  }

  #[test]
  fn chrome_action_url_encoding_is_deterministic() {
    let action = ChromeActionUrl::Navigate {
      url: "cats & dogs".to_string(),
    };
    // `application/x-www-form-urlencoded` encoding:
    // - spaces become `+`
    // - `&` becomes `%26` so it can't terminate the query pair.
    assert_eq!(
      action.to_url_string(),
      format!("{CHROME_ACTION_SCHEME}:navigate?url=cats+%26+dogs")
    );

    // Ensure the parser accepts the legacy `input=` parameter name too.
    let parsed = ChromeActionUrl::parse(&format!(
      "{CHROME_ACTION_SCHEME}:navigate?input=cats+%26+dogs"
    ))
    .expect("parse input= alias");
    assert_eq!(parsed, action);
  }
}
