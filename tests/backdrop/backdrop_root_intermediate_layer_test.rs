use fastrender::image_loader::ImageCache;
use fastrender::paint::display_list_renderer::PaintParallelism;
use fastrender::paint::painter::paint_tree_with_resources_scaled_offset;
use fastrender::scroll::ScrollState;
use fastrender::{FastRender, Point, Rgba};

fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let p = pixmap.pixel(x, y).unwrap();
  (p.red(), p.green(), p.blue(), p.alpha())
}

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
    // Keep painting deterministic; this regression focuses on Backdrop Root scoping.
    PaintParallelism::disabled(),
    &ScrollState::default(),
  )
  .expect("painted")
}

#[test]
fn backdrop_filter_composites_only_from_backdrop_root_depth() {
  // Regression: when there are nested offscreen layers, `backdrop-filter` sampling must composite
  // only the layers between the active Backdrop Root and the immediate parent. Pixels painted
  // *above* the backdrop root (e.g. the page background) must not leak into the sampled backdrop.
  //
  // Layout:
  // - body background (red) is outside the backdrop-root scope.
  // - #root establishes a backdrop root via `filter`, but paints no background.
  // - #behind paints green into #root's surface (between backdrop root and #overlay).
  // - #mid introduces an intermediate offscreen layer via `isolation: isolate` but does NOT
  //   establish a backdrop root.
  // - #overlay applies `backdrop-filter: invert(1)` and should:
  //   - see #behind (green) through #mid's layer and invert it to magenta
  //   - see transparent elsewhere in the root and therefore leave the body background unchanged.
  let html = r#"<!doctype html>
    <style>
      body { margin: 0; background: rgb(255 0 0); }
      #root { position: absolute; inset: 0; filter: blur(0px); }
      #behind {
        position: absolute;
        left: 0;
        top: 0;
        width: 20px;
        height: 40px;
        background: rgb(0 255 0);
      }
      #mid { position: absolute; inset: 0; isolation: isolate; }
      #overlay {
        position: absolute;
        left: 0;
        top: 0;
        width: 40px;
        height: 40px;
        backdrop-filter: invert(1);
        background: transparent;
      }
    </style>
    <div id="root">
      <div id="behind"></div>
      <div id="mid"><div id="overlay"></div></div>
    </div>
  "#;

  let pixmap = render(html, 64, 64);

  // Inside #overlay, over the green #behind rect: green inverted to magenta.
  assert_eq!(pixel(&pixmap, 10, 20), (255, 0, 255, 255));
  // Inside #overlay, but outside #behind: backdrop-root scope is empty, so output remains the body
  // background (red) rather than sampling it and inverting to cyan.
  assert_eq!(pixel(&pixmap, 30, 20), (255, 0, 0, 255));
}

