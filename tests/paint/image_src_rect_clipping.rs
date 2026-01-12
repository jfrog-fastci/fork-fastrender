use fastrender::geometry::Rect;
use fastrender::paint::display_list::{
  DisplayItem, DisplayList, FillRectItem, ImageData, ImageFilterQuality, ImageItem,
};
use fastrender::paint::display_list_renderer::DisplayListRenderer;
use fastrender::text::font_loader::FontContext;
use fastrender::Rgba;
use std::sync::Arc;

#[test]
fn image_src_rect_is_clipped_to_dest_rect() {
  // Construct an image with a solid red top half and a solid green bottom half.
  // We'll draw only a cropped source rect within the green region, but map it to a destination
  // rect that starts lower on the canvas. Without dest-rect clipping, the "uncropped" red region
  // would be transformed into y < dest_rect.y and bleed over the background.
  let mut pixels = Vec::with_capacity(10 * 10 * 4);
  for y in 0..10u32 {
    for _x in 0..10u32 {
      if y < 5 {
        pixels.extend_from_slice(&[255, 0, 0, 255]);
      } else {
        pixels.extend_from_slice(&[0, 255, 0, 255]);
      }
    }
  }
  let image = Arc::new(ImageData::new_pixels(10, 10, pixels));

  let mut list = DisplayList::new();
  list.push(DisplayItem::FillRect(FillRectItem {
    rect: Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
    color: Rgba::WHITE,
  }));
  list.push(DisplayItem::Image(ImageItem {
    dest_rect: Rect::from_xywh(0.0, 6.0, 10.0, 4.0),
    image,
    filter_quality: ImageFilterQuality::Linear,
    // Use a fractional `src_rect` offset so the renderer keeps the full pixmap for sampling
    // fidelity and relies on the `src_rect` transform + dest-rect clipping.
    src_rect: Some(Rect::from_xywh(0.0, 5.5, 10.0, 4.0)),
  }));

  let renderer = DisplayListRenderer::new(10, 10, Rgba::WHITE, FontContext::new()).unwrap();
  let pixmap = renderer.render(&list).expect("render");

  let bleed_px = pixmap.pixel(5, 2).unwrap();
  assert_eq!(
    (
      bleed_px.red(),
      bleed_px.green(),
      bleed_px.blue(),
      bleed_px.alpha()
    ),
    (255, 255, 255, 255),
    "image draw should not bleed above dest_rect"
  );

  let visible_px = pixmap.pixel(5, 7).unwrap();
  assert_eq!(
    (
      visible_px.red(),
      visible_px.green(),
      visible_px.blue(),
      visible_px.alpha()
    ),
    (0, 255, 0, 255),
    "image draw should render the cropped source region within dest_rect"
  );
}
