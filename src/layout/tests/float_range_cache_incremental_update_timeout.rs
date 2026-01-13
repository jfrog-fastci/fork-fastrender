use crate::error::{RenderError, RenderStage};
use crate::layout::float_context::{FloatContext, FloatSide};
use crate::render_control::{check_active, set_test_render_delay_ms, with_deadline, RenderDeadline};
use std::time::Duration;

struct TestRenderDelayGuard;

impl TestRenderDelayGuard {
  fn set(ms: Option<u64>) -> Self {
    set_test_render_delay_ms(ms);
    Self
  }
}

impl Drop for TestRenderDelayGuard {
  fn drop(&mut self) {
    set_test_render_delay_ms(None);
  }
}

/// Regression stress test for `FloatRangeCache::apply_rect_float` incremental updates.
///
/// Historically, incremental updates coalesced segments via repeated `Vec::remove` calls in a loop.
/// When many segments collapsed at once, each removal shifted the tail, leading to quadratic
/// behavior.
///
/// This test:
/// 1) Constructs a large range cache containing many narrow segments.
/// 2) Inserts many overlapping floats that cause large runs of segments to merge.
/// 3) Forces a deadline check after updates so a regression trips a timeout quickly.
#[test]
fn float_range_cache_incremental_updates_complete_before_deadline() {
  let _delay_guard = TestRenderDelayGuard::set(Some(0));

  // Keep a tight budget so a regression is caught quickly, but allow enough slack to avoid test
  // flakiness on slower CI.
  let deadline = RenderDeadline::new(Some(Duration::from_millis(1500)), None);

  // Build an initial cache with `BASE_SEGMENTS` distinct segments (one per y unit).
  const BASE_SEGMENTS: usize = 20_000;
  // Each insertion clamps a 100px-tall window, causing ~99 merges.
  const WINDOW: usize = 100;
  // Only process part of the range so the cache remains large (and a post-update range query would
  // still be expensive). The deadline check at the end guarantees we catch regressions even if
  // segment counts change in the future.
  const UPDATE_COUNT: usize = 180;

  let mut ctx = FloatContext::new(10_000.0);

  // Produce a descending staircase of left-edge constraints:
  // - all floats start at y=0
  // - widths decrease while heights increase
  // => the most-constraining float ends at each integer boundary, creating many segments.
  for i in 0..BASE_SEGMENTS {
    let width = (BASE_SEGMENTS - i) as f32;
    let height = (i + 1) as f32;
    ctx.add_float_at(FloatSide::Left, 0.0, 0.0, width, height);
  }

  // Build up a large `FloatRangeCache.segments` by scanning a big y-span once.
  let _ = ctx.available_width_in_range(0.0, BASE_SEGMENTS as f32);

  let result = with_deadline(Some(&deadline), || {
    for i in 0..UPDATE_COUNT {
      let y = (i * WINDOW) as f32;
      // Match the current maximum left edge at `y` so the update is pure coalescing of the
      // existing segments in the span (clamps all narrower constraints up to the current max).
      let width = (BASE_SEGMENTS - i * WINDOW) as f32;
      ctx.add_float_at(FloatSide::Left, 0.0, y, width, WINDOW as f32);
    }

    // Force a deadline check at the end so performance regressions in `apply_rect_float` are
    // caught even though `add_float_at` itself does not call `check_active_periodic`.
    check_active(RenderStage::Layout).err()
  });

  match result {
    None => {}
    Some(RenderError::Timeout { elapsed, .. }) => panic!(
      "expected float range-cache incremental updates to finish under deadline, timed out after {elapsed:?}"
    ),
    Some(other) => panic!("unexpected render error: {other:?}"),
  }
}

