use base64::engine::general_purpose;
use base64::Engine as _;
use fastrender::css::types::Declaration;
use fastrender::css::types::PropertyValue;
use fastrender::geometry::Rect;
use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::style::properties::apply_declaration;
use fastrender::style::properties::with_image_set_dpr;
use fastrender::tree::fragment_tree::FragmentNode;
use fastrender::{ComputedStyle, DisplayItem};
use image::codecs::png::PngEncoder;
use image::{ColorType, ImageEncoder};
use std::sync::Arc;

fn solid_png_data_url(width: u32, height: u32, rgba: [u8; 4]) -> String {
  let pixels: Vec<u8> = std::iter::repeat(rgba)
    .take((width * height) as usize)
    .flatten()
    .collect();
  let mut buf = Vec::new();
  PngEncoder::new(&mut buf)
    .write_image(&pixels, width, height, ColorType::Rgba8.into())
    .expect("encode png");
  format!("data:image/png;base64,{}", general_purpose::STANDARD.encode(buf))
}

fn assert_background_image_set_tile_size(css_function: &str) {
  // Two raster candidates with the *same intended CSS size* but different pixel sizes.
  //
  // With DPR=2, the 2× candidate should be selected, but the CSS intrinsic size should remain
  // `pixel_size / density` (so 20×20@2x → 10×10 CSS px).
  let low = solid_png_data_url(10, 10, [255, 0, 0, 255]);
  let high = solid_png_data_url(20, 20, [0, 255, 0, 255]);

  let mut style = ComputedStyle::default();
  with_image_set_dpr(2.0, || {
    apply_declaration(
      &mut style,
      &Declaration {
        property: "background-image".into(),
        value: PropertyValue::Keyword(format!(
          "{css_function}(url(\"{}\") 1x, url(\"{}\") 2x)",
          low, high
        )),
        contains_var: false,
        raw_value: String::new(),
        important: false,
      },
      &ComputedStyle::default(),
      16.0,
      16.0,
    );
  });

  let fragment = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 64.0, 64.0),
    Vec::new(),
    Arc::new(style),
  );

  let list = DisplayListBuilder::new()
    .with_device_pixel_ratio(2.0)
    .build(&fragment);

  let pattern = list
    .items()
    .iter()
    .find_map(|item| match item {
      DisplayItem::ImagePattern(p) => Some(p),
      _ => None,
    })
    .unwrap_or_else(|| {
      panic!(
        "expected a repeated background to emit an ImagePattern item, got items: {:?}",
        list.items()
      )
    });

  assert_eq!(pattern.image.width, 20);
  assert_eq!(pattern.image.height, 20);
  assert!((pattern.image.css_width - 10.0).abs() < 1e-6);
  assert!((pattern.image.css_height - 10.0).abs() < 1e-6);
  assert!((pattern.tile_size.width - 10.0).abs() < 1e-6);
  assert!((pattern.tile_size.height - 10.0).abs() < 1e-6);
}

#[test]
fn background_image_set_preserves_density_for_intrinsic_sizing() {
  assert_background_image_set_tile_size("image-set");
}

#[test]
fn background_webkit_image_set_preserves_density_for_intrinsic_sizing() {
  assert_background_image_set_tile_size("-webkit-image-set");
}

