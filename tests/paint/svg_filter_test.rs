use fastrender::geometry::Rect;
use fastrender::image_loader::ImageCache;
use fastrender::paint::svg_filter::{apply_svg_filter, parse_svg_filter_from_svg_document};
use tiny_skia::{Pixmap, PremultipliedColorU8, Transform};

fn render_resvg(svg: &str, width: u32, height: u32) -> Pixmap {
  use resvg::usvg;

  let options = usvg::Options::default();
  let tree = usvg::Tree::from_str(svg, &options).expect("parse SVG with resvg");
  let mut pixmap = Pixmap::new(width, height).expect("pixmap");
  resvg::render(&tree, Transform::identity(), &mut pixmap.as_mut());
  pixmap
}

fn empty_pixmap(width: u32, height: u32) -> Pixmap {
  let mut pixmap = Pixmap::new(width, height).expect("pixmap");
  for px in pixmap.pixels_mut() {
    *px = PremultipliedColorU8::TRANSPARENT;
  }
  pixmap
}

fn pixmap_with_pixel(width: u32, height: u32, x: u32, y: u32, color: PremultipliedColorU8) -> Pixmap {
  let mut pixmap = empty_pixmap(width, height);
  if x < width && y < height {
    pixmap.pixels_mut()[(y * width + x) as usize] = color;
  }
  pixmap
}

fn fill_rect(
  pixmap: &mut Pixmap,
  x: u32,
  y: u32,
  width: u32,
  height: u32,
  color: PremultipliedColorU8,
) {
  let pix_w = pixmap.width();
  let pix_h = pixmap.height();
  if pix_w == 0 || pix_h == 0 {
    return;
  }
  let end_x = x.saturating_add(width).min(pix_w);
  let end_y = y.saturating_add(height).min(pix_h);
  let pixels = pixmap.pixels_mut();
  for yy in y.min(pix_h)..end_y {
    let row = (yy * pix_w) as usize;
    for xx in x.min(pix_w)..end_x {
      pixels[row + xx as usize] = color;
    }
  }
}

fn gradient_pixmap() -> Pixmap {
  let mut pixmap = Pixmap::new(3, 1).expect("pixmap");
  let colors = [(255, 0, 0), (0, 255, 0), (0, 0, 255)];
  for (idx, px) in pixmap.pixels_mut().iter_mut().enumerate() {
    let (r, g, b) = colors[idx];
    *px = PremultipliedColorU8::from_rgba(r, g, b, 255).expect("premultiply");
  }
  pixmap
}

#[test]
fn displacement_map_matches_resvg_for_max_displacement() {
  // Saturated map channels should yield a max-strength displacement.
  let svg = r#"
    <svg xmlns="http://www.w3.org/2000/svg" width="5" height="5" viewBox="0 0 5 5">
      <filter id="f" x="0" y="0" width="5" height="5"
              filterUnits="userSpaceOnUse" primitiveUnits="userSpaceOnUse"
              color-interpolation-filters="sRGB">
        <feFlood flood-color="rgb(255,0,255)" result="map" />
        <feDisplacementMap in="SourceGraphic" in2="map" scale="2"
                           xChannelSelector="R" yChannelSelector="B" />
      </filter>
      <g filter="url(#f)">
        <rect width="5" height="5" fill="rgba(0,0,0,0)" />
        <rect x="4" y="4" width="1" height="1" fill="rgb(255,0,0)" />
      </g>
    </svg>
  "#;

  let expected = render_resvg(svg, 5, 5);
  let filter =
    parse_svg_filter_from_svg_document(svg, Some("f"), &ImageCache::new()).expect("filter");

  let mut pixmap = pixmap_with_pixel(5, 5, 4, 4, PremultipliedColorU8::from_rgba(255, 0, 0, 255).unwrap());
  apply_svg_filter(&filter, &mut pixmap, 1.0, Rect::from_xywh(0.0, 0.0, 5.0, 5.0)).unwrap();

  assert_eq!(
    pixmap.data(),
    expected.data(),
    "FastRender displacement map must match resvg output"
  );
}

#[test]
fn displacement_map_uses_premultiplied_map_channels() {
  // resvg interprets displacement channels as premultiplied values. When the map is
  // semi-transparent white, the premultiplied R channel equals the alpha channel (0.5), so the
  // displacement should be ~zero.
  let svg = r#"
    <svg xmlns="http://www.w3.org/2000/svg" width="5" height="5" viewBox="0 0 5 5">
      <filter id="f" x="0" y="0" width="5" height="5"
              filterUnits="userSpaceOnUse" primitiveUnits="userSpaceOnUse"
              color-interpolation-filters="sRGB">
        <feFlood flood-color="white" flood-opacity="0.5" result="map" />
        <feDisplacementMap in="SourceGraphic" in2="map" scale="2"
                           xChannelSelector="R" yChannelSelector="R" />
      </filter>
      <g filter="url(#f)">
        <rect width="5" height="5" fill="rgba(0,0,0,0)" />
        <rect x="1" y="1" width="3" height="3" fill="rgb(255,0,0)" />
      </g>
    </svg>
  "#;

  let expected = render_resvg(svg, 5, 5);
  let filter =
    parse_svg_filter_from_svg_document(svg, Some("f"), &ImageCache::new()).expect("filter");

  let mut pixmap = empty_pixmap(5, 5);
  fill_rect(
    &mut pixmap,
    1,
    1,
    3,
    3,
    PremultipliedColorU8::from_rgba(255, 0, 0, 255).expect("red"),
  );
  apply_svg_filter(&filter, &mut pixmap, 1.0, Rect::from_xywh(0.0, 0.0, 5.0, 5.0)).unwrap();

  let expected_px = expected.pixel(2, 2).expect("expected pixel");
  let actual_px = pixmap.pixel(2, 2).expect("actual pixel");
  assert_eq!(
    (actual_px.red(), actual_px.green(), actual_px.blue(), actual_px.alpha()),
    (
      expected_px.red(),
      expected_px.green(),
      expected_px.blue(),
      expected_px.alpha()
    ),
    "FastRender displacement map must use premultiplied channel values like resvg"
  );
}

