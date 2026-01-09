use fastrender::debug::runtime::RuntimeToggles;
use fastrender::paint::display_list_renderer::PaintParallelism;
use fastrender::{FastRender, FastRenderConfig, FontConfig, LayoutParallelism};
use std::collections::HashMap;
use tiny_skia::Pixmap;

pub fn create_stacking_context_bounds_renderer() -> FastRender {
  create_renderer_with_backend("display_list")
}

pub fn create_stacking_context_bounds_renderer_legacy() -> FastRender {
  create_renderer_with_backend("legacy")
}

fn create_renderer_with_backend(backend: &str) -> FastRender {
  crate::rayon_test_util::init_rayon_for_tests(2);
  let toggles = RuntimeToggles::from_map(HashMap::from([
    ("FASTR_PAINT_BACKEND".to_string(), backend.to_string()),
  ]));
  let config = FastRenderConfig::new()
    .with_runtime_toggles(toggles)
    .with_font_sources(FontConfig::bundled_only())
    .with_layout_parallelism(LayoutParallelism::disabled())
    .with_paint_parallelism(PaintParallelism::disabled());
  FastRender::with_config(config).expect("renderer")
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
