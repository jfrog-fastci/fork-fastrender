use super::util::bounding_box_for_color;
use fastrender::image_loader::ImageCache;
use fastrender::paint::display_list_renderer::PaintParallelism;
use fastrender::paint::painter::{paint_tree_with_resources_scaled_offset_backend, PaintBackend};
use fastrender::scroll::ScrollState;
use fastrender::{FastRender, Point, Rgba};

fn render_with_backend(
  html: &str,
  width: u32,
  height: u32,
  backend: PaintBackend,
) -> tiny_skia::Pixmap {
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
    Rgba::TRANSPARENT,
    font_ctx,
    image_cache,
    1.0,
    Point::ZERO,
    PaintParallelism::disabled(),
    &ScrollState::default(),
    backend,
  )
  .expect("painted")
}

#[test]
fn filter_blur_calc_length_resolves_and_paints() {
  const WIDTH: u32 = 120;
  const HEIGHT: u32 = 120;

  let html = r#"
    <!doctype html>
    <style>
      body { margin: 0; background: transparent; }
      #target {
        position: absolute;
        left: 40px;
        top: 40px;
        width: 20px;
        height: 20px;
        background: rgb(255, 0, 0);
        filter: blur(calc(2px + 3px));
      }
    </style>
    <div id="target"></div>
  "#;

  let legacy = render_with_backend(html, WIDTH, HEIGHT, PaintBackend::Legacy);
  let display = render_with_backend(html, WIDTH, HEIGHT, PaintBackend::DisplayList);

  for (label, pixmap) in [("legacy", &legacy), ("display_list", &display)] {
    let bbox = bounding_box_for_color(pixmap, |(_, _, _, a)| a != 0)
      .unwrap_or_else(|| panic!("{label} backend produced no painted pixels"));
    let (min_x, min_y, max_x, max_y) = bbox;
    assert!(
      min_x < 40 && min_y < 40 && max_x > 59 && max_y > 59,
      "{label} backend did not apply blur() with calc() length (bbox={bbox:?})"
    );
  }

  assert_eq!(
    legacy.data(),
    display.data(),
    "blur(calc()) output should match between legacy and display-list backends"
  );
}
