use egui::{Pos2, Rect, Vec2};

/// Convert `winit`/`egui` "line" scroll deltas into CSS pixel deltas.
///
/// `winit` reports some wheel devices (classic mouse wheels) in "lines" rather than pixels.
/// Different platforms use different values for what a "line" means; for browsers it's typically
/// in the ~30-60px range. We pick a single constant so behaviour is predictable and easy to tune.
pub const CSS_PX_PER_WHEEL_LINE: f32 = 40.0;

/// Wheel delta as reported by OS/UI frameworks.
///
/// - `Lines` corresponds to `winit::event::MouseScrollDelta::LineDelta` and
///   `egui::MouseWheelUnit::Line`.
/// - `Points` corresponds to pixel/trackpad deltas expressed in **egui points** (logical pixels).
/// - `Pages` corresponds to `egui::MouseWheelUnit::Page` (rare; usually from keyboard scroll
///   shortcuts); it is interpreted as a multiple of the viewport size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WheelDelta {
  Lines(Vec2),
  Points(Vec2),
  Pages(Vec2),
}

impl WheelDelta {
  /// Construct from an egui mouse wheel event.
  pub fn from_egui(unit: egui::MouseWheelUnit, delta: Vec2) -> Self {
    match unit {
      egui::MouseWheelUnit::Line => Self::Lines(delta),
      egui::MouseWheelUnit::Point => Self::Points(delta),
      egui::MouseWheelUnit::Page => Self::Pages(delta),
    }
  }

  /// Construct from a winit mouse wheel event.
  ///
  /// `pixels_per_point` should match the egui `Context::pixels_per_point()` used for the frame.
  /// It is used to convert `PixelDelta` (physical pixels) into egui points.
  pub fn from_winit(delta: winit::event::MouseScrollDelta, pixels_per_point: f32) -> Self {
    match delta {
      winit::event::MouseScrollDelta::LineDelta(x, y) => Self::Lines(Vec2::new(x, y)),
      winit::event::MouseScrollDelta::PixelDelta(pos) => {
        // `winit` uses physical pixels for `PixelDelta`; egui works in logical "points".
        // Protect against a bogus/zero ppp to avoid NaNs.
        let ppp = pixels_per_point.max(1e-6);
        Self::Points(Vec2::new(pos.x as f32 / ppp, pos.y as f32 / ppp))
      }
    }
  }
}

#[doc(inline)]
pub use super::remap_wheel_delta_for_shift;

/// Maps egui input coordinates (points) into page-space CSS pixels.
///
/// The UI draws a page pixmap (physical px) into an `egui::Rect` (points). The render worker,
/// however, speaks in **viewport CSS pixels** (`viewport_css`). If the image is drawn scaled (fit
/// to panel, DPI mismatch, zoom, etc.), we must scale pointer positions and pixel-based wheel
/// deltas accordingly.
#[derive(Debug, Clone, Copy)]
pub struct InputMapping {
  /// Where the page image is drawn, in egui points.
  pub image_rect_points: Rect,
  /// Viewport size in CSS pixels.
  pub viewport_css: (u32, u32),
}

impl InputMapping {
  pub fn new(image_rect_points: Rect, viewport_css: (u32, u32)) -> Self {
    Self {
      image_rect_points,
      viewport_css,
    }
  }

  fn viewport_css_f32(&self) -> Vec2 {
    Vec2::new(self.viewport_css.0 as f32, self.viewport_css.1 as f32)
  }

  fn css_per_point(&self) -> Option<Vec2> {
    let drawn_points = self.image_rect_points.size();
    if drawn_points.x <= 0.0 || drawn_points.y <= 0.0 {
      return None;
    }

    let viewport_css = self.viewport_css_f32();
    Some(Vec2::new(
      viewport_css.x / drawn_points.x,
      viewport_css.y / drawn_points.y,
    ))
  }

  /// Convert a pointer position (egui points) to a position in viewport CSS pixels.
  ///
  /// This applies a scale factor based on how large the page image was drawn:
  ///
  /// `pos_css = (pos_points - image_rect.min) * (viewport_css / image_drawn_points)`
  ///
  /// The returned coordinate is clamped to the viewport bounds for hit-testing.
  pub fn pos_points_to_pos_css_clamped(&self, pos_points: Pos2) -> Option<(f32, f32)> {
    let css_per_point = self.css_per_point()?;

    let local_points = pos_points - self.image_rect_points.min;
    let mut pos_css = Vec2::new(
      local_points.x * css_per_point.x,
      local_points.y * css_per_point.y,
    );

    let viewport_css = self.viewport_css_f32();
    pos_css.x = pos_css.x.clamp(0.0, viewport_css.x);
    pos_css.y = pos_css.y.clamp(0.0, viewport_css.y);

    Some((pos_css.x, pos_css.y))
  }

