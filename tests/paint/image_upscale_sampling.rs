use fastrender::geometry::Rect;
use fastrender::paint::display_list::{DisplayItem, DisplayList, ImageData, ImageFilterQuality, ImageItem};
use fastrender::paint::display_list_renderer::DisplayListRenderer;
use fastrender::style::color::Rgba;
use fastrender::text::font_loader::FontContext;
use std::sync::Arc;

#[test]
fn image_linear_upscale_matches_chrome_sampling_grid() {
  // Chrome/Skia samples scaled images on a pixel-center aligned grid and rounds channel values
  // down when converting back to 8-bit. This regression ensures we take the same path for large
  // upscales (where tiny-skia's built-in sampling can drift noticeably).
  //
  // 2px wide gradient -> 9px wide output should yield the following (floored) samples:
  // [0, 0, 14, 70, 127, 184, 240, 255, 255]
  let pixels = vec![
    // left pixel: black
    0, 0, 0, 255, //
    // right pixel: white
    255, 255, 255, 255, //
  ];
  let image = Arc::new(ImageData::new_pixels(2, 1, pixels));

  let mut list = DisplayList::new();
  list.push(DisplayItem::Image(ImageItem {
    dest_rect: Rect::from_xywh(0.0, 0.0, 9.0, 1.0),
    image,
    filter_quality: ImageFilterQuality::Linear,
    src_rect: None,
  }));

  let pixmap = DisplayListRenderer::new(9, 1, Rgba::TRANSPARENT, FontContext::new())
    .unwrap()
    .render(&list)
    .unwrap();

  let expected = [0u8, 0, 14, 70, 127, 184, 240, 255, 255];
  for (x, expected) in expected.iter().enumerate() {
    let px = pixmap.pixel(x as u32, 0).expect("pixel inside viewport");
    assert_eq!(
      (px.red(), px.green(), px.blue(), px.alpha()),
      (*expected, *expected, *expected, 255),
      "pixel x={x}",
    );
  }
}

#[test]
fn image_linear_upscale_with_fractional_src_rect_preserves_subpixel_offset() {
  // `ImageItem::src_rect` supports fractional coordinates (e.g. from
  // `background-size: cover`), and bilinear filtering must take that subpixel offset into account.
  //
  // Here we upscale the fractional center region of a 2px black->white gradient. If the source
  // rect were snapped to integer pixels, the result would become solid black; instead we should
  // see a smooth gradient with neither the 0 nor 255 endpoints.
  let pixels = vec![
    0, 0, 0, 255, //
    255, 255, 255, 255, //
  ];
  let image = Arc::new(ImageData::new_pixels(2, 1, pixels));

  let mut list = DisplayList::new();
  list.push(DisplayItem::Image(ImageItem {
    dest_rect: Rect::from_xywh(0.0, 0.0, 9.0, 1.0),
    image,
    filter_quality: ImageFilterQuality::Linear,
    src_rect: Some(Rect::from_xywh(0.5, 0.0, 1.0, 1.0)),
  }));

  let pixmap = DisplayListRenderer::new(9, 1, Rgba::TRANSPARENT, FontContext::new())
    .unwrap()
    .render(&list)
    .unwrap();

  let mut values = Vec::with_capacity(9);
  for x in 0..9 {
    let px = pixmap.pixel(x as u32, 0).expect("pixel inside viewport");
    assert_eq!(px.alpha(), 255, "pixel x={x} alpha should remain opaque");
    assert_eq!(
      (px.red(), px.green(), px.blue()),
      (px.red(), px.red(), px.red()),
      "pixel x={x} should remain grayscale"
    );
    values.push(px.red());
  }

  // Expect a gradient (monotonic) but without fully black/white endpoints.
  let min = *values.iter().min().unwrap();
  let max = *values.iter().max().unwrap();
  assert!(
    min > 0 && max < 255,
    "expected cropped src_rect to exclude endpoints; got {values:?}"
  );
  for pair in values.windows(2) {
    assert!(pair[0] <= pair[1], "expected monotonic gradient; got {values:?}");
  }
  assert!(
    (values[4] as i32 - 127).abs() <= 4,
    "expected midpoint to stay near 50% gray; got {values:?}"
  );
}

#[test]
fn image_src_rect_does_not_paint_outside_dest_rect() {
  // When `ImageItem::src_rect` is specified, the renderer maps the image via a scale+translate
  // transform (to preserve fractional offsets). The transformed full image can extend outside
  // `dest_rect`, so the draw must still be clipped to the destination rectangle.
  //
  // Regression: hero background images rendered via `background-size: cover` were bleeding into
  // fixed headers because the image draw wasn't clipped to `dest_rect`.
  let pixels = vec![
    0, 0, 0, 255, //
    255, 255, 255, 255, //
  ];
  let image = Arc::new(ImageData::new_pixels(2, 1, pixels));

  let mut list = DisplayList::new();
  list.push(DisplayItem::Image(ImageItem {
    // Draw into a sub-rect so any bleed is visible in the surrounding background.
    dest_rect: Rect::from_xywh(10.0, 0.0, 9.0, 1.0),
    image,
    filter_quality: ImageFilterQuality::Linear,
    // A fractional source rect triggers the transform path (no pre-crop), which used to paint
    // outside dest_rect.
    src_rect: Some(Rect::from_xywh(0.5, 0.0, 1.0, 1.0)),
  }));

  let pixmap = DisplayListRenderer::new(30, 1, Rgba::TRANSPARENT, FontContext::new())
    .unwrap()
    .render(&list)
    .unwrap();

  // Pixels before the destination rect should remain untouched.
  for x in 0..10 {
    let px = pixmap.pixel(x, 0).expect("pixel inside viewport");
    assert_eq!(
      (px.red(), px.green(), px.blue(), px.alpha()),
      (0, 0, 0, 0),
      "expected pixel x={x} (left of dest_rect) to remain transparent"
    );
  }

  // Pixels after the destination rect should also remain untouched.
  for x in 19..30 {
    let px = pixmap.pixel(x, 0).expect("pixel inside viewport");
    assert_eq!(
      (px.red(), px.green(), px.blue(), px.alpha()),
      (0, 0, 0, 0),
      "expected pixel x={x} (right of dest_rect) to remain transparent"
    );
  }
}
