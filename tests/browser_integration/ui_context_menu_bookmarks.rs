#![cfg(feature = "browser_ui")]

use fastrender::ui::context_menu::{
  apply_page_context_menu_action, build_page_context_menu_entries, PageContextMenuAction,
  PageContextMenuBuildInput, PageContextMenuEntry,
};
use fastrender::ui::BookmarkStore;

#[test]
fn context_menu_bookmark_link_toggles_bookmark_store() {
  let _lock = super::stage_listener_test_lock();
  let mut bookmarks = BookmarkStore::default();
  let mut history_panel_open = false;
  let mut bookmarks_panel_open = false;

  let link_url = "https://example.com/target";
  let page_url = "https://example.com/";

  let build = |bookmarks: &BookmarkStore, history_panel_open: bool, bookmarks_panel_open: bool| {
    build_page_context_menu_entries(PageContextMenuBuildInput {
      link_url: Some(link_url),
      image_url: None,
      page_url: Some(page_url),
      bookmarks,
      history_panel_open,
      bookmarks_panel_open,
      can_copy: false,
      can_cut: false,
      can_paste: false,
      can_select_all: false,
    })
  };

  let entries = build(&bookmarks, history_panel_open, bookmarks_panel_open);
  let bookmark_action = entries
    .iter()
    .find_map(|entry| match entry {
      PageContextMenuEntry::Action(item) => match &item.action {
        PageContextMenuAction::BookmarkLink(url) if url == link_url => Some(item.action.clone()),
        _ => None,
      },
      _ => None,
  })
  .expect("expected context menu to include Bookmark Link action");

  assert!(!bookmarks.contains_url(link_url));

  let res = apply_page_context_menu_action(
    &mut bookmarks,
    &mut history_panel_open,
    &mut bookmarks_panel_open,
    &bookmark_action,
  );
  assert!(res.bookmarks_changed);
  assert!(bookmarks.contains_url(link_url));

  let entries_after = build(&bookmarks, history_panel_open, bookmarks_panel_open);
  let checked_after = entries_after.iter().any(|entry| match entry {
    PageContextMenuEntry::Action(item) => match &item.action {
      PageContextMenuAction::BookmarkLink(url) if url == link_url => item.checked,
      _ => false,
    },
    _ => false,
  });
  assert!(checked_after, "expected Bookmark Link menu item to be checked");

  let res2 = apply_page_context_menu_action(
    &mut bookmarks,
    &mut history_panel_open,
    &mut bookmarks_panel_open,
    &bookmark_action,
  );
  assert!(res2.bookmarks_changed);
  assert!(!bookmarks.contains_url(link_url));
}

#[test]
fn context_menu_bookmark_page_toggles_bookmark_store() {
  let _lock = super::stage_listener_test_lock();
  let mut bookmarks = BookmarkStore::default();
  let mut history_panel_open = false;
  let mut bookmarks_panel_open = false;

  let link_url = "https://example.com/target";
  let page_url = "https://example.com/";

  let build = |bookmarks: &BookmarkStore, history_panel_open: bool, bookmarks_panel_open: bool| {
    build_page_context_menu_entries(PageContextMenuBuildInput {
      link_url: Some(link_url),
      image_url: None,
      page_url: Some(page_url),
      bookmarks,
      history_panel_open,
      bookmarks_panel_open,
      can_copy: false,
      can_cut: false,
      can_paste: false,
      can_select_all: false,
    })
  };

  let entries = build(&bookmarks, history_panel_open, bookmarks_panel_open);
  let (bookmark_page_action, checked_initial) = entries
    .iter()
    .find_map(|entry| match entry {
      PageContextMenuEntry::Action(item) => match &item.action {
        PageContextMenuAction::BookmarkPage(url) if url == page_url => {
          Some((item.action.clone(), item.checked))
        }
        _ => None,
      },
      _ => None,
    })
    .expect("expected context menu to include Bookmark Page action");

  assert!(!bookmarks.contains_url(page_url));
  assert!(
    !checked_initial,
    "expected Bookmark Page menu item to be unchecked initially"
  );

  let res = apply_page_context_menu_action(
    &mut bookmarks,
    &mut history_panel_open,
    &mut bookmarks_panel_open,
    &bookmark_page_action,
  );
  assert!(res.bookmarks_changed);
  assert!(!res.ui_changed);
  assert!(bookmarks.contains_url(page_url));

  let entries_after = build(&bookmarks, history_panel_open, bookmarks_panel_open);
  let checked_after = entries_after
    .iter()
    .find_map(|entry| match entry {
      PageContextMenuEntry::Action(item) => match &item.action {
        PageContextMenuAction::BookmarkPage(url) if url == page_url => Some(item.checked),
        _ => None,
      },
      _ => None,
    })
    .expect("expected context menu to include Bookmark Page action after toggle");
  assert!(checked_after, "expected Bookmark Page menu item to be checked");

  let res2 = apply_page_context_menu_action(
    &mut bookmarks,
    &mut history_panel_open,
    &mut bookmarks_panel_open,
    &bookmark_page_action,
  );
  assert!(res2.bookmarks_changed);
  assert!(!res2.ui_changed);
  assert!(!bookmarks.contains_url(page_url));
}

