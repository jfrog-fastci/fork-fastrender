use fastrender::image_loader::ImageCache;
use fastrender::paint::display_list_renderer::PaintParallelism;
use fastrender::paint::painter::paint_tree_with_resources_scaled_offset;
use fastrender::scroll::ScrollState;
use fastrender::{FastRender, Point, Rgba};

fn render(html: &str, width: u32, height: u32) -> tiny_skia::Pixmap {
  crate::rayon_test_util::init_rayon_for_tests(1);

  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parsed");
  let fragment_tree = renderer
    .layout_document(&dom, width, height)
    .expect("laid out");

  let font_ctx = renderer.font_context().clone();
  let image_cache = ImageCache::new();

  paint_tree_with_resources_scaled_offset(
    &fragment_tree,
    width,
    height,
    Rgba::WHITE,
    font_ctx,
    image_cache,
    1.0,
    Point::ZERO,
    // Keep painting deterministic; this test focuses on backdrop sampling through isolation layers.
    PaintParallelism::disabled(),
    &ScrollState::default(),
  )
  .expect("painted")
}

fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let p = pixmap.pixel(x, y).unwrap();
  (p.red(), p.green(), p.blue(), p.alpha())
}

fn assert_greenish(label: &str, (r, g, b, a): (u8, u8, u8, u8)) {
  assert!(
    r <= 15 && g >= 240 && b <= 15 && a >= 250,
    "{label}: expected green-ish rgba, got ({r},{g},{b},{a})"
  );
}

fn assert_redish(label: &str, (r, g, b, a): (u8, u8, u8, u8)) {
  assert!(
    r >= 240 && g <= 15 && b <= 15 && a >= 250,
    "{label}: expected red-ish rgba, got ({r},{g},{b},{a})"
  );
}

fn assert_magentaish(label: &str, (r, g, b, a): (u8, u8, u8, u8)) {
  assert!(
    r >= 240 && g <= 15 && b >= 240 && a >= 250,
    "{label}: expected magenta-ish rgba, got ({r},{g},{b},{a})"
  );
}

fn assert_cyanish(label: &str, (r, g, b, a): (u8, u8, u8, u8)) {
  assert!(
    r <= 15 && g >= 240 && b >= 240 && a >= 250,
    "{label}: expected cyan-ish rgba, got ({r},{g},{b},{a})"
  );
}

#[test]
fn backdrop_filter_samples_through_z_index_blend_isolation_layer() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(0 255 0); }
      #wrapper { position: relative; z-index: 0; width: 64px; height: 64px; }
      #blend {
        position: absolute;
        left: 0;
        top: 0;
        width: 20px;
        height: 20px;
        background: rgb(255 0 0);
        mix-blend-mode: difference;
      }
      #glass {
        position: absolute;
        left: 32px;
        top: 32px;
        width: 24px;
        height: 24px;
        backdrop-filter: invert(1);
      }
    </style>
    <div id="wrapper">
      <div id="blend"></div>
      <div id="glass"></div>
    </div>
  "#;

  let pixmap = render(html, 64, 64);

  // The mix-blend-mode descendant should isolate the wrapper: red over green should not blend to
  // yellow inside the isolated stacking context.
  assert_redish("blend isolation pixel", pixel(&pixmap, 10, 10));
  // Backdrop-filter sampling must see through the blend isolation layer to the page background.
  assert_magentaish("backdrop-filter pixel", pixel(&pixmap, 44, 44));
  // Control pixel outside the filtered region stays green.
  assert_greenish("control pixel", pixel(&pixmap, 60, 10));
}

#[test]
fn backdrop_filter_samples_through_transform_blend_isolation_layer() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(0 255 0); }
      #wrapper { position: relative; transform: translateX(0px); width: 64px; height: 64px; }
      #blend {
        position: absolute;
        left: 0;
        top: 0;
        width: 20px;
        height: 20px;
        background: rgb(255 0 0);
        mix-blend-mode: difference;
      }
      #glass {
        position: absolute;
        left: 32px;
        top: 32px;
        width: 24px;
        height: 24px;
        backdrop-filter: invert(1);
      }
    </style>
    <div id="wrapper">
      <div id="blend"></div>
      <div id="glass"></div>
    </div>
  "#;

  let pixmap = render(html, 64, 64);

  assert_redish("blend isolation pixel", pixel(&pixmap, 10, 10));
  assert_magentaish("backdrop-filter pixel", pixel(&pixmap, 44, 44));
  assert_greenish("control pixel", pixel(&pixmap, 60, 10));
}

