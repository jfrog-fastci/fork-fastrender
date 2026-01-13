//! Layout helpers for compositor-rendered browser chrome.
//!
//! Unlike the egui backend (which relies on egui panel layout), a compositor backend typically
//! computes explicit rectangles for the top chrome region and then renders hit-testable chrome
//! content (tabs/address bar) into that region.
//!
//! On macOS, when the window uses `fullsize_content_view` / a transparent titlebar (unified toolbar
//! look), the native traffic-light window controls occupy the top-left of the titlebar. To avoid
//! placing chrome content underneath them, we reserve a left inset for chrome *content* (not
//! necessarily the chrome background).

use crate::geometry::{Point, Rect};

/// Computed rectangles for compositor-rendered top chrome.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChromeCompositorLayout {
  /// Full rect of the top chrome region (background).
  pub chrome_rect: Rect,
  /// Rect used for hit-testable chrome content (tabs/address bar), potentially inset on macOS.
  pub chrome_content_rect: Rect,
}

impl ChromeCompositorLayout {
  /// Create a layout for a given top chrome region.
  ///
  /// On macOS, `chrome_content_rect` is shifted right to avoid the native traffic lights.
  pub fn new(chrome_rect: Rect) -> Self {
    let chrome_content_rect = super::titlebar_insets::inset_top_chrome_content_rect(chrome_rect);
    Self {
      chrome_rect,
      chrome_content_rect,
    }
  }

  /// Returns `true` when `point` should be routed to the chrome UI.
  ///
  /// On macOS, this excludes the traffic-light inset region so clicks on the native window controls
  /// are not consumed by chrome hit testing.
  pub fn hit_test_chrome(self, point: Point) -> bool {
    self.chrome_content_rect.contains_point(point)
  }
}

