use crate::geometry::Point;
use crate::scroll::ScrollBounds;

/// Apply a viewport-only scroll delta (in CSS pixels) to a scroll offset.
///
/// This is used by UI front-ends (e.g. `src/bin/browser.rs`) to apply *optimistic* viewport scroll
/// updates so async-scroll texture translation can happen in the same rendered UI frame, before the
/// worker acknowledges the scroll state update.
///
/// Semantics:
/// - Non-finite `delta_css` components are treated as `0.0`.
/// - Non-finite `current` components are treated as `0.0`.
/// - When `bounds_css` is provided, the next scroll offset is clamped to it.
/// - When `bounds_css` is `None`, the next scroll offset is clamped to `>= 0.0` only (no max clamp).
///
/// Returns `(next_scroll, applied_delta)`, where `applied_delta = next_scroll - sanitized_current`.
pub fn apply_viewport_delta_css(
  current: Point,
  delta_css: (f32, f32),
  bounds_css: Option<ScrollBounds>,
) -> (Point, Point) {
  let current = Point::new(
    if current.x.is_finite() { current.x } else { 0.0 },
    if current.y.is_finite() { current.y } else { 0.0 },
  );

  let dx = if delta_css.0.is_finite() { delta_css.0 } else { 0.0 };
  let dy = if delta_css.1.is_finite() { delta_css.1 } else { 0.0 };

  let add_saturating = |base: f32, delta: f32| {
    let next = base + delta;
    if next.is_finite() { next } else { base }
  };

  let mut next = Point::new(add_saturating(current.x, dx), add_saturating(current.y, dy));

  next = match bounds_css {
    Some(bounds) => bounds.clamp(next),
    None => Point::new(next.x.max(0.0), next.y.max(0.0)),
  };

  // Be defensive: `ScrollBounds::clamp` can return the input unchanged when bounds are invalid.
  // Ensure we never propagate non-finite values into the UI scroll model.
  let next = Point::new(
    if next.x.is_finite() { next.x } else { current.x },
    if next.y.is_finite() { next.y } else { current.y },
  );

  let applied_delta = Point::new(next.x - current.x, next.y - current.y);
  let applied_delta = Point::new(
    if applied_delta.x.is_finite() {
      applied_delta.x
    } else {
      0.0
    },
    if applied_delta.y.is_finite() {
      applied_delta.y
    } else {
      0.0
    },
  );

  (next, applied_delta)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn assert_point_eq(actual: Point, expected: Point) {
    let eps = 1e-5;
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
  fn clamps_to_bounds_and_reports_applied_delta() {
    let bounds = ScrollBounds {
      min_x: 0.0,
      min_y: 0.0,
      max_x: 100.0,
      max_y: 200.0,
    };

    let (next, applied) =
      apply_viewport_delta_css(Point::new(95.0, 190.0), (10.0, 20.0), Some(bounds));

    assert_point_eq(next, Point::new(100.0, 200.0));
    assert_point_eq(applied, Point::new(5.0, 10.0));
  }

  #[test]
  fn non_finite_deltas_are_treated_as_zero() {
    let bounds = ScrollBounds {
      min_x: 0.0,
      min_y: 0.0,
      max_x: 100.0,
      max_y: 100.0,
    };

    let (next, applied) =
      apply_viewport_delta_css(Point::new(10.0, 20.0), (f32::NAN, f32::INFINITY), Some(bounds));

    assert_point_eq(next, Point::new(10.0, 20.0));
    assert_point_eq(applied, Point::new(0.0, 0.0));
  }

  #[test]
  fn without_bounds_clamps_to_zero_only() {
    let (next, applied) = apply_viewport_delta_css(Point::new(1.0, 2.0), (-10.0, -1.0), None);

    assert_point_eq(next, Point::new(0.0, 1.0));
    assert_point_eq(applied, Point::new(-1.0, -1.0));
  }
}

