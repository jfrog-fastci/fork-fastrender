use crate::{Point, Rect, Size};

/// Place a popup menu within the given `bounds`, anchored to a click/hover `anchor` point.
///
/// The placement algorithm is tuned for "browser-like" context menus:
/// - Prefer placing the menu down/right from the anchor (so the cursor stays near the top-left).
/// - When near the right/bottom edges, "flip" left/up so the menu stays on-screen.
/// - Finally clamp to the bounds with a `margin` so the popup never touches the window edge.
///
/// All coordinates are in an arbitrary, consistent coordinate space (egui points in the browser UI).
pub fn place_menu(anchor: Point, menu_size: Size, bounds: Rect, margin: f32) -> Point {
  let margin = margin.max(0.0);
  let min_x = bounds.min_x() + margin;
  let min_y = bounds.min_y() + margin;
  let max_x = bounds.max_x() - margin;
  let max_y = bounds.max_y() - margin;

  let mut origin_x = anchor.x;
  let mut origin_y = anchor.y;

  // Prefer opening down+right, but flip if we'd overflow.
  if origin_x + menu_size.width > max_x {
    origin_x -= menu_size.width;
  }
  if origin_y + menu_size.height > max_y {
    origin_y -= menu_size.height;
  }

  // Clamp so the entire menu stays visible. If the menu is larger than the available bounds,
  // pin it to the top-left margin instead of panicking (f32::clamp requires min <= max).
  let max_origin_x = max_x - menu_size.width;
  let max_origin_y = max_y - menu_size.height;

  let origin_x = if max_origin_x >= min_x {
    origin_x.clamp(min_x, max_origin_x)
  } else {
    min_x
  };

  let origin_y = if max_origin_y >= min_y {
    origin_y.clamp(min_y, max_origin_y)
  } else {
    min_y
  };

  Point::new(origin_x, origin_y)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn place_menu_prefers_down_right_when_fits() {
    let bounds = Rect::from_xywh(0.0, 0.0, 200.0, 200.0);
    let menu_size = Size::new(50.0, 40.0);
    let anchor = Point::new(20.0, 30.0);
    let placed = place_menu(anchor, menu_size, bounds, 4.0);
    assert_eq!(placed, anchor);
  }

  #[test]
  fn place_menu_flips_left_when_overflowing_right_edge() {
    let bounds = Rect::from_xywh(0.0, 0.0, 100.0, 100.0);
    let menu_size = Size::new(30.0, 10.0);
    let anchor = Point::new(90.0, 30.0);
    let placed = place_menu(anchor, menu_size, bounds, 4.0);
    assert_eq!(placed, Point::new(60.0, 30.0));
  }

  #[test]
  fn place_menu_flips_up_when_overflowing_bottom_edge() {
    let bounds = Rect::from_xywh(0.0, 0.0, 100.0, 100.0);
    let menu_size = Size::new(10.0, 30.0);
    let anchor = Point::new(20.0, 90.0);
    let placed = place_menu(anchor, menu_size, bounds, 4.0);
    assert_eq!(placed, Point::new(20.0, 60.0));
  }

  #[test]
  fn place_menu_flips_both_axes_when_overflowing_bottom_right_corner() {
    let bounds = Rect::from_xywh(0.0, 0.0, 100.0, 100.0);
    let menu_size = Size::new(30.0, 30.0);
    let anchor = Point::new(90.0, 90.0);
    let placed = place_menu(anchor, menu_size, bounds, 4.0);
    assert_eq!(placed, Point::new(60.0, 60.0));
  }

  #[test]
  fn place_menu_handles_menu_larger_than_bounds() {
    let bounds = Rect::from_xywh(0.0, 0.0, 100.0, 100.0);
    let menu_size = Size::new(200.0, 200.0);
    let anchor = Point::new(50.0, 50.0);
    let placed = place_menu(anchor, menu_size, bounds, 4.0);
    assert_eq!(placed, Point::new(4.0, 4.0));
  }
}

// -----------------------------------------------------------------------------
// Page context menu actions (windowed browser UI)
// -----------------------------------------------------------------------------

#[cfg(any(test, feature = "browser_ui"))]
use crate::ui::BookmarkStore;

#[cfg(any(test, feature = "browser_ui"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageContextMenuAction {
  CopySelection,
  Cut,
  Paste,
  SelectAll,
  OpenImageInNewTab(String),
  DownloadImage(String),
  CopyImageAddress(String),
  OpenLinkInNewTab(String),
  DownloadLink(String),
  CopyLinkAddress(String),
  BookmarkLink(String),
  BookmarkPage(String),
  ToggleHistoryPanel,
  ToggleBookmarksPanel,
  Reload,
}

