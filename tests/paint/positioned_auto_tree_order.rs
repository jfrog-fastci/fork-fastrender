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
    PaintParallelism::default(),
    &ScrollState::default(),
  )
  .expect("painted")
}

fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let p = pixmap.pixel(x, y).unwrap();
  (p.red(), p.green(), p.blue(), p.alpha())
}

fn assert_close(actual: (u8, u8, u8, u8), expected: (u8, u8, u8, u8), tol: u8) {
  let diff = (
    actual.0.abs_diff(expected.0),
    actual.1.abs_diff(expected.1),
    actual.2.abs_diff(expected.2),
    actual.3.abs_diff(expected.3),
  );
  assert!(
    diff.0 <= tol && diff.1 <= tol && diff.2 <= tol && diff.3 <= tol,
    "pixel {:?} differed from {:?} by {:?} (tol {tol})",
    actual,
    expected,
    diff
  );
}

#[test]
fn positioned_auto_paints_in_dom_tree_order() {
  // Regression: `position: absolute` boxes are laid out out-of-flow and can be emitted after
  // in-flow content in the fragment tree. Stacking (CSS2.1 Appendix E) requires *tree order*
  // (DOM/box-tree order) for layer-6 painting of positioned descendants with `z-index: auto/0`.
  //
  // When correct, the later DOM sibling (`.rel`) must paint on top of the earlier DOM sibling
  // (`.abs`) in the overlap region.
  let html = r#"
    <!doctype html>
    <style>
      body { margin: 0; background: white; }
      .container { position: relative; width: 60px; height: 60px; }
      .abs { position: absolute; left: 0; top: 0; width: 40px; height: 40px; background: rgb(255 0 0); }
      .rel { position: relative; left: 10px; top: 10px; width: 20px; height: 20px; background: rgb(0 0 255); }
    </style>
    <div class="container">
      <div class="abs"></div>
      <div class="rel"></div>
    </div>
  "#;

  let pixmap = render(html, 64, 64);
  // A-only region.
  assert_close(pixel(&pixmap, 5, 5), (255, 0, 0, 255), 2);
  // Overlap region: later DOM sibling must be on top.
  assert_close(pixel(&pixmap, 15, 15), (0, 0, 255, 255), 2);
  // Background.
  assert_close(pixel(&pixmap, 55, 55), (255, 255, 255, 255), 2);
}

