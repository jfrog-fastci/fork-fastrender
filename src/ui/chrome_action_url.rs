//! Parsing/formatting for the `chrome-action:` URL scheme.
//!
//! Renderer-chrome uses HTML/CSS rendered UI surfaces (tabs, toolbar, etc). Without JavaScript
//! support, the primary interaction primitive is navigation via `<a href>`.
//!
//! The `chrome-action:` scheme lets **trusted** chrome HTML request browser actions (new tab, back,
//! open downloads panel, …) while keeping the existing `ChromeAction` action pipeline.
//!
//! Security note: Some actions include a nested URL (e.g. `navigate`). We explicitly reject nested
//! `javascript:` targets so chrome HTML cannot smuggle script URLs into the navigation pipeline.
//!
//! For scheme-level constraints and trust-boundary context, see `docs/renderer_chrome_schemes.md`.

use crate::ui::bookmarks::BookmarkId;
use crate::ui::messages::TabId;
use crate::ui::ChromeAction;
use url::Url;

/// A parsed `chrome-action:` URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChromeActionUrl {
  FocusAddressBar,
  NewWindow,
  ToggleFullScreen,
  OpenFindInPage,
  SavePage,
  PrintPage,

  FindQuery {
    tab_id: TabId,
    query: String,
    case_sensitive: bool,
  },
  FindNext {
    tab_id: TabId,
  },
  FindPrev {
    tab_id: TabId,
  },
  CloseFindInPage {
    tab_id: TabId,
  },

  NewTab,
  CloseTab {
    tab_id: TabId,
  },
  DetachTab {
    tab_id: TabId,
  },
  ReloadTab {
    tab_id: TabId,
  },
  DuplicateTab {
    tab_id: TabId,
  },
  CloseOtherTabs {
    tab_id: TabId,
  },
  CloseTabsToRight {
    tab_id: TabId,
  },
  ReopenClosedTab,
  ActivateTab {
    tab_id: TabId,
  },
  TogglePinTab {
    tab_id: TabId,
  },

  Navigate {
    url: String,
  },
  OpenUrlInNewTab {
    url: String,
  },

  Back,
  Forward,
  Reload,
  StopLoading,
  Home,

  OpenTabSearch,
  CloseTabSearch,

  ToggleBookmarksBar,
  SetShowMenuBar {
    show: bool,
  },
  AddressBarFocusChanged {
    has_focus: bool,
  },

  ToggleBookmarkForActiveTab,
  ReorderBookmarksBar {
    ids: Vec<BookmarkId>,
  },
  ToggleHistoryPanel,
  ToggleBookmarksManager,
  OpenClearBrowsingDataDialog,
  OpenHomeUrlDialog,
  ToggleDownloadsPanel,
}

impl ChromeActionUrl {
  pub const SCHEME: &'static str = "chrome-action";

