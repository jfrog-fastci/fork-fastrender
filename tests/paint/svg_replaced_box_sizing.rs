use fastrender::geometry::Point;
use fastrender::image_loader::ImageCache;
use fastrender::paint::display_list_renderer::PaintParallelism;
use fastrender::paint::painter::{paint_tree_with_resources_scaled_offset_backend, PaintBackend};
use fastrender::scroll::ScrollState;
use fastrender::style::color::Rgba;
use fastrender::{FastRender, Pixmap};

fn render_with_backend(html: &str, width: u32, height: u32, backend: PaintBackend) -> Pixmap {
  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parse html");
  let fragments = renderer
    .layout_document(&dom, width, height)
    .expect("layout document");

  paint_tree_with_resources_scaled_offset_backend(
    &fragments,
    width,
    height,
    Rgba::WHITE,
    renderer.font_context().clone(),
    ImageCache::new(),
    1.0,
    Point::ZERO,
    PaintParallelism::default(),
    &ScrollState::default(),
    backend,
  )
  .expect("paint")
}

fn pixel(pixmap: &Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let px = pixmap.pixel(x, y).expect("pixel in bounds");
  (px.red(), px.green(), px.blue(), px.alpha())
}

#[test]
fn inline_svg_viewport_uses_content_box_size_for_border_box() {
  let html = r#"
    <!doctype html>
    <style>
      body { margin: 0; background: white; }
      svg {
        display: block;
        width: 40px;
        height: 40px;
        padding: 8px;
        box-sizing: border-box;
      }
    </style>
    <svg xmlns="http://www.w3.org/2000/svg">
      <rect x="0" y="0" width="24" height="24" fill="black" />
    </svg>
  "#;

  let (width, height) = (40, 40);
  let expected_black = (0, 0, 0, 255);
  let expected_white = (255, 255, 255, 255);

  let legacy = render_with_backend(html, width, height, PaintBackend::Legacy);
  let display = render_with_backend(html, width, height, PaintBackend::DisplayList);

  for (label, pixmap) in [("legacy", &legacy), ("display list", &display)] {
    // The SVG's border box is 40x40 with 8px padding, so the content box is 24x24. The serialized
    // SVG should establish its viewport using the content box size (24x24); otherwise resvg scales
    // the 24x24 rect down and it no longer reaches the bottom/right edges.
    assert_eq!(pixel(pixmap, 30, 20), expected_black, "{label}: right edge");
    assert_eq!(pixel(pixmap, 20, 30), expected_black, "{label}: bottom edge");

    // Ensure we still treat padding as outside the replaced SVG content.
    assert_eq!(pixel(pixmap, 4, 20), expected_white, "{label}: left padding");
    assert_eq!(pixel(pixmap, 20, 4), expected_white, "{label}: top padding");
  }

  assert_eq!(
    legacy.data(),
    display.data(),
    "inline SVG output diverged between backends"
  );
}

