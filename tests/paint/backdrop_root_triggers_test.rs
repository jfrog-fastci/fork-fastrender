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
fn filter_url_triggers_backdrop_root_even_when_unresolved() {
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
        /* The filter cannot be resolved, but per filter-effects-2 the property presence still
           establishes a Backdrop Root boundary. */
        filter: url(#missing);
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

  // The unresolved filter does not affect output, but it must still stop backdrop-filter sampling
  // at `#parent`.
  assert_eq!(pixel(&pixmap, 20, 20), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn filter_url_triggers_backdrop_root_for_mix_blend_mode_even_when_unresolved() {
  let html_without_backdrop_root = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; }
      #bg { position: absolute; inset: 0; background: rgb(0 255 0); }
      #parent {
        position: absolute;
        left: 0;
        top: 0;
        width: 40px;
        height: 40px;
      }
      #overlay {
        width: 40px;
        height: 40px;
        background: rgb(255 0 0);
        mix-blend-mode: difference;
      }
    </style>
    <div id="bg"></div>
    <div id="parent"><div id="overlay"></div></div>
  "#;

  let pixmap = render(html_without_backdrop_root, 64, 64);
  assert_eq!(
    pixel(&pixmap, 20, 20),
    (255, 255, 0, 255),
    "sanity: without a backdrop-root boundary, mix-blend-mode should blend with the page backdrop"
  );

  let html_with_backdrop_root = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; }
      #bg { position: absolute; inset: 0; background: rgb(0 255 0); }
      #parent {
        position: absolute;
        left: 0;
        top: 0;
        width: 40px;
        height: 40px;
        /* The filter cannot be resolved, but per filter-effects-2 the property presence still
           establishes a Backdrop Root boundary. */
        filter: url(#missing);
      }
      #overlay {
        width: 40px;
        height: 40px;
        background: rgb(255 0 0);
        mix-blend-mode: difference;
      }
    </style>
    <div id="bg"></div>
    <div id="parent"><div id="overlay"></div></div>
  "#;

  let pixmap = render(html_with_backdrop_root, 64, 64);

  // The unresolved filter itself does not affect output, but the Backdrop Root boundary must
  // confine mix-blend-mode blending to `#parent` (which has no backdrop of its own).
  assert_eq!(pixel(&pixmap, 20, 20), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (0, 255, 0, 255));
}

#[test]
fn backdrop_filter_url_triggers_backdrop_root_even_when_unresolved() {
  let html_without_backdrop_root = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent {
        width: 20px;
        height: 20px;
        margin: 20px;
      }
      #overlay {
        width: 40px;
        height: 40px;
        position: relative;
        left: -10px;
        top: -10px;
        backdrop-filter: invert(1);
        box-sizing: border-box;
        border: 2px solid rgb(0 255 0);
      }
    </style>
    <div id="parent"><div id="overlay"></div></div>
  "#;
  let pixmap = render(html_without_backdrop_root, 64, 64);
  assert_eq!(
    pixel(&pixmap, 15, 15),
    (0, 255, 255, 255),
    "sanity: without a backdrop-root boundary, the overlay should invert the body background"
  );

  let html_with_backdrop_root = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent {
        width: 20px;
        height: 20px;
        margin: 20px;
        /* The backdrop-filter cannot be resolved, but per filter-effects-2 the property presence
           still establishes a Backdrop Root boundary for descendant backdrop-filter sampling. */
        backdrop-filter: url(#missing);
      }
      #overlay {
        width: 40px;
        height: 40px;
        position: relative;
        left: -10px;
        top: -10px;
        backdrop-filter: invert(1);
        box-sizing: border-box;
        border: 2px solid rgb(0 255 0);
      }
    </style>
    <div id="parent"><div id="overlay"></div></div>
  "#;

  let pixmap = render(html_with_backdrop_root, 64, 64);

  // The overlay extends outside `#parent`. If `backdrop-filter` establishes a Backdrop Root on the
  // parent, then the overlay's backdrop-filter must not sample the body backdrop outside
  // `#parent`, so this pixel stays red.
  assert_eq!(pixel(&pixmap, 11, 25), (0, 255, 0, 255));
  assert_eq!(pixel(&pixmap, 15, 15), (255, 0, 0, 255));
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
fn isolation_isolate_does_not_trigger_backdrop_root() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent {
        width: 20px;
        height: 20px;
        margin: 20px;
        /* `isolation: isolate` creates an isolated group (compositing), but Filter Effects Level 2
           does NOT list it as a Backdrop Root trigger. Descendant backdrop-filter effects must
           still be able to sample the page backdrop outside this element. */
        isolation: isolate;
      }
      #overlay {
        width: 40px;
        height: 40px;
        position: relative;
        left: -10px;
        top: -10px;
        backdrop-filter: invert(1);
        box-sizing: border-box;
        border: 2px solid rgb(0 255 0);
      }
    </style>
    <div id="parent"><div id="overlay"></div></div>
  "#;

  let pixmap = render(html, 64, 64);

  // Sanity-check that the overlay border landed where expected.
  assert_eq!(pixel(&pixmap, 11, 25), (0, 255, 0, 255));
  // The overlay extends outside `#parent`. Since `isolation:isolate` is NOT a Backdrop Root
  // trigger, the overlay's backdrop-filter should still invert the body background outside the
  // parent.
  assert_eq!(pixel(&pixmap, 15, 15), (0, 255, 255, 255));
}

