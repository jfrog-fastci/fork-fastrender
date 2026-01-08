use super::util::{
  create_stacking_context_bounds_renderer, create_stacking_context_bounds_renderer_legacy,
};
use std::fs;
use tiny_skia::Pixmap;

const FIXTURE_PATH: &str = "tests/fixtures/html/mask_multiple_layers_composite.html";

fn rgba_at(pixmap: &Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let pixel = pixmap.pixel(x, y).expect("pixel in bounds");
  (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha())
}

fn assert_is_white(rgba: (u8, u8, u8, u8), msg: &str) {
  let (r, g, b, a) = rgba;
  assert!(
    r > 240 && g > 240 && b > 240 && a > 240,
    "{msg}: expected white background, got rgba=({r},{g},{b},{a})"
  );
}

fn assert_is_red(rgba: (u8, u8, u8, u8), msg: &str) {
  let (r, g, b, a) = rgba;
  assert!(
    r > 200 && g < 50 && b < 50 && a > 200,
    "{msg}: expected red foreground, got rgba=({r},{g},{b},{a})"
  );
}

fn assert_is_blue(rgba: (u8, u8, u8, u8), msg: &str) {
  let (r, g, b, a) = rgba;
  assert!(
    b > 200 && r < 50 && a > 200,
    "{msg}: expected blue foreground, got rgba=({r},{g},{b},{a})"
  );
}

fn render_both(html: &str, width: u32, height: u32) -> (Pixmap, Pixmap) {
  let mut dl = create_stacking_context_bounds_renderer();
  let dl_pixmap = dl.render_html(html, width, height).expect("render display_list");

  let mut legacy = create_stacking_context_bounds_renderer_legacy();
  let legacy_pixmap = legacy
    .render_html(html, width, height)
    .expect("render legacy");

  (dl_pixmap, legacy_pixmap)
}

fn assert_composite_fixture(pixmap: &Pixmap) {
  // All boxes are 60x60; sample points are chosen well within each quadrant.
  let sample = |x, y| rgba_at(pixmap, x, y);

  // #add at (10,10)
  assert_is_white(sample(25, 25), "#add top-left");
  assert_is_blue(sample(55, 25), "#add top-right");
  assert_is_blue(sample(25, 55), "#add bottom-left");
  assert_is_blue(sample(55, 55), "#add bottom-right");

  // #intersect at (80,10)
  assert_is_white(sample(95, 25), "#intersect top-left");
  assert_is_white(sample(125, 25), "#intersect top-right");
  assert_is_white(sample(95, 55), "#intersect bottom-left");
  assert_is_blue(sample(125, 55), "#intersect bottom-right");

  // #subtract at (10,80)
  assert_is_white(sample(25, 95), "#subtract top-left");
  assert_is_blue(sample(55, 95), "#subtract top-right");
  assert_is_white(sample(25, 125), "#subtract bottom-left");
  assert_is_white(sample(55, 125), "#subtract bottom-right");

  // #exclude at (80,80)
  assert_is_white(sample(95, 95), "#exclude top-left");
  assert_is_blue(sample(125, 95), "#exclude top-right");
  assert_is_blue(sample(95, 125), "#exclude bottom-left");
  assert_is_white(sample(125, 125), "#exclude bottom-right");
}

#[test]
fn mask_multiple_layers_composite_fixture_renders_expected() {
  let html = fs::read_to_string(FIXTURE_PATH).expect("read fixture");
  let (dl, legacy) = render_both(&html, 160, 160);
  assert_composite_fixture(&dl);
  assert_composite_fixture(&legacy);
}

#[test]
fn mask_multiple_layers_respects_mask_clip_and_subtract() {
  let html = r#"
    <style>
      body { margin: 0; background: white; }
      #target {
        position: absolute;
        left: 10px;
        top: 10px;
        width: 100px;
        height: 100px;
        background: rgb(255, 0, 0);
        border: 10px solid transparent;
        padding: 10px;

        mask-image: linear-gradient(black, black), linear-gradient(black, black);
        mask-size: 100% 100%;
        mask-repeat: no-repeat;
        mask-clip: border-box, content-box;
        mask-composite: subtract;
      }
    </style>
    <div id="target"></div>
  "#;

  let (dl, legacy) = render_both(html, 140, 140);
  for (backend, pixmap) in [("display_list", dl), ("legacy", legacy)] {
    assert_is_red(
      rgba_at(&pixmap, 15, 60),
      &format!("{backend}: expected ring to keep edge pixels"),
    );
    assert_is_white(
      rgba_at(&pixmap, 60, 60),
      &format!("{backend}: expected content box to be subtracted (transparent)"),
    );
  }
}

#[test]
fn mask_multiple_layers_supports_per_layer_mask_mode() {
  let html = r#"
    <style>
      body { margin: 0; background: white; }
      #target {
        position: absolute;
        left: 10px;
        top: 10px;
        width: 100px;
        height: 100px;
        background: rgb(0, 128, 255);

        mask-image: linear-gradient(90deg, transparent 0 50%, black 50% 100%),
          linear-gradient(180deg, white 0 50%, black 50% 100%);
        mask-mode: alpha, luminance;
        mask-size: 100% 100%;
        mask-repeat: no-repeat;
        mask-composite: intersect;
      }
    </style>
    <div id="target"></div>
  "#;

  let (dl, legacy) = render_both(html, 140, 140);
  for (backend, pixmap) in [("display_list", dl), ("legacy", legacy)] {
    assert_is_blue(
      rgba_at(&pixmap, 85, 35),
      &format!("{backend}: expected top-right quadrant to be visible"),
    );
    assert_is_white(
      rgba_at(&pixmap, 85, 85),
      &format!("{backend}: expected bottom-right quadrant to be masked out"),
    );
    assert_is_white(
      rgba_at(&pixmap, 35, 35),
      &format!("{backend}: expected left half to be masked out"),
    );
  }
}
