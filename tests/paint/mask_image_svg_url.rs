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
fn mask_image_url_fragments_apply_at_high_device_pixel_ratio() {
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

        mask-image: url(#mask);
        mask-mode: alpha;
        mask-repeat: no-repeat;
        mask-size: 100% 100%;
        mask-position: 0 0;
      }
      #defs { position: absolute; width: 0; height: 0; }
    </style>
    <svg id="defs" width="0" height="0" xmlns="http://www.w3.org/2000/svg">
      <defs>
        <mask id="mask" maskUnits="userSpaceOnUse" maskContentUnits="userSpaceOnUse"
              x="0" y="0" width="100" height="100">
          <rect x="0" y="0" width="50" height="100" fill="white"></rect>
        </mask>
      </defs>
    </svg>
    <div id="target"></div>
  "#;

  let dpr = 2.0;
  let (dl, legacy) = render_both_with_dpr(html, 110, 110, dpr);
  for (backend, pixmap) in [("display_list", dl), ("legacy", legacy)] {
    assert_is_red(
      rgba_at(&pixmap, 20, 100),
      &format!("{backend}: expected left side to be visible at dpr={dpr}"),
    );
    assert_is_white(
      rgba_at(&pixmap, 180, 100),
      &format!("{backend}: expected right side to be masked out at dpr={dpr}"),
    );
  }
}

