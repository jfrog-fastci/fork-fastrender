// Future-guard regressions for Backdrop Root sampling across stacking contexts that are explicitly
// *not* Backdrop Root triggers in filter-effects-2.

use fastrender::debug::runtime::{with_thread_runtime_toggles, RuntimeToggles};
use fastrender::image_loader::ImageCache;
use fastrender::paint::display_list::DisplayItem;
use fastrender::paint::display_list::StackingContextItem;
use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::paint::display_list_renderer::PaintParallelism;
use fastrender::paint::painter::paint_tree_with_resources_scaled_offset;
use fastrender::paint::stacking::{build_stacking_tree_from_fragment_tree, StackingContextReason};
use fastrender::scroll::ScrollState;
use fastrender::{FastRender, Point, Rgba};
use std::collections::HashMap;
use std::sync::Arc;

fn collect_stacking_context_reasons(
  context: &fastrender::paint::stacking::StackingContext,
  out: &mut Vec<StackingContextReason>,
) {
  out.push(context.reason);
  for child in &context.children {
    collect_stacking_context_reasons(child, out);
  }
}

fn approx_eq(a: f32, b: f32) -> bool {
  (a - b).abs() < 0.01
}

fn find_context_by_bounds_and_z_index<'a>(
  contexts: &'a [StackingContextItem],
  width: f32,
  height: f32,
  z_index: i32,
) -> Option<&'a StackingContextItem> {
  contexts.iter().find(|ctx| {
    ctx.z_index == z_index && approx_eq(ctx.bounds.width(), width) && approx_eq(ctx.bounds.height(), height)
  })
}

