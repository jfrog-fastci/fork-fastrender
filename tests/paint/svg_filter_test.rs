use fastrender::geometry::Rect;
use fastrender::paint::svg_filter::{
  apply_svg_filter, ChannelSelector, ColorInterpolationFilters, FilterInput, FilterPrimitive,
  FilterStep, ImagePrimitive, SvgFilter, SvgFilterRegion, SvgFilterUnits, SvgLength,
};
use tiny_skia::{Pixmap, PremultipliedColorU8};

fn gradient_pixmap() -> Pixmap {
  let mut pixmap = Pixmap::new(3, 1).unwrap();
  let colors = [(255, 0, 0, 255), (0, 255, 0, 255), (0, 0, 255, 255)];
  for (idx, px) in pixmap.pixels_mut().iter_mut().enumerate() {
    let (r, g, b, a) = colors[idx];
    *px = PremultipliedColorU8::from_rgba(r, g, b, a).unwrap_or(PremultipliedColorU8::TRANSPARENT);
  }
  pixmap
}

fn displacement_map_pixmap() -> Pixmap {
  let mut pixmap = Pixmap::new(3, 1).unwrap();
  for px in pixmap.pixels_mut() {
    // Premultiplied RGBA such that the unpremultiplied red channel is 1.0 (max rightward shift)
    // and the unpremultiplied blue channel is exactly 0.5 (no Y displacement).
    //
    // Note: `apply_displacement_map` unpremultiplies the sampled map pixel before extracting
    // channel values, so using a non-opaque alpha here makes it possible to represent 0.5 exactly.
    *px = PremultipliedColorU8::from_rgba(254, 0, 127, 254)
      .unwrap_or(PremultipliedColorU8::TRANSPARENT);
  }
  pixmap
}

fn pixel(pixmap: &Pixmap, x: u32) -> (u8, u8, u8, u8) {
  let px = pixmap.pixel(x, 0).unwrap();
  (px.red(), px.green(), px.blue(), px.alpha())
}

fn displacement_filter(scale: f32) -> SvgFilter {
  let map = displacement_map_pixmap();
  let mut filter = SvgFilter {
    color_interpolation_filters: ColorInterpolationFilters::SRGB,
    steps: vec![
      FilterStep {
        result: Some("map".to_string()),
        color_interpolation_filters: None,
        primitive: FilterPrimitive::Image(ImagePrimitive::from_pixmap(map)),
        region: None,
      },
      FilterStep {
        result: None,
        color_interpolation_filters: None,
        primitive: FilterPrimitive::DisplacementMap {
          in1: FilterInput::SourceGraphic,
          in2: FilterInput::Reference("map".to_string()),
          scale,
          x_channel: ChannelSelector::R,
          y_channel: ChannelSelector::B,
        },
        region: None,
      },
    ],
    region: SvgFilterRegion {
      x: SvgLength::Percent(-0.1),
      y: SvgLength::Percent(-0.1),
      width: SvgLength::Percent(1.2),
      height: SvgLength::Percent(1.2),
      units: SvgFilterUnits::ObjectBoundingBox,
    },
    filter_res: None,
    primitive_units: SvgFilterUnits::UserSpaceOnUse,
    fingerprint: 0,
  };
  filter.refresh_fingerprint();
  filter
}

#[test]
fn displacement_map_shifts_pixels_right() {
  let mut pixmap = gradient_pixmap();
  let filter = displacement_filter(2.0);
  let bbox = Rect::from_xywh(0.0, 0.0, pixmap.width() as f32, pixmap.height() as f32);

  apply_svg_filter(&filter, &mut pixmap, 1.0, bbox).unwrap();

  assert_eq!(pixel(&pixmap, 0), (0, 255, 0, 255));
  assert_eq!(pixel(&pixmap, 1), (0, 0, 255, 255));
  assert_eq!(pixel(&pixmap, 2), (0, 0, 0, 0));
}

#[test]
fn displacement_map_scale_zero_is_identity() {
  let mut pixmap = gradient_pixmap();
  let expected: Vec<_> = (0..pixmap.width()).map(|x| pixel(&pixmap, x)).collect();

  let filter = displacement_filter(0.0);
  let bbox = Rect::from_xywh(0.0, 0.0, pixmap.width() as f32, pixmap.height() as f32);
  apply_svg_filter(&filter, &mut pixmap, 1.0, bbox).unwrap();

  for x in 0..pixmap.width() {
    assert_eq!(pixel(&pixmap, x), expected[x as usize]);
  }
}

