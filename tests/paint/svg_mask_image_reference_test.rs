use fastrender::api::{DiagnosticsLevel, RenderOptions};
use fastrender::image_loader::ImageCache;
use fastrender::paint::display_list::DisplayList;
use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::paint::display_list_renderer::{DisplayListRenderer, PaintParallelism};
use fastrender::paint::painter::{paint_tree_with_resources_scaled_offset_backend, PaintBackend};
use fastrender::scroll::ScrollState;
use fastrender::text::font_loader::FontContext;
use fastrender::tree::fragment_tree::FragmentNode;
use fastrender::{FastRender, FontConfig, Point, Rgba};

fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let p = pixmap.pixel(x, y).expect("pixel in bounds");
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
      .with_svg_id_defs(tree.svg_id_defs.clone())
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
fn svg_mask_image_reference_resolves_use_dependencies() {
  // Many real-world pages define masks inside hidden SVG <defs> blocks and then reference them
  // from CSS via `mask-image: url(#id)`. Those masks frequently use `<use xlink:href="#...">`
  // to reference other defs.
  let html = r##"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: white; }
      #box {
        width: 100px;
        height: 100px;
        background: rgb(255 0 0);
        mask-image: url(#m);
        mask-mode: alpha;
        mask-repeat: no-repeat;
        mask-size: 100% 100%;
        mask-position: 0 0;
      }
    </style>

    <svg style="display: none" xmlns="http://www.w3.org/2000/svg"
         xmlns:xlink="http://www.w3.org/1999/xlink">
      <defs>
        <rect id="shape" x="0" y="0" width="50" height="100" fill="white"/>
        <mask id="m" maskUnits="userSpaceOnUse" maskContentUnits="userSpaceOnUse"
              x="0" y="0" width="100" height="100">
          <use xlink:href="#shape"/>
        </mask>
      </defs>
    </svg>

    <div id="box"></div>
  "##;

  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parse html");
  let fragments = renderer
    .layout_document(&dom, 100, 100)
    .expect("layout document");

  assert!(
    fragments.svg_id_defs.as_ref().is_some_and(|defs| {
      defs.contains_key("m") && defs.contains_key("shape")
    }),
    "layout should retain defs required by url(#m) mask-image"
  );

  let pixmap = paint_tree_with_resources_scaled_offset_backend(
    &fragments,
    100,
    100,
    Rgba::WHITE,
    renderer.font_context().clone(),
    ImageCache::new(),
    1.0,
    Point::ZERO,
    PaintParallelism::disabled(),
    &ScrollState::default(),
    PaintBackend::DisplayList,
  )
  .expect("paint");

  // Left half is visible (mask contains a 50px-wide white rect via <use>).
  assert_eq!(pixel(&pixmap, 10, 50), (255, 0, 0, 255));
  // Right half is masked out and shows the canvas background.
  assert_eq!(pixel(&pixmap, 90, 50), (255, 255, 255, 255));
}

#[test]
fn svg_mask_image_fragment_reference_masks_element() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: white; }
      #box {
        width: 100px;
        height: 20px;
        background: rgb(255 0 0);
        mask-image: url(#m);
        mask-repeat: no-repeat;
        mask-size: 100% 100%;
        mask-position: 0 0;
      }
    </style>
    <svg width="0" height="0" style="position:absolute">
      <mask id="m">
        <rect x="0" y="0" width="100" height="20" fill="black"/>
        <rect x="50" y="0" width="50" height="20" fill="white"/>
      </mask>
    </svg>
    <div id="box"></div>
  "#;

  let (list, font_ctx) = build_display_list(html, 100, 20);
  let pixmap = DisplayListRenderer::new(100, 20, Rgba::WHITE, font_ctx)
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("render");

  assert_eq!(pixel(&pixmap, 10, 10), (255, 255, 255, 255));
  assert_eq!(pixel(&pixmap, 75, 10), (255, 0, 0, 255));
}

#[test]
fn svg_mask_image_match_source_respects_mask_type_alpha() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: white; }
      #box {
        width: 100px;
        height: 20px;
        background: rgb(255 0 0);
        mask-image: url(#m);
        mask-repeat: no-repeat;
        mask-size: 100% 100%;
        mask-position: 0 0;
      }
    </style>
    <svg width="0" height="0" style="position:absolute">
      <mask id="m" mask-type="alpha">
        <rect x="0" y="0" width="50" height="20" fill="black"/>
        <rect x="50" y="0" width="50" height="20" fill="white"/>
      </mask>
    </svg>
    <div id="box"></div>
  "#;

  let (list, font_ctx) = build_display_list(html, 100, 20);
  let pixmap = DisplayListRenderer::new(100, 20, Rgba::WHITE, font_ctx)
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("render");

  // With mask-type="alpha", opaque black and opaque white both yield full opacity.
  assert_eq!(pixel(&pixmap, 10, 10), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 75, 10), (255, 0, 0, 255));
}

