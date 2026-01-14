use fastrender::ui::{BookmarkId, ChromeActionUrl, TabId};

fn round_trip(url: &str, expected: ChromeActionUrl) {
  let parsed = ChromeActionUrl::parse(url).unwrap_or_else(|err| panic!("{url:?}: {err}"));
  assert_eq!(parsed, expected);
  let formatted = parsed.format();
  let reparsed = ChromeActionUrl::parse(&formatted)
    .unwrap_or_else(|err| panic!("round-trip parse failed for {formatted:?}: {err}"));
  assert_eq!(reparsed, expected);
}

#[test]
fn parse_round_trip_new_window() {
  round_trip("chrome-action:new-window", ChromeActionUrl::NewWindow);
}

#[test]
fn parse_round_trip_focus_address_bar() {
  round_trip(
    "chrome-action:focus-address-bar",
    ChromeActionUrl::FocusAddressBar,
  );
}

#[test]
fn parse_round_trip_basic_navigation_actions() {
  round_trip("chrome-action:back", ChromeActionUrl::Back);
  round_trip("chrome-action:forward", ChromeActionUrl::Forward);
  round_trip("chrome-action:reload", ChromeActionUrl::Reload);
  round_trip("chrome-action:stop-loading", ChromeActionUrl::StopLoading);
  round_trip("chrome-action:home", ChromeActionUrl::Home);
}

#[test]
fn parse_round_trip_tab_actions() {
  round_trip("chrome-action:new-tab", ChromeActionUrl::NewTab);
  round_trip(
    "chrome-action:close-tab?tab=1",
    ChromeActionUrl::CloseTab { tab_id: TabId(1) },
  );
  round_trip(
    "chrome-action:detach-tab?tab=1",
    ChromeActionUrl::DetachTab { tab_id: TabId(1) },
  );
  round_trip(
    "chrome-action:reload-tab?tab=1",
    ChromeActionUrl::ReloadTab { tab_id: TabId(1) },
  );
  round_trip(
    "chrome-action:duplicate-tab?tab=1",
    ChromeActionUrl::DuplicateTab { tab_id: TabId(1) },
  );
  round_trip(
    "chrome-action:close-other-tabs?tab=1",
    ChromeActionUrl::CloseOtherTabs { tab_id: TabId(1) },
  );
  round_trip(
    "chrome-action:close-tabs-to-right?tab=1",
    ChromeActionUrl::CloseTabsToRight { tab_id: TabId(1) },
  );
  round_trip(
    "chrome-action:activate-tab?tab=1",
    ChromeActionUrl::ActivateTab { tab_id: TabId(1) },
  );
  round_trip(
    "chrome-action:toggle-pin-tab?tab=1",
    ChromeActionUrl::TogglePinTab { tab_id: TabId(1) },
  );
}

#[test]
fn parse_round_trip_toggle_full_screen() {
  round_trip(
    "chrome-action:toggle-full-screen",
    ChromeActionUrl::ToggleFullScreen,
  );
}

#[test]
fn parse_round_trip_open_find_in_page() {
  round_trip(
    "chrome-action:open-find-in-page",
    ChromeActionUrl::OpenFindInPage,
  );
}

#[test]
fn parse_round_trip_save_page() {
  round_trip("chrome-action:save-page", ChromeActionUrl::SavePage);
}

#[test]
fn parse_round_trip_print_page() {
  round_trip("chrome-action:print-page", ChromeActionUrl::PrintPage);
}

#[test]
fn parse_round_trip_reopen_closed_tab() {
  round_trip(
    "chrome-action:reopen-closed-tab",
    ChromeActionUrl::ReopenClosedTab,
  );
}

#[test]
fn parse_round_trip_toggle_bookmarks_bar() {
  round_trip(
    "chrome-action:toggle-bookmarks-bar",
    ChromeActionUrl::ToggleBookmarksBar,
  );
}

#[test]
fn parse_round_trip_toggle_history_panel() {
  round_trip(
    "chrome-action:toggle-history-panel",
    ChromeActionUrl::ToggleHistoryPanel,
  );
}

#[test]
fn parse_round_trip_toggle_bookmarks_manager() {
  round_trip(
    "chrome-action:toggle-bookmarks-manager",
    ChromeActionUrl::ToggleBookmarksManager,
  );
}

