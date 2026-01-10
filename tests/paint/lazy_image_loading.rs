use base64::prelude::BASE64_STANDARD;
use base64::Engine as _;
use fastrender::api::FastRender;
use image::codecs::png::PngEncoder;
use image::ExtendedColorType;
use image::ImageEncoder;

fn solid_color_png(width: u32, height: u32, r: u8, g: u8, b: u8, a: u8) -> String {
  let mut buf = Vec::new();
  let pixels: Vec<u8> = std::iter::repeat([r, g, b, a])
    .take((width * height) as usize)
    .flatten()
    .collect();

  PngEncoder::new(&mut buf)
    .write_image(&pixels, width, height, ExtendedColorType::Rgba8)
    .expect("encode png");

  format!("data:image/png;base64,{}", BASE64_STANDARD.encode(&buf))
}

#[test]
fn lazy_image_in_viewport_is_painted() {
  let red = solid_color_png(4, 4, 255, 0, 0, 255);
  let html = format!(
    r#"
    <style>
      html, body {{ margin: 0; background: rgb(255 255 255); }}
      img {{ display: block; }}
    </style>
    <img loading="lazy" src="{red}">
    "#
  );

  let mut renderer = FastRender::new().expect("renderer");
  let pixmap = renderer.render_html(&html, 10, 10).expect("render html");
  let px = pixmap.pixel(1, 1).expect("pixel inside image");
  assert_eq!((px.red(), px.green(), px.blue()), (255, 0, 0));
}

#[test]
fn lazy_image_outside_viewport_is_not_painted() {
  let red = solid_color_png(4, 4, 255, 0, 0, 255);
  let html = format!(
    r#"
    <style>
      html, body {{ margin: 0; background: rgb(255 255 255); }}
      img {{ display: block; }}
    </style>
    <div style="height: 2000px"></div>
    <img loading="lazy" src="{red}">
    "#
  );

  let mut renderer = FastRender::new().expect("renderer");
  let pixmap = renderer.render_html(&html, 10, 10).expect("render html");
  let px = pixmap.pixel(1, 1).expect("pixel in viewport");
  assert_eq!((px.red(), px.green(), px.blue()), (255, 255, 255));
}

