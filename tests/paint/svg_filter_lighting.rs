use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use fastrender::geometry::Rect;
use fastrender::image_loader::ImageCache;
use fastrender::image_output::{encode_image, OutputFormat};
use fastrender::paint::svg_filter::{
  apply_svg_filter, parse_svg_filter_from_svg_document, ColorInterpolationFilters, FilterInput,
  FilterPrimitive, FilterStep, LightSource, SvgFilter, SvgFilterPrimitiveRegionOverride,
  SvgFilterRegion, SvgFilterUnits, SvgLength,
};
use fastrender::Rgba;
use tiny_skia::{Pixmap, PremultipliedColorU8, Transform};

fn solid_pixmap(width: u32, height: u32, color: PremultipliedColorU8) -> Pixmap {
  let mut pixmap = Pixmap::new(width, height).expect("pixmap");
  for px in pixmap.pixels_mut() {
    *px = color;
  }
  pixmap
}

fn make_bump_map_pixmap(width: u32, height: u32) -> Pixmap {
  let mut pixmap = Pixmap::new(width, height).expect("pixmap");
  if width == 0 || height == 0 {
    return pixmap;
  }

  let w = width as f32;
  let h = height as f32;

  // Non-symmetric alpha ramp with a smooth bump so normal sign, primitiveUnits scaling, and
  // kernelUnitLength sampling all matter.
  let bump_cx = w * 0.72;
  let bump_cy = h * 0.28;
  let sigma_x = (w * 0.16).max(1.0);
  let sigma_y = (h * 0.22).max(1.0);

  let denom_x = (width.saturating_sub(1)).max(1) as f32;
  let denom_y = (height.saturating_sub(1)).max(1) as f32;

  for y in 0..height {
    let yf = y as f32 / denom_y;
    for x in 0..width {
      let xf = x as f32 / denom_x;

      let mut alpha = xf * 180.0 + yf * 40.0;
      let dx = x as f32 - bump_cx;
      let dy = y as f32 - bump_cy;
      let bump = 90.0
        * (-((dx * dx) / (2.0 * sigma_x * sigma_x) + (dy * dy) / (2.0 * sigma_y * sigma_y))).exp();
      alpha += bump;

      let a = alpha.round().clamp(0.0, 255.0) as u8;
      let idx = y as usize * width as usize + x as usize;
      pixmap.pixels_mut()[idx] = PremultipliedColorU8::from_rgba(0, 0, 0, a).unwrap();
    }
  }

  pixmap
}

fn pixmap_to_data_url_png(pixmap: &Pixmap) -> String {
  let encoded = encode_image(pixmap, OutputFormat::Png).expect("encode bump map png");
  format!("data:image/png;base64,{}", BASE64.encode(encoded))
}

fn with_fingerprint(filter: SvgFilter) -> SvgFilter {
  let mut filter = filter;
  filter.refresh_fingerprint();
  filter
}

