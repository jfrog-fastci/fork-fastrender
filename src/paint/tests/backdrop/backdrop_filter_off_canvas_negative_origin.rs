use crate::image_loader::ImageCache;
use crate::paint::display_list_renderer::PaintParallelism;
use crate::paint::painter::{paint_tree_with_resources_scaled_offset_backend, PaintBackend};
use crate::scroll::ScrollState;
use crate::{FastRender, Point, Rgba};

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
fn backdrop_filter_blur_renders_with_negative_layer_origin() {
  // Regression test for negative-origin backdrop-filter layers:
  //
  // When a backdrop-filtered element (or its filter kernel) extends outside the output canvas, we
  // must still build a correctly-sized offscreen layer and crop any active clip masks without
  // panicking on negative origins.
  const WIDTH: u32 = 80;
  const HEIGHT: u32 = 50;

  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; background: white; }
      #bar {
        position: absolute;
        left: 0;
        top: 0;
        width: 20px;
        height: 50px;
        background: black;
      }
      #overlay {
        position: absolute;
        left: -10px;
        top: 0;
        width: 40px;
        height: 50px;
        backdrop-filter: blur(10px);
      }
    </style>
    <div id="bar"></div>
    <div id="overlay"></div>
  "#;

  let pixmap = render_display_list(html, WIDTH, HEIGHT);

  let (r, g, b, a) = pixel(&pixmap, 0, HEIGHT / 2);
  assert_eq!(a, 255);
  assert!(
    r < 250 && g < 250 && b < 250,
    "expected backdrop-filter blur output near black/white boundary, got rgba=({r},{g},{b},{a})"
  );

  // Sanity: outside the overlay should remain untouched.
  assert_eq!(pixel(&pixmap, WIDTH - 1, HEIGHT / 2), (255, 255, 255, 255));
}
