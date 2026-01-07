use fastrender::image_loader::ImageCache;
use fastrender::paint::display_list::DisplayList;
use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::paint::display_list_renderer::{DisplayListRenderer, PaintParallelism};
use fastrender::scroll::ScrollState;
use fastrender::text::font_loader::FontContext;
use fastrender::tree::fragment_tree::FragmentNode;
use fastrender::{FastRender, FastRenderConfig, FontConfig, LayoutParallelism, Point, Rgba};
use rayon::ThreadPoolBuilder;
use std::sync::Once;

fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let p = pixmap.pixel(x, y).unwrap();
  (p.red(), p.green(), p.blue(), p.alpha())
}

fn init_rayon_global_pool() {
  static INIT: Once = Once::new();
  INIT.call_once(|| {
    // Rayon will lazily initialize a global thread pool the first time it's used. In constrained
    // environments (e.g. CI containers) the default thread count can exceed the OS/thread quota,
    // causing initialization to fail and panic. Pre-initialize a conservative single-thread pool
    // so FastRender's internal rayon usage stays reliable for this regression.
    let _ = ThreadPoolBuilder::new().num_threads(1).build_global();
  });
}

fn build_display_list(html: &str, width: u32, height: u32) -> (DisplayList, FontContext) {
  init_rayon_global_pool();

  let mut config = FastRenderConfig::new();
  config.font_config = FontConfig::bundled_only();
  // This regression targets backdrop-filter semantics rather than layout fan-out. Disable layout
  // parallelism so tests can run in constrained environments without initializing Rayon globals.
  config.layout_parallelism = LayoutParallelism::disabled();

  let mut renderer = FastRender::with_config(config).expect("renderer");

  let dom = renderer.parse_html(html).expect("parsed");
  let tree = renderer
    .layout_document(&dom, width, height)
    .expect("laid out");
  let font_ctx = renderer.font_context().clone();
  let image_cache = ImageCache::new();
  let viewport = tree.viewport_size();

  let build_for_root = |root: &FragmentNode| -> DisplayList {
    DisplayListBuilder::with_image_cache(image_cache.clone())
      .with_font_context(font_ctx.clone())
      .with_svg_filter_defs(tree.svg_filter_defs.clone())
      .with_scroll_state(ScrollState::default())
      .with_device_pixel_ratio(1.0)
      // Keep display-list building deterministic; this test focuses on renderer semantics.
      .with_parallelism(&PaintParallelism::disabled())
      .with_viewport_size(viewport.width, viewport.height)
      .build_with_stacking_tree_offset_checked(root, Point::ZERO)
      .expect("display list")
  };

  let mut list = build_for_root(&tree.root);
  for extra in &tree.additional_fragments {
    list.append(build_for_root(extra));
  }
  (list, font_ctx)
}

#[test]
fn nested_backdrop_filters_establish_backdrop_root() {
  // Per filter-effects-2, `backdrop-filter != none` establishes a Backdrop Root. Descendants with
  // their own backdrop-filter must not sample beyond the ancestor's backdrop-filter boundary.
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; }
      #bg { position: absolute; inset: 0; background: rgb(255 0 0); }
      #parent {
        position: absolute;
        left: 0;
        top: 0;
        width: 40px;
        height: 40px;
        backdrop-filter: invert(1);
      }
      #child {
        position: absolute;
        left: 10px;
        top: 10px;
        width: 20px;
        height: 20px;
        backdrop-filter: invert(1);
      }
    </style>
    <div id="bg"></div>
    <div id="parent"><div id="child"></div></div>
  "#;

  let (list, font_ctx) = build_display_list(html, 64, 64);
  let pixmap = DisplayListRenderer::new(64, 64, Rgba::WHITE, font_ctx)
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("render");

  // Outside the parent: unchanged red.
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
  // Inside the parent but outside the child: inverted red -> cyan.
  assert_eq!(pixel(&pixmap, 5, 5), (0, 255, 255, 255));
  // Inside the child: inverts the parent's cyan backdrop back to red. If backdrop-filter does not
  // establish a Backdrop Root, the child would incorrectly re-sample the page red and invert to
  // cyan instead.
  assert_eq!(pixel(&pixmap, 15, 15), (255, 0, 0, 255));
}

#[test]
fn descendant_backdrop_filter_uses_ancestor_backdrop_filter_as_backdrop_root() {
  // Regression: descendants should treat an ancestor `backdrop-filter` element as their nearest
  // Backdrop Root, even when there is an intermediate stacking context that is isolated for other
  // reasons (e.g. `isolation: isolate`).
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; }
      #bg { position: absolute; inset: 0; background: rgb(255 0 0); }
      #root {
        position: absolute;
        left: 0;
        top: 0;
        width: 60px;
        height: 60px;
        backdrop-filter: invert(1);
      }
      #iso {
        position: absolute;
        inset: 0;
        isolation: isolate;
      }
      #child {
        position: absolute;
        left: 0;
        top: 0;
        width: 30px;
        height: 30px;
        backdrop-filter: invert(1);
      }
    </style>
    <div id="bg"></div>
    <div id="root">
      <div id="iso">
        <div id="child"></div>
      </div>
    </div>
  "#;

  let (list, font_ctx) = build_display_list(html, 64, 64);
  let pixmap = DisplayListRenderer::new(64, 64, Rgba::WHITE, font_ctx)
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("render");

  // Inside the root but outside the child: root backdrop-filter inverts red -> cyan.
  assert_eq!(pixel(&pixmap, 50, 10), (0, 255, 255, 255));
  // Inside the child: invert the root cyan backdrop back to red. Incorrect sampling (only from the
  // isolated layer) would leave the area cyan.
  assert_eq!(pixel(&pixmap, 10, 10), (255, 0, 0, 255));
  // Outside the root: unchanged red background.
  assert_eq!(pixel(&pixmap, 62, 62), (255, 0, 0, 255));
}
