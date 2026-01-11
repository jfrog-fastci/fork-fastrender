use fastrender::image_loader::ImageCache;
use fastrender::paint::display_list_renderer::PaintParallelism;
use fastrender::paint::painter::{paint_tree_with_resources_scaled_offset_backend, PaintBackend};
use fastrender::scroll::ScrollState;
use fastrender::text::font_loader::FontContext;
use fastrender::tree::fragment_tree::FragmentTree;
use fastrender::{FastRender, Point, Rgba};

fn render_with_backend(
  tree: &FragmentTree,
  font_ctx: FontContext,
  width: u32,
  height: u32,
  backend: PaintBackend,
) -> tiny_skia::Pixmap {
  paint_tree_with_resources_scaled_offset_backend(
    tree,
    width,
    height,
    Rgba::WHITE,
    font_ctx,
    ImageCache::new(),
    1.0,
    Point::ZERO,
    PaintParallelism::disabled(),
    &ScrollState::default(),
    backend,
  )
  .expect("painted")
}

fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let px = pixmap.pixel(x, y).expect("pixel inside viewport");
  (px.red(), px.green(), px.blue(), px.alpha())
}

#[test]
fn layer6_merges_positioned_auto_and_zero_contexts_in_tree_order() {
  // Regression: Out-of-flow positioned boxes (e.g. `position: absolute`) are often emitted *after*
  // in-flow content in the fragment tree. CSS2.1 Appendix E requires layer-6 painting to use
  // "tree order" and to merge:
  // - positioned descendants with `z-index:auto/0`, and
  // - child stacking contexts with `z-index:0`.
  //
  // This reproduces the washington.edu failure mode: an earlier DOM sibling (`.quicklinks`)
  // establishes a stacking context (transform → z-index 0) but is laid out after the later DOM
  // sibling (`.inner`, position: relative; z-index:auto). When layer-6 merging is correct, `.inner`
  // paints on top and hides `.quicklinks`.
  let html = r#"
    <!doctype html>
    <style>
      body { margin: 0; background: white; }
      .container { position: relative; width: 120px; height: 40px; overflow: hidden; }

      /* Earlier DOM sibling: abspos + transform creates a z-index:0 stacking context. */
      .quicklinks {
        position: absolute;
        right: 0;
        top: 0;
        width: 300px;
        height: 40px;
        background: rgb(47, 47, 47);
        transform: translateX(190px);
      }

      /* Later DOM sibling: positioned auto/0 (layer-6). Must paint on top of .quicklinks. */
      .inner {
        position: relative;
        width: 120px;
        height: 40px;
        background: rgb(255, 255, 255);
      }

      .header { height: 20px; background: rgb(75, 46, 131); }
    </style>
    <div class="container">
      <div class="quicklinks"></div>
      <div class="inner">
        <div class="header"></div>
      </div>
    </div>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parsed");
  let fragment_tree = renderer
    .layout_document(&dom, 120, 40)
    .expect("laid out");

  let display = render_with_backend(
    &fragment_tree,
    renderer.font_context().clone(),
    120,
    40,
    PaintBackend::DisplayList,
  );
  let legacy = render_with_backend(
    &fragment_tree,
    renderer.font_context().clone(),
    120,
    40,
    PaintBackend::Legacy,
  );

  for (backend, pixmap) in [("DisplayList", &display), ("Legacy", &legacy)] {
    assert_eq!(
      pixel(pixmap, 110, 10),
      (75, 46, 131, 255),
      "{backend}: expected header background to paint above quicklinks"
    );
    assert_eq!(
      pixel(pixmap, 110, 30),
      (255, 255, 255, 255),
      "{backend}: expected inner background to cover quicklinks below the header"
    );
  }

  assert_eq!(
    legacy.data(),
    display.data(),
    "legacy and display-list backends should agree on layer-6 tree order"
  );
}