/// Returns the accessibility label for a page context menu action.
///
/// Screen readers do not always announce menu-item check marks, so for "toggle" actions we include
/// the current checked state in the a11y label.
#[cfg(any(test, feature = "browser_ui"))]
pub fn format_page_context_menu_a11y_label(
  base_label: &str,
  action: &PageContextMenuAction,
  checked: bool,
) -> String {
  match action {
    // Toggle panel visibility. The visible label already reflects the action ("Show/Hide ..."), so
    // reuse it for the a11y label.
    PageContextMenuAction::ToggleHistoryPanel | PageContextMenuAction::ToggleBookmarksPanel => {
      base_label.to_string()
    }
    PageContextMenuAction::BookmarkLink(_) => {
      if checked { "Bookmark link: on" } else { "Bookmark link: off" }.to_string()
    }
    PageContextMenuAction::BookmarkPage(_) => {
      if checked { "Bookmark page: on" } else { "Bookmark page: off" }.to_string()
    }
    _ => base_label.to_string(),
  }
}

#[cfg(any(test, feature = "browser_ui"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageContextMenuItem {
  pub label: &'static str,
  pub action: PageContextMenuAction,
  pub checked: bool,
}

#[cfg(any(test, feature = "browser_ui"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageContextMenuEntry {
  Action(PageContextMenuItem),
  Separator,
}

#[cfg(any(test, feature = "browser_ui"))]
#[derive(Debug, Clone, Copy)]
pub struct PageContextMenuBuildInput<'a> {
  pub link_url: Option<&'a str>,
  pub image_url: Option<&'a str>,
  pub page_url: Option<&'a str>,
  pub bookmarks: &'a BookmarkStore,
  pub history_panel_open: bool,
  pub bookmarks_panel_open: bool,
  pub can_copy: bool,
  pub can_cut: bool,
  pub can_paste: bool,
  pub can_select_all: bool,
}

#[cfg(any(test, feature = "browser_ui"))]
pub fn build_page_context_menu_entries(
  input: PageContextMenuBuildInput<'_>,
) -> Vec<PageContextMenuEntry> {
  let mut out = Vec::new();

  let push_separator = |out: &mut Vec<PageContextMenuEntry>| {
    if out.is_empty() {
      return;
    }
    if matches!(out.last(), Some(PageContextMenuEntry::Separator)) {
      return;
    }
    out.push(PageContextMenuEntry::Separator);
  };

  // Clipboard/editing actions (context-sensitive).
  if input.can_copy {
    out.push(PageContextMenuEntry::Action(PageContextMenuItem {
      label: "Copy",
      action: PageContextMenuAction::CopySelection,
      checked: false,
    }));
  }
  if input.can_cut {
    out.push(PageContextMenuEntry::Action(PageContextMenuItem {
      label: "Cut",
      action: PageContextMenuAction::Cut,
      checked: false,
    }));
  }
  if input.can_paste {
    out.push(PageContextMenuEntry::Action(PageContextMenuItem {
      label: "Paste",
      action: PageContextMenuAction::Paste,
      checked: false,
    }));
  }
  if input.can_select_all {
    out.push(PageContextMenuEntry::Action(PageContextMenuItem {
      label: "Select All",
      action: PageContextMenuAction::SelectAll,
      checked: false,
    }));
  }
  push_separator(&mut out);

  if let Some(url) = input.image_url.map(str::trim).filter(|s| !s.is_empty()) {
    out.push(PageContextMenuEntry::Action(PageContextMenuItem {
      label: "Open Image in New Tab",
      action: PageContextMenuAction::OpenImageInNewTab(url.to_string()),
      checked: false,
    }));
    out.push(PageContextMenuEntry::Action(PageContextMenuItem {
      label: "Download Image",
      action: PageContextMenuAction::DownloadImage(url.to_string()),
      checked: false,
    }));
    out.push(PageContextMenuEntry::Action(PageContextMenuItem {
      label: "Copy Image Address",
      action: PageContextMenuAction::CopyImageAddress(url.to_string()),
      checked: false,
    }));
    push_separator(&mut out);
  }

  if let Some(url) = input.link_url.map(str::trim).filter(|s| !s.is_empty()) {
    out.push(PageContextMenuEntry::Action(PageContextMenuItem {
      label: "Open Link in New Tab",
      action: PageContextMenuAction::OpenLinkInNewTab(url.to_string()),
      checked: false,
    }));
    out.push(PageContextMenuEntry::Action(PageContextMenuItem {
      label: "Download Link",
      action: PageContextMenuAction::DownloadLink(url.to_string()),
      checked: false,
    }));
    out.push(PageContextMenuEntry::Action(PageContextMenuItem {
      label: "Copy Link Address",
      action: PageContextMenuAction::CopyLinkAddress(url.to_string()),
      checked: false,
    }));
    out.push(PageContextMenuEntry::Action(PageContextMenuItem {
      label: "Bookmark Link",
      action: PageContextMenuAction::BookmarkLink(url.to_string()),
      checked: input.bookmarks.contains_url(url),
    }));
    push_separator(&mut out);
  }

  let page_url = input.page_url.map(str::trim).unwrap_or("");
  out.push(PageContextMenuEntry::Action(PageContextMenuItem {
    label: "Bookmark Page",
    action: PageContextMenuAction::BookmarkPage(page_url.to_string()),
    checked: !page_url.is_empty() && input.bookmarks.contains_url(page_url),
  }));

  let history_toggle_label = if input.history_panel_open {
    "Hide History"
  } else {
    "Show History"
  };
  out.push(PageContextMenuEntry::Action(PageContextMenuItem {
    label: history_toggle_label,
    action: PageContextMenuAction::ToggleHistoryPanel,
    checked: input.history_panel_open,
  }));

  let bookmarks_toggle_label = if input.bookmarks_panel_open {
    "Hide Bookmarks"
  } else {
    "Show Bookmarks"
  };
  out.push(PageContextMenuEntry::Action(PageContextMenuItem {
    label: bookmarks_toggle_label,
    action: PageContextMenuAction::ToggleBookmarksPanel,
    checked: input.bookmarks_panel_open,
  }));
  out.push(PageContextMenuEntry::Separator);

  out.push(PageContextMenuEntry::Action(PageContextMenuItem {
    label: "Reload",
    action: PageContextMenuAction::Reload,
    checked: false,
  }));

  out
}

