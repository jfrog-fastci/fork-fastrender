use base64::prelude::BASE64_STANDARD;
use base64::Engine as _;
use image::codecs::png::PngEncoder;
use image::ExtendedColorType;
use image::ImageEncoder;

use super::util::create_stacking_context_bounds_renderer;

fn solid_color_png(width: u32, height: u32, r: u8, g: u8, b: u8, a: u8) -> String {
  let mut buf = Vec::new();
  let mut pixels = Vec::with_capacity((width * height * 4) as usize);
  for _ in 0..(width * height) {
    pixels.extend_from_slice(&[r, g, b, a]);
  }
  PngEncoder::new(&mut buf)
    .write_image(&pixels, width, height, ExtendedColorType::Rgba8)
    .expect("encode png");
  format!("data:image/png;base64,{}", BASE64_STANDARD.encode(&buf))
}

#[test]
fn unscaled_image_snapping_does_not_overpaint_border() {
  // Regression test for pixel-snapped 1:1 images: the display-list renderer snaps unscaled images
  // and axis-aligned integer-sized clip rects to device pixels. If that snapping floors the
  // origin, an image that is supposed to start just inside a 1px border can shift left and paint
  // over the border pixel (observed on `news.ycombinator.com`'s logo).
  let red = solid_color_png(18, 18, 255, 0, 0, 255);
  let html = format!(
    r#"<!doctype html>
      <style>
        body {{ margin: 0; background: black; }}
        img {{
          display: block;
          width: 18px;
          height: 18px;
          margin-left: 0.8px; /* forces a fractional inner edge at 1.8px once border is applied */
          border: 1px solid white;
        }}
      </style>
      <img src="{red}" width="18" height="18" alt="">
    "#
  );

  let mut renderer = create_stacking_context_bounds_renderer();
  let pixmap = renderer.render_html(&html, 30, 30).expect("render");

  let border_px = pixmap.pixel(1, 10).expect("border pixel");
  assert_eq!(
    (
      border_px.red(),
      border_px.green(),
      border_px.blue(),
      border_px.alpha()
    ),
    (255, 255, 255, 255),
    "expected the left border pixel to remain visible"
  );

  let image_px = pixmap.pixel(2, 10).expect("image pixel");
  assert_eq!(
    (
      image_px.red(),
      image_px.green(),
      image_px.blue(),
      image_px.alpha()
    ),
    (255, 0, 0, 255),
    "expected the image content to begin just to the right of the border"
  );
}