  /// Parse a `chrome-action:` URL string.
  ///
  /// Invariants:
  /// - Parsing is strict: unknown actions and malformed args fail closed.
  /// - Only the opaque form `chrome-action:<action>` is accepted (no `chrome-action://...`).
  pub fn parse(raw: &str) -> Result<Self, String> {
    let raw = trim_ascii_whitespace(raw);
    let url = Url::parse(raw).map_err(|err| format!("invalid chrome-action URL {raw:?}: {err}"))?;

    if !url.scheme().eq_ignore_ascii_case(Self::SCHEME) {
      return Err(format!(
        "invalid chrome-action URL scheme: expected {}, got {}",
        Self::SCHEME,
        url.scheme()
      ));
    }
    if !url.cannot_be_a_base() {
      return Err("chrome-action URLs must use the opaque form `chrome-action:<action>` (no `//`)".to_string());
    }

    let action = trim_ascii_whitespace(url.path()).trim_start_matches('/');
    if action.is_empty() {
      return Err("chrome-action URL missing action".to_string());
    }
    let action = action.to_ascii_lowercase();

    let params: Vec<(String, String)> = url
      .query_pairs()
      .map(|(k, v)| (k.into_owned(), v.into_owned()))
      .collect();

    match action.as_str() {
      // Window / chrome-wide actions.
      "focus-address-bar" => Ok(Self::FocusAddressBar),
      "new-window" => Ok(Self::NewWindow),
      "toggle-full-screen" => Ok(Self::ToggleFullScreen),
      "open-find-in-page" => Ok(Self::OpenFindInPage),
      "save-page" => Ok(Self::SavePage),
      "print-page" => Ok(Self::PrintPage),

      // Find in page (per-tab).
      "find-query" => Ok(Self::FindQuery {
        tab_id: parse_tab_id(required_param(&params, "tab_id")?)?,
        query: required_param(&params, "query")?.to_string(),
        case_sensitive: parse_bool_param(optional_param(&params, "case_sensitive"))?,
      }),
      "find-next" => Ok(Self::FindNext {
        tab_id: parse_tab_id(required_param(&params, "tab_id")?)?,
      }),
      "find-prev" => Ok(Self::FindPrev {
        tab_id: parse_tab_id(required_param(&params, "tab_id")?)?,
      }),
      "close-find-in-page" => Ok(Self::CloseFindInPage {
        tab_id: parse_tab_id(required_param(&params, "tab_id")?)?,
      }),

      // Tab management.
      "new-tab" => Ok(Self::NewTab),
      "close-tab" => Ok(Self::CloseTab {
        tab_id: parse_tab_id(required_param(&params, "tab_id")?)?,
      }),
      "detach-tab" => Ok(Self::DetachTab {
        tab_id: parse_tab_id(required_param(&params, "tab_id")?)?,
      }),
      "reload-tab" => Ok(Self::ReloadTab {
        tab_id: parse_tab_id(required_param(&params, "tab_id")?)?,
      }),
      "duplicate-tab" => Ok(Self::DuplicateTab {
        tab_id: parse_tab_id(required_param(&params, "tab_id")?)?,
      }),
      "close-other-tabs" => Ok(Self::CloseOtherTabs {
        tab_id: parse_tab_id(required_param(&params, "tab_id")?)?,
      }),
      "close-tabs-to-right" => Ok(Self::CloseTabsToRight {
        tab_id: parse_tab_id(required_param(&params, "tab_id")?)?,
      }),
      "reopen-closed-tab" => Ok(Self::ReopenClosedTab),
      "activate-tab" => Ok(Self::ActivateTab {
        tab_id: parse_tab_id(required_param(&params, "tab_id")?)?,
      }),
      "toggle-pin-tab" => Ok(Self::TogglePinTab {
        tab_id: parse_tab_id(required_param(&params, "tab_id")?)?,
      }),

      // Navigations.
      "navigate" => {
        let target = trim_ascii_whitespace(required_param(&params, "url")?);
        if target.is_empty() {
          return Err("navigate requires a non-empty url".to_string());
        }
        reject_javascript_nested_target(target)?;
        Ok(Self::Navigate {
          url: target.to_string(),
        })
      }
      "open-url-in-new-tab" => {
        let target = trim_ascii_whitespace(required_param(&params, "url")?);
        if target.is_empty() {
          return Err("open-url-in-new-tab requires a non-empty url".to_string());
        }
        reject_javascript_nested_target(target)?;
        Ok(Self::OpenUrlInNewTab {
          url: target.to_string(),
        })
      }

      // History controls.
      "back" => Ok(Self::Back),
      "forward" => Ok(Self::Forward),
      "reload" => Ok(Self::Reload),
      "stop-loading" => Ok(Self::StopLoading),
      "home" => Ok(Self::Home),

      // Tab search.
      "open-tab-search" => Ok(Self::OpenTabSearch),
      "close-tab-search" => Ok(Self::CloseTabSearch),

      // Panels / chrome UI.
      "toggle-bookmarks-bar" => Ok(Self::ToggleBookmarksBar),
      "set-show-menu-bar" => Ok(Self::SetShowMenuBar {
        show: parse_bool_param(Some(required_param(&params, "show")?))?,
      }),
      "address-bar-focus-changed" => {
        let raw_focus = optional_param(&params, "has_focus").or_else(|| optional_param(&params, "focused"));
        let raw_focus = raw_focus.ok_or_else(|| "missing chrome-action param: has_focus".to_string())?;
        Ok(Self::AddressBarFocusChanged {
          has_focus: parse_bool_param(Some(raw_focus))?,
        })
      }
      "toggle-bookmark" => Ok(Self::ToggleBookmarkForActiveTab),
      "reorder-bookmarks-bar" => {
        let ids = all_params(&params, "id")
          .into_iter()
          .map(|raw| parse_bookmark_id(&raw))
          .collect::<Result<Vec<_>, _>>()?;
        if ids.is_empty() {
          return Err("reorder-bookmarks-bar requires at least one id".to_string());
        }
        Ok(Self::ReorderBookmarksBar { ids })
      }
      "toggle-history-panel" => Ok(Self::ToggleHistoryPanel),
      "toggle-bookmarks-manager" => Ok(Self::ToggleBookmarksManager),
      "open-clear-browsing-data-dialog" => Ok(Self::OpenClearBrowsingDataDialog),
      "open-home-url-dialog" => Ok(Self::OpenHomeUrlDialog),
      "toggle-downloads-panel" => Ok(Self::ToggleDownloadsPanel),

      other => Err(format!("unknown chrome-action: {other}")),
    }
  }

