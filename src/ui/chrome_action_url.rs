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
//!
//! ## URL grammar
//!
//! The accepted grammar is intentionally narrow (fail closed):
//!
//! ```text
//! chrome-action:<action-name>[?<query>]
//! ```
//!
//! - The scheme (`chrome-action`) is case-insensitive.
//! - Only the opaque form `chrome-action:<action>` is accepted (no `chrome-action://...`).
//! - URL fragments (`#...`) are rejected.
//! - The query string uses `application/x-www-form-urlencoded` decoding rules (i.e. `+` → space,
//!   percent decoding).
//!
//! ## Canonical parameter names
//!
//! - `url`: nested URL string for navigation actions.
//!   - Example: `chrome-action:navigate?url=https%3A%2F%2Fexample.com%2F`
//!   - Parser also accepts legacy `input`.
//! - `tab`: tab id for tab-scoped actions.
//!   - Example: `chrome-action:close-tab?tab=123`
//!   - Parser also accepts legacy `tab_id`.
//! - `show`: boolean used by `set-show-menu-bar`.
//! - `has_focus`: boolean used by `address-bar-focus-changed` (legacy: `focused`).
//! - `query`: used by `find-query` (find-in-page text). An empty query clears highlights.
//! - `case_sensitive`: used by `find-query` (boolean).
//!
//! Boolean parameters accept `1`, `0`, `true`, or `false` (case-insensitive). The formatter uses
//! `1`/`0`.

use crate::ui::bookmarks::BookmarkId;
use crate::ui::messages::TabId;
use crate::ui::ChromeAction;
use url::Url;

/// Canonical scheme name for `chrome-action:` URLs.
pub const CHROME_ACTION_SCHEME: &str = "chrome-action";

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

/// Historical name for the chrome URL scheme (kept for compatibility with older UI code).
pub const CHROME_ACTION_SCHEME: &str = ChromeActionUrl::SCHEME;

impl ChromeActionUrl {
  pub const SCHEME: &'static str = "chrome-action";

  /// Parse a `chrome-action:` URL string.
  ///
  /// Invariants:
  /// - Parsing is strict: unknown actions and malformed args fail closed.
  /// - Only the opaque form `chrome-action:<action>` is accepted (no `chrome-action://...`).
  pub fn parse(raw: &str) -> Result<Self, String> {
    let trimmed = trim_ascii_whitespace(raw);
    if trimmed != raw {
      return Err("chrome-action URLs must not contain leading or trailing whitespace".to_string());
    }

    let url =
      Url::parse(trimmed).map_err(|err| format!("invalid chrome-action URL {trimmed:?}: {err}"))?;
    Self::parse_url(&url)
  }

