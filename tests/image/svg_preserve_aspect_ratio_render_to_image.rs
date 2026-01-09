use fastrender::image_loader::ImageCache;
use image::{Rgba, RgbaImage};

fn composite_pixel_over_white(px: Rgba<u8>) -> Rgba<u8> {
  let a = px[3] as u16;
  let inv_a = 255u16.saturating_sub(a);
  // `render_svg_to_image` rasterizes via `tiny-skia`, which stores pixels as premultiplied RGBA.
  // We only sample fully-opaque / fully-transparent pixels in these tests, but compositing over a
  // white background makes the expected colors easier to express (red vs white).
  Rgba([
    (px[0] as u16 + inv_a).min(255) as u8,
    (px[1] as u16 + inv_a).min(255) as u8,
    (px[2] as u16 + inv_a).min(255) as u8,
    255,
  ])
}

fn render_svg(svg: &str) -> RgbaImage {
  let cache = ImageCache::new();
  let (img, _ratio, _aspect_ratio_none) = cache.render_svg_to_image(svg).expect("render svg");
  img.to_rgba8()
}

#[test]
fn svg_render_to_image_preserve_aspect_ratio_xmin_ymin_meet() {
  let svg = r#"
    <svg xmlns="http://www.w3.org/2000/svg"
         width="20" height="10"
         viewBox="0 0 10 10"
         preserveAspectRatio="xMinYMin meet"
         shape-rendering="crispEdges">
      <rect x="0" y="0" width="10" height="10" fill="red" />
    </svg>
  "#;

  let rgba = render_svg(svg);
  assert_eq!(rgba.dimensions(), (20, 10));

  // With `meet`, the 10x10 viewBox fits into a 20x10 viewport without scaling (height is the
  // limiting dimension), leaving 10px of horizontal space. `xMinYMin` aligns the content to the
  // left.
  assert_eq!(
    composite_pixel_over_white(*rgba.get_pixel(2, 5)),
    Rgba([255, 0, 0, 255]),
    "expected left side to be red"
  );
  assert_eq!(
    composite_pixel_over_white(*rgba.get_pixel(18, 5)),
    Rgba([255, 255, 255, 255]),
    "expected right side to be empty (white when composited)"
  );
}

#[test]
fn svg_render_to_image_preserve_aspect_ratio_xmax_ymin_meet() {
  let svg = r#"
    <svg xmlns="http://www.w3.org/2000/svg"
         width="20" height="10"
         viewBox="0 0 10 10"
         preserveAspectRatio="xMaxYMin meet"
         shape-rendering="crispEdges">
      <rect x="0" y="0" width="10" height="10" fill="red" />
    </svg>
  "#;

  let rgba = render_svg(svg);
  assert_eq!(rgba.dimensions(), (20, 10));

  // `xMaxYMin` aligns the viewBox content to the right.
  assert_eq!(
    composite_pixel_over_white(*rgba.get_pixel(2, 5)),
    Rgba([255, 255, 255, 255]),
    "expected left side to be empty (white when composited)"
  );
  assert_eq!(
    composite_pixel_over_white(*rgba.get_pixel(18, 5)),
    Rgba([255, 0, 0, 255]),
    "expected right side to be red"
  );
}

