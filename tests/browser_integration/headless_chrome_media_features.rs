use std::sync::Once;

use fastrender::paint::display_list_renderer::PaintParallelism;
use fastrender::{FastRender, FastRenderConfig, FontConfig, LayoutParallelism};

fn init_rayon_for_tests(num_threads: usize) {
  static INIT: Once = Once::new();
  let num_threads = num_threads.max(1);

  INIT.call_once(|| {
    if let Err(err) = rayon::ThreadPoolBuilder::new()
      .num_threads(num_threads)
      .build_global()
    {
      let already_initialized = std::panic::catch_unwind(|| rayon::current_num_threads()).is_ok();
      if !already_initialized {
        panic!("failed to initialize Rayon global pool for tests: {err}");
      }
    }
  });
}

#[test]
fn headless_chrome_media_features_default_to_no_input_devices() {
  // FastRender's pageset harness compares against headless Chrome. Chrome headless reports
  // `hover: none` and `pointer: none`, so ensure the renderer's default media context matches.
  init_rayon_for_tests(2);

  let config = FastRenderConfig::new()
    .with_font_sources(FontConfig::bundled_only())
    .with_layout_parallelism(LayoutParallelism::disabled())
    .with_paint_parallelism(PaintParallelism::disabled());
  let mut renderer = FastRender::with_config(config).expect("renderer");

  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; }
      .box { width: 20px; height: 20px; background: #f00; }
      #pointer { margin-top: 20px; }
      @media (hover: none) { #hover { background: #0f0; } }
      @media (hover: hover) { #hover { background: #00f; } }
      @media (pointer: none) { #pointer { background: #000; } }
      @media (pointer: fine) { #pointer { background: #ff0; } }
    </style>
    <div id="hover" class="box"></div>
    <div id="pointer" class="box"></div>
  "#;

  let pixmap = renderer.render_html(html, 40, 60).expect("render");

  let hover_pixel = pixmap.pixel(10, 10).expect("hover pixel");
  assert!(
    hover_pixel.red() < 30
      && hover_pixel.green() > 220
      && hover_pixel.blue() < 30
      && hover_pixel.alpha() > 200,
    "expected `@media (hover: none)` to match; got rgba({}, {}, {}, {})",
    hover_pixel.red(),
    hover_pixel.green(),
    hover_pixel.blue(),
    hover_pixel.alpha()
  );

  let pointer_pixel = pixmap.pixel(10, 50).expect("pointer pixel");
  assert!(
    pointer_pixel.red() < 30
      && pointer_pixel.green() < 30
      && pointer_pixel.blue() < 30
      && pointer_pixel.alpha() > 200,
    "expected `@media (pointer: none)` to match; got rgba({}, {}, {}, {})",
    pointer_pixel.red(),
    pointer_pixel.green(),
    pointer_pixel.blue(),
    pointer_pixel.alpha()
  );
}