#[test]
fn lighting_primitives_parse_light_sources() {
  let svg = r##"
  <svg xmlns="http://www.w3.org/2000/svg">
    <filter id="f" primitiveUnits="objectBoundingBox" color-interpolation-filters="sRGB">
      <feDiffuseLighting in="SourceAlpha" surfaceScale="3" diffuseConstant="2" kernelUnitLength="2 4" lighting-color="rgb(10, 20, 30)" result="diffuse">
        <feSpotLight x="10%" y="20%" z="30" pointsAtX="0" pointsAtY="0" pointsAtZ="10" specularExponent="8" limitingConeAngle="45" />
      </feDiffuseLighting>
      <feSpecularLighting in="SourceGraphic" surfaceScale="1.5" specularConstant="0.5" specularExponent="32" kernelUnitLength="5" lighting-color="#abcdef">
        <feDistantLight azimuth="15" elevation="60" />
      </feSpecularLighting>
    </filter>
  </svg>
  "##;

  let filter =
    parse_svg_filter_from_svg_document(svg, Some("f"), &ImageCache::new()).expect("filter");

  assert_eq!(filter.primitive_units, SvgFilterUnits::ObjectBoundingBox);
  assert_eq!(
    filter.color_interpolation_filters,
    ColorInterpolationFilters::SRGB
  );

  match &filter.steps[0].primitive {
    FilterPrimitive::DiffuseLighting {
      surface_scale,
      diffuse_constant,
      kernel_unit_length,
      light,
      lighting_color,
      ..
    } => {
      assert_eq!(*surface_scale, 3.0);
      assert_eq!(*diffuse_constant, 2.0);
      assert_eq!(kernel_unit_length, &Some((2.0, 4.0)));
      assert_eq!(lighting_color.r, 10);
      assert_eq!(lighting_color.g, 20);
      assert_eq!(lighting_color.b, 30);
      assert!(matches!(
        light,
        LightSource::Spot {
          x: SvgLength::Percent(x),
          y: SvgLength::Percent(y),
          z: SvgLength::Number(z),
          points_at: (SvgLength::Number(_), SvgLength::Number(_), SvgLength::Number(_)),
          specular_exponent,
          limiting_cone_angle: Some(angle),
        } if (*x - 0.10).abs() < f32::EPSILON
          && (*y - 0.20).abs() < f32::EPSILON
          && (*z - 30.0).abs() < f32::EPSILON
          && (*specular_exponent - 8.0).abs() < f32::EPSILON
          && (*angle - 45.0).abs() < f32::EPSILON
      ));
    }
    other => panic!("unexpected first primitive {other:?}"),
  }

  match &filter.steps[1].primitive {
    FilterPrimitive::SpecularLighting {
      surface_scale,
      specular_constant,
      specular_exponent,
      kernel_unit_length,
      light,
      lighting_color,
      ..
    } => {
      assert_eq!(*surface_scale, 1.5);
      assert_eq!(*specular_constant, 0.5);
      assert_eq!(*specular_exponent, 32.0);
      assert_eq!(kernel_unit_length, &Some((5.0, 5.0)));
      assert_eq!(lighting_color.r, 0xab);
      assert_eq!(lighting_color.g, 0xcd);
      assert_eq!(lighting_color.b, 0xef);
      assert!(matches!(
        light,
        LightSource::Distant {
          azimuth,
          elevation
        } if (*azimuth - 15.0).abs() < f32::EPSILON && (*elevation - 60.0).abs() < f32::EPSILON
      ));
    }
    other => panic!("unexpected second primitive {other:?}"),
  }
}

#[test]
fn lighting_primitives_parse_defaults() {
  let svg = r##"
  <svg xmlns="http://www.w3.org/2000/svg">
    <filter id="f" color-interpolation-filters="sRGB">
      <feDiffuseLighting in="SourceAlpha">
        <feDistantLight azimuth="0" elevation="45" />
      </feDiffuseLighting>
      <feSpecularLighting in="SourceGraphic">
        <feDistantLight azimuth="0" elevation="45" />
      </feSpecularLighting>
    </filter>
  </svg>
  "##;

  let filter =
    parse_svg_filter_from_svg_document(svg, Some("f"), &ImageCache::new()).expect("filter");

  match &filter.steps[0].primitive {
    FilterPrimitive::DiffuseLighting {
      surface_scale,
      diffuse_constant,
      kernel_unit_length,
      ..
    } => {
      assert_eq!(*surface_scale, 1.0);
      assert_eq!(*diffuse_constant, 1.0);
      assert_eq!(kernel_unit_length, &None);
    }
    other => panic!("unexpected first primitive {other:?}"),
  }

  match &filter.steps[1].primitive {
    FilterPrimitive::SpecularLighting {
      surface_scale,
      specular_constant,
      specular_exponent,
      kernel_unit_length,
      ..
    } => {
      assert_eq!(*surface_scale, 1.0);
      assert_eq!(*specular_constant, 1.0);
      assert_eq!(*specular_exponent, 1.0);
      assert_eq!(kernel_unit_length, &None);
    }
    other => panic!("unexpected second primitive {other:?}"),
  }
}

#[test]
fn diffuse_lighting_colors_flat_surface() {
  let mut pixmap = solid_pixmap(1, 1, PremultipliedColorU8::from_rgba(0, 0, 0, 255).unwrap());
  let bbox = Rect::from_xywh(0.0, 0.0, 1.0, 1.0);

  let filter = with_fingerprint(SvgFilter {
    color_interpolation_filters: ColorInterpolationFilters::LinearRGB,
    steps: vec![FilterStep {
      result: None,
      color_interpolation_filters: None,
      primitive: FilterPrimitive::DiffuseLighting {
        input: FilterInput::SourceAlpha,
        surface_scale: 1.0,
        diffuse_constant: 1.0,
        kernel_unit_length: None,
        light: LightSource::Distant {
          azimuth: 0.0,
          elevation: 90.0,
        },
        lighting_color: Rgba::RED,
      },
      region: None,
    }],
    region: SvgFilterRegion {
      x: SvgLength::Number(0.0),
      y: SvgLength::Number(0.0),
      width: SvgLength::Number(1.0),
      height: SvgLength::Number(1.0),
      units: SvgFilterUnits::UserSpaceOnUse,
    },
    filter_res: None,
    primitive_units: SvgFilterUnits::UserSpaceOnUse,
    fingerprint: 0,
  });

  apply_svg_filter(&filter, &mut pixmap, 1.0, bbox).unwrap();

  let px = pixmap.pixel(0, 0).unwrap();
  assert_eq!(
    (px.red(), px.green(), px.blue(), px.alpha()),
    (255, 0, 0, 255)
  );
}

