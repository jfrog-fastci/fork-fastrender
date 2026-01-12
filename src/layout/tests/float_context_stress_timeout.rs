use crate::layout::float_context::FloatContext;
use crate::layout::formatting_context::LayoutError;
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

/// Regression stress test for `FloatContext` range scans.
///
/// The `stackoverflow.com` layout timeout profile shows float placement and range queries dominating
/// CPU time. This test constructs a float-heavy context that forces `find_fit` to advance by float
/// boundaries, and asserts it can skip over non-constraining float ends.
#[test]
fn float_context_range_queries_complete_before_deadline() {
  // Ensure deadline checks are not artificially slowed down by other tests or environment
  // variables (e.g. `FASTR_TEST_RENDER_DELAY_MS`).
  let _delay_guard = TestRenderDelayGuard::set(Some(0));

  // Keep a tight budget so a regression is caught quickly, but allow enough slack to avoid test
  // flakiness on slower CI.
  let deadline = RenderDeadline::new(Some(Duration::from_millis(1500)), None);

  // Construct a context where many floats end before the float that actually constrains the
  // available width. The boundary stepping logic should be able to jump directly to the constraining
  // float's bottom edge instead of iterating each irrelevant float end.
  const CONTAINING_WIDTH: f32 = 200.0;
  const CONSTRAINING_FLOAT_WIDTH: f32 = 80.0;
  const CONSTRAINING_FLOAT_HEIGHT: f32 = 10_000.0;
  const NON_CONSTRAINING_FLOAT_WIDTH: f32 = 10.0;
  const NON_CONSTRAINING_FLOAT_COUNT: usize = 10_000;

  let mut ctx = FloatContext::new(CONTAINING_WIDTH);

  ctx.add_float_at(
    crate::layout::float_context::FloatSide::Left,
    0.0,
    0.0,
    CONSTRAINING_FLOAT_WIDTH,
    CONSTRAINING_FLOAT_HEIGHT,
  );

  for i in 0..NON_CONSTRAINING_FLOAT_COUNT {
    ctx.add_float_at(
      crate::layout::float_context::FloatSide::Left,
      0.0,
      0.0,
      NON_CONSTRAINING_FLOAT_WIDTH,
      (i + 1) as f32,
    );
  }

  // Prime the sweep so the deadline window focuses on boundary stepping, not the initial heap
  // activation of all floats at y=0.
  let first_boundary = ctx.next_float_boundary_after(0.0);
  assert!(
    (first_boundary - CONSTRAINING_FLOAT_HEIGHT).abs() < f32::EPSILON,
    "expected boundary to skip non-constraining float ends and jump to the constraining float bottom"
  );

  let result = with_deadline(Some(&deadline), || {
    let fit_y = ctx.find_fit(150.0, 1.0, 0.0);
    let timeout = ctx.take_timeout_error();
    (fit_y, timeout)
  });

  match result {
    Ok((fit_y, None)) => assert!(
      (fit_y - CONSTRAINING_FLOAT_HEIGHT).abs() < f32::EPSILON,
      "expected fit y to be the constraining float bottom"
    ),
    Ok((_fit_y, Some(LayoutError::Timeout { elapsed }))) => panic!(
      "expected float boundary stepping to finish under deadline, timed out after {elapsed:?}"
    ),
    Ok((_fit_y, Some(other))) => panic!("unexpected layout error: {other:?}"),
    Err(err) => panic!("unexpected deadline error: {err:?}"),
  }
}

#[test]
fn float_context_many_range_queries_complete_before_deadline() {
  let _delay_guard = TestRenderDelayGuard::set(Some(0));

  // This targets `edges_in_range_min_width_with_state` / `FloatRangeCache` usage patterns from
  // float-heavy pages: many consecutive line boxes querying float constraints.
  let deadline = RenderDeadline::new(Some(Duration::from_millis(1500)), None);

  const CONTAINING_WIDTH: f32 = 200.0;
  const FLOAT_WIDTH: f32 = 80.0;
  const FLOAT_HEIGHT: f32 = 1.0;
  const FLOAT_COUNT: usize = 10_000;

  let mut ctx = FloatContext::new(CONTAINING_WIDTH);
  for i in 0..FLOAT_COUNT {
    let y = i as f32;
    if i % 2 == 0 {
      ctx.add_float_at(
        crate::layout::float_context::FloatSide::Left,
        0.0,
        y,
        FLOAT_WIDTH,
        FLOAT_HEIGHT,
      );
    } else {
      ctx.add_float_at(
        crate::layout::float_context::FloatSide::Right,
        CONTAINING_WIDTH - FLOAT_WIDTH,
        y,
        FLOAT_WIDTH,
        FLOAT_HEIGHT,
      );
    }
  }

  let result = with_deadline(Some(&deadline), || {
    let mut acc = 0.0f32;
    for i in 0..FLOAT_COUNT {
      let y = i as f32;
      let (left, width) = ctx.available_width_in_range(y, y + 20.0);
      acc += left + width;
    }
    let timeout = ctx.take_timeout_error();
    (acc, timeout)
  });

  match result {
    Ok((acc, None)) => {
      std::hint::black_box(acc);
    }
    Ok((_acc, Some(LayoutError::Timeout { elapsed }))) => panic!(
      "expected float range queries to finish under deadline, timed out after {elapsed:?}"
    ),
    Ok((_acc, Some(other))) => panic!("unexpected layout error: {other:?}"),
    Err(err) => panic!("unexpected deadline error: {err:?}"),
  }
}