#[cfg(test)]
mod a11y_label_tests {
  use super::*;

  #[test]
  fn format_page_context_menu_a11y_label_echoes_toggle_label() {
    assert_eq!(
      format_page_context_menu_a11y_label(
        "Show History",
        &PageContextMenuAction::ToggleHistoryPanel,
        true
      ),
      "Show History"
    );
    assert_eq!(
      format_page_context_menu_a11y_label(
        "Hide History",
        &PageContextMenuAction::ToggleHistoryPanel,
        false
      ),
      "Hide History"
    );

    assert_eq!(
      format_page_context_menu_a11y_label(
        "Show Bookmarks",
        &PageContextMenuAction::ToggleBookmarksPanel,
        true
      ),
      "Show Bookmarks"
    );
    assert_eq!(
      format_page_context_menu_a11y_label(
        "Hide Bookmarks",
        &PageContextMenuAction::ToggleBookmarksPanel,
        false
      ),
      "Hide Bookmarks"
    );
  }

  #[test]
  fn format_page_context_menu_a11y_label_includes_bookmark_state() {
    assert_eq!(
      format_page_context_menu_a11y_label(
        "Bookmark Page",
        &PageContextMenuAction::BookmarkPage("https://example.com".into()),
        true
      ),
      "Bookmark page: on"
    );
    assert_eq!(
      format_page_context_menu_a11y_label(
        "Bookmark Page",
        &PageContextMenuAction::BookmarkPage("https://example.com".into()),
        false
      ),
      "Bookmark page: off"
    );

    assert_eq!(
      format_page_context_menu_a11y_label(
        "Bookmark Link",
        &PageContextMenuAction::BookmarkLink("https://example.com".into()),
        true
      ),
      "Bookmark link: on"
    );
    assert_eq!(
      format_page_context_menu_a11y_label(
        "Bookmark Link",
        &PageContextMenuAction::BookmarkLink("https://example.com".into()),
        false
      ),
      "Bookmark link: off"
    );
  }

  #[test]
  fn format_page_context_menu_a11y_label_preserves_non_toggle_labels() {
    assert_eq!(
      format_page_context_menu_a11y_label("Reload", &PageContextMenuAction::Reload, false),
      "Reload"
    );
    assert_eq!(
      format_page_context_menu_a11y_label(
        "Copy Link Address",
        &PageContextMenuAction::CopyLinkAddress("https://example.com".into()),
        false
      ),
      "Copy Link Address"
    );
  }
}

#[cfg(test)]
mod entry_label_tests {
  use super::*;

