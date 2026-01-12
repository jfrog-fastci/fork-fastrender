use base64::prelude::BASE64_STANDARD;
use base64::Engine as _;
use fastrender::api::FastRender;
use image::codecs::png::PngEncoder;
use image::ExtendedColorType;
use image::ImageEncoder;

fn solid_color_png(r: u8, g: u8, b: u8, a: u8) -> String {
  let mut buf = Vec::new();
  PngEncoder::new(&mut buf)
    .write_image(&[r, g, b, a], 1, 1, ExtendedColorType::Rgba8)
    .expect("encode png");
  format!("data:image/png;base64,{}", BASE64_STANDARD.encode(&buf))
}

#[test]
fn img_srcset_tie_prefers_first_candidate() {
  let green = solid_color_png(0, 255, 0, 255);
  let red = solid_color_png(255, 0, 0, 255);
  let blue = solid_color_png(0, 0, 255, 255);

  let html = format!(
    r#"
    <style>
      body {{ margin: 0; }}
      img {{ display: block; width: 10px; height: 10px; }}
    </style>
    <img src="{green}" srcset="{red} 1x, {blue} 1x">
    "#
  );

  let mut renderer = FastRender::new().expect("renderer");
  let pixmap = renderer.render_html(&html, 10, 10).expect("render html");

  let px = pixmap.pixel(5, 5).expect("center pixel");
  assert_eq!((px.red(), px.green(), px.blue()), (255, 0, 0));
}

#[test]
fn picture_source_srcset_tie_prefers_first_candidate_when_all_below_target() {
  // Chrome keeps the first candidate when all entries resolve to the same density, even when the
  // DPR is higher than any candidate.
  let green = solid_color_png(0, 255, 0, 255);
  let red = solid_color_png(255, 0, 0, 255);
  let blue = solid_color_png(0, 0, 255, 255);

  let html = format!(
    r#"
    <style>
      body {{ margin: 0; }}
      picture, img {{ display: block; width: 10px; height: 10px; }}
    </style>
    <picture>
      <source srcset="{red} 1x, {blue} 1x" type="image/png">
      <img src="{green}">
    </picture>
    "#
  );

  let mut renderer = FastRender::builder()
    .device_pixel_ratio(2.0)
    .build()
    .expect("renderer");
  let pixmap = renderer.render_html(&html, 10, 10).expect("render picture");

  let px = pixmap
    .pixel(pixmap.width() / 2, pixmap.height() / 2)
    .expect("center pixel");
  assert_eq!((px.red(), px.green(), px.blue()), (255, 0, 0));
}
