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

#[test]
fn image_src_rect_is_clipped_to_dest_rect_with_offset_crop() {
  // Cover a non-zero src origin (cropping on both axes) and an offset destination rect.
  let mut pixels = Vec::with_capacity(4 * 4 * 4);
  for _ in 0..(4 * 4) {
    pixels.extend_from_slice(&[255, 0, 0, 255]);
  }
  let image = Arc::new(ImageData::new_pixels(4, 4, pixels));

  let dest_rect = Rect::from_xywh(2.0, 2.0, 4.0, 4.0);
  let src_rect = Rect::from_xywh(1.0, 1.0, 2.0, 2.0);

  let mut list = DisplayList::new();
  list.push(DisplayItem::Image(ImageItem {
    dest_rect,
    image,
    filter_quality: ImageFilterQuality::Nearest,
    src_rect: Some(src_rect),
  }));

  let pixmap = DisplayListRenderer::new(8, 8, Rgba::TRANSPARENT, FontContext::new())
    .unwrap()
    .render(&list)
    .unwrap();

  for y in 0..8u32 {
    for x in 0..8u32 {
      let px = pixmap.pixel(x, y).expect("pixel inside viewport");
      let inside = (2..6).contains(&x) && (2..6).contains(&y);
      if inside {
        assert_eq!(
          (px.red(), px.green(), px.blue(), px.alpha()),
          (255, 0, 0, 255),
          "pixel ({x}, {y}) should be filled"
        );
      } else {
        assert_eq!(
          (px.red(), px.green(), px.blue(), px.alpha()),
          (0, 0, 0, 0),
          "pixel ({x}, {y}) should remain transparent outside dest_rect"
        );
      }
    }
  }
}

