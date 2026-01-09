use fastrender::image_loader::ImageCache;
use fastrender::paint::display_list_renderer::PaintParallelism;
use fastrender::paint::painter::{paint_tree_with_resources_scaled_offset_backend, PaintBackend};
use fastrender::scroll::ScrollState;
use fastrender::{FastRender, Point, Rgba};

fn render(html: &str, width: u32, height: u32, backend: PaintBackend) -> tiny_skia::Pixmap {
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
    backend,
  )
  .expect("painted")
}

fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let p = pixmap.pixel(x, y).unwrap();
  (p.red(), p.green(), p.blue(), p.alpha())
}

fn bounding_box_for_predicate<F>(pixmap: &tiny_skia::Pixmap, predicate: F) -> Option<(u32, u32, u32, u32)>
where
  F: Fn((u8, u8, u8, u8)) -> bool,
{
  let mut min_x = u32::MAX;
  let mut min_y = u32::MAX;
  let mut max_x = 0u32;
  let mut max_y = 0u32;
  let mut seen = false;

  for y in 0..pixmap.height() {
    for x in 0..pixmap.width() {
      if predicate(pixel(pixmap, x, y)) {
        seen = true;
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
      }
    }
  }

  seen.then_some((min_x, min_y, max_x, max_y))
}

#[test]
fn file_selector_button_pseudo_element_paints_under_appearance_none() {
  let html = r#"
    <!doctype html>
    <style>
      body { margin: 0; background: white; }
      #file {
        position: absolute;
        left: 0;
        top: 0;
        width: 160px;
        height: 30px;
        padding: 0;
        border: 0;
        appearance: none;
        background: rgb(0 255 0);
        color: transparent;
      }
      #file::file-selector-button,
      #file::-webkit-file-upload-button {
        display: inline-block;
        width: 60px;
        height: 30px;
        padding: 0;
        border: 0;
        border-radius: 0;
        margin: 0;
        background: rgb(255 0 0);
        color: transparent;
      }
    </style>
    <input id="file" type="file" />
  "#;

  for backend in [PaintBackend::DisplayList, PaintBackend::Legacy] {
    let pixmap = render(html, 200, 60, backend);
    let red_bounds = bounding_box_for_predicate(&pixmap, |(r, g, b, a)| {
      a > 200 && r > 200 && g < 50 && b < 50
    })
    .expect(&format!(
      "expected file-selector-button pseudo-element background to paint over the input background (backend={backend:?})"
    ));
    let (min_x, min_y, max_x, max_y) = red_bounds;
    assert!(
      max_x.saturating_sub(min_x) >= 5 && max_y.saturating_sub(min_y) >= 5,
      "expected a sizable red button rect (backend={backend:?}, bounds=({min_x},{min_y})..({max_x},{max_y}))"
    );
    assert_eq!(
      pixel(&pixmap, 120, 10),
      (0, 255, 0, 255),
      "expected input background to remain visible outside the button rect (backend={backend:?})"
    );
  }
}
