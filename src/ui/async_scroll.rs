//! Async scroll compositor helpers.
//!
//! The windowed browser UI can render smooth scrolling even when the render worker is slow by
//! translating the most recently uploaded page texture based on scroll state updates. This module
//! contains the pure math for converting a scroll delta (in CSS pixels) into an on-screen
//! translation (in egui points).

use crate::geometry::Point;

/// Convert the delta between the scroll offset used by the currently uploaded texture and the
/// latest scroll offset into an on-screen translation (in egui points).
///
/// The returned vector should be applied to the page texture's destination rect when painting it
/// inside the viewport. (The viewport is expected to be clipped so the translated image does not
/// draw outside.)
///
/// `viewport_css` is the size of the viewport in **CSS pixels** for which the uploaded texture was
/// rendered. `drawn_size_points` is the size of the rect the texture is drawn into in **egui
/// points**.
///
/// Safety: returns `None` when the translation is non-finite, degenerate, or extremely large (to
/// avoid accidentally translating the page by a bogus amount due to stale state).
pub fn async_scroll_translation_points(
  rendered_scroll_css: Point,
  latest_scroll_css: Point,
  viewport_css: (u32, u32),
  drawn_size_points: (f32, f32),
) -> Option<Point> {
  let delta_css = Point::new(
    rendered_scroll_css.x - latest_scroll_css.x,
    rendered_scroll_css.y - latest_scroll_css.y,
  );
  if !delta_css.x.is_finite() || !delta_css.y.is_finite() {
    return None;
  }

  let viewport_w_css = viewport_css.0 as f32;
  let viewport_h_css = viewport_css.1 as f32;
  if viewport_w_css <= 0.0 || viewport_h_css <= 0.0 {
    return None;
  }

  let drawn_w_points = drawn_size_points.0;
  let drawn_h_points = drawn_size_points.1;
  if drawn_w_points <= 0.0 || drawn_h_points <= 0.0 {
    return None;
  }
  if !drawn_w_points.is_finite() || !drawn_h_points.is_finite() {
    return None;
  }

  // points_per_css is the inverse of `InputMapping::css_per_point`.
  let points_per_css_x = drawn_w_points / viewport_w_css;
  let points_per_css_y = drawn_h_points / viewport_h_css;
  if points_per_css_x <= 0.0 || points_per_css_y <= 0.0 {
    return None;
  }
  if !points_per_css_x.is_finite() || !points_per_css_y.is_finite() {
    return None;
  }

  let translation = Point::new(
    delta_css.x * points_per_css_x,
    delta_css.y * points_per_css_y,
  );
  if !translation.x.is_finite() || !translation.y.is_finite() {
    return None;
  }

  // Guardrail: if the translation is absurdly large relative to the viewport rect (e.g. due to a
  // mismatched/stale scroll state), fall back to drawing the texture at the origin.
  //
  // We compare in points so the threshold matches what will be drawn on screen.
  let max_x = drawn_w_points * 2.0;
  let max_y = drawn_h_points * 2.0;
  if translation.x.abs() > max_x || translation.y.abs() > max_y {
    return None;
  }

  Some(translation)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn assert_approx_point(actual: Point, expected: Point) {
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
  fn translation_matches_css_delta_at_1_to_1_mapping() {
    // viewport_css == drawn points means 1 CSS px == 1 point.
    let translation = async_scroll_translation_points(
      Point::new(0.0, 0.0),   // rendered
      Point::new(10.0, 20.0), // latest
      (800, 600),
      (800.0, 600.0),
    )
    .expect("translation should be enabled");
    assert_approx_point(translation, Point::new(-10.0, -20.0));
  }

  #[test]
  fn translation_scales_with_non_1_to_1_viewport_mapping() {
    // Viewport is 800x600 CSS px but drawn at half size (400x300 points), so 1 CSS px == 0.5 point.
    let translation = async_scroll_translation_points(
      Point::new(0.0, 0.0),    // rendered
      Point::new(100.0, 50.0), // latest
      (800, 600),
      (400.0, 300.0),
    )
    .expect("translation should be enabled");
    assert_approx_point(translation, Point::new(-50.0, -25.0));
  }

  #[test]
  fn translation_is_disabled_for_non_finite_or_extreme_values() {
    assert!(
      async_scroll_translation_points(
        Point::new(0.0, 0.0),
        Point::new(f32::NAN, 0.0),
        (800, 600),
        (800.0, 600.0)
      )
      .is_none(),
      "NaN scroll delta should disable translation"
    );

    // Translation of -1000 points for a 100-point viewport exceeds 2x threshold.
    assert!(
      async_scroll_translation_points(
        Point::new(0.0, 0.0),
        Point::new(1000.0, 0.0),
        (100, 100),
        (100.0, 100.0)
      )
      .is_none(),
      "extreme translation should disable translation"
    );
  }
}