#[test]
fn parse_round_trip_toggle_downloads_panel() {
  round_trip(
    "chrome-action:toggle-downloads-panel",
    ChromeActionUrl::ToggleDownloadsPanel,
  );
}

#[test]
fn parse_round_trip_toggle_bookmark_for_active_tab() {
  round_trip(
    "chrome-action:toggle-bookmark",
    ChromeActionUrl::ToggleBookmarkForActiveTab,
  );
}

#[test]
fn parse_round_trip_open_clear_browsing_data_dialog() {
  round_trip(
    "chrome-action:open-clear-browsing-data-dialog",
    ChromeActionUrl::OpenClearBrowsingDataDialog,
  );
}

#[test]
fn parse_round_trip_open_home_url_dialog() {
  round_trip(
    "chrome-action:open-home-url-dialog",
    ChromeActionUrl::OpenHomeUrlDialog,
  );
}

#[test]
fn parse_round_trip_open_close_tab_search() {
  round_trip(
    "chrome-action:open-tab-search",
    ChromeActionUrl::OpenTabSearch,
  );
  round_trip(
    "chrome-action:close-tab-search",
    ChromeActionUrl::CloseTabSearch,
  );
}

#[test]
fn parse_round_trip_set_show_menu_bar() {
  round_trip(
    "chrome-action:set-show-menu-bar?show=1",
    ChromeActionUrl::SetShowMenuBar { show: true },
  );
  round_trip(
    "chrome-action:set-show-menu-bar?show=0",
    ChromeActionUrl::SetShowMenuBar { show: false },
  );
}

#[test]
fn parse_round_trip_address_bar_focus_changed() {
  round_trip(
    "chrome-action:address-bar-focus-changed?has_focus=1",
    ChromeActionUrl::AddressBarFocusChanged { has_focus: true },
  );
  round_trip(
    "chrome-action:address-bar-focus-changed?has_focus=0",
    ChromeActionUrl::AddressBarFocusChanged { has_focus: false },
  );
}

#[test]
fn parse_round_trip_navigation_with_nested_url() {
  round_trip(
    "chrome-action:navigate?url=https%3A%2F%2Fexample.com%2F",
    ChromeActionUrl::Navigate {
      url: "https://example.com/".to_string(),
    },
  );
  round_trip(
    "chrome-action:open-url-in-new-tab?url=about%3Anewtab",
    ChromeActionUrl::OpenUrlInNewTab {
      url: "about:newtab".to_string(),
    },
  );
}

#[test]
fn rejects_nested_javascript_targets() {
  assert!(ChromeActionUrl::parse("chrome-action:navigate?url=javascript:alert(1)").is_err());
  assert!(ChromeActionUrl::parse(
    "chrome-action:open-url-in-new-tab?url=javascript:alert(1)"
  )
  .is_err());
}

#[test]
fn parse_round_trip_reorder_bookmarks_bar() {
  round_trip(
    "chrome-action:reorder-bookmarks-bar?id=1&id=2&id=3",
    ChromeActionUrl::ReorderBookmarksBar {
      ids: vec![BookmarkId(1), BookmarkId(2), BookmarkId(3)],
    },
  );
}

#[test]
fn parse_round_trip_find_query_and_next_prev_close() {
  round_trip(
    "chrome-action:find-query?tab=1&query=hello&case_sensitive=1",
    ChromeActionUrl::FindQuery {
      tab_id: TabId(1),
      query: "hello".to_string(),
      case_sensitive: true,
    },
  );
  round_trip(
    "chrome-action:find-next?tab=1",
    ChromeActionUrl::FindNext { tab_id: TabId(1) },
  );
  round_trip(
    "chrome-action:find-prev?tab=1",
    ChromeActionUrl::FindPrev { tab_id: TabId(1) },
  );
  round_trip(
    "chrome-action:close-find-in-page?tab=1",
    ChromeActionUrl::CloseFindInPage { tab_id: TabId(1) },
  );
}

#[test]
fn rejects_hierarchical_form() {
  assert!(ChromeActionUrl::parse("chrome-action://new-window").is_err());
  assert!(ChromeActionUrl::parse("chrome-action://navigate?url=about%3Ablank").is_err());
}