#[test]
fn specular_lighting_colors_flat_surface() {
  let mut pixmap = solid_pixmap(1, 1, PremultipliedColorU8::from_rgba(0, 0, 0, 255).unwrap());
  let bbox = Rect::from_xywh(0.0, 0.0, 1.0, 1.0);

  let filter = with_fingerprint(SvgFilter {
    color_interpolation_filters: ColorInterpolationFilters::LinearRGB,
    steps: vec![FilterStep {
      result: None,
      color_interpolation_filters: None,
      primitive: FilterPrimitive::SpecularLighting {
        input: FilterInput::SourceAlpha,
        surface_scale: 0.0,
        specular_constant: 1.0,
        specular_exponent: 1.0,
        kernel_unit_length: None,
        light: LightSource::Distant {
          azimuth: 0.0,
          elevation: 90.0,
        },
        lighting_color: Rgba::BLUE,
      },
      region: None,
    }],
    region: SvgFilterRegion {
      x: SvgLength::Number(0.0),
      y: SvgLength::Number(0.0),
      width: SvgLength::Number(1.0),
      height: SvgLength::Number(1.0),
      units: SvgFilterUnits::UserSpaceOnUse,
    },
    filter_res: None,
    primitive_units: SvgFilterUnits::UserSpaceOnUse,
    fingerprint: 0,
  });

  apply_svg_filter(&filter, &mut pixmap, 1.0, bbox).unwrap();

  let px = pixmap.pixel(0, 0).unwrap();
  assert_eq!(
    (px.red(), px.green(), px.blue(), px.alpha()),
    (0, 0, 255, 255)
  );
}

fn render_diffuse(color_space: ColorInterpolationFilters) -> PremultipliedColorU8 {
  let mut pixmap = solid_pixmap(1, 1, PremultipliedColorU8::from_rgba(0, 0, 0, 255).unwrap());
  let bbox = Rect::from_xywh(0.0, 0.0, 1.0, 1.0);
  let filter = with_fingerprint(SvgFilter {
    color_interpolation_filters: color_space,
    steps: vec![FilterStep {
      result: None,
      color_interpolation_filters: None,
      primitive: FilterPrimitive::DiffuseLighting {
        input: FilterInput::SourceAlpha,
        surface_scale: 1.0,
        diffuse_constant: 0.5,
        kernel_unit_length: None,
        light: LightSource::Distant {
          azimuth: 0.0,
          elevation: 90.0,
        },
        lighting_color: Rgba::new(128, 128, 128, 1.0),
      },
      region: None,
    }],
    region: SvgFilterRegion {
      x: SvgLength::Number(0.0),
      y: SvgLength::Number(0.0),
      width: SvgLength::Number(1.0),
      height: SvgLength::Number(1.0),
      units: SvgFilterUnits::UserSpaceOnUse,
    },
    filter_res: None,
    primitive_units: SvgFilterUnits::UserSpaceOnUse,
    fingerprint: 0,
  });

  apply_svg_filter(&filter, &mut pixmap, 1.0, bbox).unwrap();
  pixmap.pixel(0, 0).unwrap().clone()
}

#[test]
fn lighting_respects_color_interpolation_filters() {
  let srgb = render_diffuse(ColorInterpolationFilters::SRGB);
  let linear = render_diffuse(ColorInterpolationFilters::LinearRGB);

  assert_ne!(srgb.red(), linear.red());
  assert!(
    linear.red() > srgb.red(),
    "linear result should be brighter due to linear scaling"
  );
}

fn opaque_bounds(pixmap: &Pixmap) -> Option<(u32, u32, u32, u32)> {
  let mut min_x = pixmap.width();
  let mut min_y = pixmap.height();
  let mut max_x = 0;
  let mut max_y = 0;
  let mut seen = false;
  for y in 0..pixmap.height() {
    for x in 0..pixmap.width() {
      if pixmap.pixel(x, y).unwrap().alpha() > 0 {
        seen = true;
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
      }
    }
  }
  if seen {
    Some((min_x, min_y, max_x - min_x + 1, max_y - min_y + 1))
  } else {
    None
  }
}