#[test]
fn transform_does_not_trigger_backdrop_root() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent {
        width: 20px;
        height: 20px;
        margin: 20px;
        /* `transform` creates a stacking context but is not a Filter Effects Level 2 Backdrop Root
           trigger. */
        transform: translateX(0px);
      }
      #overlay {
        width: 40px;
        height: 40px;
        position: relative;
        left: -10px;
        top: -10px;
        backdrop-filter: invert(1);
        box-sizing: border-box;
        border: 2px solid rgb(0 255 0);
      }
    </style>
    <div id="parent"><div id="overlay"></div></div>
  "#;

  let pixmap = render(html, 64, 64);

  // Sanity-check that the overlay border landed where expected.
  assert_eq!(pixel(&pixmap, 11, 25), (0, 255, 0, 255));
  // The overlay extends outside `#parent`. Since `transform` is NOT a Backdrop Root trigger, the
  // overlay's backdrop-filter should still invert the body background outside the parent.
  assert_eq!(pixel(&pixmap, 15, 15), (0, 255, 255, 255));
}

#[test]
fn z_index_stacking_context_does_not_trigger_backdrop_root() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent {
        width: 20px;
        height: 20px;
        margin: 20px;
        /* Positioned z-index creates a stacking context but is not a Backdrop Root trigger. */
        position: relative;
        z-index: 0;
      }
      #overlay {
        width: 40px;
        height: 40px;
        position: relative;
        left: -10px;
        top: -10px;
        backdrop-filter: invert(1);
        box-sizing: border-box;
        border: 2px solid rgb(0 255 0);
      }
    </style>
    <div id="parent"><div id="overlay"></div></div>
  "#;

  let pixmap = render(html, 64, 64);

  assert_eq!(pixel(&pixmap, 11, 25), (0, 255, 0, 255));
  assert_eq!(pixel(&pixmap, 15, 15), (0, 255, 255, 255));
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
fn mask_image_url_triggers_backdrop_root_even_when_unresolved() {
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
        /* The mask image cannot be resolved, but per filter-effects-2 the property presence still
           establishes a Backdrop Root boundary. */
        mask-image: url(#missing);
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

  // The mask-image itself does not affect output (it is missing), but it must still stop
  // backdrop-filter sampling at `#parent`.
  assert_eq!(pixel(&pixmap, 20, 20), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}

#[test]
fn mask_border_url_triggers_backdrop_root_even_when_unresolved() {
  let html_without_backdrop_root = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent {
        width: 20px;
        height: 20px;
        margin: 20px;
      }
      #overlay {
        width: 40px;
        height: 40px;
        position: relative;
        left: -10px;
        top: -10px;
        backdrop-filter: invert(1);
        box-sizing: border-box;
        border: 2px solid rgb(0 255 0);
      }
    </style>
    <div id="parent"><div id="overlay"></div></div>
  "#;
  let pixmap = render(html_without_backdrop_root, 64, 64);
  assert_eq!(
    pixel(&pixmap, 15, 15),
    (0, 255, 255, 255),
    "sanity: without a backdrop-root boundary, the overlay should invert the body background"
  );

  let html_with_backdrop_root = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent {
        width: 20px;
        height: 20px;
        margin: 20px;
        /* The mask border cannot be resolved, but per filter-effects-2 the property presence still
           establishes a Backdrop Root boundary. */
        mask-border: url(#missing) 30;
      }
      #overlay {
        width: 40px;
        height: 40px;
        position: relative;
        left: -10px;
        top: -10px;
        backdrop-filter: invert(1);
        box-sizing: border-box;
        border: 2px solid rgb(0 255 0);
      }
    </style>
    <div id="parent"><div id="overlay"></div></div>
  "#;

  let pixmap = render(html_with_backdrop_root, 64, 64);

  // The overlay extends outside `#parent`. If `mask-border` establishes a Backdrop Root on the
  // parent, then the overlay's backdrop-filter must not sample the body backdrop outside
  // `#parent`, so this pixel stays red.
  assert_eq!(pixel(&pixmap, 11, 25), (0, 255, 0, 255));
  assert_eq!(pixel(&pixmap, 15, 15), (255, 0, 0, 255));
}

#[test]
fn mask_border_source_url_triggers_backdrop_root_even_when_unresolved() {
  let html_without_backdrop_root = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent {
        width: 20px;
        height: 20px;
        margin: 20px;
      }
      #overlay {
        width: 40px;
        height: 40px;
        position: relative;
        left: -10px;
        top: -10px;
        backdrop-filter: invert(1);
        box-sizing: border-box;
        border: 2px solid rgb(0 255 0);
      }
    </style>
    <div id="parent"><div id="overlay"></div></div>
  "#;
  let pixmap = render(html_without_backdrop_root, 64, 64);
  assert_eq!(
    pixel(&pixmap, 15, 15),
    (0, 255, 255, 255),
    "sanity: without a backdrop-root boundary, the overlay should invert the body background"
  );

  let html_with_backdrop_root = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent {
        width: 20px;
        height: 20px;
        margin: 20px;
        /* `mask-border-source` participates in the Backdrop Root trigger list. Even if the URL
           cannot be resolved, the property presence still establishes a Backdrop Root boundary. */
        mask-border-source: url(#missing);
      }
      #overlay {
        width: 40px;
        height: 40px;
        position: relative;
        left: -10px;
        top: -10px;
        backdrop-filter: invert(1);
        box-sizing: border-box;
        border: 2px solid rgb(0 255 0);
      }
    </style>
    <div id="parent"><div id="overlay"></div></div>
  "#;

  let pixmap = render(html_with_backdrop_root, 64, 64);

  assert_eq!(pixel(&pixmap, 11, 25), (0, 255, 0, 255));
  assert_eq!(pixel(&pixmap, 15, 15), (255, 0, 0, 255));
}

