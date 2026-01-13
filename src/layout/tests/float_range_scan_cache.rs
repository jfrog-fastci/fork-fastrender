use crate::debug::runtime::{with_thread_runtime_toggles, RuntimeToggles};
use crate::layout::float_context::{
  float_profile_stats, reset_float_profile_counters, FloatContext, FloatSide,
};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

static LOCK: Mutex<()> = Mutex::new(());

#[test]
fn float_range_queries_do_not_clone_sweep_state() {
  let _lock = LOCK.lock();
  let mut raw = HashMap::new();
  raw.insert("FASTR_LAYOUT_PROFILE".to_string(), "1".to_string());
  let toggles = Arc::new(RuntimeToggles::from_map(raw));

  with_thread_runtime_toggles(toggles, || {
    reset_float_profile_counters();

    let mut ctx = FloatContext::new(200.0);
    let float_count = 2_000usize;
    for i in 0..float_count {
      let y = i as f32;
      if i % 2 == 0 {
        ctx.add_float_at(FloatSide::Left, 0.0, y, 80.0, 1.0);
      } else {
        ctx.add_float_at(FloatSide::Right, 120.0, y, 80.0, 1.0);
      }
    }

    // Repeated range queries should not require cloning the sweep state each time.
    for i in 0..100 {
      let y = i as f32;
      let (left, width) = ctx.available_width_in_range(y, y + 20.0);
      assert_eq!(width, 120.0);
      // Tie-breaker prefers larger left-edge when width is equal.
      assert_eq!(left, 80.0);
    }

    // `find_fit_in_containing_block` repeatedly calls the range query machinery while advancing.
    // The first y where 150px fits is below all of the 80px floats.
    let fit_y = ctx.find_fit_in_containing_block(150.0, 1.0, 0.0, 0.0, 200.0);
    assert_eq!(fit_y, float_count as f32);

    let stats = float_profile_stats();
    assert!(stats.range_queries > 0, "expected profiling counters to be enabled");
    assert_eq!(
      stats.sweep_state_clones, 0,
      "range queries should not clone FloatSweepState (clones={})",
      stats.sweep_state_clones
    );
  });
}

#[test]
fn float_range_queries_do_not_quadratically_scan_range_segments() {
  let _lock = LOCK.lock();
  let mut raw = HashMap::new();
  raw.insert("FASTR_LAYOUT_PROFILE".to_string(), "1".to_string());
  let toggles = Arc::new(RuntimeToggles::from_map(raw));

  with_thread_runtime_toggles(toggles, || {
    reset_float_profile_counters();

    // Construct a worst-case scenario for `find_fit_in_containing_block`:
    // - many non-overlapping floats, each creating its own range-cache segment
    // - repeated range queries over a tall span, which historically produced O(n^2) segment scans
    let float_count = 4096usize;
    let containing_width = float_count as f32 + 1.0;
    let mut ctx = FloatContext::new(containing_width);
    for i in 0..float_count {
      let y = i as f32;
      // Make each float slightly narrower than the previous so the range-cache cannot coalesce
      // adjacent segments with identical constraints.
      let width = (float_count - i) as f32;
      ctx.add_float_at(FloatSide::Left, 0.0, y, width, 1.0);
    }

    // A full-width box can only fit once its entire height sits below all floats.
    let fit_y = ctx.find_fit_in_containing_block(
      containing_width,
      float_count as f32,
      0.0,
      0.0,
      containing_width,
    );
    assert_eq!(fit_y, float_count as f32);

    let stats = float_profile_stats();
    assert!(stats.range_queries > 0, "expected profiling counters to be enabled");
    assert!(
      stats.max_range_boundaries_scanned_per_query <= 256,
      "expected block-min/range acceleration to cap per-query scanning (max_scanned={}, segments_max={})",
      stats.max_range_boundaries_scanned_per_query,
      stats.max_range_cache_segments_len
    );
    assert!(
      stats.range_boundaries_scanned <= (float_count as u64) * 256,
      "expected total scanned segments to stay sub-quadratic (scanned={}, queries={})",
      stats.range_boundaries_scanned,
      stats.range_queries
    );
  });
}
