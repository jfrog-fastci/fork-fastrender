use fastrender::debug::runtime::RuntimeToggles;
use fastrender::paint::display_list_renderer::PaintParallelism;
use fastrender::{FastRender, FastRenderConfig, FontConfig, LayoutParallelism};
use rayon::ThreadPoolBuilder;
use resvg::tiny_skia::Pixmap;
use std::collections::HashMap;
use std::sync::Once;

static INIT_RAYON: Once = Once::new();

fn ensure_rayon_global_pool() {
  INIT_RAYON.call_once(|| {
    // Rayon defaults to spawning one worker per CPU; in constrained environments this can fail
    // global pool initialization (EAGAIN). Pre-initialize a conservative pool so paint tests are
    // stable under `scripts/run_limited.sh`.
    std::env::set_var("RAYON_NUM_THREADS", "2");
    let _ = ThreadPoolBuilder::new().num_threads(2).build_global();
  });
}

fn create_renderer() -> FastRender {
  ensure_rayon_global_pool();
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

fn color_at(pixmap: &Pixmap, x: u32, y: u32) -> [u8; 4] {
  let pixel = pixmap.pixel(x, y).expect("pixel");
  [pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()]
}

#[test]
fn stacking_context_layer_bounds_do_not_clip_outline() {
  let mut renderer = create_renderer();

  let html = r#"
  <style>
    body { margin: 0; background: black; }
    #box {
      position: absolute;
      left: 40px;
      top: 40px;
      width: 20px;
      height: 20px;
      background: blue;
      isolation: isolate;
      outline: 10px solid rgb(255, 0, 0);
      outline-offset: 5px;
    }
  </style>
  <div id="box"></div>
  "#;

  let pixmap = renderer.render_html(html, 120, 120).expect("render");

  let outline_px = color_at(&pixmap, 30, 50);
  assert!(
    outline_px[0] > outline_px[1] && outline_px[0] > outline_px[2] && outline_px[0] > 0,
    "expected outline to paint outside the border box, got {:?}",
    outline_px
  );
}
