use base64::prelude::BASE64_STANDARD;
use base64::Engine as _;
use fastrender::debug::runtime::RuntimeToggles;
use fastrender::{FastRender, FastRenderConfig};
use image::codecs::png::PngEncoder;
use image::ExtendedColorType;
use image::ImageEncoder;
use std::collections::HashMap;

fn solid_color_png_data_url(width: u32, height: u32, r: u8, g: u8, b: u8, a: u8) -> String {
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
fn display_list_srcset_width_descriptors_use_content_box_slot_width() {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "display_list".to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);

  // When the slot width is the 20px content box, a 20w candidate matches 1x and should be picked.
  // If the border box is incorrectly used as the slot width (20 + 2*10 padding + 2*10 border = 60px),
  // the 20w candidate becomes <1x and we'd incorrectly select the 80w candidate instead.
  let red = solid_color_png_data_url(20, 20, 255, 0, 0, 255);
  let blue = solid_color_png_data_url(80, 80, 0, 0, 255, 255);
  let html = format!(
    "<!doctype html>\
     <style>html,body{{margin:0;background:rgb(0,0,0);}}</style>\
     <img srcset=\"{red} 20w, {blue} 80w\" \
          style=\"display:block;margin:0;width:20px;height:20px;box-sizing:content-box;border:10px solid rgb(0,0,0);padding:10px;overflow:clip;\">",
    red = red,
    blue = blue
  );

  let mut renderer = FastRender::with_config(config).expect("create renderer");
  let pixmap = renderer.render_html(&html, 100, 100).expect("render img");

  // Content box starts at (border+padding) = (20,20).
  let inside = pixmap.pixel(25, 25).expect("inside pixel");
  assert!(
    inside.red() > 200 && inside.green() < 80 && inside.blue() < 80,
    "expected the 20w red candidate to be selected (got rgba=({}, {}, {}, {}))",
    inside.red(),
    inside.green(),
    inside.blue(),
    inside.alpha()
  );
}

