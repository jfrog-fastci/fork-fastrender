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
fn backdrop_filter_blur_clips_rounded_corners_with_negative_origin() {
  // Regression test for clipped backdrop-filter writes when the filtered element has a negative
  // origin (partially off-canvas). This exercises the slow-path masked writeback (rounded corners)
  // combined with out-of-bounds backdrop sampling.
  const WIDTH: u32 = 60;
  const HEIGHT: u32 = 50;

  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; background: rgb(0, 255, 0); }

      #bar {
        position: absolute;
        left: 8px;
        top: 0;
        width: 20px;
        height: 50px;
        background: rgb(255, 0, 0);
      }

      #overlay {
        position: absolute;
        left: -4px;
        top: 0;
        width: 40px;
        height: 50px;
        border-radius: 12px;
        overflow: hidden;
        backdrop-filter: blur(16px);
      }
    </style>
    <div id="bar"></div>
    <div id="overlay"></div>
  "#;

  let pixmap = render_display_list(html, WIDTH, HEIGHT);

  // The rounded corner should clip out the backdrop-filter effect at the top-left pixel, leaving
  // the green page background untouched.
  assert_eq!(pixel(&pixmap, 0, 0), (0, 255, 0, 255));

  // Just inside the overlay, the blur should mix the red bar into the green background.
  let (r, g, b, a) = pixel(&pixmap, 7, HEIGHT / 2);
  assert_eq!(a, 255);
  assert!(
    r > 0 && g < 255 && b < 5,
    "expected blur to mix red+green near boundary, got rgba=({r},{g},{b},{a})"
  );

  // Outside the overlay region should remain untouched.
  assert_eq!(pixel(&pixmap, WIDTH - 1, HEIGHT / 2), (0, 255, 0, 255));
}

