//! Canonical `chrome-action:` URL parser/formatter.
//!
//! Renderer-chrome uses *trusted*, JS-free HTML documents to render the browser chrome. UI widgets
//! in those documents (links/forms) request browser actions by navigating to `chrome-action:` URLs.
//!
//! This module provides a strict, round-trippable encoding that acts as glue between those
//! navigations and the strongly-typed [`crate::ui::ChromeAction`] pipeline.
//!
//! # URL form
//!
//! The accepted grammar is intentionally narrow (fail closed):
//!
//! ```text
//! chrome-action:<action-name>[?<query>]
//! ```
//!
//! - The scheme (`chrome-action`) is case-insensitive.
//! - The URL must be absolute and must **not** use an authority/path form like `chrome-action://…`.
//! - Fragments (`#...`) are rejected.
//! - The optional query uses `application/x-www-form-urlencoded` decoding rules (i.e. `+` → space,
//!   percent decoding).
//!
//! # Canonical parameter names
//!
//! - `url`: used by actions that accept a URL string.
//!   - Example: `chrome-action:navigate?url=https%3A%2F%2Fexample.com%2F`
//!   - (Legacy alias accepted by the parser: `input`)
//! - `tab`: used by actions that target a specific tab id.
//!   - Example: `chrome-action:close-tab?tab=123`
//!   - (Legacy alias accepted by the parser: `tab_id`)
//! - `show`: used by `set-show-menu-bar` (boolean).
//! - `has_focus`: used by `address-bar-focus-changed` (boolean).
//! - `query`: used by `find-query` (find-in-page text).
//! - `case_sensitive`: used by `find-query` (boolean).
//!
//! Boolean parameters accept `1`, `0`, `true`, or `false` (case-insensitive). The formatter uses
//! `1`/`0`.

use crate::ui::messages::TabId;
use crate::ui::ChromeAction;
use std::collections::HashMap;
use url::form_urlencoded;
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
  FindQuery {
    tab_id: TabId,
    query: String,
    case_sensitive: bool,
  },
  FindNext { tab_id: TabId },
  FindPrev { tab_id: TabId },
  CloseFindInPage { tab_id: TabId },
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

fn parse_tab_id(raw: &str) -> Result<TabId, String> {
  let raw = trim_ascii_whitespace(raw);
  let id: u64 = raw
    .parse()
    .map_err(|_| format!("invalid tab id: {raw:?}"))?;
  validate_tab_id(TabId(id))
}

fn parse_bool(raw: &str) -> Result<bool, String> {
  let v = trim_ascii_whitespace(raw);
  if v.eq_ignore_ascii_case("true") || v == "1" {
    Ok(true)
  } else if v.eq_ignore_ascii_case("false") || v == "0" {
    Ok(false)
  } else {
    Err(format!("invalid boolean value: {raw:?}"))
  }
}

fn parse_query_params(query: &str) -> Result<HashMap<String, String>, String> {
  let mut params: HashMap<String, String> = HashMap::new();

  for (key, value) in form_urlencoded::parse(query.as_bytes()) {
    if key.is_empty() {
      return Err("query parameter name must not be empty".to_string());
    }
    let key = key.into_owned();
    if params.contains_key(&key) {
      return Err(format!("duplicate query parameter: {key}"));
    }
    params.insert(key, value.into_owned());
  }

  Ok(params)
}