  /// Like [`Self::pos_points_to_pos_css_clamped`], but returns `None` when `pos_points` lies outside
  /// the drawn page image.
  ///
  /// This is useful when front-ends want to treat the page image bounds as a strict hit-test region
  /// (e.g. sending a hover update only when the cursor is inside the page).
  pub fn pos_points_to_pos_css_if_inside(&self, pos_points: Pos2) -> Option<(f32, f32)> {
    if !self.image_rect_points.contains(pos_points) {
      return None;
    }
    self.pos_points_to_pos_css_clamped(pos_points)
  }

  /// Convert a position in viewport CSS pixels to an egui position (points).
  ///
  pub fn pos_css_to_pos_points(&self, pos_css: (f32, f32)) -> Option<Pos2> {
    if !pos_css.0.is_finite() || !pos_css.1.is_finite() {
      return None;
    }

    let css_per_point = self.css_per_point()?;
    // Protect against division by 0 when the viewport is degenerate.
    if css_per_point.x <= 0.0 || css_per_point.y <= 0.0 {
      return None;
    }

    let local_points = Vec2::new(pos_css.0 / css_per_point.x, pos_css.1 / css_per_point.y);
    if !local_points.x.is_finite() || !local_points.y.is_finite() {
      return None;
    }
    Some(self.image_rect_points.min + local_points)
  }

  /// Convert a position in viewport CSS pixels to an egui position (points), clamping `pos_css` to
  /// the viewport bounds.
  ///
  /// This is the inverse of [`Self::pos_points_to_pos_css_clamped`].
  pub fn pos_css_to_pos_points_clamped(&self, pos_css: (f32, f32)) -> Option<Pos2> {
    if !pos_css.0.is_finite() || !pos_css.1.is_finite() {
      return None;
    }
    let viewport_css = self.viewport_css_f32();
    let css = (
      pos_css.0.clamp(0.0, viewport_css.x),
      pos_css.1.clamp(0.0, viewport_css.y),
    );
    self.pos_css_to_pos_points(css)
  }

  /// Convert a viewport-local CSS rect to an egui rect in points.
  ///
  /// This is useful for positioning native UI overlays (e.g. `<select>` dropdown popups) relative
  /// to an element's viewport-local layout rect.
  pub fn rect_css_to_rect_points(&self, rect_css: crate::geometry::Rect) -> Option<Rect> {
    let min = self.pos_css_to_pos_points((rect_css.min_x(), rect_css.min_y()))?;
    let max = self.pos_css_to_pos_points((rect_css.max_x(), rect_css.max_y()))?;
    Some(Rect::from_min_max(min, max))
  }

  /// Convert a viewport-local CSS rect to an egui rect in points, clamping it to the viewport
  /// bounds.
  pub fn rect_css_to_rect_points_clamped(&self, rect_css: crate::geometry::Rect) -> Option<Rect> {
    let min = self.pos_css_to_pos_points_clamped((rect_css.min_x(), rect_css.min_y()))?;
    let max = self.pos_css_to_pos_points_clamped((rect_css.max_x(), rect_css.max_y()))?;
    Some(Rect::from_min_max(min, max))
  }

  /// Convert an egui rect (points) into the coordinate space used by egui's emitted AccessKit
  /// bounds.
  ///
  /// This helper exists for the browser UI's "rendered page image" accessibility injection: when
  /// we generate AccessKit nodes for page content, their bounds must match egui's own nodes so that
  /// screen-reader highlights align.
  pub fn rect_points_to_accesskit_rect(
    rect_points: Rect,
    pixels_per_point: f32,
  ) -> accesskit::Rect {
    crate::ui::accesskit_bounds::accesskit_rect_from_egui_rect(rect_points, pixels_per_point)
  }

  /// Convert a viewport-local CSS rect into an AccessKit rect in the coordinate system used by
  /// egui's emitted AccessKit nodes.
  pub fn rect_css_to_accesskit_rect(
    &self,
    rect_css: crate::geometry::Rect,
    pixels_per_point: f32,
  ) -> Option<accesskit::Rect> {
    let rect_points = self.rect_css_to_rect_points(rect_css)?;
    Some(Self::rect_points_to_accesskit_rect(rect_points, pixels_per_point))
  }

