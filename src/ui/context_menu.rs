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

