use crate::debug::runtime::{with_thread_runtime_toggles, RuntimeToggles};
use crate::layout::float_context::{
  float_pending_events_heap_use_count, reset_float_profile_counters, FloatContext, FloatSide,
};
use std::collections::HashMap;
use std::sync::Arc;

#[test]
fn float_sweep_uses_cursor_for_monotonic_insertion() {
  let _lock = super::layout_profile_lock();
  let mut raw = HashMap::new();
  raw.insert("FASTR_LAYOUT_PROFILE".to_string(), "1".to_string());
  let toggles = Arc::new(RuntimeToggles::from_map(raw));

  with_thread_runtime_toggles(toggles, || {
    reset_float_profile_counters();

    let mut ctx = FloatContext::new(1000.0);
    let n = 5000usize;
    for i in 0..n {
      let y = i as f32;
      if i % 2 == 0 {
        ctx.add_float_at(FloatSide::Left, 0.0, y, 10.0, 1.0);
      } else {
        ctx.add_float_at(FloatSide::Right, 990.0, y, 10.0, 1.0);
      }

      // Interleave width queries to mirror real layout usage (insert + monotonic y queries).
      let qy = y + 0.5;
      let (left, width) = ctx.available_width_at_y(qy);
      assert_eq!(width, 990.0);
      let expected_left = if i % 2 == 0 { 10.0 } else { 0.0 };
      assert_eq!(left, expected_left);
    }

    let (left, width) = ctx.available_width_at_y(n as f32 + 10.0);
    assert_eq!((left, width), (0.0, 1000.0));

    assert_eq!(
      float_pending_events_heap_use_count(),
      0,
      "monotonic insertion should avoid pending-events heap mode"
    );
  });
}

#[test]
fn float_sweep_rebuilds_when_insertion_becomes_non_monotonic() {
  let _lock = super::layout_profile_lock();
  let mut raw = HashMap::new();
  raw.insert("FASTR_LAYOUT_PROFILE".to_string(), "1".to_string());
  let toggles = Arc::new(RuntimeToggles::from_map(raw));

  with_thread_runtime_toggles(toggles, || {
    reset_float_profile_counters();

    let mut ctx = FloatContext::new(100.0);

    // Two left floats inserted in increasing Y.
    ctx.add_float_at(FloatSide::Left, 0.0, 0.0, 10.0, 100.0); // [0,100)
    ctx.add_float_at(FloatSide::Left, 0.0, 200.0, 10.0, 100.0); // [200,300)

    // Advance the sweep beyond the second float start.
    assert_eq!(ctx.available_width_at_y(250.0), (10.0, 90.0));

    // Insert an out-of-order (decreasing Y) float that is active at y=250.
    ctx.add_float_at(FloatSide::Right, 90.0, 50.0, 10.0, 300.0); // [50,350)

    // Querying at the same Y must now include the new float without requiring a backwards sweep.
    assert_eq!(ctx.available_width_at_y(250.0), (10.0, 80.0));
  });
}