  /// Format this action into a canonical `chrome-action:` URL string.
  pub fn format(&self) -> String {
    let mut out = String::from(Self::SCHEME);
    out.push(':');

    match self {
      Self::FocusAddressBar => out.push_str("focus-address-bar"),
      Self::NewWindow => out.push_str("new-window"),
      Self::ToggleFullScreen => out.push_str("toggle-full-screen"),
      Self::OpenFindInPage => out.push_str("open-find-in-page"),
      Self::SavePage => out.push_str("save-page"),
      Self::PrintPage => out.push_str("print-page"),

      Self::FindQuery {
        tab_id,
        query,
        case_sensitive,
      } => {
        out.push_str("find-query");
        append_query(&mut out, &[
          ("tab_id", tab_id.0.to_string()),
          ("query", query.clone()),
          ("case_sensitive", bool_to_string(*case_sensitive)),
        ]);
      }
      Self::FindNext { tab_id } => {
        out.push_str("find-next");
        append_query(&mut out, &[("tab_id", tab_id.0.to_string())]);
      }
      Self::FindPrev { tab_id } => {
        out.push_str("find-prev");
        append_query(&mut out, &[("tab_id", tab_id.0.to_string())]);
      }
      Self::CloseFindInPage { tab_id } => {
        out.push_str("close-find-in-page");
        append_query(&mut out, &[("tab_id", tab_id.0.to_string())]);
      }

      Self::NewTab => out.push_str("new-tab"),
      Self::CloseTab { tab_id } => {
        out.push_str("close-tab");
        append_query(&mut out, &[("tab_id", tab_id.0.to_string())]);
      }
      Self::DetachTab { tab_id } => {
        out.push_str("detach-tab");
        append_query(&mut out, &[("tab_id", tab_id.0.to_string())]);
      }
      Self::ReloadTab { tab_id } => {
        out.push_str("reload-tab");
        append_query(&mut out, &[("tab_id", tab_id.0.to_string())]);
      }
      Self::DuplicateTab { tab_id } => {
        out.push_str("duplicate-tab");
        append_query(&mut out, &[("tab_id", tab_id.0.to_string())]);
      }
      Self::CloseOtherTabs { tab_id } => {
        out.push_str("close-other-tabs");
        append_query(&mut out, &[("tab_id", tab_id.0.to_string())]);
      }
      Self::CloseTabsToRight { tab_id } => {
        out.push_str("close-tabs-to-right");
        append_query(&mut out, &[("tab_id", tab_id.0.to_string())]);
      }
      Self::ReopenClosedTab => out.push_str("reopen-closed-tab"),
      Self::ActivateTab { tab_id } => {
        out.push_str("activate-tab");
        append_query(&mut out, &[("tab_id", tab_id.0.to_string())]);
      }
      Self::TogglePinTab { tab_id } => {
        out.push_str("toggle-pin-tab");
        append_query(&mut out, &[("tab_id", tab_id.0.to_string())]);
      }

      Self::Navigate { url } => {
        out.push_str("navigate");
        append_query(&mut out, &[("url", url.clone())]);
      }
      Self::OpenUrlInNewTab { url } => {
        out.push_str("open-url-in-new-tab");
        append_query(&mut out, &[("url", url.clone())]);
      }

      Self::Back => out.push_str("back"),
      Self::Forward => out.push_str("forward"),
      Self::Reload => out.push_str("reload"),
      Self::StopLoading => out.push_str("stop-loading"),
      Self::Home => out.push_str("home"),

      Self::OpenTabSearch => out.push_str("open-tab-search"),
      Self::CloseTabSearch => out.push_str("close-tab-search"),

      Self::ToggleBookmarksBar => out.push_str("toggle-bookmarks-bar"),
      Self::SetShowMenuBar { show } => {
        out.push_str("set-show-menu-bar");
        append_query(&mut out, &[("show", bool_to_string(*show))]);
      }
      Self::AddressBarFocusChanged { has_focus } => {
        out.push_str("address-bar-focus-changed");
        append_query(&mut out, &[("has_focus", bool_to_string(*has_focus))]);
      }

      Self::ToggleBookmarkForActiveTab => out.push_str("toggle-bookmark"),
      Self::ReorderBookmarksBar { ids } => {
        out.push_str("reorder-bookmarks-bar");
        out.push('?');
        for (idx, id) in ids.iter().enumerate() {
          if idx > 0 {
            out.push('&');
          }
          out.push_str("id=");
          out.push_str(&id.0.to_string());
        }
      }
      Self::ToggleHistoryPanel => out.push_str("toggle-history-panel"),
      Self::ToggleBookmarksManager => out.push_str("toggle-bookmarks-manager"),
      Self::OpenClearBrowsingDataDialog => out.push_str("open-clear-browsing-data-dialog"),
      Self::OpenHomeUrlDialog => out.push_str("open-home-url-dialog"),
      Self::ToggleDownloadsPanel => out.push_str("toggle-downloads-panel"),
    }

    out
  }

