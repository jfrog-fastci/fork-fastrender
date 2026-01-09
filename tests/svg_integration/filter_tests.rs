use fastrender::geometry::Rect;
use fastrender::image_loader::ImageCache;
use fastrender::paint::svg_filter::{
  apply_svg_filter, ColorInterpolationFilters, CompositeOperator, FilterInput, FilterPrimitive,
  FilterStep, SvgFilter, SvgFilterPrimitiveRegionOverride, SvgFilterRegion, SvgFilterUnits,
  SvgLength, TurbulenceType,
};
use fastrender::Rgba;
use tiny_skia::{Pixmap, PremultipliedColorU8};

fn pixel(pixmap: &Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let px = pixmap.pixel(x, y).unwrap();
  (px.red(), px.green(), px.blue(), px.alpha())
}

fn assert_pixmaps_match_with_tolerance(actual: &Pixmap, expected: &Pixmap, tolerance: u8) {
  assert_eq!(
    (actual.width(), actual.height()),
    (expected.width(), expected.height()),
    "pixmap size mismatch: actual={}x{} expected={}x{}",
    actual.width(),
    actual.height(),
    expected.width(),
    expected.height()
  );

  let mut max_delta = 0u8;
  let mut max_at = (0u32, 0u32);
  let mut max_actual = (0u8, 0u8, 0u8, 0u8);
  let mut max_expected = (0u8, 0u8, 0u8, 0u8);

  for y in 0..actual.height() {
    for x in 0..actual.width() {
      let a = pixel(actual, x, y);
      let e = pixel(expected, x, y);
      let delta = [
        a.0.abs_diff(e.0),
        a.1.abs_diff(e.1),
        a.2.abs_diff(e.2),
        a.3.abs_diff(e.3),
      ];
      let local = delta.into_iter().max().unwrap_or(0);
      if local > max_delta {
        max_delta = local;
        max_at = (x, y);
        max_actual = a;
        max_expected = e;
      }
    }
  }

  assert!(
    max_delta <= tolerance,
    "pixmap mismatch (tolerance={tolerance}): max_delta={max_delta} at ({},{})\n  expected={max_expected:?}\n    actual={max_actual:?}",
    max_at.0,
    max_at.1
  );
}

fn base_filter(steps: Vec<FilterStep>) -> SvgFilter {
  let mut filter = SvgFilter {
    color_interpolation_filters: ColorInterpolationFilters::LinearRGB,
    steps,
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
fn missing_reference_defaults_to_transparent_integration() {
  let mut pixmap = Pixmap::new(2, 1).unwrap();
  pixmap.pixels_mut()[0] = PremultipliedColorU8::from_rgba(0, 0, 255, 255).unwrap();

  let steps = vec![
    FilterStep {
      result: Some("filled".into()),
      color_interpolation_filters: None,
      primitive: FilterPrimitive::Flood {
        color: Rgba::from_rgba8(255, 0, 0, 255),
        opacity: 1.0,
      },
      region: None,
    },
    FilterStep {
      result: None,
      color_interpolation_filters: None,
      primitive: FilterPrimitive::Composite {
        input1: FilterInput::Reference("filled".into()),
        input2: FilterInput::Reference("does-not-exist".into()),
        operator: CompositeOperator::Over,
      },
      region: None,
    },
  ];

  let filter = base_filter(steps);
  let bbox = Rect::from_xywh(0.0, 0.0, 2.0, 1.0);
  apply_svg_filter(&filter, &mut pixmap, 1.0, bbox).unwrap();

  assert_eq!(pixel(&pixmap, 0, 0), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 1, 0), (255, 0, 0, 255));
}

#[test]
fn primitive_region_userspace_respects_scale() {
  let mut pixmap = Pixmap::new(4, 4).unwrap();
  let steps = vec![FilterStep {
    result: None,
    color_interpolation_filters: None,
    primitive: FilterPrimitive::Flood {
      color: Rgba::from_rgba8(0, 255, 0, 255),
      opacity: 1.0,
    },
    region: Some(SvgFilterPrimitiveRegionOverride {
      x: Some(SvgLength::Number(0.0)),
      y: Some(SvgLength::Number(0.0)),
      width: Some(SvgLength::Number(1.0)),
      height: Some(SvgLength::Number(1.0)),
      units: SvgFilterUnits::UserSpaceOnUse,
    }),
  }];
  let mut filter = base_filter(steps);
  filter.primitive_units = SvgFilterUnits::UserSpaceOnUse;
  filter.refresh_fingerprint();

  let bbox = Rect::from_xywh(0.0, 0.0, 4.0, 4.0);
  apply_svg_filter(&filter, &mut pixmap, 2.0, bbox).unwrap();

  // Region should be 2x2 after scaling (1px * scale=2).
  assert_eq!(pixel(&pixmap, 0, 0), (0, 255, 0, 255));
  assert_eq!(pixel(&pixmap, 1, 1), (0, 255, 0, 255));
  assert_eq!(pixel(&pixmap, 2, 0), (0, 0, 0, 0));
  assert_eq!(pixel(&pixmap, 3, 3), (0, 0, 0, 0));
}

#[test]
fn turbulence_is_deterministic() {
  let svg = r#"
    <svg xmlns="http://www.w3.org/2000/svg" width="2" height="2" shape-rendering="crispEdges">
      <defs>
        <filter id="f" x="0" y="0" width="2" height="2" filterUnits="userSpaceOnUse" primitiveUnits="userSpaceOnUse">
          <feTurbulence baseFrequency="0.5 0.5" seed="2" numOctaves="1" type="turbulence" />
        </filter>
      </defs>
      <rect x="0" y="0" width="2" height="2" fill="white" filter="url(#f)" />
    </svg>
  "#;

  let cache = ImageCache::new();
  let expected = cache
    .render_svg_pixmap_at_size(svg, 2, 2, "test://turbulence_reference", 1.0)
    .expect("render via resvg");

  let mut pixmap = Pixmap::new(2, 2).unwrap();
  let steps = vec![FilterStep {
    result: None,
    color_interpolation_filters: None,
    primitive: FilterPrimitive::Turbulence {
      base_frequency: (0.5, 0.5),
      seed: 2,
      octaves: 1,
      stitch_tiles: false,
      kind: TurbulenceType::Turbulence,
    },
    region: None,
  }];
  let mut filter = base_filter(steps);
  filter.region = SvgFilterRegion {
    x: SvgLength::Number(0.0),
    y: SvgLength::Number(0.0),
    width: SvgLength::Number(2.0),
    height: SvgLength::Number(2.0),
    units: SvgFilterUnits::UserSpaceOnUse,
  };
  filter.primitive_units = SvgFilterUnits::UserSpaceOnUse;
  filter.refresh_fingerprint();

  let bbox = Rect::from_xywh(0.0, 0.0, 2.0, 2.0);
  apply_svg_filter(&filter, &mut pixmap, 1.0, bbox).unwrap();

  assert_pixmaps_match_with_tolerance(&pixmap, expected.as_ref(), 0);
}
