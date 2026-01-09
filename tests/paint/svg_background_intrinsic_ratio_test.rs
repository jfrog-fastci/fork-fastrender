use fastrender::geometry::Point;
use fastrender::image_loader::ImageCache;
use fastrender::paint::display_list_renderer::PaintParallelism;
use fastrender::paint::painter::{paint_tree_with_resources_scaled_offset_backend, PaintBackend};
use fastrender::scroll::ScrollState;
use fastrender::style::color::Rgba;
use fastrender::{FastRender, Pixmap};
use std::fs;

const HTML_PATH: &str = "tests/fixtures/html/svg_background_intrinsic_ratio.html";

fn pixel(pixmap: &Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let px = pixmap.pixel(x, y).expect("pixel in bounds");
  (px.red(), px.green(), px.blue(), px.alpha())
}

#[test]
fn svg_background_size_uses_viewbox_intrinsic_ratio() {
  let html = fs::read_to_string(HTML_PATH).expect("read fixture");
  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(&html).expect("parse html");
  let fragments = renderer.layout_document(&dom, 100, 10).expect("layout document");

  let pixmap = paint_tree_with_resources_scaled_offset_backend(
    &fragments,
    100,
    10,
    Rgba::WHITE,
    renderer.font_context().clone(),
    ImageCache::new(),
    1.0,
    Point::ZERO,
    PaintParallelism::default(),
    &ScrollState::default(),
    PaintBackend::DisplayList,
  )
  .expect("paint");

  // The SVG is a solid black rect filling the viewBox, and background-size sets the image to the
  // div's exact size. Pixels should be black throughout the box, not letterboxed.
  assert_eq!(pixel(&pixmap, 50, 2), (0, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 50, 7), (0, 0, 0, 255));
}