  /// Parse a pre-parsed [`Url`] for the `chrome-action:` scheme.
  pub fn parse_url(url: &Url) -> Result<Self, String> {
    if !url.scheme().eq_ignore_ascii_case(Self::SCHEME) {
      return Err(format!(
        "invalid chrome-action URL scheme: expected {}, got {}",
        Self::SCHEME,
        url.scheme()
      ));
    }

    if !url.cannot_be_a_base() {
      return Err(
        "chrome-action URLs must use the opaque form `chrome-action:<action>` (no `//`)"
          .to_string(),
      );
    }
    if url.fragment().is_some() {
      return Err("chrome-action URLs must not include a fragment".to_string());
    }

    let action = trim_ascii_whitespace(url.path());
    if action.is_empty() || action.starts_with('/') {
      return Err("chrome-action URL missing action".to_string());
    }
    let action = action.to_ascii_lowercase();

    let params: Vec<(String, String)> = url
      .query_pairs()
      .map(|(k, v)| (k.into_owned(), v.into_owned()))
      .collect();

    match action.as_str() {
      // Window / chrome-wide actions.
      "focus-address-bar" => {
        reject_unknown_params(&params, &[])?;
        Ok(Self::FocusAddressBar)
      }
      "new-window" => {
        reject_unknown_params(&params, &[])?;
        Ok(Self::NewWindow)
      }
      "toggle-full-screen" => {
        reject_unknown_params(&params, &[])?;
        Ok(Self::ToggleFullScreen)
      }
      "open-find-in-page" => {
        reject_unknown_params(&params, &[])?;
        Ok(Self::OpenFindInPage)
      }
      "save-page" => {
        reject_unknown_params(&params, &[])?;
        Ok(Self::SavePage)
      }
      "print-page" => {
        reject_unknown_params(&params, &[])?;
        Ok(Self::PrintPage)
      }

      // Find in page (per-tab).
      "find-query" => {
        let tab_id = parse_tab_id(required_param_any(&params, &["tab", "tab_id"], "tab")?)?;
        let query = required_param(&params, "query")?.to_string();
        let case_sensitive = parse_bool_param(optional_param(&params, "case_sensitive")?)?;
        reject_unknown_params(&params, &["tab", "tab_id", "query", "case_sensitive"])?;
        Ok(Self::FindQuery {
          tab_id,
          query,
          case_sensitive,
        })
      }
      "find-next" => {
        let tab_id = parse_tab_id(required_param_any(&params, &["tab", "tab_id"], "tab")?)?;
        reject_unknown_params(&params, &["tab", "tab_id"])?;
        Ok(Self::FindNext { tab_id })
      }
      "find-prev" => {
        let tab_id = parse_tab_id(required_param_any(&params, &["tab", "tab_id"], "tab")?)?;
        reject_unknown_params(&params, &["tab", "tab_id"])?;
        Ok(Self::FindPrev { tab_id })
      }
      "close-find-in-page" => {
        let tab_id = parse_tab_id(required_param_any(&params, &["tab", "tab_id"], "tab")?)?;
        reject_unknown_params(&params, &["tab", "tab_id"])?;
        Ok(Self::CloseFindInPage { tab_id })
      }

      // Tab management.
      "new-tab" => {
        reject_unknown_params(&params, &[])?;
        Ok(Self::NewTab)
      }
      "close-tab" => {
        let tab_id = parse_tab_id(required_param_any(&params, &["tab", "tab_id"], "tab")?)?;
        reject_unknown_params(&params, &["tab", "tab_id"])?;
        Ok(Self::CloseTab { tab_id })
      }
      "detach-tab" => {
        let tab_id = parse_tab_id(required_param_any(&params, &["tab", "tab_id"], "tab")?)?;
        reject_unknown_params(&params, &["tab", "tab_id"])?;
        Ok(Self::DetachTab { tab_id })
      }
      "reload-tab" => {
        let tab_id = parse_tab_id(required_param_any(&params, &["tab", "tab_id"], "tab")?)?;
        reject_unknown_params(&params, &["tab", "tab_id"])?;
        Ok(Self::ReloadTab { tab_id })
      }
      "duplicate-tab" => {
        let tab_id = parse_tab_id(required_param_any(&params, &["tab", "tab_id"], "tab")?)?;
        reject_unknown_params(&params, &["tab", "tab_id"])?;
        Ok(Self::DuplicateTab { tab_id })
      }
      "close-other-tabs" => {
        let tab_id = parse_tab_id(required_param_any(&params, &["tab", "tab_id"], "tab")?)?;
        reject_unknown_params(&params, &["tab", "tab_id"])?;
        Ok(Self::CloseOtherTabs { tab_id })
      }
      "close-tabs-to-right" => {
        let tab_id = parse_tab_id(required_param_any(&params, &["tab", "tab_id"], "tab")?)?;
        reject_unknown_params(&params, &["tab", "tab_id"])?;
        Ok(Self::CloseTabsToRight { tab_id })
      }
      "reopen-closed-tab" => {
        reject_unknown_params(&params, &[])?;
        Ok(Self::ReopenClosedTab)
      }
      "activate-tab" => {
        let tab_id = parse_tab_id(required_param_any(&params, &["tab", "tab_id"], "tab")?)?;
        reject_unknown_params(&params, &["tab", "tab_id"])?;
        Ok(Self::ActivateTab { tab_id })
      }
      "toggle-pin-tab" => {
        let tab_id = parse_tab_id(required_param_any(&params, &["tab", "tab_id"], "tab")?)?;
        reject_unknown_params(&params, &["tab", "tab_id"])?;
        Ok(Self::TogglePinTab { tab_id })
      }

      // Navigations.
      "navigate" => {
        let target = trim_ascii_whitespace(required_param_any(&params, &["url", "input"], "url")?);
        if target.is_empty() {
          return Err("navigate requires a non-empty url".to_string());
        }
        reject_unknown_params(&params, &["url", "input"])?;
        reject_javascript_nested_target(target)?;
        Ok(Self::Navigate {
          url: target.to_string(),
        })
      }
      "open-url-in-new-tab" => {
        let target = trim_ascii_whitespace(required_param_any(&params, &["url", "input"], "url")?);
        if target.is_empty() {
          return Err("open-url-in-new-tab requires a non-empty url".to_string());
        }
        reject_unknown_params(&params, &["url", "input"])?;
        reject_javascript_nested_target(target)?;
        Ok(Self::OpenUrlInNewTab {
          url: target.to_string(),
        })
      }

      // History controls.
      "back" => {
        reject_unknown_params(&params, &[])?;
        Ok(Self::Back)
      }
      "forward" => {
        reject_unknown_params(&params, &[])?;
        Ok(Self::Forward)
      }
      "reload" => {
        reject_unknown_params(&params, &[])?;
        Ok(Self::Reload)
      }
      "stop-loading" => {
        reject_unknown_params(&params, &[])?;
        Ok(Self::StopLoading)
      }
      "home" => {
        reject_unknown_params(&params, &[])?;
        Ok(Self::Home)
      }

      // Tab search.
      "open-tab-search" => {
        reject_unknown_params(&params, &[])?;
        Ok(Self::OpenTabSearch)
      }
      "close-tab-search" => {
        reject_unknown_params(&params, &[])?;
        Ok(Self::CloseTabSearch)
      }

      // Panels / chrome UI.
      "toggle-bookmarks-bar" => {
        reject_unknown_params(&params, &[])?;
        Ok(Self::ToggleBookmarksBar)
      }
      "set-show-menu-bar" => {
        let show = parse_bool_param(Some(required_param(&params, "show")?))?;
        reject_unknown_params(&params, &["show"])?;
        Ok(Self::SetShowMenuBar { show })
      }
      "address-bar-focus-changed" => {
        let raw_focus = required_param_any(&params, &["has_focus", "focused"], "has_focus")?;
        let has_focus = parse_bool_param(Some(raw_focus))?;
        reject_unknown_params(&params, &["has_focus", "focused"])?;
        Ok(Self::AddressBarFocusChanged { has_focus })
      }
      "toggle-bookmark" => {
        reject_unknown_params(&params, &[])?;
        Ok(Self::ToggleBookmarkForActiveTab)
      }
      "reorder-bookmarks-bar" => {
        reject_unknown_params(&params, &["id"])?;
        let ids = all_params(&params, "id")
          .into_iter()
          .map(|raw| parse_bookmark_id(&raw))
          .collect::<Result<Vec<_>, _>>()?;
        if ids.is_empty() {
          return Err("reorder-bookmarks-bar requires at least one id".to_string());
        }
        Ok(Self::ReorderBookmarksBar { ids })
      }
      "toggle-history-panel" => {
        reject_unknown_params(&params, &[])?;
        Ok(Self::ToggleHistoryPanel)
      }
      "toggle-bookmarks-manager" => {
        reject_unknown_params(&params, &[])?;
        Ok(Self::ToggleBookmarksManager)
      }
      "open-clear-browsing-data-dialog" => {
        reject_unknown_params(&params, &[])?;
        Ok(Self::OpenClearBrowsingDataDialog)
      }
      "open-home-url-dialog" => {
        reject_unknown_params(&params, &[])?;
        Ok(Self::OpenHomeUrlDialog)
      }
      "toggle-downloads-panel" => {
        reject_unknown_params(&params, &[])?;
        Ok(Self::ToggleDownloadsPanel)
      }

      other => Err(format!("unknown chrome-action: {other}")),
    }
  }

