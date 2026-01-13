use crate::layout::float_context::{FloatContext, FloatSide};
use crate::layout::formatting_context::LayoutError;
use crate::render_control::{with_deadline, RenderDeadline};
use std::time::Duration;

fn assert_no_timeout(timeout: Option<LayoutError>) {
  match timeout {
    None => {}
    Some(LayoutError::Timeout { elapsed }) => panic!("unexpected float layout timeout: {elapsed:?}"),
    Some(other) => panic!("unexpected layout error: {other:?}"),
  }
}

#[test]
fn float_context_sanitizes_non_finite_y() {
  // Use an active deadline (even if extremely tight) so any accidental infinite scan reports via
  // `FloatContext::take_timeout_error`.
  let deadline = RenderDeadline::new(Some(Duration::ZERO), None);

  for non_finite_y in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
    let mut ctx = FloatContext::new(200.0);
    ctx.add_float_at(FloatSide::Left, 0.0, 0.0, 50.0, 10.0);

    // Simulate layout having already advanced: non-finite Y coordinates should be normalized to the
    // current float ceiling so they cannot corrupt monotonic placement.
    ctx.set_current_y(5.0);

    ctx.add_float_at(FloatSide::Left, 0.0, non_finite_y, 80.0, 10.0);

    assert!(ctx.current_y().is_finite(), "current_y must remain finite");
    assert!(
      (ctx.current_y() - 5.0).abs() < f32::EPSILON,
      "current_y should be preserved"
    );

    let (_boundary_after_0, fit_y, timeout) = with_deadline(Some(&deadline), || {
      let (left_0, width_0) = ctx.available_width_at_y(0.0);
      assert!(left_0.is_finite() && width_0.is_finite());

      // The sanitized float should start at y=5, constraining available width at that position.
      let (left_5, width_5) = ctx.available_width_at_y(5.0);
      assert!(left_5.is_finite() && width_5.is_finite());
      assert!(
        (left_5 - 80.0).abs() < f32::EPSILON,
        "expected sanitized float to constrain left edge at y=5 (y={non_finite_y:?}, left_edge={left_5})"
      );

      // A float starting at y=5 should be visible as the next boundary after y=0.
      let boundary_after_0 = ctx.next_float_boundary_after(0.0);
      assert!(
        (boundary_after_0 - 5.0).abs() < f32::EPSILON,
        "expected next boundary after 0 to be the sanitized float start (y={non_finite_y:?}, boundary={boundary_after_0})"
      );

      // Force a range query that spans across y=5 so `FloatRangeCache` is exercised under the
      // deadline. The 80px float should force us to jump below its bottom edge at y=15.
      let fit_y = ctx.find_fit(130.0, 6.0, 0.0);
      assert!(
        (fit_y - 15.0).abs() < f32::EPSILON,
        "expected find_fit to skip the sanitized float and return y=15 (y={non_finite_y:?}, fit_y={fit_y})"
      );

      let timeout = ctx.take_timeout_error();
      (boundary_after_0, fit_y, timeout)
    });

    assert!(fit_y.is_finite());
    assert_no_timeout(timeout);
  }
}

#[test]
fn float_context_sanitizes_non_finite_x() {
  let deadline = RenderDeadline::new(Some(Duration::ZERO), None);

  let mut ctx = FloatContext::new(200.0);
  ctx.add_float_at(FloatSide::Left, 0.0, 0.0, 50.0, 10.0);

  // Non-finite X coordinates should clamp to a deterministic default so the float still
  // participates in constraint computations.
  ctx.add_float_at(FloatSide::Left, f32::NAN, 0.0, 80.0, 10.0);

  assert!(ctx.current_y().is_finite(), "current_y must remain finite");

  let (fit_y, timeout) = with_deadline(Some(&deadline), || {
    let (left_edge, width) = ctx.available_width_at_y(0.0);
    assert!(left_edge.is_finite() && width.is_finite());
    assert!(
      (left_edge - 80.0).abs() < f32::EPSILON,
      "expected NaN x to be sanitized to x=0 and constrain left edge"
    );

    let fit_y = ctx.find_fit(150.0, 1.0, 0.0);
    let timeout = ctx.take_timeout_error();
    (fit_y, timeout)
  });

  assert!((fit_y - 10.0).abs() < f32::EPSILON, "expected fit below floats");
  assert_no_timeout(timeout);
}