#[test]
fn displacement_map_scale_respects_object_bounding_box_units() {
  // When primitiveUnits=objectBoundingBox, `scale` is relative to the filtered element's bbox, and
  // scales independently in X/Y. Use a non-square bbox so per-axis scaling is observable.
  let svg = r#"
    <svg xmlns="http://www.w3.org/2000/svg" width="4" height="2" viewBox="0 0 4 2">
      <filter id="f" x="0" y="0" width="4" height="2"
              filterUnits="userSpaceOnUse" primitiveUnits="objectBoundingBox"
              color-interpolation-filters="sRGB">
        <feFlood flood-color="rgb(255,0,255)" result="map" />
        <feDisplacementMap in="SourceGraphic" in2="map" scale="0.5"
                           xChannelSelector="R" yChannelSelector="B" />
      </filter>
      <g filter="url(#f)">
        <rect width="4" height="2" fill="rgba(0,0,0,0)" />
        <rect x="3" y="1" width="1" height="1" fill="rgb(255,0,0)" />
      </g>
    </svg>
  "#;

  let expected = render_resvg(svg, 4, 2);
  let filter =
    parse_svg_filter_from_svg_document(svg, Some("f"), &ImageCache::new()).expect("filter");

  let mut pixmap = pixmap_with_pixel(4, 2, 3, 1, PremultipliedColorU8::from_rgba(255, 0, 0, 255).expect("red"));
  apply_svg_filter(&filter, &mut pixmap, 1.0, Rect::from_xywh(0.0, 0.0, 4.0, 2.0)).unwrap();

  assert_eq!(
    pixmap.data(),
    expected.data(),
    "FastRender displacement map objectBoundingBox scaling must match resvg"
  );
}

#[test]
fn displacement_map_respects_anisotropic_filter_res_scale() {
  // Stretch the filter graph in X via `filterRes` so the displacement map scale must be converted
  // with separate X/Y pixel scales (i.e. not using a single `scale_avg`).
  let svg = r#"
    <svg xmlns="http://www.w3.org/2000/svg" width="3" height="1" viewBox="0 0 3 1">
      <filter id="f" x="0" y="0" width="3" height="1"
              filterUnits="userSpaceOnUse" primitiveUnits="userSpaceOnUse"
              filterRes="6 1"
              color-interpolation-filters="sRGB">
        <feFlood flood-color="rgb(128,0,0)" result="map" />
        <feDisplacementMap in="SourceGraphic" in2="map" scale="1.5"
                           xChannelSelector="R" yChannelSelector="A" />
      </filter>
      <g filter="url(#f)">
        <rect x="0" y="0" width="1" height="1" fill="rgb(255,0,0)" />
        <rect x="1" y="0" width="1" height="1" fill="rgb(0,255,0)" />
        <rect x="2" y="0" width="1" height="1" fill="rgb(0,0,255)" />
      </g>
    </svg>
  "#;

  let expected = render_resvg(svg, 3, 1);
  let filter =
    parse_svg_filter_from_svg_document(svg, Some("f"), &ImageCache::new()).expect("filter");

  let mut pixmap = gradient_pixmap();
  apply_svg_filter(&filter, &mut pixmap, 1.0, Rect::from_xywh(0.0, 0.0, 3.0, 1.0)).unwrap();

  assert_eq!(
    pixmap.data(),
    expected.data(),
    "FastRender displacement map must match resvg when filterRes scales are anisotropic"
  );
}

#[test]
fn displacement_map_interprets_map_in_color_interpolation_space() {
  // When `color-interpolation-filters` is linearRGB, the map channels must be interpreted after
  // converting to linear space.
  let svg = r#"
    <svg xmlns="http://www.w3.org/2000/svg" width="5" height="5" viewBox="0 0 5 5">
      <filter id="f" x="0" y="0" width="5" height="5"
              filterUnits="userSpaceOnUse" primitiveUnits="userSpaceOnUse"
              color-interpolation-filters="linearRGB">
        <feFlood flood-color="rgb(128,128,128)" result="map" />
        <feDisplacementMap in="SourceGraphic" in2="map" scale="2"
                           xChannelSelector="R" yChannelSelector="R" />
      </filter>
      <g filter="url(#f)">
        <rect width="5" height="5" fill="rgba(0,0,0,0)" />
        <rect x="2" y="2" width="1" height="1" fill="rgb(255,0,0)" />
      </g>
    </svg>
  "#;

  let expected = render_resvg(svg, 5, 5);
  let filter =
    parse_svg_filter_from_svg_document(svg, Some("f"), &ImageCache::new()).expect("filter");

  let mut pixmap = pixmap_with_pixel(
    5,
    5,
    2,
    2,
    PremultipliedColorU8::from_rgba(255, 0, 0, 255).expect("red"),
  );
  apply_svg_filter(&filter, &mut pixmap, 1.0, Rect::from_xywh(0.0, 0.0, 5.0, 5.0)).unwrap();

  assert_eq!(
    pixmap.data(),
    expected.data(),
    "FastRender displacement map linearRGB channel interpretation must match resvg"
  );
}

