use fastrender::geometry::Point;
use fastrender::image_loader::ImageCache;
use fastrender::paint::display_list_renderer::PaintParallelism;
use fastrender::paint::painter::{paint_tree_with_resources_scaled_offset_backend, PaintBackend};
use fastrender::scroll::ScrollState;
use fastrender::style::color::Rgba;
use fastrender::{FastRender, Pixmap};
use std::fs;

const HTML_PATH: &str = "tests/fixtures/html/svg_background_auto_size_viewbox_only.html";

fn pixel(pixmap: &Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let px = pixmap.pixel(x, y).expect("pixel in bounds");
  (px.red(), px.green(), px.blue(), px.alpha())
}

#[test]
fn svg_background_with_no_intrinsic_size_uses_positioning_area_for_auto_size() {
  let html = fs::read_to_string(HTML_PATH).expect("read fixture");
  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(&html).expect("parse html");
  let fragments = renderer.layout_document(&dom, 140, 60).expect("layout document");

  let pixmap = paint_tree_with_resources_scaled_offset_backend(
    &fragments,
    140,
    60,
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

  // When `background-size` is omitted, SVG images that do not define an intrinsic size should be
  // laid out against the background positioning area rather than using the 300×150 default object
  // size. The SVG has a wide viewBox, so correct sizing should leave black letterboxing padding at
  // the top of the box.
  assert_eq!(pixel(&pixmap, 70, 10), (0, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 70, 30), (255, 255, 255, 255));
}