fn render(
  html: &str,
  width: u32,
  height: u32,
) -> (tiny_skia::Pixmap, Vec<StackingContextReason>, Vec<StackingContextItem>) {
  // Force the display-list paint backend so this future guard catches issues introduced by
  // stacking-context compositing layers (the known risk surface for backdrop-filter sampling).
  let toggles = Arc::new(RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "display_list".to_string(),
  )])));

  with_thread_runtime_toggles(toggles, || {
    let mut renderer = FastRender::new().expect("renderer");
    let dom = renderer.parse_html(html).expect("parsed");
    let fragment_tree = renderer
      .layout_document(&dom, width, height)
      .expect("laid out");

    let stacking = build_stacking_tree_from_fragment_tree(&fragment_tree.root);
    let mut stacking_reasons = Vec::new();
    collect_stacking_context_reasons(&stacking, &mut stacking_reasons);

    let display_list = DisplayListBuilder::new()
      .with_parallelism(&PaintParallelism::disabled())
      .build_tree_with_stacking_checked(&fragment_tree)
      .expect("display list");
    let display_list_stacking_contexts = display_list
      .items()
      .iter()
      .filter_map(|item| match item {
        DisplayItem::PushStackingContext(ctx) => Some(ctx.clone()),
        _ => None,
      })
      .collect();

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

    (pixmap, stacking_reasons, display_list_stacking_contexts)
  })
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

  let (pixmap, stacking_reasons, display_list_stacking_contexts) = render(html, 64, 64);
  assert!(
    stacking_reasons.contains(&StackingContextReason::PositionedWithZIndex),
    "expected a z-index stacking context; got {stacking_reasons:?}"
  );
  let sc_context = display_list_stacking_contexts
    .iter()
    .find(|ctx| approx_eq(ctx.bounds.width(), 60.0) && approx_eq(ctx.bounds.height(), 60.0))
    .expect("expected display list stacking context for #sc");
  assert!(
    !sc_context.establishes_backdrop_root,
    "z-index stacking contexts must not establish Backdrop Roots (filter-effects-2); got {sc_context:?}"
  );
  assert_eq!(pixel(&pixmap, 20, 20), (0, 255, 255, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn backdrop_filter_crosses_positive_z_index_stacking_context() {
  let html = r#"<!doctype html>
    <style>
      body { margin: 0; background: rgb(255 0 0); }
      #sc { position: relative; z-index: 1; width: 60px; height: 60px; }
      #overlay { position: absolute; left: 0; top: 0; width: 40px; height: 40px; backdrop-filter: invert(1); }
    </style>
    <div id="sc"><div id="overlay"></div></div>
  "#;

  let (pixmap, stacking_reasons, display_list_stacking_contexts) = render(html, 64, 64);
  assert!(
    stacking_reasons.contains(&StackingContextReason::PositionedWithZIndex),
    "expected a z-index stacking context; got {stacking_reasons:?}"
  );
  let sc_context = find_context_by_bounds_and_z_index(&display_list_stacking_contexts, 60.0, 60.0, 1)
    .expect("expected display list stacking context for #sc");
  assert!(
    !sc_context.establishes_backdrop_root,
    "z-index stacking contexts must not establish Backdrop Roots (filter-effects-2); got {sc_context:?}"
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

  let (pixmap, stacking_reasons, display_list_stacking_contexts) = render(html, 64, 64);
  assert!(
    stacking_reasons.contains(&StackingContextReason::FixedPositioning),
    "expected a position: fixed stacking context; got {stacking_reasons:?}"
  );
  let sc_context = find_context_by_bounds_and_z_index(&display_list_stacking_contexts, 60.0, 60.0, 0)
    .expect("expected display list stacking context for #sc");
  assert!(
    !sc_context.establishes_backdrop_root,
    "position: fixed stacking contexts must not establish Backdrop Roots (filter-effects-2); got {sc_context:?}"
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

  let (pixmap, stacking_reasons, display_list_stacking_contexts) = render(html, 64, 64);
  assert!(
    stacking_reasons.contains(&StackingContextReason::StickyPositioning),
    "expected a position: sticky stacking context; got {stacking_reasons:?}"
  );
  let sc_context = find_context_by_bounds_and_z_index(&display_list_stacking_contexts, 60.0, 60.0, 0)
    .expect("expected display list stacking context for #sc");
  assert!(
    !sc_context.establishes_backdrop_root,
    "position: sticky stacking contexts must not establish Backdrop Roots (filter-effects-2); got {sc_context:?}"
  );
  assert_eq!(pixel(&pixmap, 20, 20), (0, 255, 255, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn backdrop_filter_crosses_negative_z_index_stacking_context() {
  let html = r#"<!doctype html>
    <style>
      body { margin: 0; background: rgb(255 0 0); }
      #sc { position: relative; z-index: -1; width: 60px; height: 60px; }
      #overlay { position: absolute; left: 0; top: 0; width: 40px; height: 40px; backdrop-filter: invert(1); }
    </style>
    <div id="sc"><div id="overlay"></div></div>
  "#;

  let (pixmap, stacking_reasons, display_list_stacking_contexts) = render(html, 64, 64);
  assert!(
    stacking_reasons.contains(&StackingContextReason::PositionedWithZIndex),
    "expected a z-index stacking context; got {stacking_reasons:?}"
  );
  let sc_context = find_context_by_bounds_and_z_index(&display_list_stacking_contexts, 60.0, 60.0, -1)
    .expect("expected display list stacking context for #sc");
  assert!(
    !sc_context.establishes_backdrop_root,
    "negative z-index stacking contexts must not establish Backdrop Roots (filter-effects-2); got {sc_context:?}"
  );
  assert_eq!(pixel(&pixmap, 20, 20), (0, 255, 255, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn backdrop_filter_crosses_transform_stacking_context() {
  let html = r#"<!doctype html>
    <style>
      body { margin: 0; background: rgb(255 0 0); }
      #sc { position: relative; transform: translate(0px, 0px); width: 60px; height: 60px; }
      #overlay { position: absolute; left: 0; top: 0; width: 40px; height: 40px; backdrop-filter: invert(1); }
    </style>
    <div id="sc"><div id="overlay"></div></div>
  "#;

  let (pixmap, stacking_reasons, display_list_stacking_contexts) = render(html, 64, 64);
  assert!(
    stacking_reasons.contains(&StackingContextReason::Transform),
    "expected a transform stacking context; got {stacking_reasons:?}"
  );
  let sc_context = find_context_by_bounds_and_z_index(&display_list_stacking_contexts, 60.0, 60.0, 0)
    .expect("expected display list stacking context for #sc");
  assert!(
    !sc_context.establishes_backdrop_root,
    "transform stacking contexts must not establish Backdrop Roots (filter-effects-2); got {sc_context:?}"
  );
  assert_eq!(pixel(&pixmap, 20, 20), (0, 255, 255, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn backdrop_filter_crosses_perspective_stacking_context() {
  let html = r#"<!doctype html>
    <style>
      body { margin: 0; background: rgb(255 0 0); }
      #sc { position: relative; perspective: 1000px; width: 60px; height: 60px; }
      #overlay { position: absolute; left: 0; top: 0; width: 40px; height: 40px; backdrop-filter: invert(1); }
    </style>
    <div id="sc"><div id="overlay"></div></div>
  "#;

  let (pixmap, stacking_reasons, display_list_stacking_contexts) = render(html, 64, 64);
  assert!(
    stacking_reasons.contains(&StackingContextReason::Perspective),
    "expected a perspective stacking context; got {stacking_reasons:?}"
  );
  let sc_context = find_context_by_bounds_and_z_index(&display_list_stacking_contexts, 60.0, 60.0, 0)
    .expect("expected display list stacking context for #sc");
  assert!(
    !sc_context.establishes_backdrop_root,
    "perspective stacking contexts must not establish Backdrop Roots (filter-effects-2); got {sc_context:?}"
  );
  assert_eq!(pixel(&pixmap, 20, 20), (0, 255, 255, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn backdrop_filter_crosses_flex_item_z_index_stacking_context() {
  let html = r#"<!doctype html>
    <style>
      body { margin: 0; background: rgb(255 0 0); display: flex; }
      #sc { z-index: 1; width: 60px; height: 60px; }
      #overlay { width: 40px; height: 40px; backdrop-filter: invert(1); }
    </style>
    <div id="sc"><div id="overlay"></div></div>
  "#;

  let (pixmap, stacking_reasons, display_list_stacking_contexts) = render(html, 64, 64);
  assert!(
    stacking_reasons.contains(&StackingContextReason::FlexItemWithZIndex),
    "expected a flex item z-index stacking context; got {stacking_reasons:?}"
  );
  let sc_context = find_context_by_bounds_and_z_index(&display_list_stacking_contexts, 60.0, 60.0, 1)
    .expect("expected display list stacking context for #sc");
  assert!(
    !sc_context.establishes_backdrop_root,
    "flex item z-index stacking contexts must not establish Backdrop Roots (filter-effects-2); got {sc_context:?}"
  );
  assert_eq!(pixel(&pixmap, 20, 20), (0, 255, 255, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn backdrop_filter_crosses_grid_item_z_index_stacking_context() {
  let html = r#"<!doctype html>
    <style>
      body { margin: 0; background: rgb(255 0 0); display: grid; }
      #sc { z-index: 1; width: 60px; height: 60px; }
      #overlay { width: 40px; height: 40px; backdrop-filter: invert(1); }
    </style>
    <div id="sc"><div id="overlay"></div></div>
  "#;

  let (pixmap, stacking_reasons, display_list_stacking_contexts) = render(html, 64, 64);
  assert!(
    stacking_reasons.contains(&StackingContextReason::GridItemWithZIndex),
    "expected a grid item z-index stacking context; got {stacking_reasons:?}"
  );
  let sc_context = find_context_by_bounds_and_z_index(&display_list_stacking_contexts, 60.0, 60.0, 1)
    .expect("expected display list stacking context for #sc");
  assert!(
    !sc_context.establishes_backdrop_root,
    "grid item z-index stacking contexts must not establish Backdrop Roots (filter-effects-2); got {sc_context:?}"
  );
  assert_eq!(pixel(&pixmap, 20, 20), (0, 255, 255, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn backdrop_filter_crosses_isolation_stacking_context() {
  let html = r#"<!doctype html>
    <style>
      body { margin: 0; background: rgb(255 0 0); }
      #sc { isolation: isolate; width: 60px; height: 60px; }
      #overlay { position: absolute; left: 0; top: 0; width: 40px; height: 40px; backdrop-filter: invert(1); }
    </style>
    <div id="sc"><div id="overlay"></div></div>
  "#;

  let (pixmap, stacking_reasons, display_list_stacking_contexts) = render(html, 64, 64);
  assert!(
    stacking_reasons.contains(&StackingContextReason::Isolation),
    "expected an isolation stacking context; got {stacking_reasons:?}"
  );
  let sc_context = find_context_by_bounds_and_z_index(&display_list_stacking_contexts, 60.0, 60.0, 0)
    .expect("expected display list stacking context for #sc");
  assert!(
    !sc_context.establishes_backdrop_root,
    "isolation stacking contexts must not establish Backdrop Roots (filter-effects-2); got {sc_context:?}"
  );
  assert_eq!(pixel(&pixmap, 20, 20), (0, 255, 255, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn backdrop_filter_crosses_backface_visibility_stacking_context() {
  let html = r#"<!doctype html>
    <style>
      body { margin: 0; background: rgb(255 0 0); }
      #sc { backface-visibility: hidden; width: 60px; height: 60px; }
      #overlay { position: absolute; left: 0; top: 0; width: 40px; height: 40px; backdrop-filter: invert(1); }
    </style>
    <div id="sc"><div id="overlay"></div></div>
  "#;

  let (pixmap, stacking_reasons, display_list_stacking_contexts) = render(html, 64, 64);
  assert!(
    stacking_reasons.contains(&StackingContextReason::BackfaceVisibility),
    "expected a backface-visibility stacking context; got {stacking_reasons:?}"
  );
  let sc_context = find_context_by_bounds_and_z_index(&display_list_stacking_contexts, 60.0, 60.0, 0)
    .expect("expected display list stacking context for #sc");
  assert!(
    !sc_context.establishes_backdrop_root,
    "backface-visibility stacking contexts must not establish Backdrop Roots (filter-effects-2); got {sc_context:?}"
  );
  assert_eq!(pixel(&pixmap, 20, 20), (0, 255, 255, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn backdrop_filter_crosses_containment_stacking_context() {
  let html = r#"<!doctype html>
    <style>
      body { margin: 0; background: rgb(255 0 0); }
      #sc { contain: paint; width: 60px; height: 60px; }
      #overlay { position: absolute; left: 0; top: 0; width: 40px; height: 40px; backdrop-filter: invert(1); }
    </style>
    <div id="sc"><div id="overlay"></div></div>
  "#;

  let (pixmap, stacking_reasons, display_list_stacking_contexts) = render(html, 64, 64);
  assert!(
    stacking_reasons.contains(&StackingContextReason::Containment),
    "expected a containment stacking context; got {stacking_reasons:?}"
  );
  let sc_context = find_context_by_bounds_and_z_index(&display_list_stacking_contexts, 60.0, 60.0, 0)
    .expect("expected display list stacking context for #sc");
  assert!(
    !sc_context.establishes_backdrop_root,
    "containment stacking contexts must not establish Backdrop Roots (filter-effects-2); got {sc_context:?}"
  );
  assert_eq!(pixel(&pixmap, 20, 20), (0, 255, 255, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn backdrop_filter_crosses_will_change_transform_stacking_context() {
  let html = r#"<!doctype html>
    <style>
      body { margin: 0; background: rgb(255 0 0); }
      #sc { will-change: transform; width: 60px; height: 60px; }
      #overlay { position: absolute; left: 0; top: 0; width: 40px; height: 40px; backdrop-filter: invert(1); }
    </style>
    <div id="sc"><div id="overlay"></div></div>
  "#;

  let (pixmap, stacking_reasons, display_list_stacking_contexts) = render(html, 64, 64);
  assert!(
    stacking_reasons.contains(&StackingContextReason::WillChange),
    "expected a will-change stacking context; got {stacking_reasons:?}"
  );
  let sc_context = find_context_by_bounds_and_z_index(&display_list_stacking_contexts, 60.0, 60.0, 0)
    .expect("expected display list stacking context for #sc");
  assert!(
    !sc_context.establishes_backdrop_root,
    "will-change: transform stacking contexts must not establish Backdrop Roots (filter-effects-2); got {sc_context:?}"
  );
  assert_eq!(pixel(&pixmap, 20, 20), (0, 255, 255, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn backdrop_filter_crosses_top_layer_stacking_context() {
  // Top-layer elements (e.g. popovers) create their own stacking contexts, but Filter Effects 2
  // does not list them as Backdrop Root triggers.
  let html = r#"<!doctype html>
    <style>
      body { margin: 0; background: rgb(255 0 0); }
      #sc {
        position: fixed;
        left: 0;
        top: 0;
        width: 60px;
        height: 60px;
        padding: 0;
        border: none;
        background: transparent;
      }
      #overlay { position: absolute; left: 0; top: 0; width: 40px; height: 40px; backdrop-filter: invert(1); }
    </style>
    <div id="sc" popover open><div id="overlay"></div></div>
  "#;

  let (pixmap, stacking_reasons, display_list_stacking_contexts) = render(html, 64, 64);
  assert!(
    stacking_reasons.contains(&StackingContextReason::TopLayer),
    "expected a top-layer stacking context; got {stacking_reasons:?}"
  );
  let sc_context = find_context_by_bounds_and_z_index(
    &display_list_stacking_contexts,
    60.0,
    60.0,
    i32::MAX,
  )
  .expect("expected display list stacking context for #sc");
  assert!(
    !sc_context.establishes_backdrop_root,
    "top-layer stacking contexts must not establish Backdrop Roots (filter-effects-2); got {sc_context:?}"
  );
  assert_eq!(pixel(&pixmap, 20, 20), (0, 255, 255, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}