#[test]
fn webkit_mask_box_image_source_url_triggers_backdrop_root_even_when_unresolved() {
  let html_without_backdrop_root = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent {
        width: 20px;
        height: 20px;
        margin: 20px;
      }
      #overlay {
        width: 40px;
        height: 40px;
        position: relative;
        left: -10px;
        top: -10px;
        backdrop-filter: invert(1);
        box-sizing: border-box;
        border: 2px solid rgb(0 255 0);
      }
    </style>
    <div id="parent"><div id="overlay"></div></div>
  "#;
  let pixmap = render(html_without_backdrop_root, 64, 64);
  assert_eq!(
    pixel(&pixmap, 15, 15),
    (0, 255, 255, 255),
    "sanity: without a backdrop-root boundary, the overlay should invert the body background"
  );

  let html_with_backdrop_root = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent {
        width: 20px;
        height: 20px;
        margin: 20px;
        /* WebKit exposes `mask-border-source` as `-webkit-mask-box-image-source`. */
        -webkit-mask-box-image-source: url(#missing);
      }
      #overlay {
        width: 40px;
        height: 40px;
        position: relative;
        left: -10px;
        top: -10px;
        backdrop-filter: invert(1);
        box-sizing: border-box;
        border: 2px solid rgb(0 255 0);
      }
    </style>
    <div id="parent"><div id="overlay"></div></div>
  "#;

  let pixmap = render(html_with_backdrop_root, 64, 64);

  assert_eq!(pixel(&pixmap, 11, 25), (0, 255, 0, 255));
  assert_eq!(pixel(&pixmap, 15, 15), (255, 0, 0, 255));
}

#[test]
fn mask_border_shorthand_with_repeat_and_mode_triggers_backdrop_root_even_when_unresolved() {
  let html_without_backdrop_root = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent {
        width: 20px;
        height: 20px;
        margin: 20px;
      }
      #overlay {
        width: 40px;
        height: 40px;
        position: relative;
        left: -10px;
        top: -10px;
        backdrop-filter: invert(1);
        box-sizing: border-box;
        border: 2px solid rgb(0 255 0);
      }
    </style>
    <div id="parent"><div id="overlay"></div></div>
  "#;
  let pixmap = render(html_without_backdrop_root, 64, 64);
  assert_eq!(
    pixel(&pixmap, 15, 15),
    (0, 255, 255, 255),
    "sanity: without a backdrop-root boundary, the overlay should invert the body background"
  );

  let html_with_backdrop_root = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent {
        width: 20px;
        height: 20px;
        margin: 20px;
        /* Backdrop Root triggers apply to the entire shorthand, including slash-separated
           width/outset portions and trailing repeat/mode keywords. */
        mask-border: url(#missing) 30 / 10px / 0 stretch alpha;
      }
      #overlay {
        width: 40px;
        height: 40px;
        position: relative;
        left: -10px;
        top: -10px;
        backdrop-filter: invert(1);
        box-sizing: border-box;
        border: 2px solid rgb(0 255 0);
      }
    </style>
    <div id="parent"><div id="overlay"></div></div>
  "#;

  let pixmap = render(html_with_backdrop_root, 64, 64);

  assert_eq!(pixel(&pixmap, 11, 25), (0, 255, 0, 255));
  assert_eq!(pixel(&pixmap, 15, 15), (255, 0, 0, 255));
}

