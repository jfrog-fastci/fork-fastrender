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

#[test]
fn context_menu_entries_order_image_group_before_link_group_when_both_present() {
  let _lock = super::stage_listener_test_lock();
  let bookmarks = BookmarkStore::default();
  let image_url = "https://example.com/img.png";
  let link_url = "https://example.com/target";

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

  let find_action = |predicate: fn(&PageContextMenuAction) -> bool| {
    entries.iter().position(|entry| match entry {
      PageContextMenuEntry::Action(item) => predicate(&item.action),
      PageContextMenuEntry::Separator => false,
    })
  };

  let open_image_idx = find_action(|action| match action {
    PageContextMenuAction::OpenImageInNewTab(url) => url == image_url,
    _ => false,
  })
  .expect("expected OpenImageInNewTab for image_url to be present");
  let download_image_idx = find_action(|action| match action {
    PageContextMenuAction::DownloadImage(url) => url == image_url,
    _ => false,
  })
  .expect("expected DownloadImage for image_url to be present");
  let copy_image_idx = find_action(|action| match action {
    PageContextMenuAction::CopyImageAddress(url) => url == image_url,
    _ => false,
  })
  .expect("expected CopyImageAddress for image_url to be present");

  let open_link_idx = find_action(|action| match action {
    PageContextMenuAction::OpenLinkInNewTab(url) => url == link_url,
    _ => false,
  })
  .expect("expected OpenLinkInNewTab for link_url to be present");

  assert!(
    open_image_idx < open_link_idx,
    "expected image actions to appear before link actions (entries: {entries:?})"
  );

  // Image action group is expected to be contiguous and ordered before the link action group.
  assert!(
    open_image_idx < download_image_idx && download_image_idx < copy_image_idx,
    "expected image actions to be ordered Open -> Download -> Copy (entries: {entries:?})"
  );

  // Ensure there is exactly one separator between the image group and the link group. This catches
  // accidental double-separator insertion when both image+link URLs are present.
  assert_eq!(
    entries.get(copy_image_idx + 1),
    Some(&PageContextMenuEntry::Separator),
    "expected a separator after the last image action (entries: {entries:?})"
  );
  assert_eq!(
    open_link_idx,
    copy_image_idx + 2,
    "expected exactly one separator between the image and link groups (entries: {entries:?})"
  );

  // Menu should always end with Reload (never a trailing separator).
  assert!(
    matches!(
      entries.last(),
      Some(PageContextMenuEntry::Action(item))
        if matches!(&item.action, PageContextMenuAction::Reload)
    ),
    "expected context menu to end with Reload action (entries: {entries:?})"
  );
}
