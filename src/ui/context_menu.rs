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

#[cfg(feature = "browser_ui")]
use crate::ui::BookmarkStore;

#[cfg(feature = "browser_ui")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageContextMenuAction {
  OpenLinkInNewTab(String),
  CopyLinkAddress(String),
  BookmarkLink(String),
  BookmarkPage(String),
  ToggleHistoryPanel,
  ToggleBookmarksPanel,
  Reload,
}

#[cfg(feature = "browser_ui")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageContextMenuItem {
  pub label: &'static str,
  pub action: PageContextMenuAction,
  pub checked: bool,
}

#[cfg(feature = "browser_ui")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageContextMenuEntry {
  Action(PageContextMenuItem),
  Separator,
}

#[cfg(feature = "browser_ui")]
#[derive(Debug, Clone, Copy)]
pub struct PageContextMenuBuildInput<'a> {
  pub link_url: Option<&'a str>,
  pub page_url: Option<&'a str>,
  pub bookmarks: &'a BookmarkStore,
  pub history_panel_open: bool,
  pub bookmarks_panel_open: bool,
}

#[cfg(feature = "browser_ui")]
pub fn build_page_context_menu_entries(
  input: PageContextMenuBuildInput<'_>,
) -> Vec<PageContextMenuEntry> {
  let mut out = Vec::new();

  if let Some(url) = input.link_url.map(str::trim).filter(|s| !s.is_empty()) {
    out.push(PageContextMenuEntry::Action(PageContextMenuItem {
      label: "Open Link in New Tab",
      action: PageContextMenuAction::OpenLinkInNewTab(url.to_string()),
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
      checked: input.bookmarks.contains(url),
    }));
    out.push(PageContextMenuEntry::Separator);
  }

  let page_url = input.page_url.map(str::trim).unwrap_or("");
  out.push(PageContextMenuEntry::Action(PageContextMenuItem {
    label: "Bookmark Page",
    action: PageContextMenuAction::BookmarkPage(page_url.to_string()),
    checked: !page_url.is_empty() && input.bookmarks.contains(page_url),
  }));

  out.push(PageContextMenuEntry::Action(PageContextMenuItem {
    label: "Show History",
    action: PageContextMenuAction::ToggleHistoryPanel,
    checked: input.history_panel_open,
  }));
  out.push(PageContextMenuEntry::Action(PageContextMenuItem {
    label: "Show Bookmarks",
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

#[cfg(feature = "browser_ui")]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ApplyPageContextMenuActionResult {
  pub bookmarks_changed: bool,
  pub ui_changed: bool,
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
      bookmarks.toggle_url(url);
      ApplyPageContextMenuActionResult {
        bookmarks_changed: true,
        ui_changed: false,
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
      }
    }
    PageContextMenuAction::OpenLinkInNewTab(_)
    | PageContextMenuAction::CopyLinkAddress(_)
    | PageContextMenuAction::Reload => ApplyPageContextMenuActionResult::default(),
  }
}
