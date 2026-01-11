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
fn overflow_clip_only_treats_root_background_as_unclipped_decoration() {
  // Regression: overflow clipping composes descendants into a separate layer so the root element's
  // own background/border are not clipped. A rect-based heuristic incorrectly treated descendant
  // backgrounds that happened to cover the full clip rect as "root decorations", moving them
  // behind all clipped content.
  //
  // This reproduces the washington.edu hero slider failure mode: absolutely-positioned slides fill
  // the overflow-hidden container, and the top slide background must cover earlier slide content.
  let html = r#"
    <!doctype html>
    <style>
      body { margin: 0; background: white; }

      .viewport {
        width: 100px;
        height: 100px;
        overflow: hidden;
        position: relative;
        background: rgb(0, 255, 0);
      }

      .slide {
        position: absolute;
        left: 0;
        top: 0;
        width: 100px;
        height: 100px;
      }

      .bottom { background: rgb(0, 0, 255); }
      .top { background: rgb(255, 255, 0); }

      .mark {
        position: absolute;
        left: 10px;
        top: 10px;
        width: 10px;
        height: 10px;
        background: rgb(0, 0, 0);
      }
    </style>
    <div class="viewport">
      <div class="slide bottom">
        <div class="mark"></div>
      </div>
      <div class="slide top"></div>
    </div>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parsed");
  let fragment_tree = renderer
    .layout_document(&dom, 100, 100)
    .expect("laid out");

  let display = render_with_backend(
    &fragment_tree,
    renderer.font_context().clone(),
    100,
    100,
    PaintBackend::DisplayList,
  );
  let legacy = render_with_backend(
    &fragment_tree,
    renderer.font_context().clone(),
    100,
    100,
    PaintBackend::Legacy,
  );

  for (backend, pixmap) in [("DisplayList", &display), ("Legacy", &legacy)] {
    assert_eq!(
      pixel(pixmap, 5, 5),
      (255, 255, 0, 255),
      "{backend}: expected top slide background to be visible"
    );
    assert_eq!(
      pixel(pixmap, 15, 15),
      (255, 255, 0, 255),
      "{backend}: expected top slide background to cover bottom slide content under overflow clip"
    );
  }

  assert_eq!(
    legacy.data(),
    display.data(),
    "legacy and display-list backends should agree on overflow-clip root decoration classification"
  );
}

