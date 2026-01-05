use fastrender::geometry::Rect;
use fastrender::image_loader::ImageCache;
use fastrender::paint::svg_filter::{apply_svg_filter, parse_svg_filter_from_svg_document};
use tiny_skia::{Pixmap, PremultipliedColorU8};

fn empty_pixmap(width: u32, height: u32) -> Pixmap {
  let mut pixmap = Pixmap::new(width, height).expect("pixmap");
  for px in pixmap.pixels_mut() {
    *px = PremultipliedColorU8::TRANSPARENT;
  }
  pixmap
}

fn pixmap_with_pixel(
  width: u32,
  height: u32,
  x: u32,
  y: u32,
  color: PremultipliedColorU8,
) -> Pixmap {
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
fn displacement_map_applies_scale_without_extra_multiplier() {
  // Chrome interprets `scale` as the full displacement range, not half-range (i.e. no extra `*2`
  // multiplier). With channel=1.0 and scale=2, the displacement is +1px (not +2px).
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

  let filter =
    parse_svg_filter_from_svg_document(svg, Some("f"), &ImageCache::new()).expect("filter");

  let mut pixmap = pixmap_with_pixel(
    5,
    5,
    4,
    4,
    PremultipliedColorU8::from_rgba(255, 0, 0, 255).unwrap(),
  );
  apply_svg_filter(
    &filter,
    &mut pixmap,
    1.0,
    Rect::from_xywh(0.0, 0.0, 5.0, 5.0),
  )
  .unwrap();

  assert_eq!(
    pixmap.data(),
    pixmap_with_pixel(
      5,
      5,
      3,
      3,
      PremultipliedColorU8::from_rgba(255, 0, 0, 255).unwrap()
    )
    .data(),
    "expected saturated map channels with scale=2 to shift by 1px"
  );
}

#[test]
fn displacement_map_interprets_map_channels_as_unpremultiplied() {
  // Chrome interprets displacement channels after un-premultiplying the map (alpha does not
  // attenuate channel values). With `flood-opacity≈0.5`, an sRGB white channel remains 1.0, so it
  // produces a non-zero displacement.
  let svg = r#"
    <svg xmlns="http://www.w3.org/2000/svg" width="3" height="1" viewBox="0 0 3 1">
      <filter id="f" x="0" y="0" width="3" height="1"
              filterUnits="userSpaceOnUse" primitiveUnits="userSpaceOnUse">
        <!--
          Use an opacity just above 0.5 so 8-bit quantization doesn't round down below 0.5 and
          accidentally introduce a small negative Y displacement at y=0 (which would sample out of
          bounds and turn the row fully transparent).
        -->
        <feFlood flood-color="white" flood-opacity="0.502" result="map" />
        <feDisplacementMap in="SourceGraphic" in2="map" scale="2"
                           xChannelSelector="R" yChannelSelector="A" />
      </filter>
    </svg>
  "#;

  let filter =
    parse_svg_filter_from_svg_document(svg, Some("f"), &ImageCache::new()).expect("filter");

  let mut pixmap = gradient_pixmap();
  apply_svg_filter(
    &filter,
    &mut pixmap,
    1.0,
    Rect::from_xywh(0.0, 0.0, 3.0, 1.0),
  )
  .unwrap();

  assert_eq!(
    pixmap.data(),
    {
      let mut expected = empty_pixmap(3, 1);
      expected.pixels_mut()[0] = PremultipliedColorU8::from_rgba(0, 255, 0, 255).unwrap();
      expected.pixels_mut()[1] = PremultipliedColorU8::from_rgba(0, 0, 255, 255).unwrap();
      expected
    }
    .data(),
    "expected semi-transparent white map to still displace by +1px"
  );
}

#[test]
fn displacement_map_object_bounding_box_scale_is_resolved_against_bbox_width() {
  // Chrome (Skia) resolves `primitiveUnits="objectBoundingBox"` scalar `scale` against the bbox
  // width (not min/avg dimension). With a 4×2 bbox and scale=1, channel=1.0 yields dx=dy=2 which
  // pulls samples out-of-bounds, leaving the output fully transparent.
  let svg = r#"
    <svg xmlns="http://www.w3.org/2000/svg" width="4" height="2" viewBox="0 0 4 2">
      <filter id="f" x="0" y="0" width="4" height="2"
              filterUnits="userSpaceOnUse" primitiveUnits="objectBoundingBox"
              color-interpolation-filters="sRGB">
        <feFlood flood-color="rgb(255,0,255)" result="map" />
        <feDisplacementMap in="SourceGraphic" in2="map" scale="1"
                           xChannelSelector="R" yChannelSelector="B" />
      </filter>
    </svg>
  "#;

  let filter =
    parse_svg_filter_from_svg_document(svg, Some("f"), &ImageCache::new()).expect("filter");

  let mut pixmap = pixmap_with_pixel(
    4,
    2,
    3,
    1,
    PremultipliedColorU8::from_rgba(255, 0, 0, 255).expect("red"),
  );
  apply_svg_filter(
    &filter,
    &mut pixmap,
    1.0,
    Rect::from_xywh(0.0, 0.0, 4.0, 2.0),
  )
  .unwrap();

  assert_eq!(
    pixmap.data(),
    empty_pixmap(4, 2).data(),
    "expected bbox-width scale to push samples out-of-bounds (transparent output)"
  );
}

#[test]
fn displacement_map_ignores_filter_res() {
  // Chrome ignores the deprecated SVG 1.1 `filterRes` attribute. The SVG filter executor follows
  // Chrome here, treating `filterRes` as unset for filter graphs that contain `feDisplacementMap`.
  let svg = r#"
    <svg xmlns="http://www.w3.org/2000/svg" width="3" height="1" viewBox="0 0 3 1">
      <filter id="with_res" x="0" y="0" width="3" height="1"
              filterUnits="userSpaceOnUse" primitiveUnits="userSpaceOnUse"
              filterRes="6 1">
        <feFlood flood-color="rgb(255,0,0)" flood-opacity="0.502" result="map" />
        <feDisplacementMap in="SourceGraphic" in2="map" scale="2"
                           xChannelSelector="R" yChannelSelector="A" />
      </filter>
      <filter id="no_res" x="0" y="0" width="3" height="1"
              filterUnits="userSpaceOnUse" primitiveUnits="userSpaceOnUse">
        <feFlood flood-color="rgb(255,0,0)" flood-opacity="0.502" result="map" />
        <feDisplacementMap in="SourceGraphic" in2="map" scale="2"
                           xChannelSelector="R" yChannelSelector="A" />
      </filter>
    </svg>
  "#;

  let filter_with_res =
    parse_svg_filter_from_svg_document(svg, Some("with_res"), &ImageCache::new()).expect("filter");
  let filter_no_res =
    parse_svg_filter_from_svg_document(svg, Some("no_res"), &ImageCache::new()).expect("filter");

  let mut with_res = gradient_pixmap();
  apply_svg_filter(
    &filter_with_res,
    &mut with_res,
    1.0,
    Rect::from_xywh(0.0, 0.0, 3.0, 1.0),
  )
  .unwrap();

  let mut no_res = gradient_pixmap();
  apply_svg_filter(
    &filter_no_res,
    &mut no_res,
    1.0,
    Rect::from_xywh(0.0, 0.0, 3.0, 1.0),
  )
  .unwrap();

  assert_eq!(
    with_res.data(),
    no_res.data(),
    "expected filterRes to have no effect on displacement map output"
  );
  assert_eq!(
    with_res.data(),
    {
      let mut expected = empty_pixmap(3, 1);
      expected.pixels_mut()[0] = PremultipliedColorU8::from_rgba(0, 255, 0, 255).unwrap();
      expected.pixels_mut()[1] = PremultipliedColorU8::from_rgba(0, 0, 255, 255).unwrap();
      expected
    }
    .data(),
    "expected the displacement map to still be applied (filterRes ignored, not short-circuited)"
  );
}

#[test]
fn displacement_map_interprets_map_channels_in_color_interpolation_space() {
  // The map channels must be interpreted after converting into the filter's
  // `color-interpolation-filters` color space.
  //
  // sRGB 128 is ~0.502 and causes an ~0px displacement with scale=2, but in linearRGB it is ~0.216
  // which becomes a -1px displacement after nearest-neighbor sampling/rounding.
  let svg = r#"
    <svg xmlns="http://www.w3.org/2000/svg" width="3" height="1" viewBox="0 0 3 1">
      <filter id="srgb" x="0" y="0" width="3" height="1"
              filterUnits="userSpaceOnUse" primitiveUnits="userSpaceOnUse"
              color-interpolation-filters="sRGB">
        <feFlood flood-color="rgb(128,0,0)" flood-opacity="0.502" result="map" />
        <feDisplacementMap in="SourceGraphic" in2="map" scale="2"
                           xChannelSelector="R" yChannelSelector="A" />
      </filter>
      <filter id="linear" x="0" y="0" width="3" height="1"
              filterUnits="userSpaceOnUse" primitiveUnits="userSpaceOnUse"
              color-interpolation-filters="linearRGB">
        <feFlood flood-color="rgb(128,0,0)" flood-opacity="0.502" result="map" />
        <feDisplacementMap in="SourceGraphic" in2="map" scale="2"
                           xChannelSelector="R" yChannelSelector="A" />
      </filter>
    </svg>
  "#;

  let srgb_filter =
    parse_svg_filter_from_svg_document(svg, Some("srgb"), &ImageCache::new()).expect("filter");
  let linear_filter =
    parse_svg_filter_from_svg_document(svg, Some("linear"), &ImageCache::new()).expect("filter");

  let mut srgb = gradient_pixmap();
  apply_svg_filter(
    &srgb_filter,
    &mut srgb,
    1.0,
    Rect::from_xywh(0.0, 0.0, 3.0, 1.0),
  )
  .unwrap();

  let mut linear = gradient_pixmap();
  apply_svg_filter(
    &linear_filter,
    &mut linear,
    1.0,
    Rect::from_xywh(0.0, 0.0, 3.0, 1.0),
  )
  .unwrap();

  assert_eq!(
    srgb.data(),
    gradient_pixmap().data(),
    "expected sRGB displacement to be effectively 0px for channel=128"
  );
  assert_eq!(
    linear.data(),
    {
      let mut expected = empty_pixmap(3, 1);
      expected.pixels_mut()[1] = PremultipliedColorU8::from_rgba(255, 0, 0, 255).unwrap();
      expected.pixels_mut()[2] = PremultipliedColorU8::from_rgba(0, 255, 0, 255).unwrap();
      expected
    }
    .data(),
    "expected linearRGB displacement to shift right by 1px"
  );
}