impl ChromeActionUrl {
  /// Parse an absolute `chrome-action:` URL string into a strongly-typed action.
  ///
  /// The parser is intentionally strict: unknown actions, unknown parameters, duplicated
  /// parameters, fragments, and disallowed URL forms (e.g. `chrome-action://...`) are rejected.
  pub fn parse(raw: &str) -> Result<Self, String> {
    if raw.is_empty() {
      return Err("empty URL".to_string());
    }
    if raw.as_bytes().iter().any(|b| b.is_ascii_whitespace()) {
      return Err("chrome-action URL must not contain ASCII whitespace".to_string());
    }

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

    // Enforce the narrow grammar `chrome-action:<action>[?<query>]`.
    if url.fragment().is_some() {
      return Err("chrome-action URLs must not contain fragments (#...)".to_string());
    }
    if !url.cannot_be_a_base() {
      return Err(
        "chrome-action URL must use the form chrome-action:<action>[?<query>] (no // or /)".to_string(),
      );
    }

    let action = url.path();
    if action.is_empty() {
      return Err("chrome-action URL missing action name".to_string());
    }
    let action = action.to_ascii_lowercase();
    let action_str = action.as_str();

    let has_query = url.query().is_some();
    let params = url.query().map(parse_query_params).transpose()?.unwrap_or_default();

    let ensure_no_params = || -> Result<(), String> {
      if has_query {
        return Err(format!(
          "chrome-action:{action_str} does not take query parameters"
        ));
      }
      Ok(())
    };

    let parse_tab_only = || -> Result<TabId, String> {
      // Allow either `tab` or legacy `tab_id`, but not both and no extra params.
      if params.len() != 1 {
        return Err(format!(
          "chrome-action:{action_str} expects exactly one query parameter: tab"
        ));
      }
      if let Some(v) = params.get("tab") {
        return parse_tab_id(v);
      }
      if let Some(v) = params.get("tab_id") {
        return parse_tab_id(v);
      }
      Err("missing required query parameter: tab".to_string())
    };

    let parse_url_only = || -> Result<String, String> {
      // Allow either canonical `url` or legacy `input`, but not both and no extra params.
      if params.len() != 1 {
        return Err(format!(
          "chrome-action:{action_str} expects exactly one query parameter: url"
        ));
      }
      let raw = if let Some(v) = params.get("url") {
        v
      } else if let Some(v) = params.get("input") {
        v
      } else {
        return Err("missing required query parameter: url".to_string());
      };

      let trimmed = trim_ascii_whitespace(raw);
      if trimmed.is_empty() {
        return Err(format!("chrome-action:{action_str} requires a non-empty url"));
      }
      if trimmed.as_bytes().len() > crate::ui::protocol_limits::MAX_URL_BYTES {
        return Err(format!(
          "url exceeds MAX_URL_BYTES ({})",
          crate::ui::protocol_limits::MAX_URL_BYTES
        ));
      }
      Ok(trimmed.to_string())
    };

    let parse_bool_only = |expected_key: &str, legacy_key: Option<&str>| -> Result<bool, String> {
      if params.len() != 1 {
        return Err(format!(
          "chrome-action:{action_str} expects exactly one query parameter: {expected_key}"
        ));
      }
      let raw = if let Some(v) = params.get(expected_key) {
        v
      } else if let Some(legacy) = legacy_key {
        params
          .get(legacy)
          .ok_or_else(|| format!("missing required query parameter: {expected_key}"))?
      } else {
        return Err(format!("missing required query parameter: {expected_key}"));
      };
      parse_bool(raw)
    };

    Ok(match action_str {
      "back" => {
        ensure_no_params()?;
        Self::Back
      }
      "forward" => {
        ensure_no_params()?;
        Self::Forward
      }
      "reload" => {
        ensure_no_params()?;
        Self::Reload
      }
      "stop-loading" => {
        ensure_no_params()?;
        Self::StopLoading
      }
      "home" => {
        ensure_no_params()?;
        Self::Home
      }
      "new-tab" => {
        ensure_no_params()?;
        Self::NewTab
      }
      "reopen-closed-tab" => {
        ensure_no_params()?;
        Self::ReopenClosedTab
      }
      "open-tab-search" => {
        ensure_no_params()?;
        Self::OpenTabSearch
      }
      "close-tab-search" => {
        ensure_no_params()?;
        Self::CloseTabSearch
      }
      "toggle-bookmarks-bar" => {
        ensure_no_params()?;
        Self::ToggleBookmarksBar
      }
      "toggle-history-panel" => {
        ensure_no_params()?;
        Self::ToggleHistoryPanel
      }
      "toggle-bookmarks-manager" => {
        ensure_no_params()?;
        Self::ToggleBookmarksManager
      }
      "open-clear-browsing-data-dialog" => {
        ensure_no_params()?;
        Self::OpenClearBrowsingDataDialog
      }
      "toggle-downloads-panel" => {
        ensure_no_params()?;
        Self::ToggleDownloadsPanel
      }
      "toggle-bookmark-for-active-tab" => {
        ensure_no_params()?;
        Self::ToggleBookmarkForActiveTab
      }
      "focus-address-bar" => {
        ensure_no_params()?;
        Self::FocusAddressBar
      }
      "new-window" => {
        ensure_no_params()?;
        Self::NewWindow
      }
      "toggle-full-screen" => {
        ensure_no_params()?;
        Self::ToggleFullScreen
      }
      "open-find-in-page" => {
        ensure_no_params()?;
        Self::OpenFindInPage
      }
      "find-query" => {
        // Allowed params: tab/tab_id + query/q + optional case_sensitive/case.
        for key in params.keys() {
          match key.as_str() {
            "tab" | "tab_id" | "query" | "q" | "case_sensitive" | "case" => {}
            other => {
              return Err(format!(
                "unknown query parameter for chrome-action:{action_str}: {other}"
              ));
            }
          }
        }

        let tab_raw = match (params.get("tab"), params.get("tab_id")) {
          (Some(_), Some(_)) => {
            return Err("find-query must specify only one of tab/tab_id".to_string());
          }
          (Some(v), None) | (None, Some(v)) => v,
          (None, None) => return Err("missing required query parameter: tab".to_string()),
        };
        let tab_id = parse_tab_id(tab_raw)?;

        let query_raw = match (params.get("query"), params.get("q")) {
          (Some(_), Some(_)) => {
            return Err("find-query must specify only one of query/q".to_string());
          }
          (Some(v), None) | (None, Some(v)) => v,
          (None, None) => return Err("missing required query parameter: query".to_string()),
        };
        if query_raw.as_bytes().len() > crate::ui::protocol_limits::MAX_FIND_QUERY_BYTES {
          return Err(format!(
            "query exceeds MAX_FIND_QUERY_BYTES ({})",
            crate::ui::protocol_limits::MAX_FIND_QUERY_BYTES
          ));
        }
        let query = query_raw.to_string();

        let case_sensitive = match (params.get("case_sensitive"), params.get("case")) {
          (Some(_), Some(_)) => {
            return Err("find-query must specify only one of case_sensitive/case".to_string());
          }
          (Some(v), None) | (None, Some(v)) => parse_bool(v)?,
          (None, None) => false,
        };

        Self::FindQuery {
          tab_id,
          query,
          case_sensitive,
        }
      }
      "find-next" => Self::FindNext {
        tab_id: parse_tab_only()?,
      },
      "find-prev" => Self::FindPrev {
        tab_id: parse_tab_only()?,
      },
      "close-find-in-page" => Self::CloseFindInPage {
        tab_id: parse_tab_only()?,
      },
      "save-page" => {
        ensure_no_params()?;
        Self::SavePage
      }
      "print-page" => {
        ensure_no_params()?;
        Self::PrintPage
      }
      "set-show-menu-bar" => Self::SetShowMenuBar {
        show: parse_bool_only("show", None)?,
      },
      "address-bar-focus-changed" => Self::AddressBarFocusChanged {
        has_focus: parse_bool_only("has_focus", Some("focus"))?,
      },
      "navigate" => Self::Navigate {
        url: parse_url_only()?,
      },
      "open-url-in-new-tab" => Self::OpenUrlInNewTab {
        url: parse_url_only()?,
      },
      "close-tab" => Self::CloseTab {
        tab_id: parse_tab_only()?,
      },
      "detach-tab" => Self::DetachTab {
        tab_id: parse_tab_only()?,
      },
      "reload-tab" => Self::ReloadTab {
        tab_id: parse_tab_only()?,
      },
      "duplicate-tab" => Self::DuplicateTab {
        tab_id: parse_tab_only()?,
      },
      "close-other-tabs" => Self::CloseOtherTabs {
        tab_id: parse_tab_only()?,
      },
      "close-tabs-to-right" => Self::CloseTabsToRight {
        tab_id: parse_tab_only()?,
      },
      "activate-tab" => Self::ActivateTab {
        tab_id: parse_tab_only()?,
      },
      "toggle-pin-tab" => Self::TogglePinTab {
        tab_id: parse_tab_only()?,
      },
      other => return Err(format!("unknown chrome-action: {other}")),
    })
  }

