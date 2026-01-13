use crate::debug::runtime::{with_thread_runtime_toggles, RuntimeToggles};
use crate::layout::float_context::{
  float_profile_stats, reset_float_profile_counters, FloatContext, FloatSide,
};
use std::collections::HashMap;
use std::sync::Arc;

#[test]
fn float_context_prunes_active_edges_without_per_float_work() {
  let _lock = super::layout_profile_lock();

  let mut raw = HashMap::new();
  raw.insert("FASTR_LAYOUT_PROFILE".to_string(), "1".to_string());
  let toggles = Arc::new(RuntimeToggles::from_map(raw));

  with_thread_runtime_toggles(toggles, || {
    reset_float_profile_counters();

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

    // Force the sweep state to jump to the y where the constraining float ends. Historically this
    // required popping O(n) ended floats out of the active heap; we want work bounded by the number
    // of distinct constraining edges instead.
    let _ = ctx.available_width_at_y(CONSTRAINING_FLOAT_HEIGHT);

    assert!(
      ctx.take_timeout_error().is_none(),
      "unexpected float layout timeout"
    );

    let stats = float_profile_stats();
    assert!(
      stats.width_queries > 0,
      "expected profiling counters to be enabled"
    );
    assert!(
      stats.active_left_prunes > 0,
      "expected active edge pruning to occur"
    );
    assert!(
      stats.active_left_prunes < 1000,
      "expected pruning work to be bounded (prunes={}, max_edges={})",
      stats.active_left_prunes,
      stats.active_left_max_edges
    );
  });
}
