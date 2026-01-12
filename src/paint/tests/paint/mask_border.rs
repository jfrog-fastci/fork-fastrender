use super::util::{
  create_stacking_context_bounds_renderer, create_stacking_context_bounds_renderer_legacy,
};
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
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
fn mask_border_transparent_source_masks_border_area() {
  // A fully transparent 3×3 image. With `mask-border-slice: 1` and `mask-border-width: 20px`,
  // the border regions of the element are masked out, while the interior remains visible (the
  // default for `mask-border-slice` without the `fill` keyword).
  let svg =
    r#"<svg xmlns="http://www.w3.org/2000/svg" width="3" height="3" viewBox="0 0 3 3"></svg>"#;
  let encoded = STANDARD.encode(svg);
  let data_url = format!("data:image/svg+xml;base64,{encoded}");

  let html = format!(
    r#"
      <style>
        body {{ margin: 0; background: white; }}
        #target {{
          position: absolute;
          left: 0;
          top: 0;
          width: 100px;
          height: 100px;
          box-sizing: border-box;
          border: 20px solid transparent;
          background: rgb(255, 0, 0);
          mask-border: url("{data_url}") 1 / 20px;
        }}
      </style>
      <div id="target"></div>
    "#
  );

  let (dl, legacy) = render_both(&html, 120, 120);
  for (backend, pixmap) in [("display_list", dl), ("legacy", legacy)] {
    assert_is_red(
      rgba_at(&pixmap, 50, 50),
      &format!("{backend}: expected center to remain visible"),
    );
    assert_is_white(
      rgba_at(&pixmap, 10, 50),
      &format!("{backend}: expected border area to be masked out"),
    );
  }
}
