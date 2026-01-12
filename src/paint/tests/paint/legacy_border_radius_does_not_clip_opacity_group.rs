use super::util::create_stacking_context_bounds_renderer_legacy;
use tiny_skia::Pixmap;

fn rgba_at(pixmap: &Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let pixel = pixmap.pixel(x, y).expect("pixel in bounds");
  (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha())
}

fn assert_red_tinted(rgba: (u8, u8, u8, u8), msg: &str) {
  let (r, g, b, a) = rgba;
  assert!(
    r > 200 && g < 200 && b < 200 && a > 200,
    "{msg}: expected red-tinted pixel, got rgba=({r},{g},{b},{a})"
  );
}

fn assert_red(rgba: (u8, u8, u8, u8), msg: &str) {
  let (r, g, b, a) = rgba;
  assert!(
    r > 200 && g < 50 && b < 50 && a > 200,
    "{msg}: expected red pixel, got rgba=({r},{g},{b},{a})"
  );
}

fn assert_green(rgba: (u8, u8, u8, u8), msg: &str) {
  let (r, g, b, a) = rgba;
  assert!(
    r < 50 && g > 200 && b < 50 && a > 200,
    "{msg}: expected green pixel, got rgba=({r},{g},{b},{a})"
  );
}

fn assert_magenta(rgba: (u8, u8, u8, u8), msg: &str) {
  let (r, g, b, a) = rgba;
  assert!(
    r > 200 && g < 50 && b > 200 && a > 200,
    "{msg}: expected magenta pixel, got rgba=({r},{g},{b},{a})"
  );
}

#[test]
fn legacy_border_radius_does_not_clip_opacity_group_overflow_visible_descendants() {
  let html = r#"
    <!doctype html>
    <style>
      html, body { margin: 0; background: white; }
      #target {
        position: absolute;
        left: 0;
        top: 0;
        width: 80px;
        height: 80px;
        opacity: 0.5;
        overflow: visible;
        border-radius: 30px;
      }
      #child {
        position: absolute;
        left: 0;
        top: 0;
        width: 20px;
        height: 20px;
        background: rgb(255, 0, 0);
      }
    </style>
    <div id="target">
      <div id="child"></div>
    </div>
  "#;

  let mut renderer = create_stacking_context_bounds_renderer_legacy();
  let pixmap = renderer.render_html(html, 100, 100).expect("render");

  assert_red_tinted(
    rgba_at(&pixmap, 5, 5),
    "overflow:visible child pixels should not be clipped by parent border-radius when compositing opacity group",
  );
}

#[test]
fn legacy_border_radius_does_not_clip_backdrop_filter_group_overflow_visible_descendants() {
  let html = r#"
    <!doctype html>
    <style>
      html, body { margin: 0; background: rgb(0, 255, 0); }
      #target {
        position: absolute;
        left: 0;
        top: 0;
        width: 80px;
        height: 80px;
        overflow: visible;
        border-radius: 30px;
        backdrop-filter: invert(1);
      }
      #child {
        position: absolute;
        left: 0;
        top: 0;
        width: 10px;
        height: 10px;
        background: rgb(255, 0, 0);
      }
    </style>
    <div id="target">
      <div id="child"></div>
    </div>
  "#;

  let mut renderer = create_stacking_context_bounds_renderer_legacy();
  let pixmap = renderer.render_html(html, 100, 100).expect("render");

  assert_red(
    rgba_at(&pixmap, 5, 5),
    "child should remain visible outside the parent's border-radius when overflow is visible",
  );
  assert_green(
    rgba_at(&pixmap, 0, 11),
    "pixels outside the border-radius and outside the child should show the unfiltered backdrop",
  );
  assert_magenta(
    rgba_at(&pixmap, 50, 50),
    "backdrop-filter should apply inside the border-radius",
  );
}