  /// Parse a pre-parsed [`Url`] into a [`ChromeActionUrl`].
  ///
  /// This is a convenience wrapper around [`ChromeActionUrl::parse`]. Callers that already have a
  /// parsed URL can avoid re-threading raw strings through their code paths.
  pub fn parse_url(url: &Url) -> Result<Self, String> {
    Self::parse(url.as_str())
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
        append_query(
          &mut out,
          &[
            ("tab", tab_id.0.to_string()),
            ("query", query.clone()),
            ("case_sensitive", bool_to_string(*case_sensitive)),
          ],
        );
      }
      Self::FindNext { tab_id } => {
        out.push_str("find-next");
        append_query(&mut out, &[("tab", tab_id.0.to_string())]);
      }
      Self::FindPrev { tab_id } => {
        out.push_str("find-prev");
        append_query(&mut out, &[("tab", tab_id.0.to_string())]);
      }
      Self::CloseFindInPage { tab_id } => {
        out.push_str("close-find-in-page");
        append_query(&mut out, &[("tab", tab_id.0.to_string())]);
      }

      Self::NewTab => out.push_str("new-tab"),
      Self::CloseTab { tab_id } => {
        out.push_str("close-tab");
        append_query(&mut out, &[("tab", tab_id.0.to_string())]);
      }
      Self::DetachTab { tab_id } => {
        out.push_str("detach-tab");
        append_query(&mut out, &[("tab", tab_id.0.to_string())]);
      }
      Self::ReloadTab { tab_id } => {
        out.push_str("reload-tab");
        append_query(&mut out, &[("tab", tab_id.0.to_string())]);
      }
      Self::DuplicateTab { tab_id } => {
        out.push_str("duplicate-tab");
        append_query(&mut out, &[("tab", tab_id.0.to_string())]);
      }
      Self::CloseOtherTabs { tab_id } => {
        out.push_str("close-other-tabs");
        append_query(&mut out, &[("tab", tab_id.0.to_string())]);
      }
      Self::CloseTabsToRight { tab_id } => {
        out.push_str("close-tabs-to-right");
        append_query(&mut out, &[("tab", tab_id.0.to_string())]);
      }
      Self::ReopenClosedTab => out.push_str("reopen-closed-tab"),
      Self::ActivateTab { tab_id } => {
        out.push_str("activate-tab");
        append_query(&mut out, &[("tab", tab_id.0.to_string())]);
      }
      Self::TogglePinTab { tab_id } => {
        out.push_str("toggle-pin-tab");
        append_query(&mut out, &[("tab", tab_id.0.to_string())]);
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

  /// Format this action into a canonical `chrome-action:` URL string.
  ///
  /// This is an alias for [`ChromeActionUrl::format`].
  pub fn to_url(&self) -> String {
    self.format()
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

/// `chrome-action` URL scheme string.
///
/// Prefer [`ChromeActionUrl::SCHEME`] when possible; this constant exists for backwards-compatible
/// call sites that imported `CHROME_ACTION_SCHEME` from `crate::ui`.
pub const CHROME_ACTION_SCHEME: &str = ChromeActionUrl::SCHEME;

impl std::fmt::Display for ChromeActionUrl {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(&self.format())
  }
}

/// Parse a `chrome-action:` href value into a typed [`ChromeActionUrl`].
///
/// This is intentionally strict (unknown actions fail closed) and only accepts the opaque
/// `chrome-action:<action>?<query>` grammar (not `chrome-action://...`). See
/// `docs/renderer_chrome_schemes.md`.
pub fn parse_chrome_action_url(href: &str) -> Option<ChromeActionUrl> {
  ChromeActionUrl::parse(href).ok()
}

/// Build a canonical `chrome-action:` href for the given action.
///
/// This round-trips with [`parse_chrome_action_url`]. Query parameters are percent-encoded using
/// `application/x-www-form-urlencoded` rules (via `url::form_urlencoded`).
pub fn chrome_action_href(action: &ChromeActionUrl) -> String {
  action.to_url_string()
}

pub fn chrome_action_back() -> String {
  chrome_action_href(&ChromeActionUrl::Back)
}

pub fn chrome_action_forward() -> String {
  chrome_action_href(&ChromeActionUrl::Forward)
}

pub fn chrome_action_reload() -> String {
  chrome_action_href(&ChromeActionUrl::Reload)
}

pub fn chrome_action_stop_loading() -> String {
  chrome_action_href(&ChromeActionUrl::StopLoading)
}

pub fn chrome_action_home() -> String {
  chrome_action_href(&ChromeActionUrl::Home)
}

pub fn chrome_action_new_tab() -> String {
  chrome_action_href(&ChromeActionUrl::NewTab)
}

pub fn chrome_action_activate_tab(tab_id: TabId) -> String {
  chrome_action_href(&ChromeActionUrl::ActivateTab { tab_id })
}

pub fn chrome_action_close_tab(tab_id: TabId) -> String {
  chrome_action_href(&ChromeActionUrl::CloseTab { tab_id })
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
  required_param_any(params, &[key], key)
}

fn optional_param<'a>(
  params: &'a [(String, String)],
  key: &str,
) -> Result<Option<&'a str>, String> {
  optional_param_any(params, &[key])
}