#[test]
fn invalid_mask_border_shorthand_is_ignored_and_does_not_establish_backdrop_root() {
  let html_without_backdrop_root = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent {
        width: 20px;
        height: 20px;
        margin: 20px;
      }
      #overlay {
        width: 40px;
        height: 40px;
        position: relative;
        left: -10px;
        top: -10px;
        backdrop-filter: invert(1);
        box-sizing: border-box;
        border: 2px solid rgb(0 255 0);
      }
    </style>
    <div id="parent"><div id="overlay"></div></div>
  "#;
  let pixmap = render(html_without_backdrop_root, 64, 64);
  assert_eq!(
    pixel(&pixmap, 15, 15),
    (0, 255, 255, 255),
    "sanity: without a backdrop-root boundary, the overlay should invert the body background"
  );

  let html_with_invalid_mask_border = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent {
        width: 20px;
        height: 20px;
        margin: 20px;
        /* Invalid shorthand values must be ignored. */
        mask-border: url(#missing) bogus;
      }
      #overlay {
        width: 40px;
        height: 40px;
        position: relative;
        left: -10px;
        top: -10px;
        backdrop-filter: invert(1);
        box-sizing: border-box;
        border: 2px solid rgb(0 255 0);
      }
    </style>
    <div id="parent"><div id="overlay"></div></div>
  "#;

  let pixmap = render(html_with_invalid_mask_border, 64, 64);
  assert_eq!(pixel(&pixmap, 11, 25), (0, 255, 0, 255));
  assert_eq!(pixel(&pixmap, 15, 15), (0, 255, 255, 255));
}

#[test]
fn webkit_mask_box_image_url_triggers_backdrop_root_even_when_unresolved() {
  let html_without_backdrop_root = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent {
        width: 20px;
        height: 20px;
        margin: 20px;
      }
      #overlay {
        width: 40px;
        height: 40px;
        position: relative;
        left: -10px;
        top: -10px;
        backdrop-filter: invert(1);
        box-sizing: border-box;
        border: 2px solid rgb(0 255 0);
      }
    </style>
    <div id="parent"><div id="overlay"></div></div>
  "#;
  let pixmap = render(html_without_backdrop_root, 64, 64);
  assert_eq!(
    pixel(&pixmap, 15, 15),
    (0, 255, 255, 255),
    "sanity: without a backdrop-root boundary, the overlay should invert the body background"
  );

  let html_with_backdrop_root = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent {
        width: 20px;
        height: 20px;
        margin: 20px;
        /* Legacy alias for `mask-border` used by WebKit/Safari. Even if the URL cannot be resolved,
           the property presence still establishes a Backdrop Root boundary. */
        -webkit-mask-box-image: url(#missing) 30;
      }
      #overlay {
        width: 40px;
        height: 40px;
        position: relative;
        left: -10px;
        top: -10px;
        backdrop-filter: invert(1);
        box-sizing: border-box;
        border: 2px solid rgb(0 255 0);
      }
    </style>
    <div id="parent"><div id="overlay"></div></div>
  "#;

  let pixmap = render(html_with_backdrop_root, 64, 64);

  assert_eq!(pixel(&pixmap, 11, 25), (0, 255, 0, 255));
  assert_eq!(pixel(&pixmap, 15, 15), (255, 0, 0, 255));
}

