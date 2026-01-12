use base64::prelude::BASE64_STANDARD;
use base64::Engine as _;
use fastrender::api::FastRender;
use image::codecs::png::PngEncoder;
use image::ExtendedColorType;
use image::ImageEncoder;

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
fn image_set_selected_density_affects_intrinsic_background_size() {
  let green_10x10 = solid_color_png(10, 10, [0, 255, 0, 255]);
  let green_20x20 = solid_color_png(20, 20, [0, 255, 0, 255]);

  let html = format!(
    r#"
    <style>
      body {{ margin:0; }}
      #box {{
        width: 20px;
        height: 20px;
        background-color: rgb(255 0 0);
        background-repeat: no-repeat;
        background-image: image-set(
          url("{green_10x10}") 1x,
          url("{green_20x20}") 2x
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

  let pixmap = renderer.render_html(&html, 20, 20).expect("render html");

  let inside = pixmap
    .pixel(pixmap.width() / 4, pixmap.height() / 4)
    .expect("inside pixel");
  assert_eq!(
    (inside.red(), inside.green(), inside.blue()),
    (0, 255, 0),
    "expected 2× image-set candidate to be painted at its intrinsic CSS size"
  );

  let outside = pixmap
    .pixel(pixmap.width() * 3 / 4, pixmap.height() * 3 / 4)
    .expect("outside pixel");
  assert_eq!(
    (outside.red(), outside.green(), outside.blue()),
    (255, 0, 0),
    "expected background color to show outside intrinsic image area when background-size is auto"
  );
}

