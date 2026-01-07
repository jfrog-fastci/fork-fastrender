use fastrender::image_loader::ImageCache;
use fastrender::paint::display_list_renderer::PaintParallelism;
use fastrender::paint::painter::paint_tree_with_resources_scaled_offset;
use fastrender::scroll::ScrollState;
use fastrender::{FastRender, Point, Rgba};

fn render(html: &str, width: u32, height: u32) -> tiny_skia::Pixmap {
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
    // Keep painting deterministic; this test focuses on Backdrop Root boundaries.
    PaintParallelism::disabled(),
    &ScrollState::default(),
  )
  .expect("painted")
}

fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let p = pixmap.pixel(x, y).unwrap();
  (p.red(), p.green(), p.blue(), p.alpha())
}

#[test]
fn backdrop_filter_stops_at_filter_backdrop_root() {
  let html = r#"<!doctype html>
    <style>
      body { margin: 0; background: rgb(255 0 0); }
      #root { position: absolute; inset: 0; filter: blur(0px); }
      #overlay {
        position: absolute;
        left: 0;
        top: 0;
        width: 40px;
        height: 40px;
        backdrop-filter: invert(1);
      }
    </style>
    <div id="root"><div id="overlay"></div></div>
  "#;

  let pixmap = render(html, 64, 64);
  // If `filter` is treated as a Backdrop Root trigger, the backdrop-filter sampling for `#overlay`
  // must stop at `#root`. Since `#root` paints no background, sampling should *not* reach the body
  // background and the visible output should stay red.
  assert_eq!(pixel(&pixmap, 20, 20), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn backdrop_filter_stops_at_mask_backdrop_root() {
  let html = r#"<!doctype html>
    <style>
      body { margin: 0; background: rgb(255 0 0); }
      #root {
        position: absolute;
        inset: 0;
        mask-image: linear-gradient(to bottom, black 0% 100%);
        mask-mode: alpha;
        mask-repeat: no-repeat;
        mask-size: 100% 100%;
      }
      #overlay {
        position: absolute;
        left: 0;
        top: 0;
        width: 40px;
        height: 40px;
        backdrop-filter: invert(1);
      }
    </style>
    <div id="root"><div id="overlay"></div></div>
  "#;

  let pixmap = render(html, 64, 64);
  // `mask-image` should also trigger a Backdrop Root boundary. The chosen mask is fully opaque, so
  // it should not affect the output beyond stopping backdrop sampling at `#root`.
  assert_eq!(pixel(&pixmap, 20, 20), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

