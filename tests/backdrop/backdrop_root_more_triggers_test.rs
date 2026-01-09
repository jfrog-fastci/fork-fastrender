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
  crate::rayon_test_util::init_rayon_for_tests(2);
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
      // Keep display-list building deterministic; these tests focus on paint-time backdrop roots.
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

fn render(html: &str, width: u32, height: u32) -> tiny_skia::Pixmap {
  let (list, font_ctx) = build_display_list(html, width, height);
  DisplayListRenderer::new(width, height, Rgba::WHITE, font_ctx)
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("render")
}

#[test]
fn backdrop_filter_crosses_isolation_inside_mix_blend_mode_backdrop_root() {
  let html = r#"<!doctype html>
    <style>
      body { margin: 0; background: rgb(255 255 255); }
      #blendroot { position: absolute; inset: 0; background: rgb(0 255 0); mix-blend-mode: multiply; }
      /* `isolation:isolate` forces an offscreen surface but does NOT establish a backdrop root. */
      #iso { position: absolute; inset: 0; isolation: isolate; }
      #overlay { position: absolute; left: 0; top: 0; width: 40px; height: 40px; backdrop-filter: invert(1); }
    </style>
    <div id=blendroot><div id=iso><div id=overlay></div></div></div>
  "#;

  let pixmap = render(html, 64, 64);

  // `mix-blend-mode` establishes the nearest backdrop root, so the overlay should sample the
  // already-painted green background of #blendroot even though #iso forced an offscreen surface.
  assert_eq!(pixel(&pixmap, 20, 20), (255, 0, 255, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (0, 255, 0, 255));
}

#[test]
fn backdrop_filter_crosses_transform_ancestor() {
  let html = r#"<!doctype html>
    <style>
      body { margin: 0; background: rgb(255 0 0); }
      /* Transforms create stacking contexts and may become offscreen layers, but are not backdrop roots. */
      #xform { position: absolute; inset: 0; transform: translate(1px, 0px); }
      #overlay { position: absolute; left: 0; top: 0; width: 40px; height: 40px; backdrop-filter: invert(1); }
    </style>
    <div id=xform><div id=overlay></div></div>
  "#;

  let pixmap = render(html, 64, 64);

  // The overlay should still sample the body backdrop (red) through the transform ancestor.
  assert_eq!(pixel(&pixmap, 20, 20), (0, 255, 255, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}
