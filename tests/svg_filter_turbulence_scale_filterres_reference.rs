use fastrender::geometry::Rect;
use fastrender::paint::svg_filter::{
  apply_svg_filter, ColorInterpolationFilters, FilterPrimitive, FilterStep, SvgFilter,
  SvgFilterRegion, SvgFilterUnits, SvgLength, TurbulenceType,
};
use tiny_skia::{Color, FilterQuality, Pixmap, PixmapPaint, Transform};

fn render_resvg(svg: &str, width: u32, height: u32) -> Pixmap {
  let mut options = resvg::usvg::Options::default();
  options.resources_dir = None;
  let tree = resvg::usvg::Tree::from_str(svg, &options).expect("parse svg with resvg");
  let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height).expect("allocate resvg pixmap");
  resvg::render(
    &tree,
    resvg::tiny_skia::Transform::default(),
    &mut pixmap.as_mut(),
  );
  pixmap
}

fn clip_pixmap_to_region(pixmap: &mut Pixmap, region: (u32, u32, u32, u32)) {
  let (rx, ry, rw, rh) = region;
  let width = pixmap.width() as usize;
  let height = pixmap.height() as usize;
  let row_stride = width * 4;
  let x0 = rx as usize;
  let y0 = ry as usize;
  let x1 = (rx + rw) as usize;
  let y1 = (ry + rh) as usize;

  for y in 0..height {
    let row = &mut pixmap.data_mut()[y * row_stride..(y + 1) * row_stride];
    if y < y0 || y >= y1 {
      row.fill(0);
      continue;
    }
    row[..x0.min(width) * 4].fill(0);
    row[x1.min(width) * 4..].fill(0);
  }
}

fn render_fastrender(
  filter: &SvgFilter,
  width: u32,
  height: u32,
  scale: f32,
  bbox: Rect,
) -> Pixmap {
  let mut pixmap = Pixmap::new(width, height).expect("allocate fastrender pixmap");
  pixmap.fill(Color::WHITE);
  apply_svg_filter(filter, &mut pixmap, scale, bbox).expect("apply_svg_filter");
  pixmap
}

fn turbulence_filter(
  region: (f32, f32, f32, f32),
  filter_res: Option<(u32, u32)>,
  color_interpolation_filters: ColorInterpolationFilters,
  base_frequency: (f32, f32),
  seed: i32,
  octaves: u32,
) -> SvgFilter {
  let mut filter = SvgFilter {
    color_interpolation_filters,
    steps: vec![FilterStep {
      result: None,
      color_interpolation_filters: None,
      primitive: FilterPrimitive::Turbulence {
        base_frequency,
        seed,
        octaves,
        stitch_tiles: false,
        kind: TurbulenceType::Turbulence,
      },
      region: None,
    }],
    region: SvgFilterRegion {
      x: SvgLength::Number(region.0),
      y: SvgLength::Number(region.1),
      width: SvgLength::Number(region.2),
      height: SvgLength::Number(region.3),
      units: SvgFilterUnits::UserSpaceOnUse,
    },
    filter_res,
    primitive_units: SvgFilterUnits::UserSpaceOnUse,
    fingerprint: 0,
  };
  filter.refresh_fingerprint();
  filter
}

fn assert_pixmaps_match(
  test_case: &str,
  expected: &Pixmap,
  actual: &Pixmap,
  tolerance: u8,
  params: &str,
) {
  assert_eq!(
    (expected.width(), expected.height()),
    (actual.width(), actual.height()),
    "{test_case}: pixmap dimensions differ (expected {}x{} got {}x{})",
    expected.width(),
    expected.height(),
    actual.width(),
    actual.height()
  );

  if expected.data() == actual.data() {
    return;
  }

  let width = expected.width() as usize;
  let mut max_delta = 0u8;
  let mut max_at = (0u32, 0u32, 'r', 0u8, 0u8);

  for (idx, (&expected_byte, &actual_byte)) in
    expected.data().iter().zip(actual.data().iter()).enumerate()
  {
    let delta = expected_byte.abs_diff(actual_byte);
    if delta <= max_delta {
      continue;
    }

    max_delta = delta;
    let px = idx / 4;
    let channel = match idx % 4 {
      0 => 'r',
      1 => 'g',
      2 => 'b',
      3 => 'a',
      _ => unreachable!(),
    };
    max_at = (
      px as u32 % width as u32,
      px as u32 / width as u32,
      channel,
      expected_byte,
      actual_byte,
    );
  }

  if max_delta <= tolerance {
    return;
  }

  panic!(
    "{test_case}: resvg vs fastrender mismatch (max channel Δ={max_delta} > {tolerance} at ({},{}) channel {} (resvg={} fastrender={}))\nparams: {params}",
    max_at.0, max_at.1, max_at.2, max_at.3, max_at.4
  );
}

