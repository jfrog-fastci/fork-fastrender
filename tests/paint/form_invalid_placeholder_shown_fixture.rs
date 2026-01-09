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

fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let p = pixmap.pixel(x, y).unwrap();
  (p.red(), p.green(), p.blue(), p.alpha())
}

#[test]
fn fixture_invalid_and_placeholder_shown_pseudos_affect_paint_output() {
  let html = include_str!("../pages/fixtures/form_invalid_placeholder_shown/index.html");

  for backend in [PaintBackend::DisplayList, PaintBackend::Legacy] {
    let pixmap = render(html, 100, 70, backend);

    // Each input is 20px tall and stacked vertically:
    // - #empty (required + empty): :invalid + :placeholder-shown => blue
    // - #bad (invalid email): :invalid only => red
    // - #ok (valid email): base rule => green
    let (r, g, b, a) = pixel(&pixmap, 10, 10);
    assert!(
      a > 200 && b > 80 && b > r.saturating_add(80) && b > g.saturating_add(80),
      "expected #empty to be blue (backend={backend:?}, rgba=({r},{g},{b},{a}))"
    );

    let (r, g, b, a) = pixel(&pixmap, 10, 30);
    assert!(
      a > 200 && r > 80 && r > g.saturating_add(80) && r > b.saturating_add(80),
      "expected #bad to be red (backend={backend:?}, rgba=({r},{g},{b},{a}))"
    );

    let (r, g, b, a) = pixel(&pixmap, 10, 50);
    assert!(
      a > 200 && g > 80 && g > r.saturating_add(80) && g > b.saturating_add(80),
      "expected #ok to be green (backend={backend:?}, rgba=({r},{g},{b},{a}))"
    );
  }
}
