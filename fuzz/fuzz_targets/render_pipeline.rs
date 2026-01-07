#![no_main]

use fastrender::debug::runtime::RuntimeToggles;
use fastrender::{
  AllowedSchemes, FastRender, FastRenderConfig, LayoutParallelism, PaintParallelism, RenderOptions,
  ResourcePolicy,
};
use libfuzzer_sys::fuzz_target;
use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Duration;

const MAX_INPUT_LEN: usize = 64 * 1024;
const VIEWPORT: u32 = 256;
const DPR: f32 = 1.0;
const RENDER_TIMEOUT: Duration = Duration::from_millis(50);

thread_local! {
  static RENDERER: RefCell<Option<FastRender>> = RefCell::new(None);
}

fn build_renderer() -> Option<FastRender> {
  let policy = ResourcePolicy::new()
    .with_allowed_schemes(AllowedSchemes {
      http: false,
      https: false,
      file: false,
      data: true,
    })
    // Cap individual responses even though the fuzz harness also caps total input size.
    .with_max_response_bytes(256 * 1024)
    // Redirects only matter for HTTP(S), which is disabled above.
    .with_max_redirects(1);

  let runtime_toggles = RuntimeToggles::from_map(HashMap::new());

  let mut config = FastRenderConfig::new()
    .with_default_viewport(VIEWPORT, VIEWPORT)
    .with_device_pixel_ratio(DPR)
    .with_resource_policy(policy)
    .with_max_iframe_depth(1)
    .with_runtime_toggles(runtime_toggles)
    .with_paint_parallelism(PaintParallelism::disabled())
    .with_layout_parallelism(LayoutParallelism::disabled());
  // Avoid pixmap blow-ups from pathological layout bounds.
  config.fit_canvas_to_content = false;
  // Avoid caching resources across fuzz iterations.
  config.resource_cache = None;

  FastRender::with_config(config).ok()
}

fn render_input(renderer: &mut FastRender, html: &str) {
  let options = RenderOptions::new()
    .with_viewport(VIEWPORT, VIEWPORT)
    .with_device_pixel_ratio(DPR)
    .with_fit_canvas_to_content(false)
    .with_timeout(Some(RENDER_TIMEOUT))
    .with_paint_parallelism(PaintParallelism::disabled())
    .with_layout_parallelism(LayoutParallelism::disabled())
    // Avoid pathological "link rel=stylesheet" storms (inputs are already capped, but this is
    // cheap extra defense).
    .with_stylesheet_limit(Some(16));

  let _ = renderer.render_html_with_options(html, options);
}

fuzz_target!(|data: &[u8]| {
  let bytes = if data.len() > MAX_INPUT_LEN {
    &data[..MAX_INPUT_LEN]
  } else {
    data
  };
  let html = String::from_utf8_lossy(bytes);

  RENDERER.with(|cell| {
    if cell.borrow().is_none() {
      *cell.borrow_mut() = build_renderer();
    }
    let mut guard = cell.borrow_mut();
    let Some(renderer) = guard.as_mut() else {
      return;
    };
    render_input(renderer, html.as_ref());
  });
});

