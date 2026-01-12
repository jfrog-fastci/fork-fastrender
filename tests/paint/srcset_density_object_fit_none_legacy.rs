use base64::prelude::BASE64_STANDARD;
use base64::Engine as _;
use fastrender::debug::runtime::RuntimeToggles;
use fastrender::{FastRender, FastRenderConfig};
use image::codecs::png::PngEncoder;
use image::ExtendedColorType;
use image::ImageEncoder;
use std::collections::HashMap;

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
fn img_srcset_density_affects_object_fit_none_legacy() {
  let green = solid_color_png(20, 10, 0, 255, 0, 255);
  let html = format!(
    r#"
    <style>
      body {{ margin: 0; background: rgb(255 0 0); }}
      img {{
        display: block;
        width: 20px;
        height: 10px;
        background: rgb(255 0 0);
        object-fit: none;
        object-position: left top;
      }}
    </style>
    <img srcset="{green} 2x">
    "#
  );

  let config = FastRenderConfig::default().with_runtime_toggles(RuntimeToggles::from_map(
    HashMap::from([(
      "FASTR_PAINT_BACKEND".to_string(),
      "legacy".to_string(),
    )]),
  ));
  let mut renderer = FastRender::with_config(config).expect("renderer");
  let pixmap = renderer.render_html(&html, 20, 10).expect("render html");

  let px = pixmap.pixel(15, 5).expect("pixel within viewport");
  assert_eq!(
    (px.red(), px.green(), px.blue()),
    (255, 0, 0),
    "selected srcset density should affect object-fit sizing"
  );
}

