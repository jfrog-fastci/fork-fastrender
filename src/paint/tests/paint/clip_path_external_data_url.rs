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
fn clip_path_external_data_url_clips_triangle() {
  let svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><defs><clipPath id="tri"><polygon points="0 0, 100 0, 50 100"/></clipPath></defs></svg>"#;
  let encoded = STANDARD.encode(svg);
  let data_url = format!("data:image/svg+xml;base64,{encoded}#tri");

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
          background: rgb(255, 0, 0);
          clip-path: url("{data_url}");
        }}
      </style>
      <div id="target"></div>
    "#
  );

  let (dl, legacy) = render_both(&html, 120, 120);
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
