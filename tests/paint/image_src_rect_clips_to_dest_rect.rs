use fastrender::geometry::Rect;
use fastrender::paint::display_list::{DisplayItem, DisplayList, ImageData, ImageFilterQuality, ImageItem};
use fastrender::paint::display_list_renderer::DisplayListRenderer;
use fastrender::style::color::Rgba;
use fastrender::text::font_loader::FontContext;
use std::sync::Arc;

#[test]
fn image_src_rect_is_clipped_to_dest_rect() {
  let mut pixels = Vec::with_capacity(10 * 10 * 4);
  for _ in 0..(10 * 10) {
    pixels.extend_from_slice(&[255, 0, 0, 255]);
  }
  let image = Arc::new(ImageData::new_pixels(10, 10, pixels));

  let mut list = DisplayList::new();
  list.push(DisplayItem::Image(ImageItem {
    dest_rect: Rect::from_xywh(0.0, 5.0, 10.0, 5.0),
    image,
    filter_quality: ImageFilterQuality::Nearest,
    src_rect: Some(Rect::from_xywh(0.0, 2.5, 10.0, 5.0)),
  }));

  let pixmap = DisplayListRenderer::new(16, 16, Rgba::TRANSPARENT, FontContext::new())
    .unwrap()
    .render(&list)
    .unwrap();

  let inside = pixmap.pixel(0, 5).expect("pixel inside viewport");
  assert_eq!(
    (inside.red(), inside.green(), inside.blue(), inside.alpha()),
    (255, 0, 0, 255)
  );

  for (x, y) in [(0, 4), (0, 11)] {
    let px = pixmap.pixel(x, y).expect("pixel inside viewport");
    assert_eq!(
      (px.red(), px.green(), px.blue(), px.alpha()),
      (0, 0, 0, 0),
      "pixel outside dest_rect should remain unmodified at ({x},{y})"
    );
  }
}

#[test]
fn image_src_rect_is_clipped_to_dest_rect_linear() {
  let mut pixels = Vec::with_capacity(10 * 10 * 4);
  for _ in 0..(10 * 10) {
    pixels.extend_from_slice(&[255, 0, 0, 255]);
  }
  let image = Arc::new(ImageData::new_pixels(10, 10, pixels));

  let mut list = DisplayList::new();
  list.push(DisplayItem::Image(ImageItem {
    dest_rect: Rect::from_xywh(0.0, 5.0, 10.0, 5.0),
    image,
    filter_quality: ImageFilterQuality::Linear,
    src_rect: Some(Rect::from_xywh(0.0, 2.5, 10.0, 5.0)),
  }));

  let pixmap = DisplayListRenderer::new(16, 16, Rgba::TRANSPARENT, FontContext::new())
    .unwrap()
    .render(&list)
    .unwrap();

  let inside = pixmap.pixel(0, 5).expect("pixel inside viewport");
  assert_eq!(
    (inside.red(), inside.green(), inside.blue(), inside.alpha()),
    (255, 0, 0, 255)
  );

  for (x, y) in [(0, 4), (0, 11)] {
    let px = pixmap.pixel(x, y).expect("pixel inside viewport");
    assert_eq!(
      (px.red(), px.green(), px.blue(), px.alpha()),
      (0, 0, 0, 0),
      "pixel outside dest_rect should remain unmodified at ({x},{y})"
    );
  }
}
