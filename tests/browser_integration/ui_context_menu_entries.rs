#![cfg(feature = "browser_ui")]

use fastrender::ui::context_menu::{
  build_page_context_menu_entries, PageContextMenuAction, PageContextMenuBuildInput,
  PageContextMenuEntry,
};
use fastrender::ui::BookmarkStore;

fn actions(entries: &[PageContextMenuEntry]) -> Vec<PageContextMenuAction> {
  entries
    .iter()
    .filter_map(|entry| match entry {
      PageContextMenuEntry::Action(item) => Some(item.action.clone()),
      PageContextMenuEntry::Separator => None,
    })
    .collect()
}

#[test]
fn context_menu_entries_include_image_actions_when_image_url_present() {
  let _lock = super::stage_listener_test_lock();
  let bookmarks = BookmarkStore::default();
  let image_url = "https://example.com/image.png";

  let entries = build_page_context_menu_entries(PageContextMenuBuildInput {
    link_url: None,
    image_url: Some(image_url),
    page_url: Some("https://example.com/"),
    bookmarks: &bookmarks,
    history_panel_open: false,
    bookmarks_panel_open: false,
    can_copy: false,
    can_cut: false,
    can_paste: false,
    can_select_all: false,
  });

  let actions = actions(&entries);
  assert!(
    actions.contains(&PageContextMenuAction::OpenImageInNewTab(
      image_url.to_string()
    )),
    "expected OpenImageInNewTab action (got {actions:?})"
  );
  assert!(
    actions.contains(&PageContextMenuAction::DownloadImage(image_url.to_string())),
    "expected DownloadImage action (got {actions:?})"
  );
  assert!(
    actions.contains(&PageContextMenuAction::CopyImageAddress(
      image_url.to_string()
    )),
    "expected CopyImageAddress action (got {actions:?})"
  );
}

#[test]
fn context_menu_entries_include_clipboard_actions_when_enabled() {
  let _lock = super::stage_listener_test_lock();
  let bookmarks = BookmarkStore::default();

  let entries = build_page_context_menu_entries(PageContextMenuBuildInput {
    link_url: None,
    image_url: None,
    page_url: Some("https://example.com/"),
    bookmarks: &bookmarks,
    history_panel_open: false,
    bookmarks_panel_open: false,
    can_copy: true,
    can_cut: true,
    can_paste: true,
    can_select_all: true,
  });

  let actions = actions(&entries);
  assert_eq!(
    actions.get(0),
    Some(&PageContextMenuAction::CopySelection),
    "expected CopySelection as first action (got {actions:?})"
  );
  assert_eq!(
    actions.get(1),
    Some(&PageContextMenuAction::Cut),
    "expected Cut as second action (got {actions:?})"
  );
  assert_eq!(
    actions.get(2),
    Some(&PageContextMenuAction::Paste),
    "expected Paste as third action (got {actions:?})"
  );
  assert_eq!(
    actions.get(3),
    Some(&PageContextMenuAction::SelectAll),
    "expected SelectAll as fourth action (got {actions:?})"
  );
}