  /// Convert a wheel delta (from egui/winit) to a delta in viewport CSS pixels.
  ///
  /// Sign convention:
  /// - The output uses "document scroll" semantics: scrolling down increases `scroll_y`.
  /// - `winit`/`egui` wheel deltas use the opposite convention (positive is typically up/left),
  ///   so we negate the delta to match the CSS/DOM convention.
  pub fn wheel_delta_to_delta_css(&self, delta: WheelDelta) -> Option<(f32, f32)> {
    match delta {
      WheelDelta::Lines(lines) => {
        let delta_css = Vec2::new(
          lines.x * CSS_PX_PER_WHEEL_LINE,
          lines.y * CSS_PX_PER_WHEEL_LINE,
        );
        Some((-delta_css.x, -delta_css.y))
      }
      WheelDelta::Points(points) => {
        let css_per_point = self.css_per_point()?;
        let delta_css = Vec2::new(points.x * css_per_point.x, points.y * css_per_point.y);
        Some((-delta_css.x, -delta_css.y))
      }
      WheelDelta::Pages(pages) => {
        let viewport_css = self.viewport_css_f32();
        let delta_css = Vec2::new(pages.x * viewport_css.x, pages.y * viewport_css.y);
        Some((-delta_css.x, -delta_css.y))
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn assert_approx2(actual: (f32, f32), expected: (f32, f32)) {
    let eps = 1e-4;
    assert!(
      (actual.0 - expected.0).abs() <= eps,
      "x mismatch: actual={} expected={}",
      actual.0,
      expected.0
    );
    assert!(
      (actual.1 - expected.1).abs() <= eps,
      "y mismatch: actual={} expected={}",
      actual.1,
      expected.1
    );
  }

  fn assert_approx_pos2(actual: Pos2, expected: Pos2) {
    let eps = 1e-4;
    assert!(
      (actual.x - expected.x).abs() <= eps,
      "x mismatch: actual={} expected={}",
      actual.x,
      expected.x
    );
    assert!(
      (actual.y - expected.y).abs() <= eps,
      "y mismatch: actual={} expected={}",
      actual.y,
      expected.y
    );
  }

  #[test]
  fn identity_mapping_at_1_to_1_draw() {
    let image_rect = Rect::from_min_size(Pos2::new(10.0, 20.0), Vec2::new(800.0, 600.0));
    let mapping = InputMapping::new(image_rect, (800, 600));

    let pos = mapping
      .pos_points_to_pos_css_clamped(Pos2::new(110.0, 70.0))
      .unwrap();
    assert_approx2(pos, (100.0, 50.0));
  }

  #[test]
  fn identity_mapping_points_from_css() {
    let image_rect = Rect::from_min_size(Pos2::new(10.0, 20.0), Vec2::new(800.0, 600.0));
    let mapping = InputMapping::new(image_rect, (800, 600));

    let pos = mapping.pos_css_to_pos_points((100.0, 50.0)).unwrap();
    assert_approx_pos2(pos, Pos2::new(110.0, 70.0));
  }

  #[test]
  fn scaled_draw_maps_points_into_css_space() {
    // Viewport is 800x600 CSS px, but the image is drawn at half size (400x300 points).
    let image_rect = Rect::from_min_size(Pos2::new(0.0, 0.0), Vec2::new(400.0, 300.0));
    let mapping = InputMapping::new(image_rect, (800, 600));

    let pos = mapping
      .pos_points_to_pos_css_clamped(Pos2::new(200.0, 150.0))
      .unwrap();
    assert_approx2(pos, (400.0, 300.0));
  }

  #[test]
  fn scaled_draw_maps_css_into_points_space() {
    // Viewport is 800x600 CSS px, but the image is drawn at half size (400x300 points).
    let image_rect = Rect::from_min_size(Pos2::new(0.0, 0.0), Vec2::new(400.0, 300.0));
    let mapping = InputMapping::new(image_rect, (800, 600));

    let pos = mapping.pos_css_to_pos_points((400.0, 300.0)).unwrap();
    assert_approx_pos2(pos, Pos2::new(200.0, 150.0));
  }

  #[test]
  fn clamping_keeps_pos_within_viewport_bounds() {
    let image_rect = Rect::from_min_size(Pos2::new(50.0, 50.0), Vec2::new(400.0, 300.0));
    let mapping = InputMapping::new(image_rect, (800, 600));

    let pos_before = mapping
      .pos_points_to_pos_css_clamped(Pos2::new(0.0, 0.0))
      .unwrap();
    assert_approx2(pos_before, (0.0, 0.0));

    let pos_after = mapping
      .pos_points_to_pos_css_clamped(Pos2::new(9999.0, 9999.0))
      .unwrap();
    assert_approx2(pos_after, (800.0, 600.0));
  }

  #[test]
  fn degenerate_image_rect_returns_none_for_css_to_points() {
    let image_rect = Rect::from_min_size(Pos2::new(0.0, 0.0), Vec2::new(0.0, 0.0));
    let mapping = InputMapping::new(image_rect, (800, 600));
    assert!(mapping.pos_css_to_pos_points((10.0, 10.0)).is_none());
  }

  #[test]
  fn wheel_line_delta_converts_to_css_pixels() {
    let image_rect = Rect::from_min_size(Pos2::new(0.0, 0.0), Vec2::new(800.0, 600.0));
    let mapping = InputMapping::new(image_rect, (800, 600));

    // In winit/egui convention, negative y is a "scroll down" gesture.
    let delta_css = mapping
      .wheel_delta_to_delta_css(WheelDelta::Lines(Vec2::new(0.0, -2.0)))
      .unwrap();
    assert_approx2(delta_css, (0.0, 2.0 * CSS_PX_PER_WHEEL_LINE));
  }

  #[test]
  fn wheel_pixel_delta_converts_to_css_pixels_with_scale() {
    let image_rect = Rect::from_min_size(Pos2::new(0.0, 0.0), Vec2::new(400.0, 300.0));
    let mapping = InputMapping::new(image_rect, (800, 600));

    // Negative y is scroll down; drawn at 0.5 scale means 1 point = 2 CSS px.
    let delta_css = mapping
      .wheel_delta_to_delta_css(WheelDelta::Points(Vec2::new(0.0, -10.0)))
      .unwrap();
    assert_approx2(delta_css, (0.0, 20.0));
  }

  #[test]
  fn wheel_page_delta_converts_to_viewport_css_units() {
    let image_rect = Rect::from_min_size(Pos2::new(0.0, 0.0), Vec2::new(800.0, 600.0));
    let mapping = InputMapping::new(image_rect, (800, 600));

    // Negative y is scroll down; 1 page corresponds to the viewport height.
    let delta_css = mapping
      .wheel_delta_to_delta_css(WheelDelta::Pages(Vec2::new(0.0, -1.0)))
      .unwrap();
    assert_approx2(delta_css, (0.0, 600.0));
  }

  #[test]
  fn wheel_delta_from_winit_pixel_delta_divides_by_pixels_per_point() {
    use winit::dpi::PhysicalPosition;
    use winit::event::MouseScrollDelta;

    let image_rect = Rect::from_min_size(Pos2::new(0.0, 0.0), Vec2::new(800.0, 600.0));
    let mapping = InputMapping::new(image_rect, (800, 600));

    // winit PixelDelta is in physical pixels. With pixels_per_point=2, -20px becomes -10 points.
    let wheel = WheelDelta::from_winit(
      MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, -20.0)),
      2.0,
    );
    assert_eq!(wheel, WheelDelta::Points(Vec2::new(0.0, -10.0)));

    let delta_css = mapping.wheel_delta_to_delta_css(wheel).unwrap();
    assert_approx2(delta_css, (0.0, 10.0));
  }

  #[test]
  fn css_to_points_inverts_points_to_css_at_identity_scale() {
    let image_rect = Rect::from_min_size(Pos2::new(10.0, 20.0), Vec2::new(800.0, 600.0));
    let mapping = InputMapping::new(image_rect, (800, 600));

    let points = mapping
      .pos_css_to_pos_points_clamped((100.0, 50.0))
      .expect("pos_css_to_pos_points_clamped");
    assert_approx2((points.x, points.y), (110.0, 70.0));
  }

  #[test]
  fn css_rect_converts_to_points_rect_with_scale() {
    let image_rect = Rect::from_min_size(Pos2::new(0.0, 0.0), Vec2::new(400.0, 300.0));
    let mapping = InputMapping::new(image_rect, (800, 600));

    let rect_points = mapping
      .rect_css_to_rect_points_clamped(crate::geometry::Rect::from_xywh(100.0, 50.0, 200.0, 100.0))
      .expect("rect_css_to_rect_points_clamped");

    // Drawn at 0.5 scale means 1 point = 2 CSS px.
    assert_approx2((rect_points.min.x, rect_points.min.y), (50.0, 25.0));
    assert_approx2((rect_points.width(), rect_points.height()), (100.0, 50.0));
  }

  #[test]
  fn pos_points_to_pos_css_if_inside_returns_none_when_outside_image_rect() {
    let image_rect = Rect::from_min_size(Pos2::new(10.0, 20.0), Vec2::new(800.0, 600.0));
    let mapping = InputMapping::new(image_rect, (800, 600));

    assert!(mapping
      .pos_points_to_pos_css_if_inside(Pos2::new(0.0, 0.0))
      .is_none());
    assert!(mapping
      .pos_points_to_pos_css_if_inside(Pos2::new(900.0, 700.0))
      .is_none());
  }

  #[test]
  fn pos_points_to_pos_css_if_inside_matches_clamped_mapping_when_inside() {
    let image_rect = Rect::from_min_size(Pos2::new(10.0, 20.0), Vec2::new(800.0, 600.0));
    let mapping = InputMapping::new(image_rect, (800, 600));

    let clamped = mapping
      .pos_points_to_pos_css_clamped(Pos2::new(110.0, 70.0))
      .unwrap();
    let inside = mapping
      .pos_points_to_pos_css_if_inside(Pos2::new(110.0, 70.0))
      .unwrap();
    assert_approx2(inside, clamped);
  }
}