#[test]
fn backdrop_filter_samples_through_fixed_blend_isolation_layer() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(0 255 0); }
      #wrapper { position: fixed; left: 0; top: 0; width: 64px; height: 64px; }
      #blend {
        position: absolute;
        left: 0;
        top: 0;
        width: 20px;
        height: 20px;
        background: rgb(255 0 0);
        mix-blend-mode: difference;
      }
      #glass {
        position: absolute;
        left: 32px;
        top: 32px;
        width: 24px;
        height: 24px;
        backdrop-filter: invert(1);
      }
    </style>
    <div id="wrapper">
      <div id="blend"></div>
      <div id="glass"></div>
    </div>
  "#;

  let pixmap = render(html, 64, 64);

  assert_redish("blend isolation pixel", pixel(&pixmap, 10, 10));
  assert_magentaish("backdrop-filter pixel", pixel(&pixmap, 44, 44));
  assert_greenish("control pixel", pixel(&pixmap, 60, 10));
}

#[test]
fn backdrop_filter_samples_through_sticky_blend_isolation_layer() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(0 255 0); }
      #wrapper { position: sticky; top: 0; left: 0; width: 64px; height: 64px; }
      #blend {
        position: absolute;
        left: 0;
        top: 0;
        width: 20px;
        height: 20px;
        background: rgb(255 0 0);
        mix-blend-mode: difference;
      }
      #glass {
        position: absolute;
        left: 32px;
        top: 32px;
        width: 24px;
        height: 24px;
        backdrop-filter: invert(1);
      }
    </style>
    <div id="wrapper">
      <div id="blend"></div>
      <div id="glass"></div>
    </div>
  "#;

  let pixmap = render(html, 64, 64);

  assert_redish("blend isolation pixel", pixel(&pixmap, 10, 10));
  assert_magentaish("backdrop-filter pixel", pixel(&pixmap, 44, 44));
  assert_greenish("control pixel", pixel(&pixmap, 60, 10));
}

#[test]
fn backdrop_filter_samples_through_offset_blend_isolation_layer() {
  // Cover the case where the isolated stacking-context layer is bounded and offset from the page
  // origin. The backdrop-filter sampling must still be in page coordinates (not the isolation
  // layer's local 0,0 space).
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(0 255 0); }
      #behind {
        position: absolute;
        left: 40px;
        top: 40px;
        width: 20px;
        height: 20px;
        background: rgb(255 0 0);
      }
      #wrapper {
        position: absolute;
        left: 10px;
        top: 10px;
        width: 64px;
        height: 64px;
        z-index: 0;
      }
      #blend {
        position: absolute;
        left: 0;
        top: 0;
        width: 20px;
        height: 20px;
        background: rgb(255 0 0);
        mix-blend-mode: difference;
      }
      #glass {
        position: absolute;
        left: 30px;
        top: 30px;
        width: 24px;
        height: 24px;
        backdrop-filter: invert(1);
      }
    </style>
    <div id="behind"></div>
    <div id="wrapper">
      <div id="blend"></div>
      <div id="glass"></div>
    </div>
  "#;

  let pixmap = render(html, 80, 80);

  // Prove the wrapper is isolated for mix-blend-mode: red over green remains red-ish.
  assert_redish("blend isolation pixel", pixel(&pixmap, 20, 20));
  // Sample point inside the red square behind the backdrop-filter area: red inverted to cyan.
  assert_cyanish("backdrop-filter over red pixel", pixel(&pixmap, 45, 45));
  // Sample point inside the backdrop-filter area but outside the red square: green inverted to magenta.
  assert_magentaish("backdrop-filter over green pixel", pixel(&pixmap, 62, 62));
  // Control pixel outside the filtered region stays green.
  assert_greenish("control pixel", pixel(&pixmap, 75, 5));
}
