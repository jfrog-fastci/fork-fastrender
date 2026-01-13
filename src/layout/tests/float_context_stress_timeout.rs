use crate::debug::runtime::{with_thread_runtime_toggles, RuntimeToggles};
use crate::layout::float_context::{
  float_profile_stats, reset_float_profile_counters, FloatContext, FloatSide,
};
use crate::layout::formatting_context::LayoutError;
use crate::render_control::{with_deadline, RenderDeadline};
use std::collections::HashMap;
use std::sync::Arc;
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

  let (fit_y, timeout) = with_deadline(Some(&deadline), || {
    let fit_y = ctx.find_fit(150.0, 1.0, 0.0);
    let timeout = ctx.take_timeout_error();
    (fit_y, timeout)
  });

  match timeout {
    None => assert!(
      (fit_y - CONSTRAINING_FLOAT_HEIGHT).abs() < f32::EPSILON,
      "expected fit y to be the constraining float bottom"
    ),
    Some(LayoutError::Timeout { elapsed }) => panic!(
      "expected float boundary stepping to finish under deadline, timed out after {elapsed:?}"
    ),
    Some(other) => panic!("unexpected layout error: {other:?}"),
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

  let (acc, timeout) = with_deadline(Some(&deadline), || {
    let mut acc = 0.0f32;
    for i in 0..FLOAT_COUNT {
      let y = i as f32;
      let (left, width) = ctx.available_width_in_range(y, y + 20.0);
      acc += left + width;
    }
    let timeout = ctx.take_timeout_error();
    (acc, timeout)
  });

  std::hint::black_box(acc);
  match timeout {
    None => {}
    Some(LayoutError::Timeout { elapsed }) => panic!(
      "expected float range queries to finish under deadline, timed out after {elapsed:?}"
    ),
    Some(other) => panic!("unexpected layout error: {other:?}"),
  }
}

#[test]
fn float_context_incremental_float_placement_complete_before_deadline() {
  let _delay_guard = TestRenderDelayGuard::set(Some(0));

  // Regression target: float-heavy pages often interleave float placement with many range queries.
  // If `FloatRangeCache` rebuilds from scratch for every inserted float, placement devolves into
  // O(n^2) heap construction.
  let deadline = RenderDeadline::new(Some(Duration::from_millis(1500)), None);

  const CONTAINING_WIDTH: f32 = 200.0;
  const FLOAT_WIDTH: f32 = 80.0;
  const FLOAT_HEIGHT: f32 = 1.0;
  const FLOAT_COUNT: usize = 10_000;

  let timeout = with_deadline(Some(&deadline), || {
    let mut ctx = FloatContext::new(CONTAINING_WIDTH);
    for i in 0..FLOAT_COUNT {
      let side = if i % 2 == 0 {
        crate::layout::float_context::FloatSide::Left
      } else {
        crate::layout::float_context::FloatSide::Right
      };
      let (x, y) = ctx.compute_float_position(side, FLOAT_WIDTH, FLOAT_HEIGHT, 0.0);
      ctx.add_float_at(side, x, y, FLOAT_WIDTH, FLOAT_HEIGHT);
    }
    ctx.take_timeout_error()
  });

  match timeout {
    None => {}
    Some(LayoutError::Timeout { elapsed }) => panic!(
      "expected incremental float placement to finish under deadline, timed out after {elapsed:?}"
    ),
    Some(other) => panic!("unexpected layout error: {other:?}"),
  }
}

#[test]
fn float_context_dense_boundary_find_fit_does_not_rescan_quadratically() {
  let _delay_guard = TestRenderDelayGuard::set(Some(0));
  let _lock = super::layout_profile_lock();

  // A dense-boundary regression: alternate left/right floats on every row and request a find_fit
  // with a tall height so the naive "range query + advance boundary" loop would repeatedly rescan
  // many overlapping FloatRangeCache segments.
  let deadline = RenderDeadline::new(Some(Duration::from_millis(1500)), None);

  const CONTAINING_WIDTH: f32 = 200.0;
  const FLOAT_WIDTH: f32 = 80.0;
  const FLOAT_HEIGHT: f32 = 1.0;
  const FLOAT_COUNT: usize = 10_000;
  const QUERY_HEIGHT: f32 = 500.0;
  const QUERY_WIDTH: f32 = 150.0;
  const QUERY_COUNT: usize = 50;

  let mut raw = HashMap::new();
  raw.insert("FASTR_LAYOUT_PROFILE".to_string(), "1".to_string());
  let toggles = Arc::new(RuntimeToggles::from_map(raw));

  with_thread_runtime_toggles(toggles, || {
    reset_float_profile_counters();

    let mut ctx = FloatContext::new(CONTAINING_WIDTH);
    for i in 0..FLOAT_COUNT {
      let y = i as f32;
      if i % 2 == 0 {
        ctx.add_float_at(FloatSide::Left, 0.0, y, FLOAT_WIDTH, FLOAT_HEIGHT);
      } else {
        ctx.add_float_at(
          FloatSide::Right,
          CONTAINING_WIDTH - FLOAT_WIDTH,
          y,
          FLOAT_WIDTH,
          FLOAT_HEIGHT,
        );
      }
    }

    let (acc, timeout) = with_deadline(Some(&deadline), || {
      let mut acc = 0.0f32;
      for _ in 0..QUERY_COUNT {
        acc += ctx.find_fit(QUERY_WIDTH, QUERY_HEIGHT, 0.0);
      }
      let timeout = ctx.take_timeout_error();
      (acc, timeout)
    });

    std::hint::black_box(acc);
    match timeout {
      None => {}
      Some(LayoutError::Timeout { elapsed }) => panic!(
        "expected dense-boundary find_fit to finish under deadline, timed out after {elapsed:?}"
      ),
      Some(other) => panic!("unexpected layout error: {other:?}"),
    }

    let stats = float_profile_stats();
    assert!(
      stats.range_boundaries_scanned < (FLOAT_COUNT as u64 * (QUERY_COUNT as u64) * 4),
      "expected find_fit scan to be roughly linear (segments_scanned={})",
      stats.range_boundaries_scanned
    );
  });
}
