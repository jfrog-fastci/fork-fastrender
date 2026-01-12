use crate::image_loader::ImageCache;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use image::{ImageFormat, Rgba, RgbaImage};
use resvg::usvg;
use std::fs;
use std::io::Cursor;
use tiny_skia::{Pixmap, PremultipliedColorU8, Transform};
use url::Url;

#[test]
fn svg_root_viewport_resolves_percent_lengths_for_rasterization() {
  // Regression test for SVG-as-image rasterization: when the outermost <svg> omits width/height,
  // SVG defaults them to 100%. Percent-based sizes (like <image width="100%">) must then resolve
  // against the concrete render size supplied by the embedding context.
  //
  // `resvg/usvg` resolve percentage lengths during parse. If the outermost viewport is missing,
  // percent values can collapse to zero, producing fully transparent output. Ensure we inject a
  // definite viewport size before rasterization.
  let cache = ImageCache::new();

  // This is a minimized version of Next.js' blur placeholder SVG (as seen on theverge.com):
  // - outermost <svg> has no width/height (defaults to 100%),
  // - <image width="100%" height="100%"> is filtered via `style="filter: url(#b);"`
  // - referenced PNG is a 1×1 opaque light pixel (L=233, A=255).
  //
  // If percent lengths in SVG are resolved without the concrete raster size, the <image> can end
  // up with a 0×0 bounding box and the entire render becomes transparent.
  const PIXEL_PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mN8+R8AAtcB6oaHtZcAAAAASUVORK5CYII=";
  let svg = format!(
    r#"<svg xmlns='http://www.w3.org/2000/svg' ><filter id='b' color-interpolation-filters='sRGB'><feGaussianBlur stdDeviation='20'/><feColorMatrix values='1 0 0 0 0 0 1 0 0 0 0 0 1 0 0 0 0 0 100 -1' result='s'/><feFlood x='0' y='0' width='100%' height='100%'/><feComposite operator='out' in='s'/><feComposite in2='SourceGraphic'/><feGaussianBlur stdDeviation='20'/></filter><image width='100%' height='100%' x='0' y='0' preserveAspectRatio='none' style='filter: url(#b);' href='data:image/png;base64,{PIXEL_PNG}'/></svg>"#
  );

  let svg_explicit = format!(
    r#"<svg xmlns='http://www.w3.org/2000/svg' width='100' height='100'><filter id='b' color-interpolation-filters='sRGB'><feGaussianBlur stdDeviation='20'/><feColorMatrix values='1 0 0 0 0 0 1 0 0 0 0 0 1 0 0 0 0 0 100 -1' result='s'/><feFlood x='0' y='0' width='100%' height='100%'/><feComposite operator='out' in='s'/><feComposite in2='SourceGraphic'/><feGaussianBlur stdDeviation='20'/></filter><image width='100%' height='100%' x='0' y='0' preserveAspectRatio='none' style='filter: url(#b);' href='data:image/png;base64,{PIXEL_PNG}'/></svg>"#
  );

  let pixmap_explicit = cache
    .render_svg_pixmap_at_size(&svg_explicit, 100, 100, "test://svg", 1.0)
    .expect("render svg pixmap (explicit)");
  let explicit_center = pixmap_explicit.pixel(50, 50).expect("center pixel");
  assert!(
    explicit_center.alpha() > 0,
    "setup sanity check: expected explicit-viewport SVG to render; got rgba=({}, {}, {}, {})",
    explicit_center.red(),
    explicit_center.green(),
    explicit_center.blue(),
    explicit_center.alpha()
  );

  let pixmap = cache
    .render_svg_pixmap_at_size(&svg, 100, 100, "test://svg", 1.0)
    .expect("render svg pixmap");

  let center = pixmap.pixel(50, 50).expect("center pixel");
  assert!(
    center.alpha() > 0,
    "expected filtered percent-sized <image> to render when root viewport is implicit; got rgba=({}, {}, {}, {})",
    center.red(),
    center.green(),
    center.blue(),
    center.alpha()
  );
}

#[test]
fn resvg_ignores_css_transform_translate_percent() {
  let svg = r#"
    <svg xmlns="http://www.w3.org/2000/svg" width="30" height="10" viewBox="0 0 30 10" shape-rendering="crispEdges">
      <g style="transform-box: fill-box; transform: translateX(100%);">
        <rect width="10" height="10" fill="rgb(255,0,0)" />
      </g>
    </svg>
  "#;

  let tree = usvg::Tree::from_str(svg, &usvg::Options::default()).expect("parse svg");
  let mut pixmap = Pixmap::new(30, 10).expect("pixmap");
  resvg::render(&tree, Transform::identity(), &mut pixmap.as_mut());

  let red = PremultipliedColorU8::from_rgba(255, 0, 0, 255).expect("color");
  let pixels = pixmap.pixels_mut();
  let at = |x: u32, y: u32| pixels[(y * 30 + x) as usize];

  assert_eq!(
    at(5, 5),
    red,
    "resvg/usvg currently ignores CSS `transform` in the style attribute (percent case)"
  );
  assert_eq!(
    at(15, 5),
    PremultipliedColorU8::TRANSPARENT,
    "rect should remain at the origin when CSS `transform` is ignored"
  );
}

#[test]
fn svg_image_href_resolves_against_svg_url() {
  let dir = tempfile::tempdir().expect("temp dir");
  let png_path = dir.path().join("img.png");
  let png = RgbaImage::from_pixel(4, 4, Rgba([255, 0, 0, 255]));
  png.save(&png_path).expect("write png");

  let svg_path = dir.path().join("icon.svg");
  let svg_content = r#"
    <svg xmlns="http://www.w3.org/2000/svg" width="4" height="4">
      <image href="img.png" width="4" height="4" />
    </svg>
  "#;
  fs::write(&svg_path, svg_content).expect("write svg");

  let svg_url = Url::from_file_path(&svg_path).unwrap().to_string();

  let mut cache = ImageCache::new();
  cache.set_base_url("file:///not-used-for-svg-base/");

  let image = cache.load(&svg_url).expect("render svg with image href");
  let rgba = image.image.to_rgba8();

  assert_eq!(rgba.dimensions(), (4, 4));
  assert_eq!(*rgba.get_pixel(0, 0), Rgba([255, 0, 0, 255]));
  assert_eq!(*rgba.get_pixel(3, 3), Rgba([255, 0, 0, 255]));
}

#[test]
fn svg_image_href_supports_data_url() {
  let mut cache = ImageCache::new();

  let data_image = RgbaImage::from_pixel(2, 2, Rgba([0, 0, 255, 255]));
  let mut buf = Vec::new();
  data_image
    .write_to(&mut Cursor::new(&mut buf), ImageFormat::Png)
    .expect("encode png");
  let data_url = format!("data:image/png;base64,{}", STANDARD.encode(&buf));

  let svg = format!(
    r#"<svg xmlns="http://www.w3.org/2000/svg" width="2" height="2">
        <image href="{data_url}" width="2" height="2" />
      </svg>"#
  );

  let (rendered, _, _) = cache.render_svg_to_image(&svg).expect("render svg");
  let rgba = rendered.to_rgba8();
  assert_eq!(*rgba.get_pixel(0, 0), Rgba([0, 0, 255, 255]));
  assert_eq!(*rgba.get_pixel(1, 1), Rgba([0, 0, 255, 255]));
}

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