  /// Convenience wrapper for UI HTML templates.
  ///
  /// This preserves the historical API name used by the chrome-frame HTML generator.
  pub fn to_url_string(&self) -> String {
    self.format()
  }

  /// Convert this URL action to the corresponding [`ChromeAction`].
  pub fn into_chrome_action(self) -> Result<ChromeAction, String> {
    Ok(match self {
      Self::FocusAddressBar => ChromeAction::FocusAddressBar,
      Self::NewWindow => ChromeAction::NewWindow,
      Self::ToggleFullScreen => ChromeAction::ToggleFullScreen,
      Self::OpenFindInPage => ChromeAction::OpenFindInPage,
      Self::SavePage => ChromeAction::SavePage,
      Self::PrintPage => ChromeAction::PrintPage,

      Self::FindQuery {
        tab_id,
        query,
        case_sensitive,
      } => ChromeAction::FindQuery {
        tab_id: validate_tab_id(tab_id)?,
        query,
        case_sensitive,
      },
      Self::FindNext { tab_id } => ChromeAction::FindNext(validate_tab_id(tab_id)?),
      Self::FindPrev { tab_id } => ChromeAction::FindPrev(validate_tab_id(tab_id)?),
      Self::CloseFindInPage { tab_id } => ChromeAction::CloseFindInPage(validate_tab_id(tab_id)?),

      Self::NewTab => ChromeAction::NewTab,
      Self::CloseTab { tab_id } => ChromeAction::CloseTab(validate_tab_id(tab_id)?),
      Self::DetachTab { tab_id } => ChromeAction::DetachTab(validate_tab_id(tab_id)?),
      Self::ReloadTab { tab_id } => ChromeAction::ReloadTab(validate_tab_id(tab_id)?),
      Self::DuplicateTab { tab_id } => ChromeAction::DuplicateTab(validate_tab_id(tab_id)?),
      Self::CloseOtherTabs { tab_id } => ChromeAction::CloseOtherTabs(validate_tab_id(tab_id)?),
      Self::CloseTabsToRight { tab_id } => ChromeAction::CloseTabsToRight(validate_tab_id(tab_id)?),
      Self::ReopenClosedTab => ChromeAction::ReopenClosedTab,
      Self::ActivateTab { tab_id } => ChromeAction::ActivateTab(validate_tab_id(tab_id)?),
      Self::TogglePinTab { tab_id } => ChromeAction::TogglePinTab(validate_tab_id(tab_id)?),

      Self::Navigate { url } => {
        let url = trim_ascii_whitespace(&url);
        if url.is_empty() {
          return Err("Navigate action requires a non-empty url".to_string());
        }
        reject_javascript_nested_target(url)?;
        ChromeAction::NavigateTo(url.to_string())
      }
      Self::OpenUrlInNewTab { url } => {
        let url = trim_ascii_whitespace(&url);
        if url.is_empty() {
          return Err("OpenUrlInNewTab action requires a non-empty url".to_string());
        }
        reject_javascript_nested_target(url)?;
        ChromeAction::OpenUrlInNewTab(url.to_string())
      }

      Self::Back => ChromeAction::Back,
      Self::Forward => ChromeAction::Forward,
      Self::Reload => ChromeAction::Reload,
      Self::StopLoading => ChromeAction::StopLoading,
      Self::Home => ChromeAction::Home,

      Self::OpenTabSearch => ChromeAction::OpenTabSearch,
      Self::CloseTabSearch => ChromeAction::CloseTabSearch,

      Self::ToggleBookmarksBar => ChromeAction::ToggleBookmarksBar,
      Self::SetShowMenuBar { show } => ChromeAction::SetShowMenuBar(show),
      Self::AddressBarFocusChanged { has_focus } => ChromeAction::AddressBarFocusChanged(has_focus),

      Self::ToggleBookmarkForActiveTab => ChromeAction::ToggleBookmarkForActiveTab,
      Self::ReorderBookmarksBar { ids } => {
        for id in &ids {
          validate_bookmark_id(*id)?;
        }
        ChromeAction::ReorderBookmarksBar(ids)
      }
      Self::ToggleHistoryPanel => ChromeAction::ToggleHistoryPanel,
      Self::ToggleBookmarksManager => ChromeAction::ToggleBookmarksManager,
      Self::OpenClearBrowsingDataDialog => ChromeAction::OpenClearBrowsingDataDialog,
      Self::OpenHomeUrlDialog => ChromeAction::OpenHomeUrlDialog,
      Self::ToggleDownloadsPanel => ChromeAction::ToggleDownloadsPanel,
    })
  }
}

