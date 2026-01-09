use fastrender::debug::runtime::{self, RuntimeToggles};
use fastrender::image_loader::ImageCache;
use fastrender::paint::display_list::DisplayList;
use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::paint::display_list_renderer::{DisplayListRenderer, PaintParallelism};
use fastrender::scroll::ScrollState;
use fastrender::text::font_loader::FontContext;
use fastrender::tree::fragment_tree::FragmentNode;
use fastrender::{FastRender, FontConfig, Point, Rgba};
use std::collections::HashMap;
use std::sync::Arc;

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
fn trace_backdrop_stack_smoke() {
  let toggles = Arc::new(RuntimeToggles::from_map(HashMap::from([
    ("FASTR_TRACE_BACKDROP_STACK".to_string(), "1".to_string()),
    ("FASTR_DISPLAY_LIST_PARALLEL".to_string(), "0".to_string()),
  ])));

  runtime::with_runtime_toggles(toggles, || {
    crate::rayon_test_util::init_rayon_for_tests(1);

    let html = r#"<!doctype html>
      <style>
        html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
        #overlay {
          position: absolute;
          left: 0;
          top: 0;
          width: 40px;
          height: 40px;
          backdrop-filter: invert(1);
          background: transparent;
        }
      </style>
      <div id="overlay"></div>
    "#;

    let (list, font_ctx) = build_display_list(html, 64, 64);
    let pixmap = DisplayListRenderer::new(64, 64, Rgba::WHITE, font_ctx)
      .expect("renderer")
      .with_parallelism(PaintParallelism::disabled())
      .render(&list)
      .expect("render");
    assert_eq!(pixmap.width(), 64);
    assert_eq!(pixmap.height(), 64);
  });
}
