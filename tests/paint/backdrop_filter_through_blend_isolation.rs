use fastrender::image_loader::ImageCache;
use fastrender::paint::display_list_renderer::PaintParallelism;
use fastrender::paint::painter::paint_tree_with_resources_scaled_offset;
use fastrender::scroll::ScrollState;
use fastrender::{FastRender, Point, Rgba};
use std::sync::Once;

fn init_rayon() {
  static INIT: Once = Once::new();
  INIT.call_once(|| {
    // Under the tight `RLIMIT_AS` used by `scripts/run_limited.sh`, Rayon's default global pool can
    // fail to spawn (it tries to match the host CPU count). Force a tiny pool so painter internals
    // that rely on Rayon helpers (e.g. `rayon::current_num_threads`) are reliable when running
    // isolated test filters.
    rayon::ThreadPoolBuilder::new()
      .num_threads(1)
      .build_global()
      .expect("init global rayon pool");
  });
}

fn render(html: &str, width: u32, height: u32) -> tiny_skia::Pixmap {
  init_rayon();

  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parsed");
  let fragment_tree = renderer
    .layout_document(&dom, width, height)
    .expect("laid out");

  let font_ctx = renderer.font_context().clone();
  let image_cache = ImageCache::new();

  paint_tree_with_resources_scaled_offset(
    &fragment_tree,
    width,
    height,
    Rgba::WHITE,
    font_ctx,
    image_cache,
    1.0,
    Point::ZERO,
    // Keep painting deterministic; this test focuses on backdrop sampling through isolation layers.
    PaintParallelism::disabled(),
    &ScrollState::default(),
  )
  .expect("painted")
}

fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let p = pixmap.pixel(x, y).unwrap();
  (p.red(), p.green(), p.blue(), p.alpha())
}

fn assert_greenish(label: &str, (r, g, b, a): (u8, u8, u8, u8)) {
  assert!(
    r <= 15 && g >= 240 && b <= 15 && a >= 250,
    "{label}: expected green-ish rgba, got ({r},{g},{b},{a})"
  );
}

fn assert_redish(label: &str, (r, g, b, a): (u8, u8, u8, u8)) {
  assert!(
    r >= 240 && g <= 15 && b <= 15 && a >= 250,
    "{label}: expected red-ish rgba, got ({r},{g},{b},{a})"
  );
}

fn assert_magentaish(label: &str, (r, g, b, a): (u8, u8, u8, u8)) {
  assert!(
    r >= 240 && g <= 15 && b >= 240 && a >= 250,
    "{label}: expected magenta-ish rgba, got ({r},{g},{b},{a})"
  );
}

#[test]
fn backdrop_filter_samples_through_z_index_blend_isolation_layer() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(0 255 0); }
      #wrapper { position: relative; z-index: 0; width: 64px; height: 64px; }
      #blend {
        position: absolute;
        left: 0;
        top: 0;
        width: 20px;
        height: 20px;
        background: rgb(255 0 0);
        mix-blend-mode: difference;
      }
      #glass {
        position: absolute;
        left: 32px;
        top: 32px;
        width: 24px;
        height: 24px;
        backdrop-filter: invert(1);
      }
    </style>
    <div id="wrapper">
      <div id="blend"></div>
      <div id="glass"></div>
    </div>
  "#;

  let pixmap = render(html, 64, 64);

  // The mix-blend-mode descendant should isolate the wrapper: red over green should not blend to
  // yellow inside the isolated stacking context.
  assert_redish("blend isolation pixel", pixel(&pixmap, 10, 10));
  // Backdrop-filter sampling must see through the blend isolation layer to the page background.
  assert_magentaish("backdrop-filter pixel", pixel(&pixmap, 44, 44));
  // Control pixel outside the filtered region stays green.
  assert_greenish("control pixel", pixel(&pixmap, 60, 10));
}

#[test]
fn backdrop_filter_samples_through_transform_blend_isolation_layer() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(0 255 0); }
      #wrapper { position: relative; transform: translateX(0px); width: 64px; height: 64px; }
      #blend {
        position: absolute;
        left: 0;
        top: 0;
        width: 20px;
        height: 20px;
        background: rgb(255 0 0);
        mix-blend-mode: difference;
      }
      #glass {
        position: absolute;
        left: 32px;
        top: 32px;
        width: 24px;
        height: 24px;
        backdrop-filter: invert(1);
      }
    </style>
    <div id="wrapper">
      <div id="blend"></div>
      <div id="glass"></div>
    </div>
  "#;

  let pixmap = render(html, 64, 64);

  assert_redish("blend isolation pixel", pixel(&pixmap, 10, 10));
  assert_magentaish("backdrop-filter pixel", pixel(&pixmap, 44, 44));
  assert_greenish("control pixel", pixel(&pixmap, 60, 10));
}