impl std::fmt::Display for ChromeActionUrl {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(&self.format())
  }
}

fn validate_tab_id(tab_id: TabId) -> Result<TabId, String> {
  if tab_id.0 == 0 {
    return Err("invalid tab id: 0".to_string());
  }
  Ok(tab_id)
}

fn validate_bookmark_id(id: BookmarkId) -> Result<BookmarkId, String> {
  if id.0 == 0 {
    return Err("invalid bookmark id: 0".to_string());
  }
  Ok(id)
}

fn trim_ascii_whitespace(value: &str) -> &str {
  // Match HTML URL-ish attribute whitespace rules (TAB/LF/FF/CR/SPACE).
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
}

fn required_param<'a>(params: &'a [(String, String)], key: &str) -> Result<&'a str, String> {
  optional_param(params, key).ok_or_else(|| format!("missing chrome-action param: {key}"))
}

fn optional_param<'a>(params: &'a [(String, String)], key: &str) -> Option<&'a str> {
  params
    .iter()
    .find(|(k, _)| k == key)
    .map(|(_, v)| v.as_str())
}

fn all_params(params: &[(String, String)], key: &str) -> Vec<String> {
  params
    .iter()
    .filter(|(k, _)| k == key)
    .map(|(_, v)| v.clone())
    .collect()
}

fn parse_tab_id(raw: &str) -> Result<TabId, String> {
  let raw = trim_ascii_whitespace(raw);
  let parsed: u64 = raw
    .parse()
    .map_err(|_| format!("invalid tab_id value: {raw:?}"))?;
  if parsed == 0 {
    return Err("invalid tab_id value: 0".to_string());
  }
  Ok(TabId(parsed))
}

fn parse_bookmark_id(raw: &str) -> Result<BookmarkId, String> {
  let raw = trim_ascii_whitespace(raw);
  let parsed: u64 = raw
    .parse()
    .map_err(|_| format!("invalid bookmark id value: {raw:?}"))?;
  if parsed == 0 {
    return Err("invalid bookmark id value: 0".to_string());
  }
  Ok(BookmarkId(parsed))
}

fn parse_bool_param(raw: Option<&str>) -> Result<bool, String> {
  let Some(raw) = raw else {
    return Ok(false);
  };
  let raw = trim_ascii_whitespace(raw);
  if raw.is_empty() {
    return Ok(false);
  }
  match raw.to_ascii_lowercase().as_str() {
    "1" | "true" | "yes" | "on" => Ok(true),
    "0" | "false" | "no" | "off" => Ok(false),
    other => Err(format!("invalid bool value: {other:?}")),
  }
}

fn bool_to_string(value: bool) -> String {
  if value { "1" } else { "0" }.to_string()
}

