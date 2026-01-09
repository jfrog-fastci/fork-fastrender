use fastrender::image_loader::ImageCache;
use fastrender::paint::display_list::DisplayList;
use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::paint::display_list_renderer::{DisplayListRenderer, PaintParallelism};
use fastrender::scroll::ScrollState;
use fastrender::text::font_loader::FontContext;
use fastrender::tree::fragment_tree::FragmentNode;
use fastrender::{FastRender, FastRenderConfig, FontConfig, LayoutParallelism, Point, Rgba};
use rayon::ThreadPoolBuilder;

fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let p = pixmap.pixel(x, y).unwrap();
  (p.red(), p.green(), p.blue(), p.alpha())
}

fn build_display_list(html: &str, width: u32, height: u32) -> (DisplayList, FontContext) {
  let config = FastRenderConfig::new()
    .with_font_sources(FontConfig::bundled_only())
    // Keep tests deterministic and avoid relying on a multi-threaded Rayon pool.
    .with_layout_parallelism(LayoutParallelism::disabled());
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
  // Use a dedicated single-threaded Rayon pool so the test binary doesn't need to initialize the
  // global pool (which can fail under constrained CI sandboxes).
  let pool = ThreadPoolBuilder::new()
    .num_threads(1)
    .build()
    .expect("rayon thread pool");
  pool.install(|| {
    let (list, font_ctx) = build_display_list(html, width, height);
    DisplayListRenderer::new(width, height, Rgba::WHITE, font_ctx)
      .expect("renderer")
      .with_parallelism(PaintParallelism::disabled())
      .render(&list)
      .expect("render")
  })
}

#[test]
fn backdrop_filter_crosses_isolation_isolate_layer() {
  let html = r#"<!doctype html>
    <style>
      body { margin:0; background: rgb(255 0 0); }
      #iso { position:absolute; inset:0; isolation: isolate; }
      #overlay {
        position:absolute;
        left:0;
        top:0;
        width:40px;
        height:40px;
        backdrop-filter: invert(1);
      }
    </style>
    <div id="iso">
      <div id="overlay"></div>
    </div>
  "#;

  let pixmap = render(html, 64, 64);
  // Inverting the red body backdrop yields cyan even though the parent stacking context is
  // isolated. This should fail on implementations that only sample the immediate parent layer.
  assert_eq!(pixel(&pixmap, 20, 20), (0, 255, 255, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn backdrop_filter_stops_at_opacity_backdrop_root() {
  let html = r#"<!doctype html>
    <style>
      body { margin:0; background: rgb(255 0 0); }
      #root { position:absolute; inset:0; opacity: 0.5; }
      #overlay {
        position:absolute;
        left:0;
        top:0;
        width:40px;
        height:40px;
        backdrop-filter: invert(1);
      }
    </style>
    <div id="root">
      <div id="overlay"></div>
    </div>
  "#;

  let pixmap = render(html, 64, 64);
  // Opacity establishes a Backdrop Root: the red body background is outside the root's Backdrop
  // Root Image and must not be sampled.
  assert_eq!(pixel(&pixmap, 20, 20), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}