#[test]
fn invalid_mask_border_declaration_does_not_clear_backdrop_root() {
  let html_without_backdrop_root = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent {
        width: 20px;
        height: 20px;
        margin: 20px;
      }
      #overlay {
        width: 40px;
        height: 40px;
        position: relative;
        left: -10px;
        top: -10px;
        backdrop-filter: invert(1);
        box-sizing: border-box;
        border: 2px solid rgb(0 255 0);
      }
    </style>
    <div id="parent"><div id="overlay"></div></div>
  "#;
  let pixmap = render(html_without_backdrop_root, 64, 64);
  assert_eq!(
    pixel(&pixmap, 15, 15),
    (0, 255, 255, 255),
    "sanity: without a backdrop-root boundary, the overlay should invert the body background"
  );

  let html_with_backdrop_root = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent {
        width: 20px;
        height: 20px;
        margin: 20px;
        mask-border: url(#missing) 30;
        /* Invalid declarations must be ignored per spec; they should not clear the previously-set
           mask-border value (and thus must not remove the Backdrop Root boundary). */
        mask-border: bogus;
      }
      #overlay {
        width: 40px;
        height: 40px;
        position: relative;
        left: -10px;
        top: -10px;
        backdrop-filter: invert(1);
        box-sizing: border-box;
        border: 2px solid rgb(0 255 0);
      }
    </style>
    <div id="parent"><div id="overlay"></div></div>
  "#;

  let pixmap = render(html_with_backdrop_root, 64, 64);
  assert_eq!(pixel(&pixmap, 11, 25), (0, 255, 0, 255));
  assert_eq!(pixel(&pixmap, 15, 15), (255, 0, 0, 255));
}

#[test]
fn mask_triggers_backdrop_root() {
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
        mask: linear-gradient(black, black);
        mask-mode: alpha;
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

  // `mask` establishes a Backdrop Root boundary; the mask itself is fully opaque, so the only
  // observable difference is whether `#overlay` can sample and invert `#bg`.
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
fn mix_blend_mode_triggers_backdrop_root_with_offset() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent {
        position: absolute;
        left: 10px;
        top: 10px;
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

  // Same as `mix_blend_mode_triggers_backdrop_root`, but exercises non-zero layer origins (bounded
  // group surfaces initialized from backdrop).
  assert_eq!(pixel(&pixmap, 20, 20), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 5, 5), (255, 0, 0, 255));
}

#[test]
fn mix_blend_mode_triggers_backdrop_root_with_offset_and_negative_overlay_origin() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent {
        position: absolute;
        left: 10px;
        top: 10px;
        width: 40px;
        height: 40px;
        mix-blend-mode: multiply;
      }
      #overlay {
        position: relative;
        left: -10px;
        top: -10px;
        width: 40px;
        height: 40px;
        backdrop-filter: invert(1);
      }
    </style>
    <div id="parent"><div id="overlay"></div></div>
  "#;

  let pixmap = render(html, 64, 64);

  // Exercises Backdrop Root sampling when `backdrop-filter` extends outside the Backdrop Root
  // element (negative sampling origin) while the Backdrop Root surface itself is offset.
  assert_eq!(pixel(&pixmap, 15, 15), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 5, 5), (255, 0, 0, 255));
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

#[test]
fn degenerate_clip_path_triggers_backdrop_root_even_when_resolved_none() {
  let html_without_backdrop_root = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent {
        width: 20px;
        height: 20px;
        margin: 20px;
      }
      #overlay {
        width: 40px;
        height: 40px;
        position: relative;
        left: -10px;
        top: -10px;
        backdrop-filter: invert(1);
        box-sizing: border-box;
        border: 2px solid rgb(0 255 0);
      }
    </style>
    <div id="parent"><div id="overlay"></div></div>
  "#;
  let pixmap = render(html_without_backdrop_root, 64, 64);
  assert_eq!(
    pixel(&pixmap, 15, 15),
    (0, 255, 255, 255),
    "sanity: without a backdrop-root boundary, the overlay should invert the body background"
  );

  let html_with_backdrop_root = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255 0 0); }
      #parent {
        width: 20px;
        height: 20px;
        margin: 20px;
        /* This clip-path value parses, but resolves to `None` at paint time because it is
           degenerate. Per filter-effects-2, the property presence must still establish a Backdrop
           Root boundary. */
        clip-path: polygon(0 0, 1px 0);
      }
      #overlay {
        width: 40px;
        height: 40px;
        position: relative;
        left: -10px;
        top: -10px;
        backdrop-filter: invert(1);
        box-sizing: border-box;
        border: 2px solid rgb(0 255 0);
      }
    </style>
    <div id="parent"><div id="overlay"></div></div>
  "#;

  let pixmap = render(html_with_backdrop_root, 64, 64);

  // The overlay extends outside `#parent`. If `clip-path` establishes a Backdrop Root on the
  // parent, then the overlay's backdrop-filter must not sample the body backdrop outside
  // `#parent`, so this pixel stays red.
  assert_eq!(pixel(&pixmap, 11, 25), (0, 255, 0, 255));
  assert_eq!(pixel(&pixmap, 15, 15), (255, 0, 0, 255));
}
