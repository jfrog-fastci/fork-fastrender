use fastrender::image_loader::ImageCache;
use fastrender::paint::display_list::DisplayList;
use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::paint::display_list_renderer::{DisplayListRenderer, PaintParallelism};
use fastrender::scroll::ScrollState;
use fastrender::text::font_loader::FontContext;
use fastrender::tree::fragment_tree::FragmentNode;
use fastrender::{FastRender, FontConfig, Point, Rgba};
use std::sync::{Mutex, MutexGuard};

// The Rust test harness runs tests in parallel by default. These paint regressions build full
// render/layout pipelines and can exceed memory limits when too many execute concurrently under
// `scripts/run_limited.sh`.
static TEST_LOCK: Mutex<()> = Mutex::new(());

fn lock_tests() -> MutexGuard<'static, ()> {
  TEST_LOCK.lock().unwrap_or_else(|err| err.into_inner())
}

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
      // Keep display-list building deterministic; these tests focus on backdrop root behaviour.
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

fn render_backdrop_invert_with_parent_will_change(value: &str) -> tiny_skia::Pixmap {
  let html = format!(
    r#"<!doctype html>
      <style>
        html, body {{ margin: 0; padding: 0; background: rgb(255 0 0); }}
        #parent {{ position: absolute; inset: 0; will-change: {value}; }}
        #child {{
          position: absolute;
          left: 0;
          top: 0;
          width: 40px;
          height: 40px;
          backdrop-filter: invert(1);
          background: transparent;
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

#[test]
fn will_change_filter_establishes_backdrop_root() {
  let _guard = lock_tests();
  // Per Filter Effects Level 2, `will-change` hints for properties that would establish a
  // Backdrop Root (e.g. `filter`) must do so immediately.
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent { position: absolute; inset: 0; will-change: filter; }
      #child {
        position: absolute;
        left: 0;
        top: 0;
        width: 40px;
        height: 40px;
        backdrop-filter: invert(1);
        background: transparent;
      }
    </style>
    <div id="parent"><div id="child"></div></div>
  "#;

  let (list, font_ctx) = build_display_list(html, 64, 64);
  let pixmap = DisplayListRenderer::new(64, 64, Rgba::WHITE, font_ctx)
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("render");

  // The child's backdrop-filter sees the empty backdrop-root image (transparent), producing a
  // transparent result that lets the underlying page background show through unchanged.
  assert_eq!(pixel(&pixmap, 20, 20), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn will_change_transform_does_not_establish_backdrop_root() {
  let _guard = lock_tests();
  // `transform` is not a Backdrop Root trigger; `will-change: transform` must not clip the
  // backdrop that descendant `backdrop-filter` elements can see.
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent { position: absolute; inset: 0; will-change: transform; }
      #child {
        position: absolute;
        left: 0;
        top: 0;
        width: 40px;
        height: 40px;
        backdrop-filter: invert(1);
        background: transparent;
      }
    </style>
    <div id="parent"><div id="child"></div></div>
  "#;

  let (list, font_ctx) = build_display_list(html, 64, 64);
  let pixmap = DisplayListRenderer::new(64, 64, Rgba::WHITE, font_ctx)
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("render");

  // Red backdrop inverted to cyan.
  assert_eq!(pixel(&pixmap, 20, 20), (0, 255, 255, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn will_change_webkit_transform_does_not_establish_backdrop_root() {
  let _guard = lock_tests();
  // `-webkit-transform` should be treated as `transform` for `will-change` purposes, which means it
  // must *not* establish a Backdrop Root.
  let pixmap = render_backdrop_invert_with_parent_will_change("-webkit-transform");
  assert_eq!(pixel(&pixmap, 20, 20), (0, 255, 255, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn will_change_mix_blend_mode_establishes_backdrop_root() {
  let _guard = lock_tests();
  // `mix-blend-mode` is a Backdrop Root trigger; `will-change` hints for it must do so immediately.
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent { position: absolute; inset: 0; will-change: mix-blend-mode; }
      #child {
        position: absolute;
        left: 0;
        top: 0;
        width: 40px;
        height: 40px;
        backdrop-filter: invert(1);
        background: transparent;
      }
    </style>
    <div id="parent"><div id="child"></div></div>
  "#;

  let (list, font_ctx) = build_display_list(html, 64, 64);
  let pixmap = DisplayListRenderer::new(64, 64, Rgba::WHITE, font_ctx)
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("render");

  // The child's backdrop-filter sees the empty backdrop-root image (transparent), producing a
  // transparent result that lets the underlying page background show through unchanged.
  assert_eq!(pixel(&pixmap, 20, 20), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn will_change_clip_path_establishes_backdrop_root() {
  let _guard = lock_tests();
  // `clip-path` is a Backdrop Root trigger; `will-change` hints for it must do so immediately,
  // even before the element has a non-`none` clip-path applied.
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent { position: absolute; inset: 0; will-change: clip-path; }
      #child {
        position: absolute;
        left: 0;
        top: 0;
        width: 40px;
        height: 40px;
        backdrop-filter: invert(1);
        background: transparent;
      }
    </style>
    <div id="parent"><div id="child"></div></div>
  "#;

  let (list, font_ctx) = build_display_list(html, 64, 64);
  let pixmap = DisplayListRenderer::new(64, 64, Rgba::WHITE, font_ctx)
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("render");

  assert_eq!(pixel(&pixmap, 20, 20), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn will_change_opacity_establishes_backdrop_root() {
  let _guard = lock_tests();
  // Per Filter Effects Level 2, `opacity < 1` establishes a Backdrop Root, so `will-change: opacity`
  // must establish the boundary immediately even before the opacity value changes.
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent { position: absolute; inset: 0; will-change: opacity; }
      #child {
        position: absolute;
        left: 0;
        top: 0;
        width: 40px;
        height: 40px;
        backdrop-filter: invert(1);
        background: transparent;
      }
    </style>
    <div id="parent"><div id="child"></div></div>
  "#;

  let (list, font_ctx) = build_display_list(html, 64, 64);
  let pixmap = DisplayListRenderer::new(64, 64, Rgba::WHITE, font_ctx)
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("render");

  // The child's backdrop-filter sees the empty backdrop-root image (transparent), producing a
  // transparent result that lets the underlying page background show through unchanged.
  assert_eq!(pixel(&pixmap, 20, 20), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn will_change_backdrop_filter_establishes_backdrop_root() {
  let _guard = lock_tests();
  // `backdrop-filter` itself is a Backdrop Root trigger; `will-change` hints must establish the
  // boundary even when the element currently has no backdrop-filter applied.
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent { position: absolute; inset: 0; will-change: backdrop-filter; }
      #child {
        position: absolute;
        left: 0;
        top: 0;
        width: 40px;
        height: 40px;
        backdrop-filter: invert(1);
        background: transparent;
      }
    </style>
    <div id="parent"><div id="child"></div></div>
  "#;

  let (list, font_ctx) = build_display_list(html, 64, 64);
  let pixmap = DisplayListRenderer::new(64, 64, Rgba::WHITE, font_ctx)
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("render");

  assert_eq!(pixel(&pixmap, 20, 20), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn will_change_webkit_backdrop_filter_establishes_backdrop_root() {
  let _guard = lock_tests();
  // `-webkit-backdrop-filter` aliases `backdrop-filter`; will-change hints should behave
  // equivalently for Backdrop Root semantics.
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent { position: absolute; inset: 0; will-change: -webkit-backdrop-filter; }
      #child {
        position: absolute;
        left: 0;
        top: 0;
        width: 40px;
        height: 40px;
        backdrop-filter: invert(1);
        background: transparent;
      }
    </style>
    <div id="parent"><div id="child"></div></div>
  "#;

  let (list, font_ctx) = build_display_list(html, 64, 64);
  let pixmap = DisplayListRenderer::new(64, 64, Rgba::WHITE, font_ctx)
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("render");

  assert_eq!(pixel(&pixmap, 20, 20), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn will_change_webkit_filter_establishes_backdrop_root() {
  let _guard = lock_tests();
  // `-webkit-filter` is a common legacy alias for `filter`; will-change hints should behave
  // equivalently for Backdrop Root semantics.
  let pixmap = render_backdrop_invert_with_parent_will_change("-webkit-filter");
  assert_eq!(pixel(&pixmap, 20, 20), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn will_change_ms_filter_is_not_aliased_to_filter() {
  let _guard = lock_tests();
  // `-ms-filter` is the old IE filter syntax and does not alias modern `filter`. Ensure we do not
  // treat it as a Backdrop Root trigger when it appears in `will-change`.
  let pixmap = render_backdrop_invert_with_parent_will_change("-ms-filter");
  assert_eq!(pixel(&pixmap, 20, 20), (0, 255, 255, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn will_change_webkit_mask_image_establishes_backdrop_root() {
  let _guard = lock_tests();
  // WebKit mask properties are widely used on the web; will-change should treat vendor-prefixed
  // property names as aliases of their unprefixed forms for Backdrop Root semantics.
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent { position: absolute; inset: 0; will-change: -webkit-mask-image; }
      #child {
        position: absolute;
        left: 0;
        top: 0;
        width: 40px;
        height: 40px;
        backdrop-filter: invert(1);
        background: transparent;
      }
    </style>
    <div id="parent"><div id="child"></div></div>
  "#;

  let (list, font_ctx) = build_display_list(html, 64, 64);
  let pixmap = DisplayListRenderer::new(64, 64, Rgba::WHITE, font_ctx)
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("render");

  assert_eq!(pixel(&pixmap, 20, 20), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn will_change_webkit_mask_establishes_backdrop_root() {
  let _guard = lock_tests();
  // `-webkit-mask` is a legacy alias for the `mask` shorthand; treat it equivalently for Backdrop
  // Root semantics.
  let pixmap = render_backdrop_invert_with_parent_will_change("-webkit-mask");
  assert_eq!(pixel(&pixmap, 20, 20), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn will_change_webkit_mask_box_image_establishes_backdrop_root() {
  let _guard = lock_tests();
  // WebKit exposes `mask-border` as `-webkit-mask-box-image`; will-change hints should behave
  // equivalently for Backdrop Root semantics.
  let pixmap = render_backdrop_invert_with_parent_will_change("-webkit-mask-box-image");
  assert_eq!(pixel(&pixmap, 20, 20), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn will_change_webkit_clip_path_establishes_backdrop_root() {
  let _guard = lock_tests();
  // `-webkit-clip-path` aliases `clip-path`; will-change hints should behave equivalently for
  // Backdrop Root semantics.
  let pixmap = render_backdrop_invert_with_parent_will_change("-webkit-clip-path");
  assert_eq!(pixel(&pixmap, 20, 20), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn will_change_multiple_properties_establishes_backdrop_root_if_any_hint_does() {
  let _guard = lock_tests();
  // `will-change` accepts a comma-separated list. If any hinted property is a Backdrop Root trigger,
  // the element must establish a Backdrop Root immediately.
  //
  // This also guards against implementations that only consider the first hint.
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent { position: absolute; inset: 0; will-change: transform, filter; }
      #child {
        position: absolute;
        left: 0;
        top: 0;
        width: 40px;
        height: 40px;
        backdrop-filter: invert(1);
        background: transparent;
      }
    </style>
    <div id="parent"><div id="child"></div></div>
  "#;

  let (list, font_ctx) = build_display_list(html, 64, 64);
  let pixmap = DisplayListRenderer::new(64, 64, Rgba::WHITE, font_ctx)
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("render");

  // Backdrop Root means the child's backdrop-filter samples an empty backdrop and yields transparent,
  // letting the page background show through unchanged.
  assert_eq!(pixel(&pixmap, 20, 20), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn will_change_mask_image_establishes_backdrop_root() {
  let _guard = lock_tests();
  // `mask-image` is a Backdrop Root trigger; `will-change: mask-image` must establish the boundary
  // proactively even before the element has a non-`none` mask-image applied.
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent { position: absolute; inset: 0; will-change: mask-image; }
      #child {
        position: absolute;
        left: 0;
        top: 0;
        width: 40px;
        height: 40px;
        backdrop-filter: invert(1);
        background: transparent;
      }
    </style>
    <div id="parent"><div id="child"></div></div>
  "#;

  let (list, font_ctx) = build_display_list(html, 64, 64);
  let pixmap = DisplayListRenderer::new(64, 64, Rgba::WHITE, font_ctx)
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("render");

  // The child's backdrop-filter sees the empty backdrop-root image (transparent), producing a
  // transparent result that lets the underlying page background show through unchanged.
  assert_eq!(pixel(&pixmap, 20, 20), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn will_change_mask_establishes_backdrop_root() {
  let _guard = lock_tests();
  // `mask` is also a Backdrop Root trigger; `will-change: mask` should establish the same boundary.
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent { position: absolute; inset: 0; will-change: mask; }
      #child {
        position: absolute;
        left: 0;
        top: 0;
        width: 40px;
        height: 40px;
        backdrop-filter: invert(1);
        background: transparent;
      }
    </style>
    <div id="parent"><div id="child"></div></div>
  "#;

  let (list, font_ctx) = build_display_list(html, 64, 64);
  let pixmap = DisplayListRenderer::new(64, 64, Rgba::WHITE, font_ctx)
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("render");

  // The child's backdrop-filter sees the empty backdrop-root image (transparent), producing a
  // transparent result that lets the underlying page background show through unchanged.
  assert_eq!(pixel(&pixmap, 20, 20), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn will_change_mask_border_establishes_backdrop_root() {
  let _guard = lock_tests();
  // `mask-border` is a Backdrop Root trigger; `will-change` hints for it must establish the
  // boundary proactively.
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent { position: absolute; inset: 0; will-change: mask-border; }
      #child {
        position: absolute;
        left: 0;
        top: 0;
        width: 40px;
        height: 40px;
        backdrop-filter: invert(1);
        background: transparent;
      }
    </style>
    <div id="parent"><div id="child"></div></div>
  "#;

  let (list, font_ctx) = build_display_list(html, 64, 64);
  let pixmap = DisplayListRenderer::new(64, 64, Rgba::WHITE, font_ctx)
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("render");

  // The child's backdrop-filter sees the empty backdrop-root image (transparent), producing a
  // transparent result that lets the underlying page background show through unchanged.
  assert_eq!(pixel(&pixmap, 20, 20), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn will_change_mask_border_source_establishes_backdrop_root() {
  let _guard = lock_tests();
  // `mask-border-source` is the longhand that controls whether a mask border is active; `will-change`
  // hints for it must establish a Backdrop Root proactively.
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent { position: absolute; inset: 0; will-change: mask-border-source; }
      #child {
        position: absolute;
        left: 0;
        top: 0;
        width: 40px;
        height: 40px;
        backdrop-filter: invert(1);
        background: transparent;
      }
    </style>
    <div id="parent"><div id="child"></div></div>
  "#;

  let (list, font_ctx) = build_display_list(html, 64, 64);
  let pixmap = DisplayListRenderer::new(64, 64, Rgba::WHITE, font_ctx)
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("render");

  assert_eq!(pixel(&pixmap, 20, 20), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn will_change_webkit_mask_box_image_source_establishes_backdrop_root() {
  let _guard = lock_tests();
  // WebKit exposes `mask-border-source` as `-webkit-mask-box-image-source`; will-change hints should
  // behave equivalently for Backdrop Root semantics.
  let pixmap = render_backdrop_invert_with_parent_will_change("-webkit-mask-box-image-source");
  assert_eq!(pixel(&pixmap, 20, 20), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn will_change_perspective_does_not_establish_backdrop_root() {
  let _guard = lock_tests();
  // `perspective` is not a Backdrop Root trigger; `will-change: perspective` must not clip the
  // backdrop that descendant `backdrop-filter` elements can see.
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent { position: absolute; inset: 0; will-change: perspective; }
      #child {
        position: absolute;
        left: 0;
        top: 0;
        width: 40px;
        height: 40px;
        backdrop-filter: invert(1);
        background: transparent;
      }
    </style>
    <div id="parent"><div id="child"></div></div>
  "#;

  let (list, font_ctx) = build_display_list(html, 64, 64);
  let pixmap = DisplayListRenderer::new(64, 64, Rgba::WHITE, font_ctx)
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("render");

  // Red backdrop inverted to cyan.
  assert_eq!(pixel(&pixmap, 20, 20), (0, 255, 255, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn will_change_other_stacking_context_hints_do_not_establish_backdrop_root() {
  let _guard = lock_tests();
  // `will-change` can proactively create stacking contexts for performance (e.g. transforms),
  // but only hints for Backdrop Root triggers should stop backdrop-filter sampling.
  for value in [
    "translate",
    "rotate",
    "scale",
    "isolation",
    "contain",
    "z-index",
    "-moz-transform",
    "-ms-transform",
    "scroll-position",
    "contents",
  ] {
    let pixmap = render_backdrop_invert_with_parent_will_change(value);
    assert_eq!(
      pixel(&pixmap, 20, 20),
      (0, 255, 255, 255),
      "will-change: {value} must not establish a Backdrop Root"
    );
    assert_eq!(
      pixel(&pixmap, 50, 50),
      (255, 0, 0, 255),
      "will-change: {value} must not establish a Backdrop Root"
    );
  }
}

#[test]
fn will_change_auto_does_not_establish_backdrop_root() {
  let _guard = lock_tests();
  let pixmap = render_backdrop_invert_with_parent_will_change("auto");
  // Red backdrop inverted to cyan.
  assert_eq!(pixel(&pixmap, 20, 20), (0, 255, 255, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn will_change_unknown_property_does_not_establish_backdrop_root() {
  let _guard = lock_tests();
  // Unknown properties should not become Backdrop Root triggers just because they are mentioned in
  // `will-change`.
  let pixmap = render_backdrop_invert_with_parent_will_change("background-color");
  // Red backdrop inverted to cyan.
  assert_eq!(pixel(&pixmap, 20, 20), (0, 255, 255, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn will_change_filter_is_case_insensitive() {
  let _guard = lock_tests();
  let pixmap = render_backdrop_invert_with_parent_will_change("FILTER");
  // Backdrop sampling stops at the will-change backdrop root.
  assert_eq!(pixel(&pixmap, 20, 20), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn will_change_filter_with_comment_establishes_backdrop_root() {
  let _guard = lock_tests();
  // CSS comments are ignored during parsing; they must not affect will-change hint recognition.
  let pixmap = render_backdrop_invert_with_parent_will_change("filter/*comment*/");
  assert_eq!(pixel(&pixmap, 20, 20), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn will_change_auto_with_comment_does_not_establish_backdrop_root() {
  let _guard = lock_tests();
  let pixmap = render_backdrop_invert_with_parent_will_change("auto/*comment*/");
  // Red backdrop inverted to cyan.
  assert_eq!(pixel(&pixmap, 20, 20), (0, 255, 255, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn will_change_auto_with_comment_overrides_hints() {
  let _guard = lock_tests();
  // Comments should not interfere with recognizing `auto`, and `auto` should override earlier valid
  // `will-change` hints via the cascade.
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent { position: absolute; inset: 0; will-change: filter; }
      #parent { will-change: auto/*comment*/; }
      #child {
        position: absolute;
        left: 0;
        top: 0;
        width: 40px;
        height: 40px;
        backdrop-filter: invert(1);
        background: transparent;
      }
    </style>
    <div id="parent"><div id="child"></div></div>
  "#;

  let (list, font_ctx) = build_display_list(html, 64, 64);
  let pixmap = DisplayListRenderer::new(64, 64, Rgba::WHITE, font_ctx)
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("render");

  // Red backdrop inverted to cyan.
  assert_eq!(pixel(&pixmap, 20, 20), (0, 255, 255, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn will_change_ident_function_establishes_backdrop_root() {
  let _guard = lock_tests();
  // CSS Values defines `ident()` to produce custom-ident values. `will-change` accepts custom-ident
  // hints, so `ident(filter)` must behave like `filter`.
  let pixmap = render_backdrop_invert_with_parent_will_change("ident(filter)");
  assert_eq!(pixel(&pixmap, 20, 20), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn will_change_multiple_hints_establishes_backdrop_root_if_any_hint_triggers() {
  let _guard = lock_tests();
  // Per the will-change spec, any hint that would establish a Backdrop Root must do so immediately.
  let pixmap = render_backdrop_invert_with_parent_will_change("transform, filter");
  assert_eq!(pixel(&pixmap, 20, 20), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn will_change_with_auto_mixed_in_is_invalid_and_ignored() {
  let _guard = lock_tests();
  // `auto` cannot appear in a comma-separated list; the whole declaration is invalid and should be
  // ignored.
  let pixmap = render_backdrop_invert_with_parent_will_change("auto, filter");
  // Red backdrop inverted to cyan.
  assert_eq!(pixel(&pixmap, 20, 20), (0, 255, 255, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn will_change_with_inherit_mixed_in_is_invalid_and_ignored() {
  let _guard = lock_tests();
  // CSS-wide keywords are not valid <<custom-ident>>s, so this value is invalid.
  let pixmap = render_backdrop_invert_with_parent_will_change("filter, inherit");
  // Red backdrop inverted to cyan.
  assert_eq!(pixel(&pixmap, 20, 20), (0, 255, 255, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn will_change_with_default_mixed_in_is_invalid_and_ignored() {
  let _guard = lock_tests();
  // css-values-4 reserves `default` so it is not a valid <<custom-ident>>.
  let pixmap = render_backdrop_invert_with_parent_will_change("filter, default");
  // Red backdrop inverted to cyan.
  assert_eq!(pixel(&pixmap, 20, 20), (0, 255, 255, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn will_change_trailing_comma_is_invalid_and_ignored() {
  let _guard = lock_tests();
  // Trailing commas are not allowed in comma-separated lists.
  let pixmap = render_backdrop_invert_with_parent_will_change("filter,");
  // Red backdrop inverted to cyan.
  assert_eq!(pixel(&pixmap, 20, 20), (0, 255, 255, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn will_change_space_separated_list_is_invalid_and_ignored() {
  let _guard = lock_tests();
  // `will-change` uses the `#` list grammar, which requires commas. Space-separated lists like
  // `transform filter` are invalid and should be ignored.
  let pixmap = render_backdrop_invert_with_parent_will_change("transform filter");
  // Red backdrop inverted to cyan.
  assert_eq!(pixel(&pixmap, 20, 20), (0, 255, 255, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn will_change_double_comma_is_invalid_and_ignored() {
  let _guard = lock_tests();
  // Empty list items are invalid (`filter,,opacity`), so the entire declaration should be ignored.
  let pixmap = render_backdrop_invert_with_parent_will_change("filter,,opacity");
  // Red backdrop inverted to cyan.
  assert_eq!(pixel(&pixmap, 20, 20), (0, 255, 255, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn will_change_quoted_string_is_invalid_and_ignored() {
  let _guard = lock_tests();
  // `will-change` does not accept quoted strings; declarations like `"filter"` should be ignored.
  let pixmap = render_backdrop_invert_with_parent_will_change(r#""filter""#);
  // Red backdrop inverted to cyan.
  assert_eq!(pixel(&pixmap, 20, 20), (0, 255, 255, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn will_change_quoted_string_is_invalid_and_does_not_override() {
  let _guard = lock_tests();
  // Quoted strings are invalid, so they must not override an earlier valid `will-change` hint list.
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent { position: absolute; inset: 0; will-change: filter; }
      #parent { will-change: "transform"; }
      #child {
        position: absolute;
        left: 0;
        top: 0;
        width: 40px;
        height: 40px;
        backdrop-filter: invert(1);
        background: transparent;
      }
    </style>
    <div id="parent"><div id="child"></div></div>
  "#;

  let (list, font_ctx) = build_display_list(html, 64, 64);
  let pixmap = DisplayListRenderer::new(64, 64, Rgba::WHITE, font_ctx)
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("render");

  assert_eq!(pixel(&pixmap, 20, 20), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn will_change_none_is_invalid_and_does_not_override() {
  let _guard = lock_tests();
  // css-will-change-1 excludes `none` from <<custom-ident>>, so it is invalid here and should not
  // override the earlier Backdrop Root-triggering value.
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent { position: absolute; inset: 0; will-change: filter; }
      #parent { will-change: none; }
      #child {
        position: absolute;
        left: 0;
        top: 0;
        width: 40px;
        height: 40px;
        backdrop-filter: invert(1);
        background: transparent;
      }
    </style>
    <div id="parent"><div id="child"></div></div>
  "#;

  let (list, font_ctx) = build_display_list(html, 64, 64);
  let pixmap = DisplayListRenderer::new(64, 64, Rgba::WHITE, font_ctx)
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("render");

  assert_eq!(pixel(&pixmap, 20, 20), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn will_change_all_is_invalid_and_does_not_override() {
  let _guard = lock_tests();
  // css-will-change-1 excludes `all` from <<custom-ident>>, so it is invalid here and should not
  // override the earlier Backdrop Root-triggering value.
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent { position: absolute; inset: 0; will-change: filter; }
      #parent { will-change: all; }
      #child {
        position: absolute;
        left: 0;
        top: 0;
        width: 40px;
        height: 40px;
        backdrop-filter: invert(1);
        background: transparent;
      }
    </style>
    <div id="parent"><div id="child"></div></div>
  "#;

  let (list, font_ctx) = build_display_list(html, 64, 64);
  let pixmap = DisplayListRenderer::new(64, 64, Rgba::WHITE, font_ctx)
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("render");

  assert_eq!(pixel(&pixmap, 20, 20), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn will_change_will_change_is_invalid_and_does_not_override() {
  let _guard = lock_tests();
  // css-will-change-1 excludes `will-change` from <<custom-ident>>, so it is invalid as a hint and
  // should not override the earlier Backdrop Root-triggering value.
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent { position: absolute; inset: 0; will-change: filter; }
      #parent { will-change: will-change; }
      #child {
        position: absolute;
        left: 0;
        top: 0;
        width: 40px;
        height: 40px;
        backdrop-filter: invert(1);
        background: transparent;
      }
    </style>
    <div id="parent"><div id="child"></div></div>
  "#;

  let (list, font_ctx) = build_display_list(html, 64, 64);
  let pixmap = DisplayListRenderer::new(64, 64, Rgba::WHITE, font_ctx)
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("render");

  assert_eq!(pixel(&pixmap, 20, 20), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn will_change_auto_is_case_insensitive_and_overrides_hints() {
  let _guard = lock_tests();
  // `auto` is a keyword (not a custom-ident) and should match ASCII case-insensitively.
  // It should also override earlier valid `will-change` declarations via the normal cascade.
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent { position: absolute; inset: 0; will-change: filter; }
      #parent { will-change: AUTO; }
      #parent { will-change: AUTO; }
      #child {
        position: absolute;
        left: 0;
        top: 0;
        width: 40px;
        height: 40px;
        backdrop-filter: invert(1);
        background: transparent;
      }
    </style>
    <div id="parent"><div id="child"></div></div>
  "#;

  let (list, font_ctx) = build_display_list(html, 64, 64);
  let pixmap = DisplayListRenderer::new(64, 64, Rgba::WHITE, font_ctx)
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("render");

  // Red backdrop inverted to cyan.
  assert_eq!(pixel(&pixmap, 20, 20), (0, 255, 255, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn will_change_default_is_invalid_and_does_not_override() {
  let _guard = lock_tests();
  // css-values-3 reserves `default` (not a valid <<custom-ident>>), so it is invalid here and
  // should not override the earlier Backdrop Root-triggering value.
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent { position: absolute; inset: 0; will-change: filter; }
      #parent { will-change: default; }
      #child {
        position: absolute;
        left: 0;
        top: 0;
        width: 40px;
        height: 40px;
        backdrop-filter: invert(1);
        background: transparent;
      }
    </style>
    <div id="parent"><div id="child"></div></div>
  "#;

  let (list, font_ctx) = build_display_list(html, 64, 64);
  let pixmap = DisplayListRenderer::new(64, 64, Rgba::WHITE, font_ctx)
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("render");

  assert_eq!(pixel(&pixmap, 20, 20), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}
