use fastrender::image_loader::ImageCache;
use fastrender::paint::display_list_renderer::PaintParallelism;
use fastrender::paint::painter::{paint_tree_with_resources_scaled_offset, paint_tree_with_resources_scaled_offset_backend, PaintBackend};
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
    Rgba::WHITE,
    font_ctx,
    image_cache,
    1.0,
    Point::ZERO,
    PaintParallelism::default(),
    &ScrollState::default(),
    backend,
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
fn mix_blend_mode_isolation_baseline_no_intermediate_stacking_context() {
  let html = r#"
    <!doctype html>
    <style>
      html, body { background: lime; margin: 0; }
      .container { width: 40px; height: 40px; }
      .blend { width: 40px; height: 40px; background: rgb(255 0 0); mix-blend-mode: difference; }
    </style>
    <div class="container">
      <div class="blend"></div>
    </div>
  "#;

  let pixmap = render(html, 64, 64);
  assert_close(pixel(&pixmap, 60, 60), (0, 255, 0, 255), 2);
  // Red (source) over lime (backdrop) with `difference` should yield yellow.
  assert_close(pixel(&pixmap, 20, 20), (255, 255, 0, 255), 5);
}

#[test]
fn mix_blend_mode_isolation_container_z_index_scopes_descendant_blend() {
  let html = r#"
    <!doctype html>
    <style>
      html, body { background: lime; margin: 0; }
      /* `position` + non-auto `z-index` establishes a stacking context. */
      .container {
        position: relative;
        z-index: 0;
        width: 40px;
        height: 40px;
      }
      .blend {
        width: 40px;
        height: 40px;
        background: rgb(255 0 0);
        mix-blend-mode: difference;
      }
    </style>
    <div class="container">
      <div class="blend"></div>
    </div>
  "#;

  let pixmap = render(html, 64, 64);
  assert_close(pixel(&pixmap, 60, 60), (0, 255, 0, 255), 2);
  // With a stacking-context boundary between the element and the page backdrop, the blend should
  // be scoped to an empty (transparent) backdrop, preserving the original red.
  assert_close(pixel(&pixmap, 20, 20), (255, 0, 0, 255), 5);
}

#[test]
fn mix_blend_mode_isolation_container_z_index_scopes_descendant_blend_legacy_backend() {
  let html = r#"
    <!doctype html>
    <style>
      html, body { background: lime; margin: 0; }
      /* `position` + non-auto `z-index` establishes a stacking context. */
      .container {
        position: relative;
        z-index: 0;
        width: 40px;
        height: 40px;
      }
      .blend {
        width: 40px;
        height: 40px;
        background: rgb(255 0 0);
        mix-blend-mode: difference;
      }
    </style>
    <div class="container">
      <div class="blend"></div>
    </div>
  "#;

  let pixmap = render_with_backend(html, 64, 64, PaintBackend::Legacy);
  assert_close(pixel(&pixmap, 60, 60), (0, 255, 0, 255), 2);
  assert_close(pixel(&pixmap, 20, 20), (255, 0, 0, 255), 5);
}

#[test]
fn mix_blend_mode_isolation_container_transform_scopes_descendant_blend() {
  let html = r#"
    <!doctype html>
    <style>
      html, body { background: lime; margin: 0; }
      /* `transform` establishes a stacking context. */
      .container {
        transform: translateX(0px);
        width: 40px;
        height: 40px;
      }
      .blend {
        width: 40px;
        height: 40px;
        background: rgb(255 0 0);
        mix-blend-mode: difference;
      }
    </style>
    <div class="container">
      <div class="blend"></div>
    </div>
  "#;

  let pixmap = render(html, 64, 64);
  assert_close(pixel(&pixmap, 60, 60), (0, 255, 0, 255), 2);
  assert_close(pixel(&pixmap, 20, 20), (255, 0, 0, 255), 5);
}

#[test]
fn mix_blend_mode_isolation_container_transform_scopes_descendant_blend_legacy_backend() {
  let html = r#"
    <!doctype html>
    <style>
      html, body { background: lime; margin: 0; }
      /* `transform` establishes a stacking context. */
      .container {
        transform: translateX(0px);
        width: 40px;
        height: 40px;
      }
      .blend {
        width: 40px;
        height: 40px;
        background: rgb(255 0 0);
        mix-blend-mode: difference;
      }
    </style>
    <div class="container">
      <div class="blend"></div>
    </div>
  "#;

  let pixmap = render_with_backend(html, 64, 64, PaintBackend::Legacy);
  assert_close(pixel(&pixmap, 60, 60), (0, 255, 0, 255), 2);
  assert_close(pixel(&pixmap, 20, 20), (255, 0, 0, 255), 5);
}
