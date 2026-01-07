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
  crate::rayon_test_util::init_rayon_for_tests(1);

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
      // Keep display-list building deterministic; this test focuses on backdrop-root selection.
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

fn render(parent_style: &str) -> tiny_skia::Pixmap {
  let html = format!(
    r#"<!doctype html>
      <style>
        html, body {{ margin: 0; padding: 0; }}
        body {{ background: rgb(255 0 0); }}
        #parent {{
          width: 40px;
          height: 40px;
          isolation: isolate;
          {parent_style}
        }}
        #child {{
          width: 20px;
          height: 20px;
          position: relative;
          left: 10px;
          top: 10px;
          backdrop-filter: invert(1);
        }}
      </style>
      <div id="parent"><div id="child"></div></div>
    "#
  );

  let (list, font_ctx) = build_display_list(&html, 64, 64);
  DisplayListRenderer::new(64, 64, Rgba::WHITE, font_ctx)
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("render")
}

fn assert_child_inverts_body_bg(pixmap: &tiny_skia::Pixmap) {
  assert_eq!(pixel(pixmap, 5, 5), (255, 0, 0, 255));
  assert_eq!(pixel(pixmap, 15, 15), (0, 255, 255, 255));
}

#[test]
fn transform_is_not_a_backdrop_root() {
  let pixmap = render("transform: translate(0px, 0px);");
  assert_child_inverts_body_bg(&pixmap);
}

#[test]
fn translate_is_not_a_backdrop_root() {
  let pixmap = render("translate: 0px 0px;");
  assert_child_inverts_body_bg(&pixmap);
}

#[test]
fn rotate_is_not_a_backdrop_root() {
  let pixmap = render("rotate: 0deg;");
  assert_child_inverts_body_bg(&pixmap);
}

#[test]
fn scale_is_not_a_backdrop_root() {
  let pixmap = render("scale: 1 1;");
  assert_child_inverts_body_bg(&pixmap);
}

#[test]
fn z_index_is_not_a_backdrop_root() {
  let pixmap = render("position: relative; z-index: 1;");
  assert_child_inverts_body_bg(&pixmap);
}

#[test]
fn fixed_positioning_is_not_a_backdrop_root() {
  let pixmap = render("position: fixed; top: 0; left: 0;");
  assert_child_inverts_body_bg(&pixmap);
}

#[test]
fn sticky_positioning_is_not_a_backdrop_root() {
  let pixmap = render("position: sticky; top: 0; left: 0;");
  assert_child_inverts_body_bg(&pixmap);
}

#[test]
fn will_change_transform_is_not_a_backdrop_root() {
  let pixmap = render("will-change: transform;");
  assert_child_inverts_body_bg(&pixmap);
}

#[test]
fn perspective_is_not_a_backdrop_root() {
  let pixmap = render("perspective: 1000px;");
  assert_child_inverts_body_bg(&pixmap);
}

#[test]
fn will_change_perspective_is_not_a_backdrop_root() {
  let pixmap = render("will-change: perspective;");
  assert_child_inverts_body_bg(&pixmap);
}

#[test]
fn contain_paint_is_not_a_backdrop_root() {
  let pixmap = render("contain: paint;");
  assert_child_inverts_body_bg(&pixmap);
}

#[test]
fn will_change_contain_is_not_a_backdrop_root() {
  let pixmap = render("will-change: contain;");
  assert_child_inverts_body_bg(&pixmap);
}

#[test]
fn backface_visibility_hidden_is_not_a_backdrop_root() {
  let pixmap = render("backface-visibility: hidden;");
  assert_child_inverts_body_bg(&pixmap);
}

#[test]
fn will_change_contents_is_not_a_backdrop_root() {
  let pixmap = render("will-change: contents;");
  assert_child_inverts_body_bg(&pixmap);
}

#[test]
fn will_change_scroll_position_is_not_a_backdrop_root() {
  let pixmap = render("will-change: scroll-position;");
  assert_child_inverts_body_bg(&pixmap);
}

#[test]
fn will_change_isolation_is_not_a_backdrop_root() {
  let pixmap = render("will-change: isolation;");
  assert_child_inverts_body_bg(&pixmap);
}

#[test]
fn overflow_hidden_is_not_a_backdrop_root() {
  let pixmap = render("overflow: hidden;");
  assert_child_inverts_body_bg(&pixmap);
}

#[test]
fn overflow_clip_is_not_a_backdrop_root() {
  let pixmap = render("overflow: clip;");
  assert_child_inverts_body_bg(&pixmap);
}
