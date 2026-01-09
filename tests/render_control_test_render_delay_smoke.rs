#[test]
fn test_render_delay_hook_is_linkable_without_browser_ui() {
  // Regression test: `render_control::set_test_render_delay_ms` must remain callable when the
  // library is built as a dependency (integration tests compile `fastrender` without `cfg(test)`).
  //
  // `spawn_ui_worker_for_test` uses this hook to inject deterministic delays into deadline checks
  // without requiring the `browser_ui` feature.
  fastrender::render_control::set_test_render_delay_ms(None);
}

