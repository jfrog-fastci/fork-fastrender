use crate::debug::runtime::{with_thread_runtime_toggles, RuntimeToggles};
use crate::layout::float_context::{
  float_profile_stats, reset_float_profile_counters, FloatContext, FloatSide,
};
use crate::layout::inline::float_integration::{InlineFloatIntegration, LineSpaceOptions};
use std::collections::HashMap;
use std::sync::Arc;

#[test]
fn inline_float_find_fit_no_double_range_query() {
  let _lock = super::layout_profile_lock();

  let mut raw = HashMap::new();
  raw.insert("FASTR_LAYOUT_PROFILE".to_string(), "1".to_string());
  let toggles = Arc::new(RuntimeToggles::from_map(raw));

  with_thread_runtime_toggles(toggles, || {
    let mut ctx = FloatContext::new(200.0);
    // Ensure `InlineFloatIntegration::has_floats()` is true, but keep the queried range free of
    // constraints so `find_fit` succeeds immediately (1 range scan per call).
    ctx.add_float_at(FloatSide::Left, 0.0, 10_000.0, 50.0, 10.0);
    let integration = InlineFloatIntegration::new(&ctx);

    reset_float_profile_counters();

    let n = 100u64;
    let opts = LineSpaceOptions::with_min_width(50.0).line_height(20.0);

    for _ in 0..n {
      let space = integration.find_line_space(0.0, opts);
      assert_eq!(space.y, 0.0);
      assert_eq!(space.left_edge, 0.0);
      assert_eq!(space.width, 200.0);
    }

    let stats = float_profile_stats();
    assert_eq!(
      stats.range_queries, n,
      "expected exactly one float range query per find_line_space() call"
    );
  });
}
