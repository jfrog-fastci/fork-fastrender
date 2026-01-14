use crate::geometry::Rect;
use crate::paint::display_list::{
  DisplayItem, DisplayList, ImageData, ImageFilterQuality, ImageItem,
};
use crate::paint::display_list_renderer::DisplayListRenderer;
use crate::style::color::Rgba;
use crate::text::font_loader::FontContext;
use std::sync::Arc;

#[test]
fn image_large_fractional_src_rect_avoids_tinyskia_pattern_sampling_bug() {
  // Regression for the `discord.com` pageset fixture: a large `background-size: cover` draw can
  // produce a fractional `src_rect` that is upscaled to the viewport size (~1.3M pixels).
  //
  // When the renderer falls back to tiny-skia's pattern shader sampling for these large draws, it
  // can clamp to the wrong edge and paint near-black instead of the expected image content.
  //
  // This test ensures we take the phase-aware pre-rasterization path (which matches Skia/Chrome)
  // for a moderately-large viewport-sized draw.
  let src_w = 131u32;
  let src_h = 768u32;

  // Construct a mostly-blue image with a black bottom row (matching the "clamp to last row"
  // signature observed in the fixture).
  let mut pixels = Vec::with_capacity((src_w * src_h * 4) as usize);
  for y in 0..src_h {
    for _ in 0..src_w {
      if y == src_h - 1 {
        pixels.extend_from_slice(&[0, 0, 0, 255]);
      } else {
        pixels.extend_from_slice(&[0, 0, 255, 255]);
      }
    }
  }
  let image = Arc::new(ImageData::new_pixels(src_w, src_h, pixels));

  let dest_w = 1040u32;
  let dest_h = 1240u32;
  let mut list = DisplayList::new();
  list.push(DisplayItem::Image(ImageItem {
    dest_rect: Rect::from_xywh(0.0, 0.0, dest_w as f32, dest_h as f32),
    image,
    filter_quality: ImageFilterQuality::Linear,
    // Fractional crop produced by `background-size: cover` + `background-position: 50% 0`.
    src_rect: Some(Rect::from_xywh(0.0, 0.0, src_w as f32, 156.2)),
  }));

  let pixmap = DisplayListRenderer::new(dest_w, dest_h, Rgba::TRANSPARENT, FontContext::new())
    .unwrap()
    .render(&list)
    .unwrap();

  // Sample a pixel well within the destination rect; it should come from the blue region, not the
  // black bottom edge.
  let px = pixmap.pixel(0, dest_h - 2).expect("pixel inside viewport");
  assert_eq!(
    (px.red(), px.green(), px.blue(), px.alpha()),
    (0, 0, 255, 255),
    "expected large fractional-src-rect draw to preserve image content"
  );
}