#[test]
fn userspace_percent_regions_resolve_against_bbox() {
  let mut pixmap = Pixmap::new(80, 40).expect("pixmap");
  let bbox = Rect::from_xywh(10.0, 10.0, 80.0, 40.0);
  let filter = with_fingerprint(SvgFilter {
    color_interpolation_filters: ColorInterpolationFilters::SRGB,
    steps: vec![FilterStep {
      result: None,
      color_interpolation_filters: None,
      primitive: FilterPrimitive::Flood {
        color: Rgba::GREEN,
        opacity: 1.0,
      },
      region: Some(SvgFilterPrimitiveRegionOverride {
        x: Some(SvgLength::Percent(0.25)),
        y: Some(SvgLength::Percent(0.25)),
        width: Some(SvgLength::Percent(0.5)),
        height: Some(SvgLength::Percent(0.5)),
        units: SvgFilterUnits::UserSpaceOnUse,
      }),
    }],
    region: SvgFilterRegion {
      x: SvgLength::Number(0.0),
      y: SvgLength::Number(0.0),
      width: SvgLength::Number(bbox.width()),
      height: SvgLength::Number(bbox.height()),
      units: SvgFilterUnits::UserSpaceOnUse,
    },
    filter_res: None,
    primitive_units: SvgFilterUnits::UserSpaceOnUse,
    fingerprint: 0,
  });

  apply_svg_filter(&filter, &mut pixmap, 1.0, bbox).unwrap();

  let bounds = opaque_bounds(&pixmap).expect("flood should produce opaque pixels");
  assert_eq!(bounds, (30, 20, 40, 20));
}

#[test]
fn point_light_percentages_follow_bbox_in_userspace() {
  let (surface_w, surface_h) = (8u32, 8u32);
  let (bbox_x, bbox_y) = (2u32, 3u32);
  let bbox = Rect::from_xywh(bbox_x as f32, bbox_y as f32, 4.0, 4.0);

  let mut pixmap = solid_pixmap(surface_w, surface_h, PremultipliedColorU8::TRANSPARENT);
  let opaque = PremultipliedColorU8::from_rgba(0, 0, 0, 255).unwrap();
  for y in bbox_y..bbox_y + 4 {
    for x in bbox_x..bbox_x + 4 {
      pixmap.pixels_mut()[y as usize * surface_w as usize + x as usize] = opaque;
    }
  }
  let filter = with_fingerprint(SvgFilter {
    color_interpolation_filters: ColorInterpolationFilters::SRGB,
    steps: vec![FilterStep {
      result: None,
      color_interpolation_filters: None,
      primitive: FilterPrimitive::DiffuseLighting {
        input: FilterInput::SourceAlpha,
        surface_scale: 0.0,
        diffuse_constant: 1.0,
        kernel_unit_length: None,
        light: LightSource::Point {
          x: SvgLength::Percent(0.0),
          y: SvgLength::Percent(0.0),
          z: SvgLength::Number(1.0),
        },
        lighting_color: Rgba::WHITE,
      },
      region: None,
    }],
    region: SvgFilterRegion {
      x: SvgLength::Number(0.0),
      y: SvgLength::Number(0.0),
      width: SvgLength::Number(4.0),
      height: SvgLength::Number(4.0),
      units: SvgFilterUnits::UserSpaceOnUse,
    },
    filter_res: None,
    primitive_units: SvgFilterUnits::UserSpaceOnUse,
    fingerprint: 0,
  });

  apply_svg_filter(&filter, &mut pixmap, 1.0, bbox).unwrap();

  assert_eq!(
    pixmap.pixel(0, 0).unwrap(),
    PremultipliedColorU8::TRANSPARENT,
    "expected pixels outside the resolved filter region to stay transparent"
  );
  let px = pixmap.pixel(bbox_x, bbox_y).unwrap();
  assert_eq!(
    [px.red(), px.green(), px.blue(), px.alpha()],
    [255, 255, 255, 255],
    "expected bbox-relative point light to fully illuminate the bbox corner"
  );
}

