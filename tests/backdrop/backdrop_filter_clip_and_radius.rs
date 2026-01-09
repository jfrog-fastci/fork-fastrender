use fastrender::image_loader::ImageCache;
use fastrender::paint::display_list_renderer::PaintParallelism;
use fastrender::paint::painter::{paint_tree_with_resources_scaled_offset_backend, PaintBackend};
use fastrender::scroll::ScrollState;
use fastrender::{FastRender, Point, Rgba};

fn render_display_list(html: &str, width: u32, height: u32) -> tiny_skia::Pixmap {
  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parsed");
  let fragment_tree = renderer
    .layout_document(&dom, width, height)
    .expect("laid out");

  let font_ctx = renderer.font_context().clone();
  let image_cache = ImageCache::new();

  paint_tree_with_resources_scaled_offset_backend(
    &fragment_tree,
    width,
    height,
    Rgba::WHITE,
    font_ctx,
    image_cache,
    1.0,
    Point::ZERO,
    PaintParallelism::disabled(),
    &ScrollState::default(),
    PaintBackend::DisplayList,
  )
  .expect("painted")
}

fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let p = pixmap
    .pixel(x, y)
    .unwrap_or_else(|| panic!("pixel out of bounds: {x},{y}"));
  (p.red(), p.green(), p.blue(), p.alpha())
}

#[test]
fn backdrop_filter_snapshot_respects_overflow_clip_and_border_radius() {
  const WIDTH: u32 = 120;
  const HEIGHT: u32 = 80;

  let html = r#"
    <!doctype html>
    <style>
      body { margin: 0; background: rgb(0, 255, 0); }

      #container {
        position: absolute;
        left: 20px;
        top: 20px;
        width: 60px;
        height: 40px;
        overflow: hidden;
        border-radius: 8px;
      }

      #left {
        position: absolute;
        left: 0;
        top: 0;
        width: 40px;
        height: 40px;
        background: rgb(255, 0, 0);
      }

      #right {
        position: absolute;
        left: 40px;
        top: 0;
        width: 20px;
        height: 40px;
        background: rgb(0, 0, 255);
      }

      #overlay {
        position: absolute;
        inset: 0;
        backdrop-filter: blur(16px);
      }
    </style>
    <div id="container">
      <div id="left"></div>
      <div id="right"></div>
      <div id="overlay"></div>
    </div>
  "#;

  let pixmap = render_display_list(html, WIDTH, HEIGHT);

  let boundary_px = pixel(&pixmap, 20 + 40, 20 + 20);
  assert!(
    boundary_px.0 > 0 && boundary_px.2 > 0,
    "expected backdrop-filter blur to mix red+blue at boundary, got {boundary_px:?}"
  );

  let edge_px = pixel(&pixmap, 20 + 3, 20 + 20);
  assert!(
    edge_px.0 > 250 && edge_px.1 < 5 && edge_px.2 < 5 && edge_px.3 == 255,
    "expected clipped backdrop-filter edge pixel to stay red, got {edge_px:?}"
  );
}
