use fastrender::image_loader::ImageCache;
use fastrender::paint::display_list::DisplayList;
use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::paint::display_list_renderer::{DisplayListRenderer, PaintParallelism};
use fastrender::scroll::ScrollState;
use fastrender::text::font_loader::FontContext;
use fastrender::tree::fragment_tree::FragmentNode;
use fastrender::{FastRender, FontConfig, Point, Rgba};
use std::sync::Once;

fn init_rayon_for_tests() {
  static INIT: Once = Once::new();
  INIT.call_once(|| {
    // Many CI machines have very high CPU counts; combining that with an address-space cap can
    // cause Rayon global pool initialization to fail when it tries to spawn one worker per CPU.
    // Constrain the default pool so these paint regressions are stable under `run_limited.sh`.
    std::env::set_var("RAYON_NUM_THREADS", "2");
    let _ = rayon::ThreadPoolBuilder::new().num_threads(2).build_global();
  });
}

fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let p = pixmap.pixel(x, y).unwrap();
  (p.red(), p.green(), p.blue(), p.alpha())
}

fn build_display_list(html: &str, width: u32, height: u32) -> (DisplayList, FontContext) {
  init_rayon_for_tests();
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

fn render(html: &str, width: u32, height: u32) -> tiny_skia::Pixmap {
  let (list, font_ctx) = build_display_list(html, width, height);
  DisplayListRenderer::new(width, height, Rgba::WHITE, font_ctx)
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("render")
}

#[test]
fn filter_triggers_backdrop_root() {
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
        filter: blur(0.1px);
      }
      #overlay {
        width: 40px;
        height: 40px;
        backdrop-filter: invert(1);
      }
    </style>
    <div id="bg"></div>
    <div id="parent"><div id="overlay"></div></div>
  "#;

  let pixmap = render(html, 64, 64);

  // If `filter` establishes a Backdrop Root (per filter-effects-2), the backdrop image for
  // `#overlay` cannot include `#bg` (which is outside `#parent`), so the overlay is transparent.
  assert_eq!(pixel(&pixmap, 20, 20), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn opacity_triggers_backdrop_root() {
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
        opacity: 0.5;
      }
      #overlay {
        width: 40px;
        height: 40px;
        backdrop-filter: invert(1);
      }
    </style>
    <div id="bg"></div>
    <div id="parent"><div id="overlay"></div></div>
  "#;

  let pixmap = render(html, 64, 64);

  // If `opacity < 1` establishes a Backdrop Root, `#overlay` cannot sample `#bg`. Otherwise,
  // it would invert the red backdrop to cyan and then `#parent`'s opacity would blend it back
  // onto red, yielding mid-gray.
  assert_eq!(pixel(&pixmap, 20, 20), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn mask_image_triggers_backdrop_root() {
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
        mask-image: linear-gradient(black, black);
        mask-mode: alpha;
        mask-repeat: no-repeat;
        mask-size: 100% 100%;
      }
      #overlay {
        width: 40px;
        height: 40px;
        backdrop-filter: invert(1);
      }
    </style>
    <div id="bg"></div>
    <div id="parent"><div id="overlay"></div></div>
  "#;

  let pixmap = render(html, 64, 64);

  // `mask-image` establishes a Backdrop Root (like clip-path). The mask itself is fully opaque
  // so the only observable difference is whether the backdrop-filter samples beyond `#parent`.
  assert_eq!(pixel(&pixmap, 20, 20), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn mix_blend_mode_triggers_backdrop_root() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent {
        position: absolute;
        left: 0;
        top: 0;
        width: 40px;
        height: 40px;
        mix-blend-mode: multiply;
      }
      #overlay {
        width: 40px;
        height: 40px;
        backdrop-filter: invert(1);
      }
    </style>
    <div id="parent"><div id="overlay"></div></div>
  "#;

  let pixmap = render(html, 64, 64);

  // `mix-blend-mode` establishes a Backdrop Root (filter-effects-2). Without that boundary,
  // `#overlay` would sample and invert the body background, producing cyan that would then be
  // multiplied with red to yield black.
  assert_eq!(pixel(&pixmap, 20, 20), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn clip_path_triggers_backdrop_root() {
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
        clip-path: inset(0);
      }
      #overlay {
        width: 40px;
        height: 40px;
        backdrop-filter: invert(1);
      }
    </style>
    <div id="bg"></div>
    <div id="parent"><div id="overlay"></div></div>
  "#;

  let pixmap = render(html, 64, 64);

  // `clip-path` establishes a Backdrop Root even when the clip is a no-op. The parent has no
  // backdrop of its own, so the overlay's backdrop-filter must not sample and invert `#bg`.
  assert_eq!(pixel(&pixmap, 20, 20), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}
