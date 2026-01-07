// Future-guard regressions for Backdrop Root sampling across stacking contexts that are explicitly
// *not* Backdrop Root triggers in filter-effects-2.

use fastrender::image_loader::ImageCache;
use fastrender::paint::display_list_renderer::PaintParallelism;
use fastrender::paint::painter::paint_tree_with_resources_scaled_offset;
use fastrender::paint::stacking::{build_stacking_tree_from_fragment_tree, StackingContextReason};
use fastrender::scroll::ScrollState;
use fastrender::{FastRender, Point, Rgba};

fn collect_stacking_context_reasons(
  context: &fastrender::paint::stacking::StackingContext,
  out: &mut Vec<StackingContextReason>,
) {
  out.push(context.reason);
  for child in &context.children {
    collect_stacking_context_reasons(child, out);
  }
}

fn render(html: &str, width: u32, height: u32) -> (tiny_skia::Pixmap, Vec<StackingContextReason>) {
  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parsed");
  let fragment_tree = renderer
    .layout_document(&dom, width, height)
    .expect("laid out");

  let stacking = build_stacking_tree_from_fragment_tree(&fragment_tree.root);
  let mut stacking_reasons = Vec::new();
  collect_stacking_context_reasons(&stacking, &mut stacking_reasons);

  let font_ctx = renderer.font_context().clone();
  let image_cache = ImageCache::new();
  let pixmap = paint_tree_with_resources_scaled_offset(
    &fragment_tree,
    width,
    height,
    Rgba::WHITE,
    font_ctx,
    image_cache,
    1.0,
    Point::ZERO,
    // Keep painting deterministic; these tests focus on backdrop sampling boundaries.
    PaintParallelism::disabled(),
    &ScrollState::default(),
  )
  .expect("painted");

  (pixmap, stacking_reasons)
}

fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let p = pixmap.pixel(x, y).unwrap();
  (p.red(), p.green(), p.blue(), p.alpha())
}

#[test]
fn backdrop_filter_crosses_z_index_stacking_context() {
  let html = r#"<!doctype html>
    <style>
      body { margin: 0; background: rgb(255 0 0); }
      #sc { position: relative; z-index: 0; width: 60px; height: 60px; }
      #overlay { position: absolute; left: 0; top: 0; width: 40px; height: 40px; backdrop-filter: invert(1); }
    </style>
    <div id="sc"><div id="overlay"></div></div>
  "#;

  let (pixmap, stacking_reasons) = render(html, 64, 64);
  assert!(
    stacking_reasons.contains(&StackingContextReason::PositionedWithZIndex),
    "expected a z-index stacking context; got {stacking_reasons:?}"
  );
  assert_eq!(pixel(&pixmap, 20, 20), (0, 255, 255, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn backdrop_filter_crosses_fixed_position_stacking_context() {
  let html = r#"<!doctype html>
    <style>
      body { margin: 0; background: rgb(255 0 0); }
      #sc { position: fixed; left: 0; top: 0; width: 60px; height: 60px; }
      #overlay { position: absolute; left: 0; top: 0; width: 40px; height: 40px; backdrop-filter: invert(1); }
    </style>
    <div id="sc"><div id="overlay"></div></div>
  "#;

  let (pixmap, stacking_reasons) = render(html, 64, 64);
  assert!(
    stacking_reasons.contains(&StackingContextReason::FixedPositioning),
    "expected a position: fixed stacking context; got {stacking_reasons:?}"
  );
  assert_eq!(pixel(&pixmap, 20, 20), (0, 255, 255, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn backdrop_filter_crosses_sticky_position_stacking_context() {
  let html = r#"<!doctype html>
    <style>
      body { margin: 0; background: rgb(255 0 0); }
      #scroll { height: 100px; overflow: auto; }
      #sc { position: sticky; top: 0; width: 60px; height: 60px; }
      #overlay { position: absolute; left: 0; top: 0; width: 40px; height: 40px; backdrop-filter: invert(1); }
      #spacer { height: 200px; }
    </style>
    <div id="scroll">
      <div id="sc"><div id="overlay"></div></div>
      <div id="spacer"></div>
    </div>
  "#;

  let (pixmap, stacking_reasons) = render(html, 64, 64);
  assert!(
    stacking_reasons.contains(&StackingContextReason::StickyPositioning),
    "expected a position: sticky stacking context; got {stacking_reasons:?}"
  );
  assert_eq!(pixel(&pixmap, 20, 20), (0, 255, 255, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}
