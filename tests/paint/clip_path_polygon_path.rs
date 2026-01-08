use super::util::{
  create_stacking_context_bounds_renderer, create_stacking_context_bounds_renderer_legacy,
};
use tiny_skia::Pixmap;

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

fn assert_is_green(rgba: (u8, u8, u8, u8), msg: &str) {
  let (r, g, b, a) = rgba;
  assert!(
    g > 200 && r < 50 && b < 50 && a > 200,
    "{msg}: expected green foreground, got rgba=({r},{g},{b},{a})"
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

#[test]
fn clip_path_polygon_clips_triangle() {
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
        clip-path: polygon(0 0, 100% 0, 50% 100%);
      }
    </style>
    <div id="target"></div>
  "#;

  let (dl, legacy) = render_both(html, 140, 140);
  for (backend, pixmap) in [("display_list", dl), ("legacy", legacy)] {
    assert_is_red(
      rgba_at(&pixmap, 60, 35),
      &format!("{backend}: expected point inside triangle to be red"),
    );
    assert_is_white(
      rgba_at(&pixmap, 35, 85),
      &format!("{backend}: expected point outside triangle to be clipped"),
    );
  }
}

#[test]
fn clip_path_path_evenodd_punches_hole() {
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
        clip-path: path(evenodd, "M0 0 L100 0 L100 100 L0 100 Z M25 25 L75 25 L75 75 L25 75 Z");
      }
    </style>
    <div id="target"></div>
  "#;

  let (dl, legacy) = render_both(html, 140, 140);
  for (backend, pixmap) in [("display_list", dl), ("legacy", legacy)] {
    assert_is_blue(
      rgba_at(&pixmap, 30, 30),
      &format!("{backend}: expected area outside hole to remain visible"),
    );
    assert_is_white(
      rgba_at(&pixmap, 60, 60),
      &format!("{backend}: expected evenodd hole to be clipped out"),
    );
  }
}

#[test]
fn clip_path_inset_round_clips_corners() {
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
        clip-path: inset(0 round 20px);
      }
    </style>
    <div id="target"></div>
  "#;

  let (dl, legacy) = render_both(html, 140, 140);
  for (backend, pixmap) in [("display_list", dl), ("legacy", legacy)] {
    assert_is_white(
      rgba_at(&pixmap, 12, 12),
      &format!("{backend}: expected rounded corner to be clipped"),
    );
    assert_is_blue(
      rgba_at(&pixmap, 35, 35),
      &format!("{backend}: expected center to remain visible"),
    );
  }
}

#[test]
fn clip_path_box_reference_respects_content_box() {
  let html = r#"
    <style>
      body { margin: 0; background: white; }
      #target {
        position: absolute;
        left: 10px;
        top: 10px;
        width: 100px;
        height: 100px;
        background: rgb(0, 255, 0);
        border: 10px solid rgb(0, 0, 0);
        padding: 10px;
        clip-path: content-box;
      }
    </style>
    <div id="target"></div>
  "#;

  let (dl, legacy) = render_both(html, 140, 140);
  for (backend, pixmap) in [("display_list", dl), ("legacy", legacy)] {
    // Sample within the border area but outside the content box; should be clipped away.
    assert_is_white(
      rgba_at(&pixmap, 15, 15),
      &format!("{backend}: expected border area to be clipped by content-box"),
    );
    // Sample within the content box; should remain visible.
    assert_is_green(
      rgba_at(&pixmap, 60, 60),
      &format!("{backend}: expected content area to remain visible"),
    );
  }
}