fn required_param_any<'a>(
  params: &'a [(String, String)],
  keys: &[&str],
  canonical: &str,
) -> Result<&'a str, String> {
  optional_param_any(params, keys)?
    .ok_or_else(|| format!("missing chrome-action param: {canonical}"))
}

fn optional_param_any<'a>(
  params: &'a [(String, String)],
  keys: &[&str],
) -> Result<Option<&'a str>, String> {
  let mut found_key: Option<&str> = None;
  let mut found_val: Option<&'a str> = None;

  for &key in keys {
    let mut iter = params.iter().filter(|(k, _)| k == key);
    let val = iter.next().map(|(_, v)| v.as_str());
    if val.is_some() && iter.next().is_some() {
      return Err(format!("duplicate chrome-action param: {key}"));
    }
    if let Some(val) = val {
      if let Some(prev) = found_key {
        return Err(format!(
          "conflicting chrome-action params: {prev} and {key}"
        ));
      }
      found_key = Some(key);
      found_val = Some(val);
    }
  }

  Ok(found_val)
}

fn all_params(params: &[(String, String)], key: &str) -> Vec<String> {
  params
    .iter()
    .filter(|(k, _)| k == key)
    .map(|(_, v)| v.clone())
    .collect()
}

fn reject_unknown_params(params: &[(String, String)], allowed: &[&str]) -> Result<(), String> {
  for (k, _) in params {
    if !allowed.iter().any(|allowed| allowed == &k.as_str()) {
      return Err(format!("unknown chrome-action param: {k}"));
    }
  }
  Ok(())
}

