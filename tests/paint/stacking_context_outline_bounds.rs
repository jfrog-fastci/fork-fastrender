use fastrender::debug::runtime::RuntimeToggles;
use fastrender::{FastRender, FastRenderConfig};
use rayon::ThreadPoolBuilder;
use resvg::tiny_skia::Pixmap;
use std::collections::HashMap;
use std::sync::Once;

static INIT_RAYON: Once = Once::new();

fn ensure_rayon_global_pool() {
  INIT_RAYON.call_once(|| {
    let _ = ThreadPoolBuilder::new().num_threads(1).build_global();
  });
}

fn color_at(pixmap: &Pixmap, x: u32, y: u32) -> [u8; 4] {
  let pixel = pixmap.pixel(x, y).expect("pixel");
  [pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()]
}

#[test]
fn stacking_context_layer_bounds_do_not_clip_outline() {
  ensure_rayon_global_pool();

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

  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "display_list".to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);
  let mut renderer = FastRender::with_config(config).expect("renderer");
  let pixmap = renderer.render_html(html, 120, 120).expect("render");

  let outline_px = color_at(&pixmap, 30, 50);
  assert!(
    outline_px[0] > outline_px[1] && outline_px[0] > outline_px[2] && outline_px[0] > 0,
    "expected outline to paint outside the border box, got {:?}",
    outline_px
  );
}