#[test]
fn svg_mask_image_respects_maskContentUnits_object_bounding_box() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: white; }
      #box {
        width: 100px;
        height: 20px;
        background: rgb(255 0 0);
        mask-image: url(#m);
        mask-repeat: no-repeat;
        mask-size: 100% 100%;
        mask-position: 0 0;
      }
    </style>
    <svg width="0" height="0" style="position:absolute">
      <mask id="m" maskContentUnits="objectBoundingBox" maskUnits="objectBoundingBox">
        <rect x="0" y="0" width="0.5" height="1" fill="black"/>
        <rect x="0.5" y="0" width="0.5" height="1" fill="white"/>
      </mask>
    </svg>
    <div id="box"></div>
  "#;

  let (list, font_ctx) = build_display_list(html, 100, 20);
  let pixmap = DisplayListRenderer::new(100, 20, Rgba::WHITE, font_ctx)
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("render");

  assert_eq!(pixel(&pixmap, 10, 10), (255, 255, 255, 255));
  assert_eq!(pixel(&pixmap, 75, 10), (255, 0, 0, 255));
}

#[test]
fn svg_mask_image_respects_maskUnits_user_space_on_use() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: white; }
      #box {
        width: 100px;
        height: 20px;
        background: rgb(255 0 0);
        mask-image: url(#m);
        mask-repeat: no-repeat;
        mask-size: 100% 100%;
        mask-position: 0 0;
      }
    </style>
    <svg width="0" height="0" style="position:absolute">
      <mask id="m" maskUnits="userSpaceOnUse" x="0" y="0" width="50" height="20">
        <rect width="50" height="20" fill="white"/>
      </mask>
    </svg>
    <div id="box"></div>
  "#;

  let (list, font_ctx) = build_display_list(html, 100, 20);
  let pixmap = DisplayListRenderer::new(100, 20, Rgba::WHITE, font_ctx)
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("render");

  assert_eq!(pixel(&pixmap, 25, 10), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 75, 10), (255, 255, 255, 255));
}

#[test]
fn svg_mask_image_does_not_trigger_fetch_errors() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: white; }
      #box {
        width: 100px;
        height: 20px;
        background: rgb(255 0 0);
        mask-image: url(#m);
        mask-repeat: no-repeat;
        mask-size: 100% 100%;
        mask-position: 0 0;
      }
    </style>
    <svg width="0" height="0" style="position:absolute">
      <mask id="m">
        <rect x="0" y="0" width="100" height="20" fill="black"/>
        <rect x="50" y="0" width="50" height="20" fill="white"/>
      </mask>
    </svg>
    <div id="box"></div>
  "#;

  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("renderer");

  let options = RenderOptions::new()
    .with_viewport(100, 20)
    .with_diagnostics_level(DiagnosticsLevel::Basic);
  let result = renderer
    .render_html_with_diagnostics(html, options)
    .expect("render");

  assert!(
    result.diagnostics.fetch_errors.is_empty(),
    "expected mask-image:url(#m) to stay local, got fetch errors: {:?}",
    result.diagnostics.fetch_errors
  );
}

#[test]
fn svg_mask_image_missing_id_does_not_trigger_fetch_errors() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: white; }
      #box {
        width: 20px;
        height: 20px;
        background: rgb(255 0 0);
        mask-image: url(#missing);
        mask-repeat: no-repeat;
        mask-size: 100% 100%;
        mask-position: 0 0;
      }
    </style>
    <div id="box"></div>
  "#;

  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("renderer");

  let options = RenderOptions::new()
    .with_viewport(20, 20)
    .with_diagnostics_level(DiagnosticsLevel::Basic);
  let result = renderer
    .render_html_with_diagnostics(html, options)
    .expect("render");

  assert!(
    result.diagnostics.fetch_errors.is_empty(),
    "expected mask-image:url(#missing) to stay local, got fetch errors: {:?}",
    result.diagnostics.fetch_errors
  );
  assert_eq!(pixel(&result.pixmap, 10, 10), (255, 0, 0, 255));
}
