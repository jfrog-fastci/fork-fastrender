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

fn checked_state(entries: &[PageContextMenuEntry], action: PageContextMenuAction) -> Option<bool> {
  entries.iter().find_map(|entry| match entry {
    PageContextMenuEntry::Action(item) if item.action == action => Some(item.checked),
    _ => None,
  })
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
fn context_menu_entries_include_link_actions_when_link_url_present() {
  let _lock = super::stage_listener_test_lock();
  let bookmarks = BookmarkStore::default();
  let link_url = "https://example.com/target";

  let entries = build_page_context_menu_entries(PageContextMenuBuildInput {
    link_url: Some(link_url),
    image_url: None,
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
    actions.contains(&PageContextMenuAction::OpenLinkInNewTab(link_url.to_string())),
    "expected OpenLinkInNewTab action (got {actions:?})"
  );
  assert!(
    actions.contains(&PageContextMenuAction::DownloadLink(link_url.to_string())),
    "expected DownloadLink action (got {actions:?})"
  );
  assert!(
    actions.contains(&PageContextMenuAction::CopyLinkAddress(link_url.to_string())),
    "expected CopyLinkAddress action (got {actions:?})"
  );
}

#[test]
fn context_menu_entries_include_image_and_link_actions_when_both_urls_present() {
  let _lock = super::stage_listener_test_lock();
  let bookmarks = BookmarkStore::default();
  let link_url = "https://example.com/target";
  let image_url = "https://example.com/img.png";

  let entries = build_page_context_menu_entries(PageContextMenuBuildInput {
    link_url: Some(link_url),
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

  // Regression: when right-clicking an <img> inside an <a>, both the image URL and the link URL
  // are provided. We should surface actions for *both* URLs (not one-or-the-other).
  let has_any_image_action = actions.iter().any(|action| {
    matches!(
      action,
      PageContextMenuAction::OpenImageInNewTab(_)
        | PageContextMenuAction::DownloadImage(_)
        | PageContextMenuAction::CopyImageAddress(_)
    )
  });
  let has_any_link_action = actions.iter().any(|action| {
    matches!(
      action,
      PageContextMenuAction::OpenLinkInNewTab(_)
        | PageContextMenuAction::DownloadLink(_)
        | PageContextMenuAction::CopyLinkAddress(_)
    )
  });
  assert!(
    has_any_image_action && has_any_link_action,
    "expected both image and link actions to be present (got {actions:?})"
  );

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

  assert!(
    actions.contains(&PageContextMenuAction::OpenLinkInNewTab(link_url.to_string())),
    "expected OpenLinkInNewTab action (got {actions:?})"
  );
  assert!(
    actions.contains(&PageContextMenuAction::DownloadLink(link_url.to_string())),
    "expected DownloadLink action (got {actions:?})"
  );
  assert!(
    actions.contains(&PageContextMenuAction::CopyLinkAddress(link_url.to_string())),
    "expected CopyLinkAddress action (got {actions:?})"
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

#[test]
fn context_menu_entries_set_checked_state_for_panel_toggles() {
  let _lock = super::stage_listener_test_lock();
  let bookmarks = BookmarkStore::default();

  for (history_panel_open, bookmarks_panel_open) in [(true, false), (false, true)] {
    let entries = build_page_context_menu_entries(PageContextMenuBuildInput {
      link_url: None,
      image_url: None,
      page_url: Some("https://example.com/"),
      bookmarks: &bookmarks,
      history_panel_open,
      bookmarks_panel_open,
      can_copy: false,
      can_cut: false,
      can_paste: false,
      can_select_all: false,
    });

    assert_eq!(
      checked_state(&entries, PageContextMenuAction::ToggleHistoryPanel),
      Some(history_panel_open),
      "ToggleHistoryPanel checked state mismatch (history_panel_open={history_panel_open}, bookmarks_panel_open={bookmarks_panel_open})",
    );
    assert_eq!(
      checked_state(&entries, PageContextMenuAction::ToggleBookmarksPanel),
      Some(bookmarks_panel_open),
      "ToggleBookmarksPanel checked state mismatch (history_panel_open={history_panel_open}, bookmarks_panel_open={bookmarks_panel_open})",
    );
  }
}
