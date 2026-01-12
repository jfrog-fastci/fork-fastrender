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

fn assert_is_green(rgba: (u8, u8, u8, u8), msg: &str) {
  let (r, g, b, a) = rgba;
  assert!(
    g > 200 && r < 50 && b < 50 && a > 200,
    "{msg}: expected green foreground, got rgba=({r},{g},{b},{a})"
  );
}

fn assert_is_red(rgba: (u8, u8, u8, u8), msg: &str) {
  let (r, g, b, a) = rgba;
  assert!(
    r > 200 && g < 50 && b < 50 && a > 200,
    "{msg}: expected red foreground, got rgba=({r},{g},{b},{a})"
  );
}

fn render_both(html: &str, width: u32, height: u32) -> (Pixmap, Pixmap) {
  let mut dl = create_stacking_context_bounds_renderer();
  let dl_pixmap = dl
    .render_html(html, width, height)
    .expect("render display_list");

  let mut legacy = create_stacking_context_bounds_renderer_legacy();
  let legacy_pixmap = legacy
    .render_html(html, width, height)
    .expect("render legacy");

  (dl_pixmap, legacy_pixmap)
}

#[test]
fn clip_path_url_svg_clips_triangle() {
  let html = r#"
    <style>
      body { margin: 0; background: white; }
      svg { position: absolute; width: 0; height: 0; }
      #target {
        position: absolute;
        left: 0;
        top: 0;
        width: 100px;
        height: 100px;
        background: rgb(255, 0, 0);
        clip-path: url(#tri);
      }
    </style>
    <svg xmlns="http://www.w3.org/2000/svg">
      <defs>
        <clipPath id="tri">
          <polygon points="0 0, 100 0, 50 100"></polygon>
        </clipPath>
      </defs>
    </svg>
    <div id="target"></div>
  "#;

  let (dl, legacy) = render_both(html, 120, 120);
  for (backend, pixmap) in [("display_list", dl), ("legacy", legacy)] {
    assert_is_red(
      rgba_at(&pixmap, 50, 20),
      &format!("{backend}: expected point inside triangle to be red"),
    );
    assert_is_white(
      rgba_at(&pixmap, 20, 80),
      &format!("{backend}: expected point outside triangle to be clipped"),
    );
  }
}

#[test]
fn clip_path_url_svg_allows_clip_region_outside_border_box() {
  let html = r#"
    <style>
      body { margin: 0; background: white; }
      #target {
        position: absolute;
        left: 20px;
        top: 20px;
        width: 100px;
        height: 100px;
        background: rgb(255, 0, 0);
        clip-path: url(#big);
      }
      #child {
        position: absolute;
        left: -20px;
        top: 0;
        width: 20px;
        height: 20px;
        background: rgb(0, 255, 0);
      }
    </style>
    <svg width="0" height="0" style="position:absolute">
      <defs>
        <clipPath id="big" clipPathUnits="userSpaceOnUse">
          <rect x="-20" y="0" width="60" height="100" />
        </clipPath>
      </defs>
    </svg>
    <div id="target"><div id="child"></div></div>
  "#;

  let (dl, legacy) = render_both(html, 140, 140);
  for (backend, pixmap) in [("display_list", dl), ("legacy", legacy)] {
    // Sample a point in the overflow-visible child area (left of the border box). The SVG
    // clipPath extends to x=-20, so this area should remain visible (green).
    assert_is_green(
      rgba_at(&pixmap, 5, 30),
      &format!("{backend}: expected overflow-visible child to remain visible"),
    );

    // Sample a point inside the border box but outside the clipPath rect; should be clipped away.
    assert_is_white(
      rgba_at(&pixmap, 110, 30),
      &format!("{backend}: expected border-box area outside clipPath to be clipped"),
    );
  }
}

#[test]
fn clip_path_url_svg_allows_object_bounding_box_clip_region_outside_border_box() {
  let html = r#"
    <style>
      body { margin: 0; background: white; }
      #target {
        position: absolute;
        left: 20px;
        top: 20px;
        width: 100px;
        height: 100px;
        background: rgb(255, 0, 0);
        clip-path: url(#big);
      }
      #child {
        position: absolute;
        left: -20px;
        top: 0;
        width: 20px;
        height: 20px;
        background: rgb(0, 255, 0);
      }
    </style>
    <svg width="0" height="0" style="position:absolute">
      <defs>
        <clipPath id="big" clipPathUnits="objectBoundingBox">
          <rect x="-0.2" y="0" width="0.6" height="1" />
        </clipPath>
      </defs>
    </svg>
    <div id="target"><div id="child"></div></div>
  "#;

  let (dl, legacy) = render_both(html, 140, 140);
  for (backend, pixmap) in [("display_list", dl), ("legacy", legacy)] {
    // Sample a point in the overflow-visible child area (left of the border box). The clipPath
    // extends to x=-0.2 of the reference box, so this area should remain visible (green).
    assert_is_green(
      rgba_at(&pixmap, 5, 30),
      &format!("{backend}: expected overflow-visible child to remain visible"),
    );

    // Sample a point inside the border box but outside the clipPath rect; should be clipped away.
    assert_is_white(
      rgba_at(&pixmap, 110, 30),
      &format!("{backend}: expected border-box area outside clipPath to be clipped"),
    );
  }
}

#[test]
fn clip_path_url_svg_allows_object_bounding_box_clip_region_outside_border_box_with_transform() {
  let html = r#"
    <style>
      body { margin: 0; background: white; }
      #target {
        position: absolute;
        left: 20px;
        top: 20px;
        width: 100px;
        height: 100px;
        background: rgb(255, 0, 0);
        clip-path: url(#big);
      }
      #child {
        position: absolute;
        left: -20px;
        top: 0;
        width: 20px;
        height: 20px;
        background: rgb(0, 255, 0);
      }
    </style>
    <svg width="0" height="0" style="position:absolute">
      <defs>
        <clipPath id="big" clipPathUnits="objectBoundingBox" transform="translate(-0.2 0)">
          <rect x="0" y="0" width="0.6" height="1" />
        </clipPath>
      </defs>
    </svg>
    <div id="target"><div id="child"></div></div>
  "#;

  let (dl, legacy) = render_both(html, 140, 140);
  for (backend, pixmap) in [("display_list", dl), ("legacy", legacy)] {
    // Sample a point in the overflow-visible child area (left of the border box). The clipPath is
    // shifted left by 0.2 of the reference box via `transform`, so this area should remain visible.
    assert_is_green(
      rgba_at(&pixmap, 5, 30),
      &format!("{backend}: expected overflow-visible child to remain visible"),
    );

    // Sample a point inside the border box but outside the clipPath rect; should be clipped away.
    assert_is_white(
      rgba_at(&pixmap, 110, 30),
      &format!("{backend}: expected border-box area outside clipPath to be clipped"),
    );
  }
}
