use crate::geometry::{Point, Rect};

/// Approximate left inset (in logical "points") occupied by macOS traffic-light window controls.
///
/// When using a transparent titlebar + full-size content view (`fullsize_content_view`), browser
/// chrome is typically drawn into the titlebar area (unified toolbar style). The system traffic
/// lights (close/minimize/zoom) live in the top-left of that same region, so we must avoid placing
/// interactive chrome content (tabs, address bar) directly underneath them.
///
/// This value is intentionally approximate (3 × ~12px buttons + padding) and only needs to be large
/// enough to keep controls out from under the traffic lights.
pub const MACOS_TRAFFIC_LIGHTS_LEFT_INSET_POINTS: f32 = 72.0;

#[inline]
fn inset_left(rect: Rect, inset_points: f32) -> Rect {
  if !inset_points.is_finite() || inset_points <= 0.0 {
    return rect;
  }
  let inset = inset_points.min(rect.width().max(0.0));
  Rect::from_xywh(
    rect.x() + inset,
    rect.y(),
    (rect.width() - inset).max(0.0),
    rect.height(),
  )
}

/// Returns the left inset (in logical points) to reserve for macOS traffic lights.
///
/// - On macOS: [`MACOS_TRAFFIC_LIGHTS_LEFT_INSET_POINTS`]
/// - On other platforms: `0.0`
#[inline]
pub fn traffic_lights_left_inset_points() -> f32 {
  #[cfg(target_os = "macos")]
  {
    MACOS_TRAFFIC_LIGHTS_LEFT_INSET_POINTS
  }
  #[cfg(not(target_os = "macos"))]
  {
    0.0
  }
}

/// Returns the *content* rect for the top chrome region, inset on macOS to avoid traffic lights.
///
/// This is intended for compositor-style browser chrome layout where the chrome background may
/// still span `chrome_rect`, but interactive chrome content (tabs/address bar) should be placed
/// inside this returned rect.
#[inline]
pub fn inset_top_chrome_content_rect(chrome_rect: Rect) -> Rect {
  inset_left(chrome_rect, traffic_lights_left_inset_points())
}

/// Returns `true` if `point` should be considered inside interactive chrome content.
///
/// Compositor backends should use this (or `inset_top_chrome_content_rect`) for chrome hit-testing
/// so clicks on macOS traffic lights are not consumed by the chrome UI.
#[inline]
pub fn hit_test_top_chrome_content(chrome_rect: Rect, point: Point) -> bool {
  inset_top_chrome_content_rect(chrome_rect).contains_point(point)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn inset_left_clamps_to_rect_width() {
    let rect = Rect::from_xywh(0.0, 0.0, 50.0, 10.0);
    let inset = inset_left(rect, 72.0);
    assert_eq!(inset.x(), 50.0);
    assert_eq!(inset.width(), 0.0);
  }

  #[test]
  fn inset_left_shifts_origin_and_reduces_width() {
    let rect = Rect::from_xywh(0.0, 0.0, 200.0, 40.0);
    let inset = inset_left(rect, 72.0);
    assert_eq!(inset.x(), 72.0);
    assert_eq!(inset.width(), 128.0);
    assert_eq!(inset.height(), 40.0);
  }

  #[test]
  fn hit_test_excludes_left_inset_region() {
    let chrome = Rect::from_xywh(0.0, 0.0, 200.0, 40.0);
    let content = inset_left(chrome, 72.0);
    assert!(!content.contains_point(Point::new(10.0, 10.0)));
    assert!(content.contains_point(Point::new(80.0, 10.0)));
  }
}

