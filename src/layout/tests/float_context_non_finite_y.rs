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
    let (left, width) = ctx.available_width_at_y(f32::NAN);
    assert!(left.is_finite());
    assert!(width.is_finite());

    let next = ctx.next_float_boundary_after(f32::NAN);
    assert!(next.is_finite());

    let (left, width) = ctx.available_width_in_range(f32::NAN, 10.0);
    assert!(left.is_finite());
    assert!(width.is_finite());

    let (left, width) = ctx.available_width_in_range(0.0, f32::NAN);
    assert!(left.is_finite());
    assert!(width.is_finite());

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

