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

#[test]
fn mix_blend_mode_difference_without_intermediate_stacking_context() {
  let html = r#"
    <!doctype html>
    <style>
      html, body { margin: 0; background: rgb(0 255 0); }
      .container { width: 20px; height: 20px; }
      .blend { width: 20px; height: 20px; background: rgb(255 0 0); mix-blend-mode: difference; }
    </style>
    <div class="container">
      <div class="blend"></div>
    </div>
  "#;

  let pixmap = render(html, 32, 32);
  assert_eq!(
    pixel(&pixmap, 25, 25),
    (0, 255, 0, 255),
    "background should be lime"
  );

  let (r, g, b, a) = pixel(&pixmap, 10, 10);
  assert!(
    r >= 240 && g >= 240 && b <= 10 && a >= 250,
    "expected yellow-ish output from `difference(red, lime)`, got ({r},{g},{b},{a})"
  );
}

#[test]
fn mix_blend_mode_difference_isolated_by_z_index_stacking_context() {
  let html = r#"
    <!doctype html>
    <style>
      html, body { margin: 0; background: rgb(0 255 0); }
      .container { width: 20px; height: 20px; position: relative; z-index: 0; }
      .blend { width: 20px; height: 20px; background: rgb(255 0 0); mix-blend-mode: difference; }
    </style>
    <div class="container">
      <div class="blend"></div>
    </div>
  "#;

  let pixmap = render(html, 32, 32);
  assert_eq!(
    pixel(&pixmap, 25, 25),
    (0, 255, 0, 255),
    "background should be lime"
  );

  let (r, g, b, a) = pixel(&pixmap, 10, 10);
  assert!(
    r >= 240 && g <= 10 && b <= 10 && a >= 250,
    "expected red-ish output from `difference(red, transparent)` inside isolated stacking context, got ({r},{g},{b},{a})"
  );
}

#[test]
fn mix_blend_mode_difference_isolated_by_transform_stacking_context() {
  let html = r#"
    <!doctype html>
    <style>
      html, body { margin: 0; background: rgb(0 255 0); }
      .container { width: 20px; height: 20px; transform: translateX(0px); }
      .blend { width: 20px; height: 20px; background: rgb(255 0 0); mix-blend-mode: difference; }
    </style>
    <div class="container">
      <div class="blend"></div>
    </div>
  "#;

  let pixmap = render(html, 32, 32);
  assert_eq!(
    pixel(&pixmap, 25, 25),
    (0, 255, 0, 255),
    "background should be lime"
  );

  let (r, g, b, a) = pixel(&pixmap, 10, 10);
  assert!(
    r >= 240 && g <= 10 && b <= 10 && a >= 250,
    "expected red-ish output from `difference(red, transparent)` inside isolated stacking context, got ({r},{g},{b},{a})"
  );
}