fn turbulence_svg(
  width: u32,
  height: u32,
  view_box: (u32, u32, u32, u32),
  filter_region: (u32, u32, u32, u32),
  filter_res: Option<(u32, u32)>,
  color_interpolation_filters: ColorInterpolationFilters,
  base_frequency: (f32, f32),
  seed: f32,
  octaves: u32,
) -> String {
  let (vb_x, vb_y, vb_w, vb_h) = view_box;
  let (fx, fy, fw, fh) = filter_region;
  let filter_res_attr = filter_res
    .map(|(rw, rh)| format!(r#" filterRes="{rw} {rh}""#))
    .unwrap_or_default();
  let color_interpolation_attr = match color_interpolation_filters {
    ColorInterpolationFilters::LinearRGB => "linearRGB",
    ColorInterpolationFilters::SRGB => "sRGB",
  };
  format!(
    r#"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="{vb_x} {vb_y} {vb_w} {vb_h}" shape-rendering="crispEdges">
  <defs>
    <filter id="f" filterUnits="userSpaceOnUse" primitiveUnits="userSpaceOnUse"
      x="{fx}" y="{fy}" width="{fw}" height="{fh}" color-interpolation-filters="{color_interpolation_attr}"{filter_res_attr}>
      <feTurbulence type="turbulence" baseFrequency="{bfx} {bfy}" seed="{seed}" numOctaves="{octaves}" />
    </filter>
  </defs>
  <rect x="{vb_x}" y="{vb_y}" width="{vb_w}" height="{vb_h}" fill="white" filter="url(#f)"/>
</svg>"#,
    bfx = base_frequency.0,
    bfy = base_frequency.1,
  )
}

#[test]
fn fe_turbulence_dpr_scale_mapping_user_space_on_use_matches_resvg() {
  let css_size = 32u32;
  let dpr = 2.0;
  let device_size = css_size * dpr as u32;
  let base_frequency = (0.12, 0.08);
  let seed_attr = -3.6;
  let seed = seed_attr as i32;
  let octaves = 2;

  let svg = turbulence_svg(
    device_size,
    device_size,
    (0, 0, css_size, css_size),
    (0, 0, css_size, css_size),
    None,
    ColorInterpolationFilters::LinearRGB,
    base_frequency,
    seed_attr,
    octaves,
  );
  let expected = render_resvg(&svg, device_size, device_size);

  let filter = turbulence_filter(
    (0.0, 0.0, css_size as f32, css_size as f32),
    None,
    ColorInterpolationFilters::LinearRGB,
    base_frequency,
    seed,
    octaves,
  );
  let bbox = Rect::from_xywh(0.0, 0.0, device_size as f32, device_size as f32);
  let actual = render_fastrender(&filter, device_size, device_size, dpr, bbox);

  let params = format!(
    "scale={dpr} bbox=device({device_size}x{device_size}) css_bbox=({css_size}x{css_size}) baseFrequency={:?} seed={seed_attr} numOctaves={octaves}",
    base_frequency
  );
  assert_pixmaps_match("dpr/scale mapping", &expected, &actual, 1, &params);
}

#[test]
fn fe_turbulence_filter_res_changes_sampling_matches_resvg() {
  let size = 64u32;
  let filter_res = (32u32, 32u32);
  let base_frequency = (0.21, 0.13);
  let seed_attr = 5.0;
  let seed = seed_attr as i32;
  let octaves = 2;

  // resvg ignores SVG 1.1 `filterRes` (Chrome-aligned SVG2 behavior). To still regression-test
  // FastRender's `filterRes` resampling + scale mapping, render the same filter at the requested
  // filterRes resolution in resvg and then bilinear-upsample into the final surface.
  //
  // Note: we run this case in `color-interpolation-filters="sRGB"` so our manual bilinear upscale
  // matches FastRender's filterRes resample path without needing to duplicate its LinearRGB gamma
  // conversions. The coordinate mapping (scale/region origin) is identical either way.
  let working_svg = turbulence_svg(
    filter_res.0,
    filter_res.1,
    (0, 0, size, size),
    (0, 0, size, size),
    None,
    ColorInterpolationFilters::SRGB,
    base_frequency,
    seed_attr,
    octaves,
  );
  let working = render_resvg(&working_svg, filter_res.0, filter_res.1);

  let mut expected = Pixmap::new(size, size).expect("allocate expected pixmap");
  let mut paint = PixmapPaint::default();
  paint.quality = FilterQuality::Bilinear;
  let transform = Transform::from_row(
    size as f32 / filter_res.0 as f32,
    0.0,
    0.0,
    size as f32 / filter_res.1 as f32,
    0.0,
    0.0,
  );
  expected.draw_pixmap(0, 0, working.as_ref(), &paint, transform, None);
  clip_pixmap_to_region(&mut expected, (0, 0, size, size));

  let filter = turbulence_filter(
    (0.0, 0.0, size as f32, size as f32),
    Some(filter_res),
    ColorInterpolationFilters::SRGB,
    base_frequency,
    seed,
    octaves,
  );
  let bbox = Rect::from_xywh(0.0, 0.0, size as f32, size as f32);
  let actual = render_fastrender(&filter, size, size, 1.0, bbox);

  let params = format!(
    "filterRes={filter_res:?} region=0,0,{size},{size} baseFrequency={:?} seed={seed_attr} numOctaves={octaves} (note: resvg ignores filterRes; baseline renders at filterRes then bilinear-upscales)",
    base_frequency
  );
  assert_pixmaps_match("filterRes sampling", &expected, &actual, 1, &params);
}

#[test]
fn fe_turbulence_filter_res_with_offset_region_matches_resvg() {
  let size = 64u32;
  let filter_res = (32u32, 32u32);
  let region = (10u32, 7u32, 40u32, 40u32);
  let base_frequency = (0.19, 0.11);
  let seed_attr = 3.0;
  let seed = seed_attr as i32;
  let octaves = 2;

  // resvg ignores SVG 1.1 `filterRes`; simulate FastRender's filterRes behaviour by rendering the
  // turbulence at filterRes resolution over the offset filter region, then scaling and translating
  // it back into the destination surface.
  //
  // As in the non-offset case, use `sRGB` so our manual upscale matches FastRender's resample path
  // without reimplementing its LinearRGB conversions.
  let working_svg = turbulence_svg(
    filter_res.0,
    filter_res.1,
    (region.0, region.1, region.2, region.3),
    region,
    None,
    ColorInterpolationFilters::SRGB,
    base_frequency,
    seed_attr,
    octaves,
  );
  let working = render_resvg(&working_svg, filter_res.0, filter_res.1);
  let mut expected = Pixmap::new(size, size).expect("allocate expected pixmap");
  let mut paint = PixmapPaint::default();
  paint.quality = FilterQuality::Bilinear;
  let transform = Transform::from_row(
    region.2 as f32 / filter_res.0 as f32,
    0.0,
    0.0,
    region.3 as f32 / filter_res.1 as f32,
    region.0 as f32,
    region.1 as f32,
  );
  expected.draw_pixmap(0, 0, working.as_ref(), &paint, transform, None);
  clip_pixmap_to_region(&mut expected, region);

  let filter = turbulence_filter(
    (
      region.0 as f32,
      region.1 as f32,
      region.2 as f32,
      region.3 as f32,
    ),
    Some(filter_res),
    ColorInterpolationFilters::SRGB,
    base_frequency,
    seed,
    octaves,
  );
  let bbox = Rect::from_xywh(0.0, 0.0, size as f32, size as f32);
  let actual = render_fastrender(&filter, size, size, 1.0, bbox);

  let params = format!(
    "filterRes={filter_res:?} region={:?} baseFrequency={:?} seed={seed_attr} numOctaves={octaves} (note: resvg ignores filterRes; baseline renders at filterRes then bilinear-upscales)",
    region, base_frequency
  );
  assert_pixmaps_match("filterRes + offset region", &expected, &actual, 1, &params);
}