#[test]
fn kernel_unit_length_changes_lighting_output() {
  let width = 32;
  let height = 24;
  let bump_map = make_bump_map_pixmap(width, height);
  let bbox = Rect::from_xywh(0.0, 0.0, width as f32, height as f32);

  let make_filter = |kernel_unit_length| {
    with_fingerprint(SvgFilter {
      color_interpolation_filters: ColorInterpolationFilters::LinearRGB,
      steps: vec![FilterStep {
        result: None,
        color_interpolation_filters: None,
        primitive: FilterPrimitive::DiffuseLighting {
          input: FilterInput::SourceAlpha,
          surface_scale: 2.0,
          diffuse_constant: 1.0,
          kernel_unit_length,
          light: LightSource::Distant {
            azimuth: 0.0,
            elevation: 45.0,
          },
          lighting_color: Rgba::WHITE,
        },
        region: None,
      }],
      region: SvgFilterRegion {
        x: SvgLength::Number(0.0),
        y: SvgLength::Number(0.0),
        width: SvgLength::Number(width as f32),
        height: SvgLength::Number(height as f32),
        units: SvgFilterUnits::UserSpaceOnUse,
      },
      filter_res: None,
      primitive_units: SvgFilterUnits::UserSpaceOnUse,
      fingerprint: 0,
    })
  };

  let mut default = bump_map.clone();
  apply_svg_filter(&make_filter(None), &mut default, 1.0, bbox).unwrap();
  let mut widened = bump_map.clone();
  apply_svg_filter(&make_filter(Some((5.0, 3.0))), &mut widened, 1.0, bbox).unwrap();

  let changed = default
    .data()
    .iter()
    .zip(widened.data())
    .any(|(a, b)| a != b);
  assert!(
    changed,
    "expected kernelUnitLength to affect surface normals"
  );
}

#[test]
fn object_bounding_box_units_match_user_space_equivalent_for_point_light() {
  // Non-square bbox to ensure x/y primitiveUnits scaling stays axis-specific.
  let width = 40;
  let height = 20;
  let bump_map = make_bump_map_pixmap(width, height);
  let (bbox_x, bbox_y) = (7u32, 11u32);
  let bbox = Rect::from_xywh(bbox_x as f32, bbox_y as f32, width as f32, height as f32);

  // Use the same values for x/y and let the bbox shape make the resolved distances diverge.
  let kernel_unit_obj = (0.10, 0.10);
  let surface_scale_obj = 0.12;
  let point_obj = (0.25, 0.60, 0.40);

  let scalar_ref = (bbox.width().abs() + bbox.height().abs()) * 0.5;
  let surface_scale_user = surface_scale_obj * scalar_ref;
  let kernel_unit_user = (
    kernel_unit_obj.0 * bbox.width().abs(),
    kernel_unit_obj.1 * bbox.height().abs(),
  );
  // resvg/Chrome resolve point/spot light numeric coordinates in user space even when
  // `primitiveUnits="objectBoundingBox"`, so the userSpace equivalent should keep the same
  // x/y/z values for the light source.
  let point_user = point_obj;

  let filter_obj = with_fingerprint(SvgFilter {
    color_interpolation_filters: ColorInterpolationFilters::LinearRGB,
    steps: vec![FilterStep {
      result: None,
      color_interpolation_filters: None,
      primitive: FilterPrimitive::DiffuseLighting {
        input: FilterInput::SourceAlpha,
        surface_scale: surface_scale_obj,
        diffuse_constant: 1.0,
        kernel_unit_length: Some(kernel_unit_obj),
        light: LightSource::Point {
          x: SvgLength::Number(point_obj.0),
          y: SvgLength::Number(point_obj.1),
          z: SvgLength::Number(point_obj.2),
        },
        lighting_color: Rgba::WHITE,
      },
      region: None,
    }],
    region: SvgFilterRegion {
      x: SvgLength::Number(0.0),
      y: SvgLength::Number(0.0),
      width: SvgLength::Number(width as f32),
      height: SvgLength::Number(height as f32),
      units: SvgFilterUnits::UserSpaceOnUse,
    },
    filter_res: None,
    primitive_units: SvgFilterUnits::ObjectBoundingBox,
    fingerprint: 0,
  });

  let filter_user = with_fingerprint(SvgFilter {
    color_interpolation_filters: ColorInterpolationFilters::LinearRGB,
    steps: vec![FilterStep {
      result: None,
      color_interpolation_filters: None,
      primitive: FilterPrimitive::DiffuseLighting {
        input: FilterInput::SourceAlpha,
        surface_scale: surface_scale_user,
        diffuse_constant: 1.0,
        kernel_unit_length: Some(kernel_unit_user),
        light: LightSource::Point {
          x: SvgLength::Number(point_user.0),
          y: SvgLength::Number(point_user.1),
          z: SvgLength::Number(point_user.2),
        },
        lighting_color: Rgba::WHITE,
      },
      region: None,
    }],
    region: SvgFilterRegion {
      x: SvgLength::Number(0.0),
      y: SvgLength::Number(0.0),
      width: SvgLength::Number(width as f32),
      height: SvgLength::Number(height as f32),
      units: SvgFilterUnits::UserSpaceOnUse,
    },
    filter_res: None,
    primitive_units: SvgFilterUnits::UserSpaceOnUse,
    fingerprint: 0,
  });

  let surface_w = bbox_x + width;
  let surface_h = bbox_y + height;
  let mut surface = Pixmap::new(surface_w, surface_h).expect("pixmap");
  surface.pixels_mut().fill(PremultipliedColorU8::TRANSPARENT);
  for y in 0..height {
    for x in 0..width {
      let src_idx = y as usize * width as usize + x as usize;
      let dst_idx = (y + bbox_y) as usize * surface_w as usize + (x + bbox_x) as usize;
      surface.pixels_mut()[dst_idx] = bump_map.pixels()[src_idx];
    }
  }

  let mut out_obj = surface.clone();
  apply_svg_filter(&filter_obj, &mut out_obj, 1.0, bbox).unwrap();
  let mut out_user = surface;
  apply_svg_filter(&filter_user, &mut out_user, 1.0, bbox).unwrap();

  assert_eq!(
    out_obj.data(),
    out_user.data(),
    "primitiveUnits=objectBoundingBox should match the userSpaceOnUse equivalent after resolving primitives"
  );
}

