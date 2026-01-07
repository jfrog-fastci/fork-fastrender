use fastrender::debug::runtime::RuntimeToggles;
use fastrender::paint::display_list_renderer::PaintParallelism;
use fastrender::{FastRender, FastRenderConfig, FontConfig, LayoutParallelism};
use rayon::ThreadPoolBuilder;
use std::collections::HashMap;
use std::sync::Once;
use tiny_skia::Pixmap;

fn ensure_conservative_rayon_global_pool() {
  static INIT: Once = Once::new();
  INIT.call_once(|| {
    // In CI/container environments the default Rayon thread count (host CPU count) can be high
    // enough to fail global pool initialization (EAGAIN). Clamp the pool for stability under
    // `scripts/run_limited.sh`.
    if !std::env::var_os("RAYON_NUM_THREADS").is_some_and(|value| !value.is_empty()) {
      std::env::set_var("RAYON_NUM_THREADS", "2");
    }
    let _ = ThreadPoolBuilder::new().num_threads(2).build_global();
  });
}

pub fn create_stacking_context_bounds_renderer() -> FastRender {
  ensure_conservative_rayon_global_pool();
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "display_list".to_string(),
  )]));
  let config = FastRenderConfig::new()
    .with_runtime_toggles(toggles)
    .with_font_sources(FontConfig::bundled_only())
    .with_layout_parallelism(LayoutParallelism::disabled())
    .with_paint_parallelism(PaintParallelism::disabled());
  FastRender::with_config(config).expect("renderer")
}

pub fn create_layer_bounds_renderer() -> FastRender {
  create_stacking_context_bounds_renderer()
}

/// Compute the bounding box of pixels matching the predicate.
pub fn bounding_box_for_color<F>(pixmap: &Pixmap, predicate: F) -> Option<(u32, u32, u32, u32)>
where
  F: Fn((u8, u8, u8, u8)) -> bool,
{
  let mut min_x = u32::MAX;
  let mut min_y = u32::MAX;
  let mut max_x = 0u32;
  let mut max_y = 0u32;
  let mut seen = false;

  for y in 0..pixmap.height() {
    for x in 0..pixmap.width() {
      let p = pixmap.pixel(x, y).unwrap();
      let rgba = (p.red(), p.green(), p.blue(), p.alpha());
      if predicate(rgba) {
        seen = true;
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
      }
    }
  }

  if seen {
    Some((min_x, min_y, max_x, max_y))
  } else {
    None
  }
}