#[test]
fn rejects_fragments() {
  let err = ChromeActionUrl::parse("chrome-action:back#frag").unwrap_err();
  assert!(
    err.to_ascii_lowercase().contains("fragment"),
    "unexpected error: {err}"
  );
}

#[test]
fn rejects_unknown_action() {
  let err = ChromeActionUrl::parse("chrome-action:not-a-real-action").unwrap_err();
  assert!(
    err.to_ascii_lowercase().contains("unknown"),
    "unexpected error: {err}"
  );
}

#[test]
fn rejects_missing_required_params() {
  let err = ChromeActionUrl::parse("chrome-action:close-tab").unwrap_err();
  assert!(
    err.to_ascii_lowercase().contains("missing") && err.contains("tab"),
    "unexpected error: {err}"
  );

  let err = ChromeActionUrl::parse("chrome-action:navigate").unwrap_err();
  assert!(
    err.to_ascii_lowercase().contains("missing") && err.contains("url"),
    "unexpected error: {err}"
  );
}

#[test]
fn rejects_duplicate_or_conflicting_params() {
  let err = ChromeActionUrl::parse("chrome-action:close-tab?tab=1&tab=2").unwrap_err();
  assert!(
    err.to_ascii_lowercase().contains("duplicate"),
    "unexpected error: {err}"
  );

  let err = ChromeActionUrl::parse("chrome-action:close-tab?tab=1&tab_id=2").unwrap_err();
  assert!(
    err.to_ascii_lowercase().contains("conflicting"),
    "unexpected error: {err}"
  );

  let err = ChromeActionUrl::parse("chrome-action:navigate?url=about%3Ablank&url=about%3Ablank").unwrap_err();
  assert!(
    err.to_ascii_lowercase().contains("duplicate"),
    "unexpected error: {err}"
  );
}

#[test]
fn rejects_unknown_params() {
  let err = ChromeActionUrl::parse("chrome-action:back?x=1").unwrap_err();
  assert!(
    err.to_ascii_lowercase().contains("unknown") && err.contains("x"),
    "unexpected error: {err}"
  );

  let err = ChromeActionUrl::parse("chrome-action:close-tab?tab=1&x=1").unwrap_err();
  assert!(
    err.to_ascii_lowercase().contains("unknown") && err.contains("x"),
    "unexpected error: {err}"
  );
}

#[test]
fn accepts_legacy_param_names() {
  assert_eq!(
    ChromeActionUrl::parse("chrome-action:close-tab?tab_id=1").unwrap(),
    ChromeActionUrl::CloseTab { tab_id: TabId(1) }
  );
  assert_eq!(
    ChromeActionUrl::parse("chrome-action:navigate?input=about%3Ablank").unwrap(),
    ChromeActionUrl::Navigate {
      url: "about:blank".to_string()
    }
  );
  assert_eq!(
    ChromeActionUrl::parse("chrome-action:address-bar-focus-changed?focused=1").unwrap(),
    ChromeActionUrl::AddressBarFocusChanged { has_focus: true }
  );
}

mod mapping_tests {
  use super::*;
  use fastrender::ui::ChromeAction;

  #[test]
  fn maps_new_actions_to_chrome_action() {
    let cases = [
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
        ChromeActionUrl::ReopenClosedTab,
        ChromeAction::ReopenClosedTab,
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
        ChromeActionUrl::ToggleDownloadsPanel,
        ChromeAction::ToggleDownloadsPanel,
      ),
      (
        ChromeActionUrl::OpenClearBrowsingDataDialog,
        ChromeAction::OpenClearBrowsingDataDialog,
      ),
      (
        ChromeActionUrl::OpenHomeUrlDialog,
        ChromeAction::OpenHomeUrlDialog,
      ),
      (ChromeActionUrl::OpenTabSearch, ChromeAction::OpenTabSearch),
      (ChromeActionUrl::CloseTabSearch, ChromeAction::CloseTabSearch),
    ];

    for (url_action, expected) in cases {
      assert_eq!(url_action.into_chrome_action().unwrap(), expected);
    }
  }
}
