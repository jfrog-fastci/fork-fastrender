use crate::debug::runtime::{with_thread_runtime_toggles, RuntimeToggles};
use crate::layout::float_context::{
  float_profile_stats, reset_float_profile_counters, FloatContext, FloatSide,
};
use std::collections::HashMap;
use std::sync::Arc;

#[test]
fn float_range_queries_do_not_clone_sweep_state() {
  let _lock = super::layout_profile_lock();
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