  #[test]
  fn build_page_context_menu_entries_toggle_labels_reflect_open_state() {
    let bookmarks = BookmarkStore::default();
    let base = PageContextMenuBuildInput {
      link_url: None,
      image_url: None,
      page_url: Some("https://example.com/"),
      bookmarks: &bookmarks,
      history_panel_open: false,
      bookmarks_panel_open: false,
      can_copy: false,
      can_cut: false,
      can_paste: false,
      can_select_all: false,
    };

    let entries = build_page_context_menu_entries(base);
    let history_label = entries.iter().find_map(|entry| match entry {
      PageContextMenuEntry::Action(item)
        if matches!(item.action, PageContextMenuAction::ToggleHistoryPanel) =>
      {
        Some(item.label)
      }
      _ => None,
    });
    assert_eq!(history_label, Some("Show History"));

    let bookmarks_label = entries.iter().find_map(|entry| match entry {
      PageContextMenuEntry::Action(item)
        if matches!(item.action, PageContextMenuAction::ToggleBookmarksPanel) =>
      {
        Some(item.label)
      }
      _ => None,
    });
    assert_eq!(bookmarks_label, Some("Show Bookmarks"));

    let entries = build_page_context_menu_entries(PageContextMenuBuildInput {
      history_panel_open: true,
      bookmarks_panel_open: true,
      ..base
    });
    let history_label = entries.iter().find_map(|entry| match entry {
      PageContextMenuEntry::Action(item)
        if matches!(item.action, PageContextMenuAction::ToggleHistoryPanel) =>
      {
        Some(item.label)
      }
      _ => None,
    });
    assert_eq!(history_label, Some("Hide History"));

    let bookmarks_label = entries.iter().find_map(|entry| match entry {
      PageContextMenuEntry::Action(item)
        if matches!(item.action, PageContextMenuAction::ToggleBookmarksPanel) =>
      {
        Some(item.label)
      }
      _ => None,
    });
    assert_eq!(bookmarks_label, Some("Hide Bookmarks"));
  }
}

#[cfg(feature = "browser_ui")]
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ApplyPageContextMenuActionResult {
  pub bookmarks_changed: bool,
  pub ui_changed: bool,
  pub bookmark_deltas: Vec<super::BookmarkDelta>,
}

#[cfg(feature = "browser_ui")]
pub fn apply_page_context_menu_action(
  bookmarks: &mut BookmarkStore,
  history_panel_open: &mut bool,
  bookmarks_panel_open: &mut bool,
  action: &PageContextMenuAction,
) -> ApplyPageContextMenuActionResult {
  match action {
    PageContextMenuAction::BookmarkLink(url) | PageContextMenuAction::BookmarkPage(url) => {
      let url = url.trim();
      if url.is_empty() {
        return ApplyPageContextMenuActionResult::default();
      }
      let mut deltas = Vec::new();
      let _after = bookmarks.toggle_with_deltas(url, None, &mut deltas);
      ApplyPageContextMenuActionResult {
        bookmarks_changed: !deltas.is_empty(),
        ui_changed: false,
        bookmark_deltas: deltas,
      }
    }
    PageContextMenuAction::ToggleHistoryPanel => {
      let prev_history = *history_panel_open;
      let prev_bookmarks = *bookmarks_panel_open;
      *history_panel_open = !*history_panel_open;
      if *history_panel_open {
        *bookmarks_panel_open = false;
      }
      ApplyPageContextMenuActionResult {
        bookmarks_changed: false,
        ui_changed: prev_history != *history_panel_open || prev_bookmarks != *bookmarks_panel_open,
        bookmark_deltas: Vec::new(),
      }
    }
    PageContextMenuAction::ToggleBookmarksPanel => {
      let prev_history = *history_panel_open;
      let prev_bookmarks = *bookmarks_panel_open;
      *bookmarks_panel_open = !*bookmarks_panel_open;
      if *bookmarks_panel_open {
        *history_panel_open = false;
      }
      ApplyPageContextMenuActionResult {
        bookmarks_changed: false,
        ui_changed: prev_history != *history_panel_open || prev_bookmarks != *bookmarks_panel_open,
        bookmark_deltas: Vec::new(),
      }
    }
    PageContextMenuAction::OpenLinkInNewTab(_)
    | PageContextMenuAction::DownloadLink(_)
    | PageContextMenuAction::CopySelection
    | PageContextMenuAction::Cut
    | PageContextMenuAction::Paste
    | PageContextMenuAction::SelectAll
    | PageContextMenuAction::OpenImageInNewTab(_)
    | PageContextMenuAction::DownloadImage(_)
    | PageContextMenuAction::CopyImageAddress(_)
    | PageContextMenuAction::CopyLinkAddress(_)
    | PageContextMenuAction::Reload => ApplyPageContextMenuActionResult::default(),
  }
}
