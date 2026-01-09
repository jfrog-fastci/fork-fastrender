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
fn image_set_picks_candidate_by_device_pixel_ratio() {
  let red = solid_color_png(255, 0, 0, 255);
  let green = solid_color_png(0, 255, 0, 255);

  let html = format!(
    r#"
    <style>
      body {{ margin: 0; }}
      #box {{
        width: 10px;
        height: 10px;
        background-repeat: no-repeat;
        background-size: 10px 10px;
        background-image: image-set(
          url("{red}") 1x type("image/png"),
          url("{green}") 2x type("image/png")
        );
      }}
    </style>
    <div id="box"></div>
    "#
  );

  let mut renderer_1x = FastRender::builder()
    .device_pixel_ratio(1.0)
    .build()
    .expect("renderer 1x");
  let pixmap_1x = renderer_1x.render_html(&html, 10, 10).expect("render 1x");
  let px_1x = pixmap_1x.pixel(1, 1).expect("pixel 1x");
  assert_eq!(
    (px_1x.red(), px_1x.green(), px_1x.blue()),
    (255, 0, 0),
    "expected 1x image-set candidate"
  );

  let mut renderer_2x = FastRender::builder()
    .device_pixel_ratio(2.0)
    .build()
    .expect("renderer 2x");
  let pixmap_2x = renderer_2x.render_html(&html, 10, 10).expect("render 2x");
  let px_2x = pixmap_2x.pixel(5, 5).expect("pixel 2x");
  assert_eq!(
    (px_2x.red(), px_2x.green(), px_2x.blue()),
    (0, 255, 0),
    "expected 2x image-set candidate"
  );
}

#[test]
fn image_set_filters_candidates_by_type() {
  let red = solid_color_png(255, 0, 0, 255);
  let green = solid_color_png(0, 255, 0, 255);

  // The 2x candidate is the best density match, but has an unsupported MIME type and should be
  // ignored.
  let html = format!(
    r#"
    <style>
      body {{ margin: 0; }}
      #box {{
        width: 10px;
        height: 10px;
        background-repeat: no-repeat;
        background-size: 10px 10px;
        background-image: image-set(
          "{red}" 1x type("image/png"),
          url("{green}") 2x type("image/unsupported")
        );
      }}
    </style>
    <div id="box"></div>
    "#
  );

  let mut renderer = FastRender::builder()
    .device_pixel_ratio(2.0)
    .build()
    .expect("renderer");
  let pixmap = renderer.render_html(&html, 10, 10).expect("render html");
  let px = pixmap.pixel(5, 5).expect("pixel");
  assert_eq!(
    (px.red(), px.green(), px.blue()),
    (255, 0, 0),
    "expected unsupported type() candidate to be ignored"
  );
}

