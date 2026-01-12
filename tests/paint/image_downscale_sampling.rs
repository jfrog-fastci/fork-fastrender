use fastrender::geometry::Rect;
use fastrender::paint::display_list::{
  DisplayItem, DisplayList, ImageData, ImageFilterQuality, ImageItem,
};
use fastrender::paint::display_list_renderer::DisplayListRenderer;
use fastrender::style::color::Rgba;
use fastrender::text::font_loader::FontContext;
use std::sync::Arc;

fn grayscale_gradient(width: u32) -> (Arc<ImageData>, Vec<u8>) {
  let denom = (width - 1).max(1);
  let mut values = Vec::with_capacity(width as usize);
  let mut pixels = Vec::with_capacity((width * 4) as usize);
  for i in 0..width {
    let v = ((i * 255) / denom) as u8;
    values.push(v);
    pixels.extend_from_slice(&[v, v, v, 255]);
  }
  (Arc::new(ImageData::new_pixels(width, 1, pixels)), values)
}

fn expected_linear_samples(values: &[u8], dest_x: f32, dest_w: f32) -> Vec<u8> {
  let src_w = values.len() as f32;
  let scale_x = src_w / dest_w;
  let max_x = src_w - 1.0;

  let x0 = (dest_x - 0.5).ceil();
  let x1 = (dest_x + dest_w - 0.5).ceil();
  let out_w = (x1 - x0).max(0.0) as usize;
  let phase_x = x0 - dest_x;

  let mut out = Vec::with_capacity(out_w);
  for x in 0..out_w {
    let mut sx = (x as f32 + 0.5 + phase_x) * scale_x - 0.5;
    sx = sx.clamp(0.0, max_x);
    let sx0 = sx.floor() as usize;
    let sx1 = (sx0 + 1).min(values.len() - 1);
    let t = sx - sx0 as f32;
    let v0 = values[sx0] as f32;
    let v1 = values[sx1] as f32;
    out.push((v0 + (v1 - v0) * t).floor().clamp(0.0, 255.0) as u8);
  }
  out
}

#[test]
fn image_linear_downscale_with_fractional_dest_size_matches_pixel_center_grid() {
  // Regression test for downscaled images whose destination rect width is fractional. The renderer
  // should sample onto the destination device-pixel grid using a pixel-center rule, rather than
  // baking a ceil-rounded intermediate pixmap and scaling it again at draw time.
  let (image, values) = grayscale_gradient(10);
  let dest_rect = Rect::from_xywh(0.0, 0.0, 7.5, 1.0);
  let expected = expected_linear_samples(&values, dest_rect.x(), dest_rect.width());
  assert_eq!(
    expected.len(),
    7,
    "expected pixel-center coverage to be 7px wide"
  );

  let mut list = DisplayList::new();
  list.push(DisplayItem::Image(ImageItem {
    dest_rect,
    image,
    filter_quality: ImageFilterQuality::Linear,
    src_rect: None,
  }));

  let pixmap = DisplayListRenderer::new(8, 1, Rgba::TRANSPARENT, FontContext::new())
    .unwrap()
    .render(&list)
    .unwrap();

  for (x, expected) in expected.iter().enumerate() {
    let px = pixmap.pixel(x as u32, 0).expect("pixel inside viewport");
    assert_eq!(
      (px.red(), px.green(), px.blue(), px.alpha()),
      (*expected, *expected, *expected, 255),
      "pixel x={x}"
    );
  }

  // Pixel x=7 (center at 7.5) lies exactly on the dest rect's max edge and should remain
  // untouched.
  let trailing = pixmap.pixel(7, 0).expect("trailing pixel inside viewport");
  assert_eq!(
    (
      trailing.red(),
      trailing.green(),
      trailing.blue(),
      trailing.alpha()
    ),
    (0, 0, 0, 0),
    "expected the pixel beyond the fractional edge to remain background"
  );
}

#[test]
fn image_linear_downscale_with_subpixel_translation_matches_pixel_center_grid() {
  // Regression test for downscaled images drawn at a fractional device-pixel offset: the subpixel
  // translation must affect the sampling grid.
  let (image, values) = grayscale_gradient(10);
  let dest_rect = Rect::from_xywh(0.3, 0.0, 7.0, 1.0);
  let expected = expected_linear_samples(&values, dest_rect.x(), dest_rect.width());
  assert_eq!(
    expected.len(),
    7,
    "expected pixel-center coverage to be 7px wide"
  );

  let mut list = DisplayList::new();
  list.push(DisplayItem::Image(ImageItem {
    dest_rect,
    image,
    filter_quality: ImageFilterQuality::Linear,
    src_rect: None,
  }));

  let pixmap = DisplayListRenderer::new(8, 1, Rgba::TRANSPARENT, FontContext::new())
    .unwrap()
    .render(&list)
    .unwrap();

  for (x, expected) in expected.iter().enumerate() {
    let px = pixmap.pixel(x as u32, 0).expect("pixel inside viewport");
    assert_eq!(
      (px.red(), px.green(), px.blue(), px.alpha()),
      (*expected, *expected, *expected, 255),
      "pixel x={x}"
    );
  }

  let trailing = pixmap.pixel(7, 0).expect("trailing pixel inside viewport");
  assert_eq!(
    (
      trailing.red(),
      trailing.green(),
      trailing.blue(),
      trailing.alpha()
    ),
    (0, 0, 0, 0),
    "expected the pixel beyond the destination rect to remain background"
  );
}
