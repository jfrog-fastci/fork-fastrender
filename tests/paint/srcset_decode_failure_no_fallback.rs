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
fn img_srcset_decode_failure_does_not_fallback_to_src() {
  let src = solid_color_png(0, 255, 0, 255);
  let broken = "data:text/plain,not-an-image";
  let html = format!(
    r#"
    <style>
      body {{ margin: 0; background: rgb(255 0 0); }}
      img {{ display: block; width: 20px; height: 20px; }}
    </style>
    <img src="{src}" srcset="{broken} 1x">
    "#
  );

  let mut renderer = FastRender::new().expect("renderer");
  let pixmap = renderer.render_html(&html, 30, 30).expect("render html");

  let px = pixmap.pixel(10, 10).expect("inside pixel");
  assert_eq!(
    (px.red(), px.green(), px.blue()),
    (255, 0, 0),
    "should not fall back to the `src` image when the selected `srcset` candidate fails to decode"
  );
}