fn parse_tab_id(raw: &str) -> Result<TabId, String> {
  let raw = trim_ascii_whitespace(raw);
  let parsed: u64 = raw
    .parse()
    .map_err(|_| format!("invalid tab value: {raw:?}"))?;
  if parsed == 0 {
    return Err("invalid tab value: 0".to_string());
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
      (
        ChromeActionUrl::ReopenClosedTab,
        ChromeAction::ReopenClosedTab,
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
      // Window/chrome.
      (
        ChromeActionUrl::FocusAddressBar,
        ChromeAction::FocusAddressBar,
      ),
      (ChromeActionUrl::NewWindow, ChromeAction::NewWindow),
      (
        ChromeActionUrl::ToggleFullScreen,
        ChromeAction::ToggleFullScreen,
      ),
      (
        ChromeActionUrl::OpenFindInPage,
        ChromeAction::OpenFindInPage,
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
      (
        ChromeActionUrl::FindNext { tab_id },
        ChromeAction::FindNext(tab_id),
      ),
      (
        ChromeActionUrl::FindPrev { tab_id },
        ChromeAction::FindPrev(tab_id),
      ),
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
      (
        ChromeActionUrl::CloseTabSearch,
        ChromeAction::CloseTabSearch,
      ),
      (
        ChromeActionUrl::ToggleBookmarksBar,
        ChromeAction::ToggleBookmarksBar,
      ),
      (
        ChromeActionUrl::ToggleHistoryPanel,
        ChromeAction::ToggleHistoryPanel,
      ),
      (
        ChromeActionUrl::ToggleBookmarksManager,
        ChromeAction::ToggleBookmarksManager,
      ),
      (
        ChromeActionUrl::OpenClearBrowsingDataDialog,
        ChromeAction::OpenClearBrowsingDataDialog,
      ),
      (
        ChromeActionUrl::OpenHomeUrlDialog,
        ChromeAction::OpenHomeUrlDialog,
      ),
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
    assert!(ChromeActionUrl::Navigate {
      url: "   ".to_string()
    }
    .into_chrome_action()
    .is_err());
    assert!(ChromeActionUrl::OpenUrlInNewTab {
      url: "".to_string()
    }
    .into_chrome_action()
    .is_err());
  }

  #[test]
  fn allows_empty_find_query() {
    let tab_id = TabId(1);
    let action = ChromeActionUrl::FindQuery {
      tab_id,
      query: String::new(),
      case_sensitive: false,
    };

    assert_eq!(
      action.clone().into_chrome_action().unwrap(),
      ChromeAction::FindQuery {
        tab_id,
        query: String::new(),
        case_sensitive: false,
      }
    );

    let url = action.to_url();
    assert_eq!(ChromeActionUrl::parse(&url).unwrap(), action);
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
      let url = chrome_action_href(&case);
      let parsed = parse_chrome_action_url(&url)
        .unwrap_or_else(|| panic!("failed to parse formatted chrome-action URL {url:?}"));
      assert_eq!(parsed, case, "roundtrip mismatch for {url}");
    }
  }

  #[test]
  fn parses_representative_strings() {
    assert_eq!(
      ChromeActionUrl::parse("chrome-action:back").unwrap(),
      ChromeActionUrl::Back
    );
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

  #[test]
  fn chrome_action_href_round_trips_all_variants() {
    let actions = vec![
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
        tab_id: TabId(7),
        query: "café & dogs + cats".to_string(),
        case_sensitive: true,
      },
      ChromeActionUrl::FindNext { tab_id: TabId(7) },
      ChromeActionUrl::FindPrev { tab_id: TabId(7) },
      ChromeActionUrl::CloseFindInPage { tab_id: TabId(7) },
      ChromeActionUrl::SavePage,
      ChromeActionUrl::PrintPage,
      ChromeActionUrl::SetShowMenuBar { show: true },
      ChromeActionUrl::SetShowMenuBar { show: false },
      ChromeActionUrl::AddressBarFocusChanged { has_focus: true },
      ChromeActionUrl::AddressBarFocusChanged { has_focus: false },
      ChromeActionUrl::Navigate {
        url: "https://example.com/a?b=c&d=hello world#frag".to_string(),
      },
      ChromeActionUrl::OpenUrlInNewTab {
        url: "file:///tmp/space here+plus.txt".to_string(),
      },
      ChromeActionUrl::CloseTab { tab_id: TabId(7) },
      ChromeActionUrl::DetachTab { tab_id: TabId(123) },
      ChromeActionUrl::ReloadTab { tab_id: TabId(55) },
      ChromeActionUrl::DuplicateTab { tab_id: TabId(99) },
      ChromeActionUrl::CloseOtherTabs { tab_id: TabId(3) },
      ChromeActionUrl::CloseTabsToRight { tab_id: TabId(3) },
      ChromeActionUrl::ActivateTab { tab_id: TabId(42) },
      ChromeActionUrl::TogglePinTab {
        tab_id: TabId(123456),
      },
    ];

    for action in actions {
      let href = chrome_action_href(&action);
      let parsed = parse_chrome_action_url(&href).unwrap_or_else(|| {
        panic!("failed to parse chrome-action href produced by formatter: {href:?}");
      });
      assert_eq!(parsed, action, "href={href}");
    }
  }
}