#[test]
fn diffuse_lighting_regression_premultiply_and_normal_sign() {
  let mut pixmap = Pixmap::new(3, 1).expect("pixmap");
  let alphas = [0u8, 255u8, 255u8];
  for (x, &a) in alphas.iter().enumerate() {
    pixmap.pixels_mut()[x] = PremultipliedColorU8::from_rgba(0, 0, 0, a).unwrap();
  }
  let bump_map = pixmap.clone();
  let bbox = Rect::from_xywh(0.0, 0.0, 3.0, 1.0);
  let filter = with_fingerprint(SvgFilter {
    color_interpolation_filters: ColorInterpolationFilters::SRGB,
    steps: vec![FilterStep {
      result: None,
      color_interpolation_filters: None,
      primitive: FilterPrimitive::DiffuseLighting {
        input: FilterInput::SourceAlpha,
        surface_scale: 1.0,
        diffuse_constant: 1.0,
        kernel_unit_length: None,
        light: LightSource::Distant {
          azimuth: 0.0,
          elevation: 45.0,
        },
        lighting_color: Rgba::WHITE,
      },
      region: None,
    }],
    region: SvgFilterRegion {
      x: SvgLength::Number(0.0),
      y: SvgLength::Number(0.0),
      width: SvgLength::Number(3.0),
      height: SvgLength::Number(1.0),
      units: SvgFilterUnits::UserSpaceOnUse,
    },
    filter_res: None,
    primitive_units: SvgFilterUnits::UserSpaceOnUse,
    fingerprint: 0,
  });

  let bump_map_url = pixmap_to_data_url_png(&bump_map);
  let svg_bump_map = format!(
    r#"
    <svg xmlns="http://www.w3.org/2000/svg" width="3" height="1">
      <image x="0" y="0" width="3" height="1" preserveAspectRatio="none" href="{bump_map_url}" />
    </svg>
    "#
  );
  let bump_map_center = center_pixel(&render_resvg(&svg_bump_map, 3, 1));
  assert_eq!(
    bump_map_center.3, 255,
    "expected resvg to load the embedded bump map image"
  );

  let svg = format!(
    r#"
    <svg xmlns="http://www.w3.org/2000/svg" width="3" height="1">
      <defs>
        <filter id="f" x="0" y="0" width="3" height="1"
                filterUnits="userSpaceOnUse" primitiveUnits="userSpaceOnUse"
                color-interpolation-filters="sRGB">
          <feDiffuseLighting in="SourceAlpha" surfaceScale="1" diffuseConstant="1" lighting-color="white">
            <feDistantLight azimuth="0" elevation="45" />
          </feDiffuseLighting>
        </filter>
      </defs>
      <image x="0" y="0" width="3" height="1" preserveAspectRatio="none"
             href="{bump_map_url}" filter="url(#f)" />
    </svg>
    "#
  );
  let expected = center_pixel(&render_resvg(&svg, 3, 1));

  apply_svg_filter(&filter, &mut pixmap, 1.0, bbox).unwrap();
  let actual = center_pixel(&pixmap);

  assert_eq!(
    actual, expected,
    "FastRender lighting must match resvg output"
  );
}

fn render_resvg(svg: &str, width: u32, height: u32) -> Pixmap {
  use resvg::usvg;

  let options = usvg::Options::default();
  let tree = usvg::Tree::from_str(svg, &options).expect("parse SVG with resvg");
  let mut pixmap = Pixmap::new(width, height).expect("pixmap");
  resvg::render(&tree, Transform::identity(), &mut pixmap.as_mut());
  pixmap
}

