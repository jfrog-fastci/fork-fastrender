use fastrender::debug::runtime::RuntimeToggles;
use fastrender::paint::display_list_renderer::PaintParallelism;
use fastrender::{FastRender, FastRenderConfig, FontConfig, LayoutParallelism};
use rayon::ThreadPoolBuilder;
use std::collections::HashMap;
use std::sync::Once;

fn init_rayon_for_tests() {
  static INIT: Once = Once::new();
  INIT.call_once(|| {
    // In constrained environments Rayon global pool initialization can fail when it tries to
    // spawn many threads (default = host CPU count). Pre-initialize a conservative pool so paint
    // regressions don't panic under `scripts/run_limited.sh`.
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
  FastRender::with_config(config).expect("renderer should construct")
}

#[test]
fn stacking_context_layer_bounds_include_descendant_paint_overflow() {
  let mut renderer = create_renderer();

  // Regression fixture:
  // - `#outer` establishes a stacking context and therefore a bounded compositing layer.
  // - `#inner` paints a box-shadow that extends outside `#outer`'s border box.
  // - `#mid` is a non-stacking wrapper, so `#inner` may not appear as a top-level fragment in the
  //   stacking context layer list.
  //
  // Without bounds expansion that considers descendant paint overflow, the shadow will be clipped
  // to the stacking context bounds and the sampled pixel will remain background black.
  let html = r#"<!doctype html>
    <style>
      body { margin:0; background:black; }
      #outer {
        isolation:isolate;
        position:absolute;
        left:40px;
        top:40px;
        width:20px;
        height:20px;
      }
      #inner {
        width:20px;
        height:20px;
        background:blue;
        box-shadow: 0 0 0 10px rgb(255,0,0);
      }
    </style>
    <div id="outer"><div id="mid"><div id="inner"></div></div></div>
  "#;

  let pixmap = renderer
    .render_html(html, 100, 100)
    .expect("render should succeed");

  // Sample a pixel that is outside `#outer`'s border box (x < 40) but within `#inner`'s shadow
  // region (shadow extends to x=30..70).
  let p = pixmap.pixel(32, 50).expect("pixel inside viewport");
  assert!(
    p.red() > p.green() && p.red() > p.blue() && p.red() > 0,
    "expected descendant shadow to be visible outside stacking-context bounds, got rgba({}, {}, {}, {})",
    p.red(),
    p.green(),
    p.blue(),
    p.alpha()
  );
}