#[test]
fn context_menu_toggle_history_panel_closes_bookmarks_panel() {
  let _lock = super::stage_listener_test_lock();
  let mut bookmarks = BookmarkStore::default();
  let mut history_panel_open = false;
  let mut bookmarks_panel_open = true;

  let res = apply_page_context_menu_action(
    &mut bookmarks,
    &mut history_panel_open,
    &mut bookmarks_panel_open,
    &PageContextMenuAction::ToggleHistoryPanel,
  );

  assert!(history_panel_open, "expected history panel to be open");
  assert!(
    !bookmarks_panel_open,
    "expected bookmarks panel to be closed when history is opened"
  );
  assert!(res.ui_changed, "expected ui_changed when toggling history panel");
  assert!(!res.bookmarks_changed);
}

#[test]
fn context_menu_toggle_history_panel_from_closed_state_opens_history() {
  let _lock = super::stage_listener_test_lock();
  let mut bookmarks = BookmarkStore::default();
  let mut history_panel_open = false;
  let mut bookmarks_panel_open = false;

  let res = apply_page_context_menu_action(
    &mut bookmarks,
    &mut history_panel_open,
    &mut bookmarks_panel_open,
    &PageContextMenuAction::ToggleHistoryPanel,
  );

  assert!(history_panel_open, "expected history panel to be open");
  assert!(
    !bookmarks_panel_open,
    "expected bookmarks panel to be closed when history is opened"
  );
  assert!(
    res.ui_changed,
    "expected ui_changed when opening history panel from closed state"
  );
  assert!(!res.bookmarks_changed);
}

#[test]
fn context_menu_toggle_bookmarks_panel_closes_history_panel() {
  let _lock = super::stage_listener_test_lock();
  let mut bookmarks = BookmarkStore::default();
  let mut history_panel_open = true;
  let mut bookmarks_panel_open = false;

  let res = apply_page_context_menu_action(
    &mut bookmarks,
    &mut history_panel_open,
    &mut bookmarks_panel_open,
    &PageContextMenuAction::ToggleBookmarksPanel,
  );

  assert!(
    !history_panel_open,
    "expected history panel to be closed when bookmarks is opened"
  );
  assert!(bookmarks_panel_open, "expected bookmarks panel to be open");
  assert!(
    res.ui_changed,
    "expected ui_changed when toggling bookmarks panel on"
  );
  assert!(!res.bookmarks_changed);
}

#[test]
fn context_menu_toggle_history_panel_off_reports_ui_changed() {
  let _lock = super::stage_listener_test_lock();
  let mut bookmarks = BookmarkStore::default();
  let mut history_panel_open = true;
  let mut bookmarks_panel_open = false;

  let res = apply_page_context_menu_action(
    &mut bookmarks,
    &mut history_panel_open,
    &mut bookmarks_panel_open,
    &PageContextMenuAction::ToggleHistoryPanel,
  );

  assert!(!history_panel_open, "expected history panel to be closed");
  assert!(!bookmarks_panel_open, "expected bookmarks panel to remain closed");
  assert!(
    res.ui_changed,
    "expected ui_changed when toggling history panel off"
  );
  assert!(!res.bookmarks_changed);
}
