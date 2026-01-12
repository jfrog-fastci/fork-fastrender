use fastrender::geometry::Rect;
use fastrender::paint::display_list::{
  DisplayItem, DisplayList, ImageData, ImageFilterQuality, ImageItem,
};
use fastrender::paint::display_list_renderer::DisplayListRenderer;
use fastrender::style::color::Rgba;
use fastrender::text::font_loader::FontContext;
use std::sync::Arc;

#[test]
fn image_fractional_src_rect_downscale_bakes_sampling_phase() {
  // 3x1 image (black/red/black). Draw a fractional src rect that spans 1.5px so the renderer needs
  // to preserve sampling phase while downscaling to a single device pixel.
  let pixels = vec![
    0, 0, 0, 255,   // black
    255, 0, 0, 255, // red
    0, 0, 0, 255,   // black
  ];
  let image = Arc::new(ImageData::new_pixels(3, 1, pixels));

  let mut list = DisplayList::new();
  list.push(DisplayItem::Image(ImageItem {
    dest_rect: Rect::from_xywh(0.0, 0.0, 1.0, 1.0),
    image,
    filter_quality: ImageFilterQuality::Linear,
    src_rect: Some(Rect::from_xywh(0.25, 0.0, 1.5, 1.0)),
  }));

  let pixmap = DisplayListRenderer::new(1, 1, Rgba::WHITE, FontContext::new())
    .unwrap()
    .render(&list)
    .unwrap();
  let px = pixmap.pixel(0, 0).expect("pixel inside viewport");
  assert_eq!(
    (px.red(), px.green(), px.blue(), px.alpha()),
    (127, 0, 0, 255),
    "expected Skia-aligned bilinear sampling for fractional src rect downscale"
  );
}

