use crate::common::global_test_lock;
use fastrender::api::{DiagnosticsLevel, FastRender, RenderOptions};
use fastrender::debug::runtime::RuntimeToggles;
use fastrender::style::cascade::{
  cascade_profile_enabled, reset_cascade_profile, set_cascade_profile_enabled,
};
use std::collections::HashMap;

struct CascadeProfileGuard {
  previous: bool,
}

impl CascadeProfileGuard {
  fn enable() -> Self {
    let previous = cascade_profile_enabled();
    set_cascade_profile_enabled(true);
    reset_cascade_profile();
    Self { previous }
  }
}

impl Drop for CascadeProfileGuard {
  fn drop(&mut self) {
    set_cascade_profile_enabled(self.previous);
  }
}

#[test]
fn cascade_profile_env_populates_cascade_diagnostics() {
  let _lock = global_test_lock();
  let _guard = CascadeProfileGuard::enable();

  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_CASCADE_PROFILE".to_string(),
    "1".to_string(),
  )]));

  let mut renderer = FastRender::new().expect("renderer");
  let html = r#"
    <style>
      div.foo { color: red; }
      span.bar { color: blue; }
      .baz .qux { margin: 0; }
    </style>
    <div class="foo baz">
      <span class="bar qux">hi</span>
    </div>
  "#;
  let options = RenderOptions::new()
    .with_viewport(64, 64)
    .with_diagnostics_level(DiagnosticsLevel::Basic)
    .with_runtime_toggles(toggles);
  let result = renderer
    .render_html_with_diagnostics(html, options)
    .expect("render");
  let stats = result
    .diagnostics
    .stats
    .expect("expected diagnostics stats with DiagnosticsLevel::Basic");

  let nodes = stats
    .cascade
    .nodes
    .expect("expected cascade.nodes when cascade profiling is enabled");
  assert!(nodes > 0, "expected cascade.nodes > 0");
  assert!(
    stats.cascade.rule_candidates.is_some(),
    "expected cascade.rule_candidates when cascade profiling is enabled"
  );
  assert!(
    stats.cascade.selector_time_ms.is_some(),
    "expected cascade.selector_time_ms when cascade profiling is enabled"
  );
}