fn center_pixel(pixmap: &Pixmap) -> (u8, u8, u8, u8) {
  let x = pixmap.width() / 2;
  let y = pixmap.height() / 2;
  let px = pixmap.pixel(x, y).expect("center pixel");
  (px.red(), px.green(), px.blue(), px.alpha())
}

#[test]
fn lighting_output_alpha_matches_resvg_for_intensity() {
  // Flat surface + distant light straight on => N·L = 1, so intensity is purely the constant.
  // This isolates the question of whether the lighting output alpha encodes intensity.
  let svg = r#"
    <svg xmlns="http://www.w3.org/2000/svg" width="10" height="10" viewBox="0 0 10 10">
      <filter id="f" x="0" y="0" width="10" height="10"
              filterUnits="userSpaceOnUse" primitiveUnits="userSpaceOnUse"
              color-interpolation-filters="sRGB">
        <feDiffuseLighting in="SourceAlpha" surfaceScale="0" diffuseConstant="0.5" lighting-color="white">
          <feDistantLight azimuth="0" elevation="90" />
        </feDiffuseLighting>
      </filter>
      <rect width="10" height="10" fill="white" filter="url(#f)" />
    </svg>
  "#;

  let expected = center_pixel(&render_resvg(svg, 10, 10));

  let filter =
    parse_svg_filter_from_svg_document(svg, Some("f"), &ImageCache::new()).expect("filter");
  let mut pixmap = solid_pixmap(
    10,
    10,
    PremultipliedColorU8::from_rgba(255, 255, 255, 255).unwrap(),
  );
  let bbox = Rect::from_xywh(0.0, 0.0, 10.0, 10.0);
  apply_svg_filter(&filter, &mut pixmap, 1.0, bbox).unwrap();
  let actual = center_pixel(&pixmap);

  assert_eq!(
    actual, expected,
    "FastRender lighting must match resvg output"
  );
}

#[test]
fn lighting_transparent_input_matches_resvg() {
  // If the bump map is fully transparent (alpha=0 everywhere), engines disagree on whether the
  // lighting primitive should still emit a flat lit surface. Lock the behavior to resvg/Chrome.
  let svg = r#"
    <svg xmlns="http://www.w3.org/2000/svg" width="10" height="10" viewBox="0 0 10 10">
      <filter id="f" x="0" y="0" width="10" height="10"
              filterUnits="userSpaceOnUse" primitiveUnits="userSpaceOnUse"
              color-interpolation-filters="sRGB">
        <feDiffuseLighting in="SourceAlpha" surfaceScale="0" diffuseConstant="0.5" lighting-color="white">
          <feDistantLight azimuth="0" elevation="90" />
        </feDiffuseLighting>
      </filter>
      <rect width="10" height="10" fill="white" fill-opacity="0" filter="url(#f)" />
    </svg>
  "#;

  let expected = center_pixel(&render_resvg(svg, 10, 10));

  let filter =
    parse_svg_filter_from_svg_document(svg, Some("f"), &ImageCache::new()).expect("filter");
  let mut pixmap = solid_pixmap(10, 10, PremultipliedColorU8::TRANSPARENT);
  let bbox = Rect::from_xywh(0.0, 0.0, 10.0, 10.0);
  apply_svg_filter(&filter, &mut pixmap, 1.0, bbox).unwrap();
  let actual = center_pixel(&pixmap);

  assert_eq!(
    actual, expected,
    "FastRender lighting must match resvg output"
  );
}

#[test]
fn specular_lighting_output_alpha_matches_resvg_for_intensity() {
  // Flat surface + distant light straight on => specular angle is 1.0, so intensity is purely the
  // constant. This isolates the question of whether the lighting output alpha encodes intensity.
  let svg = r#"
    <svg xmlns="http://www.w3.org/2000/svg" width="10" height="10" viewBox="0 0 10 10">
      <filter id="f" x="0" y="0" width="10" height="10"
              filterUnits="userSpaceOnUse" primitiveUnits="userSpaceOnUse"
              color-interpolation-filters="sRGB">
        <feSpecularLighting in="SourceAlpha" surfaceScale="0" specularConstant="0.5" specularExponent="1" lighting-color="white">
          <feDistantLight azimuth="0" elevation="90" />
        </feSpecularLighting>
      </filter>
      <rect width="10" height="10" fill="white" filter="url(#f)" />
    </svg>
  "#;

  let expected = center_pixel(&render_resvg(svg, 10, 10));

  let filter =
    parse_svg_filter_from_svg_document(svg, Some("f"), &ImageCache::new()).expect("filter");
  let mut pixmap = solid_pixmap(
    10,
    10,
    PremultipliedColorU8::from_rgba(255, 255, 255, 255).unwrap(),
  );
  let bbox = Rect::from_xywh(0.0, 0.0, 10.0, 10.0);
  apply_svg_filter(&filter, &mut pixmap, 1.0, bbox).unwrap();
  let actual = center_pixel(&pixmap);

  assert_eq!(
    actual, expected,
    "FastRender lighting must match resvg output"
  );
}

