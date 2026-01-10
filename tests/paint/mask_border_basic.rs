use super::util::{
  create_stacking_context_bounds_renderer, create_stacking_context_bounds_renderer_legacy,
};
use base64::Engine;
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

fn svg_data_url(svg: &str) -> String {
  let encoded = base64::engine::general_purpose::STANDARD.encode(svg.as_bytes());
  format!("data:image/svg+xml;base64,{encoded}")
}

fn html_for_mask_border(source_url: &str, mode: &str) -> String {
  format!(
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

          mask-border-source: url("{source_url}");
          mask-border-slice: 2 fill;
          mask-border-width: 20px;
          mask-border-repeat: stretch;
          mask-border-mode: {mode};
        }}
      </style>
      <div id="target"></div>
    "#
  )
}

fn html_for_webkit_mask_box_image(source_url: &str, mode: &str) -> String {
  // WebKit/Safari expose `mask-border` as `-webkit-mask-box-image`. Real-world sites (notably
  // WebKit-targeted stylesheets) often only ship the prefixed property, so ensure our alias mapping
  // and shorthand parsing apply end-to-end, including painting.
  format!(
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

          -webkit-mask-box-image: url("{source_url}") 2 fill / 20px stretch {mode};
        }}
      </style>
      <div id="target"></div>
    "#
  )
}

#[test]
fn mask_border_alpha_masks_center_from_svg_alpha() {
  // Transparent SVG background with an opaque 2px border ring; in `alpha` mode the transparent
  // center should mask out the element's interior.
  let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10">
    <rect x="0" y="0" width="10" height="2" fill="white"/>
    <rect x="0" y="8" width="10" height="2" fill="white"/>
    <rect x="0" y="2" width="2" height="6" fill="white"/>
    <rect x="8" y="2" width="2" height="6" fill="white"/>
  </svg>"#;
  let url = svg_data_url(svg);
  let html = html_for_mask_border(&url, "alpha");

  let dpr = 2.0;
  let (dl, legacy) = render_both_with_dpr(&html, 110, 110, dpr);
  for (backend, pixmap) in [("display_list", dl), ("legacy", legacy)] {
    assert_is_red(
      rgba_at(&pixmap, 20, 100),
      &format!("{backend}: expected masked border ring to remain visible"),
    );
    assert_is_white(
      rgba_at(&pixmap, 100, 100),
      &format!("{backend}: expected center to be masked out"),
    );
  }
}

#[test]
fn mask_border_luminance_masks_center_from_rgb_luminance() {
  // Fully opaque SVG (alpha=1 everywhere) with a black center; `mask-border-mode: luminance`
  // should derive mask values from the pixel luminance rather than alpha.
  let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10">
    <rect x="0" y="0" width="10" height="10" fill="white"/>
    <rect x="2" y="2" width="6" height="6" fill="black"/>
  </svg>"#;
  let url = svg_data_url(svg);
  let html = html_for_mask_border(&url, "luminance");

  let dpr = 2.0;
  let (dl, legacy) = render_both_with_dpr(&html, 110, 110, dpr);
  for (backend, pixmap) in [("display_list", dl), ("legacy", legacy)] {
    assert_is_red(
      rgba_at(&pixmap, 20, 100),
      &format!("{backend}: expected masked border ring to remain visible"),
    );
    assert_is_white(
      rgba_at(&pixmap, 100, 100),
      &format!("{backend}: expected center to be masked out via luminance mode"),
    );
  }
}

#[test]
fn webkit_mask_box_image_alpha_shorthand_masks_center_from_svg_alpha() {
  // Same as `mask_border_alpha_masks_center_from_svg_alpha`, but exercised via the
  // `-webkit-mask-box-image` shorthand alias.
  let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10">
    <rect x="0" y="0" width="10" height="2" fill="white"/>
    <rect x="0" y="8" width="10" height="2" fill="white"/>
    <rect x="0" y="2" width="2" height="6" fill="white"/>
    <rect x="8" y="2" width="2" height="6" fill="white"/>
  </svg>"#;
  let url = svg_data_url(svg);
  let html = html_for_webkit_mask_box_image(&url, "alpha");

  let dpr = 2.0;
  let (dl, legacy) = render_both_with_dpr(&html, 110, 110, dpr);
  for (backend, pixmap) in [("display_list", dl), ("legacy", legacy)] {
    assert_is_red(
      rgba_at(&pixmap, 20, 100),
      &format!("{backend}: expected masked border ring to remain visible"),
    );
    assert_is_white(
      rgba_at(&pixmap, 100, 100),
      &format!("{backend}: expected center to be masked out"),
    );
  }
}

#[test]
fn webkit_mask_box_image_luminance_shorthand_masks_center_from_rgb_luminance() {
  // Same as `mask_border_luminance_masks_center_from_rgb_luminance`, but exercised via the
  // `-webkit-mask-box-image` shorthand alias.
  let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10">
    <rect x="0" y="0" width="10" height="10" fill="white"/>
    <rect x="2" y="2" width="6" height="6" fill="black"/>
  </svg>"#;
  let url = svg_data_url(svg);
  let html = html_for_webkit_mask_box_image(&url, "luminance");

  let dpr = 2.0;
  let (dl, legacy) = render_both_with_dpr(&html, 110, 110, dpr);
  for (backend, pixmap) in [("display_list", dl), ("legacy", legacy)] {
    assert_is_red(
      rgba_at(&pixmap, 20, 100),
      &format!("{backend}: expected masked border ring to remain visible"),
    );
    assert_is_white(
      rgba_at(&pixmap, 100, 100),
      &format!("{backend}: expected center to be masked out via luminance mode"),
    );
  }
}
