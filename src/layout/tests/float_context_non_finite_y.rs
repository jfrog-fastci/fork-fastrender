use crate::layout::float_context::{FloatContext, FloatSide};
use crate::render_control::{with_deadline, RenderDeadline};
use std::time::Duration;

struct TestRenderDelayGuard;

impl TestRenderDelayGuard {
  fn set(ms: Option<u64>) -> Self {
    crate::render_control::set_test_render_delay_ms(ms);
    Self
  }
}

impl Drop for TestRenderDelayGuard {
  fn drop(&mut self) {
    crate::render_control::set_test_render_delay_ms(None);
  }
}

#[test]
fn float_context_non_finite_y_is_fast_and_does_not_poison_state() {
  // Ensure deadline checks are not artificially slowed down by other tests or environment
  // variables (e.g. `FASTR_TEST_RENDER_DELAY_MS`).
  let _delay_guard = TestRenderDelayGuard::set(Some(0));

  // Keep a tight budget so the test catches accidental unbounded work. (These calls should be
  // O(1) for non-finite inputs after the guards in `FloatContext`.)
  let deadline = RenderDeadline::new(Some(Duration::from_millis(50)), None);

  const CONTAINING_WIDTH: f32 = 200.0;
  const CONSTRAINING_FLOAT_WIDTH: f32 = 80.0;
  const CONSTRAINING_FLOAT_HEIGHT: f32 = 10_000.0;
  const NON_CONSTRAINING_FLOAT_WIDTH: f32 = 10.0;
  const NON_CONSTRAINING_FLOAT_COUNT: usize = 10_000;

  let mut ctx = FloatContext::new(CONTAINING_WIDTH);
  ctx.add_float_at(
    FloatSide::Left,
    0.0,
    0.0,
    CONSTRAINING_FLOAT_WIDTH,
    CONSTRAINING_FLOAT_HEIGHT,
  );
  for i in 0..NON_CONSTRAINING_FLOAT_COUNT {
    ctx.add_float_at(
      FloatSide::Left,
      0.0,
      0.0,
      NON_CONSTRAINING_FLOAT_WIDTH,
      (i + 1) as f32,
    );
  }

  let timeout = with_deadline(Some(&deadline), || {
    for non_finite_y in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
      let (left, width) = ctx.available_width_at_y(non_finite_y);
      assert!(left.is_finite());
      assert!(width.is_finite());

      let next = ctx.next_float_boundary_after(non_finite_y);
      assert!(next.is_finite());

      let (left, width) = ctx.available_width_in_range(non_finite_y, 10.0);
      assert!(left.is_finite());
      assert!(width.is_finite());

      let (left, width) = ctx.available_width_in_range(0.0, non_finite_y);
      assert!(left.is_finite());
      assert!(width.is_finite());
    }

    // Non-finite queries should not poison the sweep state. A normal query at y=0 must still
    // observe the constraining floats.
    let (left, width) = ctx.available_width_at_y(0.0);
    assert!(
      (left - CONSTRAINING_FLOAT_WIDTH).abs() < f32::EPSILON,
      "expected left edge to be constrained by floats after non-finite queries"
    );
    assert!(
      (width - (CONTAINING_WIDTH - CONSTRAINING_FLOAT_WIDTH)).abs() < f32::EPSILON,
      "expected available width to remain constrained after non-finite queries"
    );

    ctx.take_timeout_error()
  });

  assert!(
    timeout.is_none(),
    "expected non-finite float queries to finish before deadline, got {timeout:?}"
  );
}

#[test]
fn float_context_non_finite_min_y_is_sanitized_for_placement_queries() {
  let _delay_guard = TestRenderDelayGuard::set(Some(0));

  // Placement calls can legitimately do more work than simple "band" queries (they may consult
  // `FloatRangeCache`), so use a more forgiving budget while still ensuring we don't hit the
  // cooperative deadline checks.
  let deadline = RenderDeadline::new(Some(Duration::from_millis(1500)), None);

  const CONTAINING_WIDTH: f32 = 200.0;
  const CONSTRAINING_FLOAT_WIDTH: f32 = 80.0;
  const CONSTRAINING_FLOAT_HEIGHT: f32 = 10_000.0;
  const NON_CONSTRAINING_FLOAT_WIDTH: f32 = 10.0;
  const NON_CONSTRAINING_FLOAT_COUNT: usize = 2_000;

  let mut ctx = FloatContext::new(CONTAINING_WIDTH);
  ctx.add_float_at(
    FloatSide::Left,
    0.0,
    0.0,
    CONSTRAINING_FLOAT_WIDTH,
    CONSTRAINING_FLOAT_HEIGHT,
  );
  for i in 0..NON_CONSTRAINING_FLOAT_COUNT {
    ctx.add_float_at(
      FloatSide::Left,
      0.0,
      0.0,
      NON_CONSTRAINING_FLOAT_WIDTH,
      (i + 1) as f32,
    );
  }

  let timeout = with_deadline(Some(&deadline), || {
    for non_finite_min_y in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
      let fit_y = ctx.find_fit(150.0, 1.0, non_finite_min_y);
      assert!(fit_y.is_finite());

      let (x, y) = ctx.compute_float_position(FloatSide::Left, 20.0, 1.0, non_finite_min_y);
      assert!(x.is_finite() && y.is_finite());
    }

    // Ensure we didn't poison any internal sweep/range state.
    let (left, width) = ctx.available_width_at_y(0.0);
    assert!(
      (left - CONSTRAINING_FLOAT_WIDTH).abs() < f32::EPSILON,
      "expected left edge to remain constrained after non-finite placement queries"
    );
    assert!(
      (width - (CONTAINING_WIDTH - CONSTRAINING_FLOAT_WIDTH)).abs() < f32::EPSILON,
      "expected available width to remain constrained after non-finite placement queries"
    );

    ctx.take_timeout_error()
  });

  assert!(
    timeout.is_none(),
    "expected non-finite placement queries to finish before deadline, got {timeout:?}"
  );
}
