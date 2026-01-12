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
      page_url: Some(page_url),
      bookmarks,
      history_panel_open,
      bookmarks_panel_open,
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

  assert!(!bookmarks.contains(link_url));

  let res = apply_page_context_menu_action(
    &mut bookmarks,
    &mut history_panel_open,
    &mut bookmarks_panel_open,
    &bookmark_action,
  );
  assert!(res.bookmarks_changed);
  assert!(bookmarks.contains(link_url));

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
  assert!(!bookmarks.contains(link_url));
}
