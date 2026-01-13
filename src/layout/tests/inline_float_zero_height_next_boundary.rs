use crate::debug::runtime::{with_thread_runtime_toggles, RuntimeToggles};
use crate::layout::float_context::{
  float_profile_stats, reset_float_profile_counters, FloatContext, FloatSide,
};
use crate::layout::inline::float_integration::{line_spaces, InlineFloatIntegration, LineSpaceOptions};
use std::collections::HashMap;
use std::sync::Arc;

#[test]
fn inline_float_zero_height_avoids_excess_boundary_steps() {
  let _lock = super::layout_profile_lock();

  let mut raw = HashMap::new();
  raw.insert("FASTR_LAYOUT_PROFILE".to_string(), "1".to_string());
  let toggles = Arc::new(RuntimeToggles::from_map(raw));

  with_thread_runtime_toggles(toggles, || {
    const CONTAINING_WIDTH: f32 = 200.0;
    const FLOAT_WIDTH: f32 = 150.0;
    const FLOAT_HEIGHT: f32 = 1.0;
    const FLOAT_COUNT: usize = 4096;
    const MIN_WIDTH: f32 = 100.0;

    let mut ctx = FloatContext::new(CONTAINING_WIDTH);
    for i in 0..FLOAT_COUNT {
      ctx.add_float_at(
        FloatSide::Left,
        0.0,
        (i as f32) * FLOAT_HEIGHT,
        FLOAT_WIDTH,
        FLOAT_HEIGHT,
      );
    }

    let integration = InlineFloatIntegration::new(&ctx);

    // Exercise the `line_height == 0` path in `InlineFloatIntegration::find_line_space`.
    reset_float_profile_counters();
    let opts = LineSpaceOptions::with_min_width(MIN_WIDTH).line_height(0.0);
    let space = integration.find_line_space(0.0, opts);
    assert_eq!(space.y, FLOAT_COUNT as f32);
    assert_eq!(space.left_edge, 0.0);
    assert_eq!(space.width, CONTAINING_WIDTH);

    let stats = float_profile_stats();
    assert!(stats.width_queries > 0, "expected profiling counters to be enabled");
    assert!(
      stats.boundary_steps <= (FLOAT_COUNT as u64) * 11 / 10,
      "expected boundary stepping to stay near-linear (steps={}, floats={})",
      stats.boundary_steps,
      FLOAT_COUNT
    );

    // Also exercise `LineSpaceIterator`, which is used by some callers to walk float boundaries.
    reset_float_profile_counters();
    let spaces: Vec<_> = line_spaces(&ctx, 0.0, (FLOAT_COUNT as f32) + 10.0).collect();
    assert!(
      spaces.len() > 1,
      "expected multiple line spaces for float-heavy context"
    );

    let stats = float_profile_stats();
    assert!(
      stats.boundary_steps <= (FLOAT_COUNT as u64) * 11 / 10,
      "expected iterator boundary stepping to stay near-linear (steps={}, floats={})",
      stats.boundary_steps,
      FLOAT_COUNT
    );
  });
}