#[test]
fn displacement_map_interprets_map_in_color_interpolation_space() {
  let mut primary = gradient_pixmap();
  let mut map = Pixmap::new(primary.width(), primary.height()).unwrap();
  // Premultiplied 50% gray at 50% alpha.
  // When interpreted in linearRGB, this becomes ~21% gray, producing a leftward displacement.
  let half = PremultipliedColorU8::from_rgba(64, 64, 64, 128).unwrap();
  for px in map.pixels_mut() {
    *px = half;
  }

  let mut filter = SvgFilter {
    color_interpolation_filters: ColorInterpolationFilters::LinearRGB,
    steps: vec![
      FilterStep {
        result: Some("map".to_string()),
        color_interpolation_filters: None,
        primitive: FilterPrimitive::Image(ImagePrimitive::from_pixmap(map)),
        region: None,
      },
      FilterStep {
        result: None,
        color_interpolation_filters: None,
        primitive: FilterPrimitive::DisplacementMap {
          in1: FilterInput::SourceGraphic,
          in2: FilterInput::Reference("map".to_string()),
          scale: 4.0,
          x_channel: ChannelSelector::R,
          y_channel: ChannelSelector::A,
        },
        region: None,
      },
    ],
    region: SvgFilterRegion {
      x: SvgLength::Percent(-0.1),
      y: SvgLength::Percent(-0.1),
      width: SvgLength::Percent(1.2),
      height: SvgLength::Percent(1.2),
      units: SvgFilterUnits::ObjectBoundingBox,
    },
    filter_res: None,
    primitive_units: SvgFilterUnits::UserSpaceOnUse,
    fingerprint: 0,
  };
  filter.refresh_fingerprint();

  let bbox = Rect::from_xywh(0.0, 0.0, primary.width() as f32, primary.height() as f32);
  apply_svg_filter(&filter, &mut primary, 1.0, bbox).unwrap();

  let center = pixel(&primary, 1);
  assert!(
    center.0 > center.1,
    "linearRGB map values should pull color toward red, got {:?}",
    center
  );
  assert!(center.3 > 0, "displacement map should preserve coverage");
}

#[test]
fn displacement_map_object_bounding_box_scales_per_axis() {
  let mut primary = Pixmap::new(4, 2).unwrap();
  let primary_width = primary.width();
  for y in 0..primary.height() {
    for x in 0..primary_width {
      let r = (x * 50) as u8;
      let g = (y * 100) as u8;
      primary.pixels_mut()[(y * primary_width + x) as usize] =
        PremultipliedColorU8::from_rgba(r, g, 0, 255).unwrap_or(PremultipliedColorU8::TRANSPARENT);
    }
  }

  let mut map = Pixmap::new(primary.width(), primary.height()).unwrap();
  for px in map.pixels_mut() {
    *px = PremultipliedColorU8::from_rgba(255, 0, 0, 255).unwrap();
  }

  let mut filter = SvgFilter {
    color_interpolation_filters: ColorInterpolationFilters::SRGB,
    steps: vec![
      FilterStep {
        result: Some("map".to_string()),
        color_interpolation_filters: None,
        primitive: FilterPrimitive::Image(ImagePrimitive::from_pixmap(map)),
        region: None,
      },
      FilterStep {
        result: None,
        color_interpolation_filters: None,
        primitive: FilterPrimitive::DisplacementMap {
          in1: FilterInput::SourceGraphic,
          in2: FilterInput::Reference("map".to_string()),
          scale: 1.0,
          x_channel: ChannelSelector::R,
          y_channel: ChannelSelector::R,
        },
        region: None,
      },
    ],
    region: SvgFilterRegion {
      x: SvgLength::Percent(-0.1),
      y: SvgLength::Percent(-0.1),
      width: SvgLength::Percent(1.2),
      height: SvgLength::Percent(1.2),
      units: SvgFilterUnits::ObjectBoundingBox,
    },
    filter_res: None,
    primitive_units: SvgFilterUnits::ObjectBoundingBox,
    fingerprint: 0,
  };
  filter.refresh_fingerprint();

  // For a 4x2 bbox, primitiveUnits=objectBoundingBox should make scale=1.0 resolve to
  // scale_x=4px and scale_y=2px. With channel value 1.0 this yields dx=2 and dy=1.
  let bbox = Rect::from_xywh(0.0, 0.0, primary.width() as f32, primary.height() as f32);
  apply_svg_filter(&filter, &mut primary, 1.0, bbox).unwrap();

  let top_left = primary.pixel(0, 0).unwrap();
  assert_eq!(
    (top_left.red(), top_left.green(), top_left.blue(), top_left.alpha()),
    (100, 100, 0, 255)
  );
}
