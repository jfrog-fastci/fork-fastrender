use fastrender::debug::runtime::RuntimeToggles;
use fastrender::{FastRender, FastRenderConfig, RenderOptions};
use std::collections::HashMap;

#[test]
fn trace_file_includes_pipeline_events() {
  const STACK_SIZE: usize = 64 * 1024 * 1024;

  std::thread::Builder::new()
    .name("trace_file_includes_pipeline_events".to_string())
    .stack_size(STACK_SIZE)
    .spawn(|| {
       let dir = tempfile::tempdir().expect("tempdir");
       let trace_path = dir.path().join("trace.json");

      // Force the display-list paint backend so trace output coverage doesn't depend on ambient
      // environment variables (and so regressions in the display-list pipeline are caught).
      let toggles = RuntimeToggles::from_map(HashMap::from([(
        "FASTR_PAINT_BACKEND".to_string(),
        "display_list".to_string(),
      )]));
      let config = FastRenderConfig::default().with_runtime_toggles(toggles);
      let mut renderer = FastRender::with_config(config).expect("renderer");
      let options = RenderOptions::new()
        .with_viewport(64, 64)
        .with_trace_output(trace_path.clone());

      renderer
        .render_html_with_options("<div>trace me</div>", options)
        .expect("render");

      let data = std::fs::read_to_string(&trace_path).expect("trace output exists");
      assert!(data.contains("\"dom_parse\""), "dom parse span missing");
      assert!(data.contains("\"css_parse\""), "css parse span missing");
      assert!(data.contains("\"layout\""), "layout span missing");
      assert!(data.contains("\"layout_tree\""), "layout_tree span missing");
      assert!(
        data.contains("\"display_list_build\""),
        "display list build span missing"
      );
      assert!(
        data.contains("\"display_list_optimize\""),
        "display list optimize span missing"
      );
      assert!(data.contains("\"rasterize\""), "rasterization span missing");
    })
    .expect("spawn render thread")
    .join()
    .expect("render thread panicked");
}

#[test]
fn trace_file_includes_prepare_events() {
  const STACK_SIZE: usize = 64 * 1024 * 1024;

  std::thread::Builder::new()
    .name("trace_file_includes_prepare_events".to_string())
    .stack_size(STACK_SIZE)
    .spawn(|| {
      let dir = tempfile::tempdir().expect("tempdir");
      let trace_path = dir.path().join("prepare_trace.json");

      let mut renderer = FastRender::new().expect("renderer");
      let options = RenderOptions::new()
        .with_viewport(64, 64)
        .with_trace_output(trace_path.clone());

      renderer
        .prepare_html("<div>trace me</div>", options)
        .expect("prepare");

      let data = std::fs::read_to_string(&trace_path).expect("trace output exists");
      assert!(data.contains("\"dom_parse\""), "dom parse span missing");
      assert!(data.contains("\"css_parse\""), "css parse span missing");
      assert!(data.contains("\"layout_tree\""), "layout_tree span missing");
    })
    .expect("spawn prepare thread")
    .join()
    .expect("prepare thread panicked");
}