fn append_query(out: &mut String, pairs: &[(&str, String)]) {
  if pairs.is_empty() {
    return;
  }
  out.push('?');
  for (idx, (k, v)) in pairs.iter().enumerate() {
    if idx > 0 {
      out.push('&');
    }
    out.push_str(k);
    out.push('=');
    out.push_str(&urlencoding::encode(v).to_string());
  }
}

fn reject_javascript_nested_target(target: &str) -> Result<(), String> {
  let trimmed = trim_ascii_whitespace(target);
  if trimmed
    .as_bytes()
    .get(.."javascript:".len())
    .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"javascript:"))
  {
    return Err("nested javascript: targets are not allowed".to_string());
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn maps_all_variants() {
    let tab_id = TabId(123);

    let cases: Vec<(ChromeActionUrl, ChromeAction)> = vec![
      // History/nav.
      (ChromeActionUrl::Back, ChromeAction::Back),
      (ChromeActionUrl::Forward, ChromeAction::Forward),
      (ChromeActionUrl::Reload, ChromeAction::Reload),
      (ChromeActionUrl::StopLoading, ChromeAction::StopLoading),
      (ChromeActionUrl::Home, ChromeAction::Home),
      // Tab management.
      (ChromeActionUrl::NewTab, ChromeAction::NewTab),
      (ChromeActionUrl::ReopenClosedTab, ChromeAction::ReopenClosedTab),
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
      // Window/chrome.
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
      // Find in page.
      (
        ChromeActionUrl::FindQuery {
          tab_id,
          query: "hello".to_string(),
          case_sensitive: true,
        },
        ChromeAction::FindQuery {
          tab_id,
          query: "hello".to_string(),
          case_sensitive: true,
        },
      ),
      (ChromeActionUrl::FindNext { tab_id }, ChromeAction::FindNext(tab_id)),
      (ChromeActionUrl::FindPrev { tab_id }, ChromeAction::FindPrev(tab_id)),
      (
        ChromeActionUrl::CloseFindInPage { tab_id },
        ChromeAction::CloseFindInPage(tab_id),
      ),
      // Navigation targets.
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
      // Panels / chrome UI.
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
      (ChromeActionUrl::OpenHomeUrlDialog, ChromeAction::OpenHomeUrlDialog),
      (
        ChromeActionUrl::ToggleDownloadsPanel,
        ChromeAction::ToggleDownloadsPanel,
      ),
      (
        ChromeActionUrl::ToggleBookmarkForActiveTab,
        ChromeAction::ToggleBookmarkForActiveTab,
      ),
      (
        ChromeActionUrl::ReorderBookmarksBar {
          ids: vec![BookmarkId(1), BookmarkId(2), BookmarkId(3)],
        },
        ChromeAction::ReorderBookmarksBar(vec![BookmarkId(1), BookmarkId(2), BookmarkId(3)]),
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
        url: "  https://example.com/ \n".to_string()
      }
      .into_chrome_action()
      .unwrap(),
      ChromeAction::NavigateTo("https://example.com/".to_string())
    );

    assert_eq!(
      ChromeActionUrl::OpenUrlInNewTab {
        url: "\tabout:blank\r".to_string()
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
    let cases = vec![
      ChromeActionUrl::CloseTab { tab_id: TabId(0) },
      ChromeActionUrl::DetachTab { tab_id: TabId(0) },
      ChromeActionUrl::ReloadTab { tab_id: TabId(0) },
      ChromeActionUrl::DuplicateTab { tab_id: TabId(0) },
      ChromeActionUrl::CloseOtherTabs { tab_id: TabId(0) },
      ChromeActionUrl::CloseTabsToRight { tab_id: TabId(0) },
      ChromeActionUrl::ActivateTab { tab_id: TabId(0) },
      ChromeActionUrl::TogglePinTab { tab_id: TabId(0) },
      ChromeActionUrl::FindNext { tab_id: TabId(0) },
      ChromeActionUrl::FindPrev { tab_id: TabId(0) },
      ChromeActionUrl::CloseFindInPage { tab_id: TabId(0) },
      ChromeActionUrl::FindQuery {
        tab_id: TabId(0),
        query: "x".to_string(),
        case_sensitive: false,
      },
    ];

    for case in cases {
      let err = case.into_chrome_action().unwrap_err();
      assert!(err.contains("invalid tab id"), "unexpected error: {err}");
    }
  }
}
