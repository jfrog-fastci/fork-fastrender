use fastrender::image_loader::ImageCache;
use fastrender::paint::display_list::DisplayList;
use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::paint::display_list_renderer::{DisplayListRenderer, PaintParallelism};
use fastrender::scroll::ScrollState;
use fastrender::text::font_loader::FontContext;
use fastrender::tree::fragment_tree::FragmentNode;
use fastrender::{FastRender, FontConfig, Point, Rgba};

fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let p = pixmap.pixel(x, y).unwrap();
  (p.red(), p.green(), p.blue(), p.alpha())
}

fn build_display_list(html: &str, width: u32, height: u32) -> (DisplayList, FontContext) {
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("renderer");

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
      // Keep display-list building deterministic; these tests focus on renderer effects.
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
fn isolation_is_not_a_backdrop_root() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; }
      #bg { position: absolute; inset: 0; background: rgb(255 0 0); }
      #group { position: absolute; inset: 0; isolation: isolate; }
      #overlay {
        position: absolute;
        left: 0;
        top: 0;
        width: 40px;
        height: 40px;
        backdrop-filter: invert(1);
      }
    </style>
    <div id="bg"></div>
    <div id="group"><div id="overlay"></div></div>
  "#;

  let (list, font_ctx) = build_display_list(html, 64, 64);
  let pixmap = DisplayListRenderer::new(64, 64, Rgba::WHITE, font_ctx)
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("render");

  // `isolation:isolate` can insert an intermediate layer for blend isolation, but it is *not* a
  // Backdrop Root, so the backdrop-filter should still sample the root backdrop (red → cyan).
  assert_eq!(pixel(&pixmap, 20, 20), (0, 255, 255, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn mix_blend_mode_establishes_a_backdrop_root() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; }
      #bg { position: absolute; inset: 0; background: rgb(255 0 0); }
      #group { position: absolute; inset: 0; mix-blend-mode: multiply; }
      #overlay {
        position: absolute;
        left: 0;
        top: 0;
        width: 40px;
        height: 40px;
        backdrop-filter: invert(1);
      }
    </style>
    <div id="bg"></div>
    <div id="group"><div id="overlay"></div></div>
  "#;

  let (list, font_ctx) = build_display_list(html, 64, 64);
  let pixmap = DisplayListRenderer::new(64, 64, Rgba::WHITE, font_ctx)
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("render");

  // `mix-blend-mode` establishes a Backdrop Root. The group has no backdrop of its own, so the
  // child's backdrop-filter should effectively see empty/transparent and not invert the red page.
  assert_eq!(pixel(&pixmap, 20, 20), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn mix_blend_mode_backdrop_root_ignores_non_isolated_group_backdrop_init() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 255 255); }
      #group { position: absolute; inset: 0; mix-blend-mode: multiply; }
      /* Trigger non-isolated group surface initialization from backdrop. */
      #trigger {
        position: absolute;
        left: 0;
        top: 0;
        width: 1px;
        height: 1px;
        background: rgb(255 0 0);
        mix-blend-mode: difference;
      }
      #overlay {
        position: absolute;
        left: 10px;
        top: 10px;
        width: 40px;
        height: 40px;
        backdrop-filter: invert(1);
      }
    </style>
    <div id="group">
      <div id="trigger"></div>
      <div id="overlay"></div>
    </div>
  "#;

  let (list, font_ctx) = build_display_list(html, 64, 64);
  let pixmap = DisplayListRenderer::new(64, 64, Rgba::WHITE, font_ctx)
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("render");

  // `mix-blend-mode` establishes a Backdrop Root boundary for descendant backdrop-filter sampling.
  // Even if the group surface is lazily initialized from the page backdrop for descendant blending,
  // the Backdrop Root Image must treat that initialization backdrop as transparent.
  assert_eq!(pixel(&pixmap, 20, 20), (255, 255, 255, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 255, 255, 255));
}

#[test]
fn clip_path_establishes_a_backdrop_root() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; }
      #bg { position: absolute; inset: 0; background: rgb(255 0 0); }
      #group { position: absolute; inset: 0; clip-path: inset(0); }
      #overlay {
        position: absolute;
        left: 0;
        top: 0;
        width: 40px;
        height: 40px;
        backdrop-filter: invert(1);
      }
    </style>
    <div id="bg"></div>
    <div id="group"><div id="overlay"></div></div>
  "#;

  let (list, font_ctx) = build_display_list(html, 64, 64);
  let pixmap = DisplayListRenderer::new(64, 64, Rgba::WHITE, font_ctx)
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("render");

  // `clip-path` establishes a Backdrop Root even when the clip covers the full element.
  assert_eq!(pixel(&pixmap, 20, 20), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}
