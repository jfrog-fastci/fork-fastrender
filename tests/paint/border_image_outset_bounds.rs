use fastrender::debug::runtime::RuntimeToggles;
use fastrender::paint::display_list_renderer::PaintParallelism;
use fastrender::{FastRender, FastRenderConfig, FontConfig, LayoutParallelism, Pixmap};
use rayon::ThreadPoolBuilder;
use std::collections::HashMap;
use std::sync::Once;

fn init_rayon_for_tests() {
  static INIT: Once = Once::new();
  INIT.call_once(|| {
    // Rayon defaults to spawning one worker per CPU; in constrained environments this can fail
    // global pool initialization (EAGAIN). Pre-initialize a conservative pool so paint tests are
    // stable under `scripts/run_limited.sh`.
    std::env::set_var("RAYON_NUM_THREADS", "2");
    let _ = ThreadPoolBuilder::new().num_threads(2).build_global();
  });
}

fn create_renderer() -> FastRender {
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
  FastRender::with_config(config).expect("renderer")
}

fn pixel(pixmap: &Pixmap, x: u32, y: u32) -> [u8; 4] {
  let idx = (y as usize * pixmap.width() as usize + x as usize) * 4;
  let data = pixmap.data();
  [data[idx], data[idx + 1], data[idx + 2], data[idx + 3]]
}

#[test]
fn border_image_outset_extends_stacking_context_bounds() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = create_renderer();
      let html = r#"
      <style>
        body { margin: 0; background: rgb(0, 0, 0); }
        #target {
          position: absolute;
          left: 40px;
          top: 40px;
          width: 20px;
          height: 20px;
          box-sizing: border-box;
          border: 2px solid transparent;
          border-image-source: linear-gradient(rgb(255, 0, 0), rgb(255, 0, 0));
          border-image-slice: 1;
          border-image-repeat: stretch;
          border-image-outset: 10px;
          isolation: isolate;
        }
      </style>
      <div id="target"></div>
      "#;

      let pixmap = renderer.render_html(html, 100, 100).expect("render");

      // The border box spans (40,40)-(60,60). The border image should paint with an outset of
      // 10px, reaching left to x=30. Sample just outside the border box but within that outset.
      let sample = pixel(&pixmap, 31, 50);
      assert!(
        sample[0] > 200 && sample[1] < 50 && sample[2] < 50 && sample[3] > 200,
        "expected border-image-outset pixels to be red-ish, got {sample:?}"
      );
    })
    .unwrap()
    .join()
    .unwrap();
}