  /// Format this action as a canonical `chrome-action:` URL.
  ///
  /// The output is deterministic and round-trippable with [`ChromeActionUrl::parse`].
  pub fn to_url(&self) -> String {
    fn with_query(action: &str, params: &[(&str, &str)]) -> String {
      let mut serializer = form_urlencoded::Serializer::new(String::new());
      for (k, v) in params {
        serializer.append_pair(k, v);
      }
      let query = serializer.finish();
      if query.is_empty() {
        format!("{CHROME_ACTION_SCHEME}:{action}")
      } else {
        format!("{CHROME_ACTION_SCHEME}:{action}?{query}")
      }
    }

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
      Self::FindQuery {
        tab_id,
        query,
        case_sensitive,
      } => {
        let tab = tab_id.0.to_string();
        with_query(
          "find-query",
          &[
            ("tab", tab.as_str()),
            ("query", query.as_str()),
            ("case_sensitive", if *case_sensitive { "1" } else { "0" }),
          ],
        )
      }
      Self::FindNext { tab_id } => {
        let tab = tab_id.0.to_string();
        with_query("find-next", &[("tab", tab.as_str())])
      }
      Self::FindPrev { tab_id } => {
        let tab = tab_id.0.to_string();
        with_query("find-prev", &[("tab", tab.as_str())])
      }
      Self::CloseFindInPage { tab_id } => {
        let tab = tab_id.0.to_string();
        with_query("close-find-in-page", &[("tab", tab.as_str())])
      }
      Self::SavePage => format!("{CHROME_ACTION_SCHEME}:save-page"),
      Self::PrintPage => format!("{CHROME_ACTION_SCHEME}:print-page"),
      Self::SetShowMenuBar { show } => with_query(
        "set-show-menu-bar",
        &[("show", if *show { "1" } else { "0" })],
      ),
      Self::AddressBarFocusChanged { has_focus } => with_query(
        "address-bar-focus-changed",
        &[("has_focus", if *has_focus { "1" } else { "0" })],
      ),
      Self::Navigate { url } => with_query("navigate", &[("url", url.as_str())]),
      Self::OpenUrlInNewTab { url } => with_query("open-url-in-new-tab", &[("url", url.as_str())]),
      Self::CloseTab { tab_id } => {
        let tab = tab_id.0.to_string();
        with_query("close-tab", &[("tab", tab.as_str())])
      }
      Self::DetachTab { tab_id } => {
        let tab = tab_id.0.to_string();
        with_query("detach-tab", &[("tab", tab.as_str())])
      }
      Self::ReloadTab { tab_id } => {
        let tab = tab_id.0.to_string();
        with_query("reload-tab", &[("tab", tab.as_str())])
      }
      Self::DuplicateTab { tab_id } => {
        let tab = tab_id.0.to_string();
        with_query("duplicate-tab", &[("tab", tab.as_str())])
      }
      Self::CloseOtherTabs { tab_id } => {
        let tab = tab_id.0.to_string();
        with_query("close-other-tabs", &[("tab", tab.as_str())])
      }
      Self::CloseTabsToRight { tab_id } => {
        let tab = tab_id.0.to_string();
        with_query("close-tabs-to-right", &[("tab", tab.as_str())])
      }
      Self::ActivateTab { tab_id } => {
        let tab = tab_id.0.to_string();
        with_query("activate-tab", &[("tab", tab.as_str())])
      }
      Self::TogglePinTab { tab_id } => {
        let tab = tab_id.0.to_string();
        with_query("toggle-pin-tab", &[("tab", tab.as_str())])
      }
    }
  }

  /// Backwards-compatible alias for [`ChromeActionUrl::to_url`].
  pub fn to_url_string(&self) -> String {
    self.to_url()
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
      Self::FindQuery {
        tab_id,
        query,
        case_sensitive,
      } => {
        let tab_id = validate_tab_id(tab_id)?;
        let query = trim_ascii_whitespace(&query);
        if query.is_empty() {
          return Err("FindQuery action requires a non-empty query".to_string());
        }
        if query.as_bytes().len() > crate::ui::protocol_limits::MAX_FIND_QUERY_BYTES {
          return Err(format!(
            "FindQuery action query exceeds MAX_FIND_QUERY_BYTES ({})",
            crate::ui::protocol_limits::MAX_FIND_QUERY_BYTES
          ));
        }
        ChromeAction::FindQuery {
          tab_id,
          query: query.to_string(),
          case_sensitive,
        }
      }
      Self::FindNext { tab_id } => ChromeAction::FindNext(validate_tab_id(tab_id)?),
      Self::FindPrev { tab_id } => ChromeAction::FindPrev(validate_tab_id(tab_id)?),
      Self::CloseFindInPage { tab_id } => ChromeAction::CloseFindInPage(validate_tab_id(tab_id)?),
      Self::SavePage => ChromeAction::SavePage,
      Self::PrintPage => ChromeAction::PrintPage,
      Self::SetShowMenuBar { show } => ChromeAction::SetShowMenuBar(show),
      Self::AddressBarFocusChanged { has_focus } => ChromeAction::AddressBarFocusChanged(has_focus),
      Self::Navigate { url } => {
        let url = trim_ascii_whitespace(&url);
        if url.is_empty() {
          return Err("Navigate action requires a non-empty url".to_string());
        }
        if url.as_bytes().len() > crate::ui::protocol_limits::MAX_URL_BYTES {
          return Err(format!(
            "Navigate action url exceeds MAX_URL_BYTES ({})",
            crate::ui::protocol_limits::MAX_URL_BYTES
          ));
        }
        ChromeAction::NavigateTo(url.to_string())
      }
      Self::OpenUrlInNewTab { url } => {
        let url = trim_ascii_whitespace(&url);
        if url.is_empty() {
          return Err("OpenUrlInNewTab action requires a non-empty url".to_string());
        }
        if url.as_bytes().len() > crate::ui::protocol_limits::MAX_URL_BYTES {
          return Err(format!(
            "OpenUrlInNewTab action url exceeds MAX_URL_BYTES ({})",
            crate::ui::protocol_limits::MAX_URL_BYTES
          ));
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
      (
        ChromeActionUrl::FindQuery {
          tab_id,
          query: "cats".to_string(),
          case_sensitive: true,
        },
        ChromeAction::FindQuery {
          tab_id,
          query: "cats".to_string(),
          case_sensitive: true,
        },
      ),
      (ChromeActionUrl::FindNext { tab_id }, ChromeAction::FindNext(tab_id)),
      (ChromeActionUrl::FindPrev { tab_id }, ChromeAction::FindPrev(tab_id)),
      (
        ChromeActionUrl::CloseFindInPage { tab_id },
        ChromeAction::CloseFindInPage(tab_id),
      ),
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
    assert!(ChromeActionUrl::Navigate { url: "   ".to_string() }
      .into_chrome_action()
      .is_err());
    assert!(ChromeActionUrl::OpenUrlInNewTab { url: "".to_string() }
      .into_chrome_action()
      .is_err());
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
    ];

    for case in cases {
      let err = case.into_chrome_action().unwrap_err();
      assert!(err.contains("invalid tab id"), "unexpected error: {err}");
    }
  }

  #[test]
  fn roundtrips_all_variants_via_url_format() {
    let tab_id = TabId(123);

    let cases: Vec<ChromeActionUrl> = vec![
      ChromeActionUrl::Back,
      ChromeActionUrl::Forward,
      ChromeActionUrl::Reload,
      ChromeActionUrl::StopLoading,
      ChromeActionUrl::Home,
      ChromeActionUrl::NewTab,
      ChromeActionUrl::ReopenClosedTab,
      ChromeActionUrl::OpenTabSearch,
      ChromeActionUrl::CloseTabSearch,
      ChromeActionUrl::ToggleBookmarksBar,
      ChromeActionUrl::ToggleHistoryPanel,
      ChromeActionUrl::ToggleBookmarksManager,
      ChromeActionUrl::OpenClearBrowsingDataDialog,
      ChromeActionUrl::ToggleDownloadsPanel,
      ChromeActionUrl::ToggleBookmarkForActiveTab,
      ChromeActionUrl::FocusAddressBar,
      ChromeActionUrl::NewWindow,
      ChromeActionUrl::ToggleFullScreen,
      ChromeActionUrl::OpenFindInPage,
      ChromeActionUrl::FindQuery {
        tab_id,
        query: "cats".to_string(),
        case_sensitive: false,
      },
      ChromeActionUrl::FindNext { tab_id },
      ChromeActionUrl::FindPrev { tab_id },
      ChromeActionUrl::CloseFindInPage { tab_id },
      ChromeActionUrl::SavePage,
      ChromeActionUrl::PrintPage,
      ChromeActionUrl::SetShowMenuBar { show: true },
      ChromeActionUrl::AddressBarFocusChanged { has_focus: false },
      ChromeActionUrl::Navigate {
        url: "https://example.com/a?b=c&d=e".to_string(),
      },
      ChromeActionUrl::OpenUrlInNewTab {
        url: "about:blank".to_string(),
      },
      ChromeActionUrl::CloseTab { tab_id },
      ChromeActionUrl::DetachTab { tab_id },
      ChromeActionUrl::ReloadTab { tab_id },
      ChromeActionUrl::DuplicateTab { tab_id },
      ChromeActionUrl::CloseOtherTabs { tab_id },
      ChromeActionUrl::CloseTabsToRight { tab_id },
      ChromeActionUrl::ActivateTab { tab_id },
      ChromeActionUrl::TogglePinTab { tab_id },
    ];

    for case in cases {
      let url = case.to_url();
      let parsed = ChromeActionUrl::parse(&url).unwrap_or_else(|err| {
        panic!("failed to parse formatted chrome-action URL {url:?}: {err}");
      });
      assert_eq!(parsed, case, "roundtrip mismatch for {url}");
    }
  }

  #[test]
  fn parses_representative_strings() {
    assert_eq!(ChromeActionUrl::parse("chrome-action:back").unwrap(), ChromeActionUrl::Back);
    assert_eq!(
      ChromeActionUrl::parse("chrome-action:navigate?url=https%3A%2F%2Fexample.com%2F").unwrap(),
      ChromeActionUrl::Navigate {
        url: "https://example.com/".to_string(),
      }
    );

    // Scheme must be case-insensitive.
    assert_eq!(
      ChromeActionUrl::parse("ChRoMe-AcTiOn:reload").unwrap(),
      ChromeActionUrl::Reload
    );
  }

  #[test]
  fn rejects_whitespace_and_invalid_inputs() {
    let cases = vec![
      " chrome-action:back",
      "chrome-action:back ",
      "chrome-action:back\n",
      "chrome-action://back",
      "chrome-action:unknown",
      "chrome-action:navigate?url=",
      "chrome-action:navigate?url=+",
      "chrome-action:navigate",
      "chrome-action:close-tab?tab=0",
      "chrome-action:close-tab?tab=not-a-number",
      "chrome-action:navigate?url=a&url=b",
      "chrome-action:back?x=1",
      "chrome-action:set-show-menu-bar?show=maybe",
      // Mixed alias keys should be rejected.
      "chrome-action:activate-tab?tab=1&tab_id=2",
    ];

    for input in cases {
      assert!(
        ChromeActionUrl::parse(input).is_err(),
        "expected parse error for input {input:?}"
      );
    }
  }

  #[test]
  fn chrome_action_url_encoding_is_deterministic() {
    let action = ChromeActionUrl::Navigate {
      url: "cats & dogs".to_string(),
    };

    assert_eq!(
      action.to_url(),
      format!("{CHROME_ACTION_SCHEME}:navigate?url=cats+%26+dogs")
    );

    assert_eq!(
      ChromeActionUrl::parse(&format!(
        "{CHROME_ACTION_SCHEME}:navigate?url=cats+%26+dogs"
      ))
      .unwrap(),
      action
    );

    // Legacy alias accepted.
    assert_eq!(
      ChromeActionUrl::parse(&format!(
        "{CHROME_ACTION_SCHEME}:navigate?input=cats+%26+dogs"
      ))
      .unwrap(),
      action
    );
  }
}