#[test]
fn specular_lighting_transparent_input_matches_resvg() {
  // If the bump map is fully transparent (alpha=0 everywhere), engines disagree on whether the
  // lighting primitive should still emit a flat lit surface. Lock the behavior to resvg/Chrome.
  let svg = r#"
    <svg xmlns="http://www.w3.org/2000/svg" width="10" height="10" viewBox="0 0 10 10">
      <filter id="f" x="0" y="0" width="10" height="10"
              filterUnits="userSpaceOnUse" primitiveUnits="userSpaceOnUse"
              color-interpolation-filters="sRGB">
        <feSpecularLighting in="SourceAlpha" surfaceScale="0" specularConstant="0.5" specularExponent="1" lighting-color="white">
          <feDistantLight azimuth="0" elevation="90" />
        </feSpecularLighting>
      </filter>
      <rect width="10" height="10" fill="white" fill-opacity="0" filter="url(#f)" />
    </svg>
  "#;

  let expected = center_pixel(&render_resvg(svg, 10, 10));

  let filter =
    parse_svg_filter_from_svg_document(svg, Some("f"), &ImageCache::new()).expect("filter");
  let mut pixmap = solid_pixmap(10, 10, PremultipliedColorU8::TRANSPARENT);
  let bbox = Rect::from_xywh(0.0, 0.0, 10.0, 10.0);
  apply_svg_filter(&filter, &mut pixmap, 1.0, bbox).unwrap();
  let actual = center_pixel(&pixmap);

  assert_eq!(
    actual, expected,
    "FastRender lighting must match resvg output"
  );
}

#[test]
fn point_light_numbers_follow_filter_userspace_when_bbox_is_offset() {
  let svg = r##"
    <svg xmlns="http://www.w3.org/2000/svg" width="128" height="96">
      <defs>
        <filter id="f" filterUnits="userSpaceOnUse" primitiveUnits="userSpaceOnUse"
                x="0" y="0" width="128" height="96">
          <feDiffuseLighting in="SourceAlpha" surfaceScale="0" diffuseConstant="1" result="lit">
            <fePointLight x="50" y="35" z="60" />
          </feDiffuseLighting>
          <feComposite in="lit" in2="SourceAlpha" operator="in" />
        </filter>
      </defs>
      <rect x="30" y="20" width="40" height="30" fill="white" filter="url(#f)" />
    </svg>
  "##;

  let expected = render_resvg(svg, 128, 96);

  let filter =
    parse_svg_filter_from_svg_document(svg, Some("f"), &ImageCache::new()).expect("filter");
  let mut pixmap = Pixmap::new(128, 96).expect("pixmap");
  let fill = PremultipliedColorU8::from_rgba(255, 255, 255, 255).unwrap();
  let width = pixmap.width();
  {
    let pixels = pixmap.pixels_mut();
    for y in 20..50 {
      for x in 30..70 {
        pixels[(y * width + x) as usize] = fill;
      }
    }
  }
  let bbox = Rect::from_xywh(30.0, 20.0, 40.0, 30.0);
  apply_svg_filter(filter.as_ref(), &mut pixmap, 1.0, bbox).unwrap();

  let rgba_at = |pixmap: &Pixmap, x: u32, y: u32| {
    let px = pixmap.pixel(x, y).expect("pixel in bounds");
    (px.red(), px.green(), px.blue(), px.alpha())
  };
  let assert_close = |label: &str, actual: (u8, u8, u8, u8), expected: (u8, u8, u8, u8)| {
    let tolerance = 3u8;
    for (chan, a, e) in [
      ("r", actual.0, expected.0),
      ("g", actual.1, expected.1),
      ("b", actual.2, expected.2),
      ("a", actual.3, expected.3),
    ] {
      let diff = a.abs_diff(e);
      assert!(
        diff <= tolerance,
        "{label} {chan} differs too much: got {a} expected {e} (diff {diff} > {tolerance}); actual={actual:?} expected={expected:?}"
      );
    }
  };

  for (label, x, y) in [("center", 50, 35), ("corner", 30, 20), ("outside", 0, 0)] {
    assert_close(label, rgba_at(&pixmap, x, y), rgba_at(&expected, x, y));
  }
}
