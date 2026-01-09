use super::util::{
  create_stacking_context_bounds_renderer, create_stacking_context_bounds_renderer_legacy,
};
use fastrender::api::RenderOptions;
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

fn render_both(html: &str, width: u32, height: u32) -> (Pixmap, Pixmap) {
  let mut dl = create_stacking_context_bounds_renderer();
  let dl_pixmap = dl.render_html(html, width, height).expect("render display_list");

  let mut legacy = create_stacking_context_bounds_renderer_legacy();
  let legacy_pixmap = legacy
    .render_html(html, width, height)
    .expect("render legacy");

  (dl_pixmap, legacy_pixmap)
}

fn render_both_with_dpr(html: &str, width: u32, height: u32, dpr: f32) -> (Pixmap, Pixmap) {
  let options = RenderOptions::new()
    .with_viewport(width, height)
    .with_device_pixel_ratio(dpr);

  let mut dl = create_stacking_context_bounds_renderer();
  let dl_pixmap = dl
    .render_html_with_options(html, options.clone())
    .expect("render display_list");

  let mut legacy = create_stacking_context_bounds_renderer_legacy();
  let legacy_pixmap = legacy
    .render_html_with_options(html, options)
    .expect("render legacy");

  (dl_pixmap, legacy_pixmap)
}

#[test]
fn clip_path_url_clips_with_in_document_clip_path_defs() {
  let html = r#"
    <style>
      body { margin: 0; background: white; }
      #target {
        position: absolute;
        left: 0;
        top: 0;
        width: 100px;
        height: 100px;
        background: rgb(255, 0, 0);
        clip-path: url(#clip);
      }
      #defs { position: absolute; width: 0; height: 0; }
    </style>
    <svg id="defs" width="0" height="0">
      <defs>
        <clipPath id="clip">
          <circle cx="50" cy="50" r="50"></circle>
        </clipPath>
      </defs>
    </svg>
    <div id="target"></div>
  "#;

  let (dl, legacy) = render_both(html, 110, 110);
  for (backend, pixmap) in [("display_list", dl), ("legacy", legacy)] {
    assert_is_red(
      rgba_at(&pixmap, 50, 50),
      &format!("{backend}: expected circle center to remain visible"),
    );
    assert_is_white(
      rgba_at(&pixmap, 5, 5),
      &format!("{backend}: expected corner outside circle to be clipped"),
    );
  }
}

#[test]
fn clip_path_url_accepts_reference_box_and_respects_content_box() {
  let html = r#"
    <style>
      body { margin: 0; background: white; }
      #target {
        position: absolute;
        left: 0;
        top: 0;
        width: 100px;
        height: 100px;
        padding: 20px;
        background: rgb(255, 0, 0);
        /* Non-standard ordering used by some content. */
        clip-path: content-box url(#clip);
      }
      #defs { position: absolute; width: 0; height: 0; }
    </style>
    <svg id="defs" width="0" height="0">
      <defs>
        <clipPath id="clip" clipPathUnits="objectBoundingBox">
          <rect x="0" y="0" width="1" height="1"></rect>
        </clipPath>
      </defs>
    </svg>
    <div id="target"></div>
  "#;

  let (dl, legacy) = render_both(html, 110, 110);
  for (backend, pixmap) in [("display_list", dl), ("legacy", legacy)] {
    // In the padding area (outside content box) – should be clipped away.
    assert_is_white(
      rgba_at(&pixmap, 10, 10),
      &format!("{backend}: expected padding to be clipped by content-box reference"),
    );
    // In the content box.
    assert_is_red(
      rgba_at(&pixmap, 50, 50),
      &format!("{backend}: expected content to remain visible"),
    );
  }
}

#[test]
fn clip_path_url_clips_at_high_device_pixel_ratio() {
  let html = r#"
    <style>
      body { margin: 0; background: white; }
      #target {
        position: absolute;
        left: 0;
        top: 0;
        width: 100px;
        height: 100px;
        background: rgb(255, 0, 0);
        clip-path: url(#clip);
      }
      #defs { position: absolute; width: 0; height: 0; }
    </style>
    <svg id="defs" width="0" height="0">
      <defs>
        <clipPath id="clip">
          <circle cx="50" cy="50" r="50"></circle>
        </clipPath>
      </defs>
    </svg>
    <div id="target"></div>
  "#;

  let dpr = 2.0;
  let (dl, legacy) = render_both_with_dpr(html, 110, 110, dpr);
  for (backend, pixmap) in [("display_list", dl), ("legacy", legacy)] {
    assert_is_red(
      rgba_at(&pixmap, 100, 100),
      &format!("{backend}: expected circle center to remain visible at dpr={dpr}"),
    );
    assert_is_white(
      rgba_at(&pixmap, 10, 10),
      &format!("{backend}: expected corner outside circle to be clipped at dpr={dpr}"),
    );
  }
}
