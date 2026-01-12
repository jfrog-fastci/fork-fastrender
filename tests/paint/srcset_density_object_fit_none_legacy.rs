use base64::prelude::BASE64_STANDARD;
use base64::Engine as _;
use image::codecs::png::PngEncoder;
use image::ExtendedColorType;
use image::ImageEncoder;

use super::util::create_stacking_context_bounds_renderer_legacy;

fn solid_color_png(width: u32, height: u32, rgba: [u8; 4]) -> String {
  let mut buf = Vec::new();
  let pixels: Vec<u8> = std::iter::repeat(rgba)
    .take((width * height) as usize)
    .flatten()
    .collect();

  PngEncoder::new(&mut buf)
    .write_image(&pixels, width, height, ExtendedColorType::Rgba8)
    .expect("encode png");

  format!("data:image/png;base64,{}", BASE64_STANDARD.encode(&buf))
}

#[test]
fn srcset_density_affects_object_fit_none() {
  let green_4x4 = solid_color_png(4, 4, [0, 255, 0, 255]);
  let html = format!(
    r#"
    <style>
      body {{ margin:0; background: rgb(255 0 0); }}
      img {{
        display:block;
        width:4px;
        height:4px;
        object-fit:none;
        object-position:left top;
      }}
    </style>
    <img srcset="{green_4x4} 2x">
    "#
  );

  let mut renderer = create_stacking_context_bounds_renderer_legacy();
  let pixmap = renderer.render_html(&html, 4, 4).expect("render html");

  // With `srcset` density=2, the chosen 4×4 image has a natural size of 2×2 CSS px, so
  // `object-fit:none` paints only the top-left 2×2 of the 4×4 replaced box and leaves the
  // bottom-right showing the red page background.
  let px = pixmap.pixel(3, 3).expect("pixel (3,3)");
  assert_eq!(
    (px.red(), px.green(), px.blue()),
    (255, 0, 0),
    "expected bottom-right pixel to show red background"
  );
}
