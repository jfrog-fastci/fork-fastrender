use fastrender::debug::runtime::RuntimeToggles;
use fastrender::paint::display_list_renderer::PaintParallelism;
use fastrender::{FastRender, FastRenderConfig, FontConfig, LayoutParallelism};
use tiny_skia::Pixmap;
use std::collections::HashMap;
use std::sync::Once;

fn init_rayon_for_tests() {
  static INIT: Once = Once::new();
  INIT.call_once(|| {
    // Rayon lazily initializes a global thread pool on first use. In constrained environments the
    // default thread count (host CPU count) can be large enough to fail global pool init (EAGAIN),
    // which then panics on any subsequent Rayon usage. Pre-initialize a conservative pool so these
    // paint regressions are stable under `scripts/run_limited.sh`.
    std::env::set_var("RAYON_NUM_THREADS", "2");
    let _ = rayon::ThreadPoolBuilder::new().num_threads(2).build_global();
  });
}

fn render(html: &str, width: u32, height: u32) -> Pixmap {
  init_rayon_for_tests();
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "display_list".to_string(),
  )]));
  let config = FastRenderConfig::new()
    .with_runtime_toggles(toggles)
    .with_font_sources(FontConfig::bundled_only())
    .with_layout_parallelism(LayoutParallelism::disabled())
    .with_paint_parallelism(PaintParallelism::disabled());

  let mut renderer = FastRender::with_config(config).expect("renderer");
  renderer.render_html(html, width, height).expect("render")
}

fn color_at(pixmap: &Pixmap, x: u32, y: u32) -> [u8; 4] {
  let pixel = pixmap.pixel(x, y).expect("pixel");
  [pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()]
}

#[test]
fn stacking_context_layer_bounds_include_box_shadow_overflow() {
  let html = r#"
    <style>
      body { margin: 0; background: black; }
      #target {
        position: absolute;
        left: 40px;
        top: 40px;
        width: 20px;
        height: 20px;
        background: rgb(0, 0, 255);
        box-shadow: 0 0 0 10px rgb(255, 0, 0);
        isolation: isolate;
      }
    </style>
    <div id="target"></div>
  "#;

  let pixmap = render(html, 100, 100);

  let outside = color_at(&pixmap, 32, 50);
  assert!(
    outside[0] > outside[1] && outside[0] > outside[2] && outside[0] > 0,
    "expected box-shadow pixels outside the border box to remain visible, got {:?}",
    outside
  );
}
