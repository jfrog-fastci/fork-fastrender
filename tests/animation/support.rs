use std::sync::Once;

use fastrender::debug::runtime::RuntimeToggles;
use fastrender::{BrowserDocument, FastRender, FontConfig, RenderOptions, Result};

static INIT: Once = Once::new();

pub fn ensure_test_env() {
  INIT.call_once(|| {
    // FastRender uses Rayon for parallel layout/paint. Rayon defaults to the host CPU count, which
    // can exceed sandbox thread budgets and cause the global pool init to fail.
    let threads = std::env::var("RAYON_NUM_THREADS")
      .ok()
      .and_then(|value| value.parse::<usize>().ok())
      .unwrap_or(1)
      .max(1);
    // Do not mutate process environment variables here; integration tests run in a shared process.
    crate::common::rayon_test_util::init_rayon_for_tests(threads);
  });
}

pub fn create_test_renderer() -> FastRender {
  ensure_test_env();
  FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    // Avoid letting host `FASTR_*` env vars influence suite results.
    .runtime_toggles(RuntimeToggles::default())
    .build()
    .expect("renderer")
}

// Some existing tests use this older helper name.
pub fn test_renderer() -> FastRender {
  create_test_renderer()
}

pub fn create_test_document(html: &str, options: RenderOptions) -> Result<BrowserDocument> {
  BrowserDocument::new(create_test_renderer(), html, options)
}

pub fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let px = pixmap.pixel(x, y).unwrap_or_else(|| {
    panic!(
      "pixel({x}, {y}) out of bounds (pixmap size {}x{})",
      pixmap.width(),
      pixmap.height()
    )
  });
  (px.red(), px.green(), px.blue(), px.alpha())
}
