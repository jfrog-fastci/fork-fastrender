use crate::image_loader::ImageCache;
use crate::paint::display_list::DisplayList;
use crate::paint::display_list_builder::DisplayListBuilder;
use crate::paint::display_list_renderer::{DisplayListRenderer, PaintParallelism};
use crate::scroll::ScrollState;
use crate::text::font_loader::FontContext;
use crate::tree::fragment_tree::FragmentNode;
use crate::{FastRender, FontConfig, Point, Rgba};

fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let p = pixmap.pixel(x, y).expect("pixel in bounds");
  (p.red(), p.green(), p.blue(), p.alpha())
}

fn build_display_list(
  html: &str,
  width: u32,
  height: u32,
) -> (
  DisplayList,
  FontContext,
  crate::tree::fragment_tree::FragmentTree,
) {
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
      .with_svg_id_defs(tree.svg_id_defs.clone())
      .with_scroll_state(ScrollState::default())
      .with_device_pixel_ratio(1.0)
      // Keep display-list building deterministic; these tests focus on clip-path effects.
      .with_parallelism(&PaintParallelism::disabled())
      .with_viewport_size(viewport.width, viewport.height)
      .build_with_stacking_tree_offset_checked(root, Point::ZERO)
      .expect("display list")
  };

  let mut list = build_for_root(&tree.root);
  for extra in &tree.additional_fragments {
    list.append(build_for_root(extra));
  }
  (list, font_ctx, tree)
}

#[test]
fn svg_clip_path_url_fragment_resolves_use_dependencies() {
  let html = r##"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: white; }
      #box {
        width: 100px;
        height: 100px;
        background: rgb(255 0 0);
        clip-path: url(#c);
      }
    </style>
    <svg width="0" height="0" style="position:absolute" xmlns="http://www.w3.org/2000/svg"
         xmlns:xlink="http://www.w3.org/1999/xlink">
      <defs>
        <rect id="shape" x="0" y="0" width="50" height="100" fill="white"/>
        <clipPath id="c">
          <use xlink:href="#shape"/>
        </clipPath>
      </defs>
    </svg>
    <div id="box"></div>
  "##;

  let (list, font_ctx, fragments) = build_display_list(html, 100, 100);

  assert!(
    fragments
      .svg_id_defs
      .as_ref()
      .is_some_and(|defs| { defs.contains_key("c") && defs.contains_key("shape") }),
    "layout should retain defs required by url(#c) clip-path"
  );

  let pixmap = DisplayListRenderer::new(100, 100, Rgba::WHITE, font_ctx)
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("render");

  assert_eq!(pixel(&pixmap, 10, 50), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 90, 50), (255, 255, 255, 255));
}

#[test]
fn svg_clip_path_url_fragment_respects_clip_path_units_object_bounding_box() {
  let html = r##"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: white; }
      #box {
        width: 100px;
        height: 100px;
        background: rgb(255 0 0);
        clip-path: url(#c);
      }
    </style>
    <svg width="0" height="0" style="position:absolute" xmlns="http://www.w3.org/2000/svg">
      <defs>
        <clipPath id="c" clipPathUnits="objectBoundingBox">
          <rect x="0" y="0" width="0.5" height="1"/>
        </clipPath>
      </defs>
    </svg>
    <div id="box"></div>
  "##;

  let (list, font_ctx, fragments) = build_display_list(html, 100, 100);

  assert!(
    fragments
      .svg_id_defs
      .as_ref()
      .is_some_and(|defs| defs.contains_key("c")),
    "layout should retain defs required by url(#c) clip-path"
  );

  let pixmap = DisplayListRenderer::new(100, 100, Rgba::WHITE, font_ctx)
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("render");

  assert_eq!(pixel(&pixmap, 10, 50), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 90, 50), (255, 255, 255, 255));
}

#[test]
fn svg_clip_path_url_fragment_serializes_css_transform_overriding_transform_attribute() {
  let html = r##"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: white; }
      #box {
        width: 100px;
        height: 100px;
        background: rgb(255 0 0);
        clip-path: url(#c);
      }
      #shape { transform: translate(50px, 0px); }
    </style>
    <svg width="0" height="0" style="position:absolute" xmlns="http://www.w3.org/2000/svg">
      <defs>
        <clipPath id="c">
          <rect id="shape" x="0" y="0" width="50" height="100" transform="translate(0 0)" />
        </clipPath>
      </defs>
    </svg>
    <div id="box"></div>
  "##;

  let (list, font_ctx, fragments) = build_display_list(html, 100, 100);

  assert!(
    fragments
      .svg_id_defs
      .as_ref()
      .is_some_and(|defs| defs.contains_key("c")),
    "layout should retain defs required by url(#c) clip-path"
  );

  let pixmap = DisplayListRenderer::new(100, 100, Rgba::WHITE, font_ctx)
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("render");

  assert_eq!(
    pixel(&pixmap, 10, 50),
    (255, 255, 255, 255),
    "CSS transform should override the authored SVG transform attribute inside defs"
  );
  assert_eq!(pixel(&pixmap, 90, 50), (255, 0, 0, 255));
}
