use fastrender::geometry::Rect;
use fastrender::paint::svg_filter::{
  apply_svg_filter, ChannelSelector, ColorInterpolationFilters, FilterInput, FilterPrimitive,
  FilterStep, SvgFilter, SvgFilterRegion, SvgFilterUnits, SvgLength, TurbulenceType,
};
use tiny_skia::{Pixmap, PremultipliedColorU8};

fn turbulence_filter(primitive: FilterPrimitive) -> SvgFilter {
  let mut filter = SvgFilter {
    color_interpolation_filters: ColorInterpolationFilters::LinearRGB,
    steps: vec![FilterStep {
      result: None,
      color_interpolation_filters: None,
      primitive,
      region: None,
    }],
    region: SvgFilterRegion {
      x: SvgLength::Percent(0.0),
      y: SvgLength::Percent(0.0),
      width: SvgLength::Percent(1.0),
      height: SvgLength::Percent(1.0),
      units: SvgFilterUnits::ObjectBoundingBox,
    },
    filter_res: None,
    primitive_units: SvgFilterUnits::ObjectBoundingBox,
    fingerprint: 0,
  };
  filter.refresh_fingerprint();
  filter
}

fn turbulence_filter_with_options(
  primitive: FilterPrimitive,
  color_interpolation_filters: ColorInterpolationFilters,
  primitive_units: SvgFilterUnits,
) -> SvgFilter {
  let mut filter = SvgFilter {
    color_interpolation_filters,
    steps: vec![FilterStep {
      result: None,
      color_interpolation_filters: None,
      primitive,
      region: None,
    }],
    region: SvgFilterRegion {
      x: SvgLength::Percent(0.0),
      y: SvgLength::Percent(0.0),
      width: SvgLength::Percent(1.0),
      height: SvgLength::Percent(1.0),
      units: SvgFilterUnits::ObjectBoundingBox,
    },
    filter_res: None,
    primitive_units,
    fingerprint: 0,
  };
  filter.refresh_fingerprint();
  filter
}

fn turbulence_displacement_filter(scale: f32) -> SvgFilter {
  let mut filter = SvgFilter {
    color_interpolation_filters: ColorInterpolationFilters::LinearRGB,
    steps: vec![
      FilterStep {
        result: Some("noise".to_string()),
        color_interpolation_filters: None,
        primitive: FilterPrimitive::Turbulence {
          base_frequency: (0.0, 0.0),
          seed: 0,
          octaves: 1,
          stitch_tiles: false,
          kind: TurbulenceType::FractalNoise,
        },
        region: None,
      },
      FilterStep {
        result: None,
        color_interpolation_filters: None,
        primitive: FilterPrimitive::DisplacementMap {
          in1: FilterInput::SourceGraphic,
          in2: FilterInput::Reference("noise".to_string()),
          scale,
          x_channel: ChannelSelector::R,
          y_channel: ChannelSelector::G,
        },
        region: None,
      },
    ],
    region: SvgFilterRegion {
      x: SvgLength::Percent(0.0),
      y: SvgLength::Percent(0.0),
      width: SvgLength::Percent(1.0),
      height: SvgLength::Percent(1.0),
      units: SvgFilterUnits::ObjectBoundingBox,
    },
    filter_res: None,
    primitive_units: SvgFilterUnits::UserSpaceOnUse,
    fingerprint: 0,
  };
  filter.refresh_fingerprint();
  filter
}

fn apply_filter(filter: &SvgFilter, pixmap: &mut Pixmap) {
  let bbox = Rect::from_xywh(0.0, 0.0, pixmap.width() as f32, pixmap.height() as f32);
  apply_svg_filter(filter, pixmap, 1.0, bbox).unwrap();
}

#[test]
fn turbulence_is_deterministic() {
  let filter = turbulence_filter(FilterPrimitive::Turbulence {
    base_frequency: (0.15, 0.2),
    seed: 42,
    octaves: 3,
    stitch_tiles: false,
    kind: TurbulenceType::FractalNoise,
  });

  let mut first = Pixmap::new(32, 24).unwrap();
  let mut second = Pixmap::new(32, 24).unwrap();
  apply_filter(&filter, &mut first);
  apply_filter(&filter, &mut second);

  assert_eq!(first.data(), second.data());
}

#[test]
fn turbulence_seed_changes_output() {
  let filter_a = turbulence_filter(FilterPrimitive::Turbulence {
    base_frequency: (0.1, 0.12),
    seed: 1,
    octaves: 2,
    stitch_tiles: false,
    kind: TurbulenceType::Turbulence,
  });
  let filter_b = turbulence_filter(FilterPrimitive::Turbulence {
    base_frequency: (0.1, 0.12),
    seed: 99,
    octaves: 2,
    stitch_tiles: false,
    kind: TurbulenceType::Turbulence,
  });

  let mut first = Pixmap::new(24, 24).unwrap();
  let mut second = Pixmap::new(24, 24).unwrap();
  apply_filter(&filter_a, &mut first);
  apply_filter(&filter_b, &mut second);

  assert_ne!(first.data(), second.data());
}

#[test]
fn turbulence_stitches_edges() {
  const W: u32 = 32;
  const H: u32 = 32;

  let turbulence_filter = |stitch_tiles: bool| {
    let mut filter = SvgFilter {
      color_interpolation_filters: ColorInterpolationFilters::LinearRGB,
      steps: vec![FilterStep {
        result: None,
        color_interpolation_filters: None,
        primitive: FilterPrimitive::Turbulence {
          base_frequency: (0.08, 0.1),
          seed: 7,
          octaves: 2,
          stitch_tiles,
          kind: TurbulenceType::Turbulence,
        },
        region: None,
      }],
      region: SvgFilterRegion {
        x: SvgLength::Number(0.0),
        y: SvgLength::Number(0.0),
        width: SvgLength::Number(W as f32),
        height: SvgLength::Number(H as f32),
        units: SvgFilterUnits::UserSpaceOnUse,
      },
      filter_res: None,
      primitive_units: SvgFilterUnits::UserSpaceOnUse,
      fingerprint: 0,
    };
    filter.refresh_fingerprint();
    filter
  };

  let max_delta = |a: PremultipliedColorU8, b: PremultipliedColorU8| -> u8 {
    [
      a.red().abs_diff(b.red()),
      a.green().abs_diff(b.green()),
      a.blue().abs_diff(b.blue()),
      a.alpha().abs_diff(b.alpha()),
    ]
    .into_iter()
    .max()
    .unwrap_or(0)
  };

  let mut stitched = Pixmap::new(W, H).unwrap();
  apply_filter(&turbulence_filter(true), &mut stitched);

  let stitched_pixels = stitched.pixels();
  let width = stitched.width() as usize;
  let height = stitched.height() as usize;

  let mut max_lr = 0u8;
  let mut max_lr_at = (0usize, 0usize);
  for y in 0..height {
    let a = stitched_pixels[y * width];
    let b = stitched_pixels[y * width + (width - 1)];
    let delta = max_delta(a, b);
    if delta > max_lr {
      max_lr = delta;
      max_lr_at = (0, y);
    }
  }

  let mut max_tb = 0u8;
  let mut max_tb_at = (0usize, 0usize);
  for x in 0..width {
    let a = stitched_pixels[x];
    let b = stitched_pixels[(height - 1) * width + x];
    let delta = max_delta(a, b);
    if delta > max_tb {
      max_tb = delta;
      max_tb_at = (x, 0);
    }
  }

  // `stitchTiles="stitch"` is intended to make the turbulence output tile seamlessly. The sampled
  // bytes are not necessarily identical between first/last pixels, but large discontinuities at the
  // edges usually indicate that stitchTiles was ignored.
  const MAX_EDGE_DELTA: u8 = 160;
  assert!(
    max_lr <= MAX_EDGE_DELTA,
    "unexpectedly large left/right edge delta for stitchTiles=\"stitch\": max Δ={max_lr} at ({},{})",
    max_lr_at.0,
    max_lr_at.1
  );
  assert!(
    max_tb <= MAX_EDGE_DELTA,
    "unexpectedly large top/bottom edge delta for stitchTiles=\"stitch\": max Δ={max_tb} at ({},{})",
    max_tb_at.0,
    max_tb_at.1
  );

  // Ensure the test is meaningful: stitch_tiles should affect the output for non-trivial
  // parameters.
  let mut no_stitch = Pixmap::new(W, H).unwrap();
  apply_filter(&turbulence_filter(false), &mut no_stitch);
  assert_ne!(
    stitched.data(),
    no_stitch.data(),
    "expected stitchTiles to affect the turbulence output"
  );
}

#[test]
fn turbulence_output_is_rgba_noise() {
  let filter = turbulence_filter(FilterPrimitive::Turbulence {
    base_frequency: (0.1, 0.12),
    seed: 3,
    octaves: 2,
    stitch_tiles: false,
    kind: TurbulenceType::Turbulence,
  });

  let mut pixmap = Pixmap::new(32, 32).unwrap();
  apply_filter(&filter, &mut pixmap);

  let mut seen_non_gray = false;
  let mut seen_non_opaque_alpha = false;
  let mut seen_non_zero_alpha = false;
  for px in pixmap.pixels() {
    if px.alpha() != 0 {
      seen_non_zero_alpha = true;
    }
    if px.alpha() != 255 {
      seen_non_opaque_alpha = true;
    }
    if px.red() != px.green() || px.green() != px.blue() {
      seen_non_gray = true;
    }
  }

  assert!(seen_non_zero_alpha, "expected at least one non-transparent pixel");
  assert!(
    seen_non_gray,
    "expected at least one pixel with non-grayscale RGB output"
  );
  assert!(
    seen_non_opaque_alpha,
    "expected alpha channel to not be constant 255"
  );
}

#[test]
fn turbulence_generates_independent_rgb_channels() {
  let filter = turbulence_filter(FilterPrimitive::Turbulence {
    base_frequency: (0.2, 0.25),
    seed: 123,
    octaves: 2,
    stitch_tiles: false,
    kind: TurbulenceType::FractalNoise,
  });

  let mut pixmap = Pixmap::new(32, 32).unwrap();
  apply_filter(&filter, &mut pixmap);

  let mut saw_difference = false;
  for px in pixmap.pixels() {
    if px.red() != px.green() || px.red() != px.blue() || px.green() != px.blue() {
      saw_difference = true;
      break;
    }
  }

  assert!(
    saw_difference,
    "expected feTurbulence to generate independent RGB channels"
  );
}

#[test]
fn turbulence_output_spans_both_sides_of_midgray() {
  let filter = turbulence_filter_with_options(
    FilterPrimitive::Turbulence {
      base_frequency: (0.12, 0.18),
      seed: 9,
      octaves: 3,
      stitch_tiles: false,
      kind: TurbulenceType::Turbulence,
    },
    ColorInterpolationFilters::SRGB,
    SvgFilterUnits::UserSpaceOnUse,
  );

  let mut pixmap = Pixmap::new(64, 64).unwrap();
  apply_filter(&filter, &mut pixmap);

  // `Pixmap` stores premultiplied bytes; since `feTurbulence` also generates alpha noise, we need
  // to unpremultiply before checking the distribution around midgray.
  let mut min = u8::MAX;
  let mut max = u8::MIN;
  let mut saw_opaque = false;
  for px in pixmap.pixels() {
    let a = px.alpha();
    if a == 0 {
      continue;
    }
    saw_opaque = true;
    let unpremul_red = (px.red() as u32)
      .saturating_mul(255)
      .saturating_add((a as u32) / 2)
      / (a as u32);
    let unpremul_red = (unpremul_red.min(255)) as u8;
    min = min.min(unpremul_red);
    max = max.max(unpremul_red);
  }

  assert!(
    saw_opaque,
    "expected turbulence output to contain some non-transparent pixels"
  );
  assert!(
    min < 128 && max > 128,
    "expected turbulence output to cross 0.5; got unpremultiplied red range [{min}, {max}]"
  );
}

#[test]
fn turbulence_userspace_translation_changes_pattern() {
  let filter = turbulence_filter_with_options(
    FilterPrimitive::Turbulence {
      base_frequency: (0.08, 0.11),
      seed: 42,
      octaves: 2,
      stitch_tiles: false,
      kind: TurbulenceType::FractalNoise,
    },
    ColorInterpolationFilters::SRGB,
    SvgFilterUnits::UserSpaceOnUse,
  );

  let mut a = Pixmap::new(64, 64).unwrap();
  let mut b = Pixmap::new(64, 64).unwrap();
  let bbox_w = 32.0;
  let bbox_h = 32.0;
  let dx = 7.0;
  let dy = 5.0;
  apply_svg_filter(
    &filter,
    &mut a,
    1.0,
    Rect::from_xywh(0.0, 0.0, bbox_w, bbox_h),
  )
  .unwrap();
  apply_svg_filter(
    &filter,
    &mut b,
    1.0,
    Rect::from_xywh(dx, dy, bbox_w, bbox_h),
  )
  .unwrap();

  let dx = dx as u32;
  let dy = dy as u32;
  let mut any_diff = false;
  for y in 0..bbox_h as u32 {
    for x in 0..bbox_w as u32 {
      let left = a.pixel(x, y).unwrap();
      let right = b.pixel(x + dx, y + dy).unwrap();
      if left != right {
        any_diff = true;
        break;
      }
    }
    if any_diff {
      break;
    }
  }

  assert!(
    any_diff,
    "expected userSpaceOnUse turbulence to change when the bbox is translated"
  );
}

#[test]
fn turbulence_userspace_translation_changes_pattern_with_filter_res() {
  let mut filter = turbulence_filter_with_options(
    FilterPrimitive::Turbulence {
      base_frequency: (0.08, 0.11),
      seed: 42,
      octaves: 2,
      stitch_tiles: false,
      kind: TurbulenceType::FractalNoise,
    },
    ColorInterpolationFilters::SRGB,
    SvgFilterUnits::UserSpaceOnUse,
  );
  filter.filter_res = Some((16, 16));
  filter.refresh_fingerprint();

  let mut a = Pixmap::new(64, 64).unwrap();
  let mut b = Pixmap::new(64, 64).unwrap();
  let bbox_w = 32.0;
  let bbox_h = 32.0;
  let dx = 7.0;
  let dy = 5.0;
  apply_svg_filter(
    &filter,
    &mut a,
    1.0,
    Rect::from_xywh(0.0, 0.0, bbox_w, bbox_h),
  )
  .unwrap();
  apply_svg_filter(
    &filter,
    &mut b,
    1.0,
    Rect::from_xywh(dx, dy, bbox_w, bbox_h),
  )
  .unwrap();

  let dx = dx as u32;
  let dy = dy as u32;
  let mut any_diff = false;
  for y in 0..bbox_h as u32 {
    for x in 0..bbox_w as u32 {
      let left = a.pixel(x, y).unwrap();
      let right = b.pixel(x + dx, y + dy).unwrap();
      if left != right {
        any_diff = true;
        break;
      }
    }
    if any_diff {
      break;
    }
  }

  assert!(
    any_diff,
    "expected userSpaceOnUse turbulence to change when bbox is translated, even under filterRes resampling"
  );
}

#[test]
fn turbulence_stitches_edges_with_offset_filter_region() {
  const START_X: u32 = 10;
  const START_Y: u32 = 5;
  const W: u32 = 64;
  const H: u32 = 32;

  let primitive = FilterPrimitive::Turbulence {
    base_frequency: (0.08, 0.1),
    seed: 7,
    octaves: 2,
    stitch_tiles: true,
    kind: TurbulenceType::Turbulence,
  };
  let mut filter = SvgFilter {
    color_interpolation_filters: ColorInterpolationFilters::SRGB,
    steps: vec![FilterStep {
      result: None,
      color_interpolation_filters: None,
      primitive,
      region: None,
    }],
    region: SvgFilterRegion {
      x: SvgLength::Number(START_X as f32),
      y: SvgLength::Number(START_Y as f32),
      width: SvgLength::Number(W as f32),
      height: SvgLength::Number(H as f32),
      units: SvgFilterUnits::UserSpaceOnUse,
    },
    filter_res: None,
    primitive_units: SvgFilterUnits::UserSpaceOnUse,
    fingerprint: 0,
  };
  filter.refresh_fingerprint();

  let mut pixmap = Pixmap::new(80, 48).unwrap();
  let bbox = Rect::from_xywh(0.0, 0.0, pixmap.width() as f32, pixmap.height() as f32);
  apply_svg_filter(&filter, &mut pixmap, 1.0, bbox).unwrap();

  let max_delta = |a: PremultipliedColorU8, b: PremultipliedColorU8| -> u8 {
    [
      a.red().abs_diff(b.red()),
      a.green().abs_diff(b.green()),
      a.blue().abs_diff(b.blue()),
      a.alpha().abs_diff(b.alpha()),
    ]
    .into_iter()
    .max()
    .unwrap_or(0)
  };

  let mut max_lr = 0u8;
  for y in START_Y..START_Y + H {
    let a = pixmap.pixel(START_X, y).unwrap();
    let b = pixmap.pixel(START_X + W - 1, y).unwrap();
    max_lr = max_lr.max(max_delta(a, b));
  }
  let mut max_tb = 0u8;
  for x in START_X..START_X + W {
    let a = pixmap.pixel(x, START_Y).unwrap();
    let b = pixmap.pixel(x, START_Y + H - 1).unwrap();
    max_tb = max_tb.max(max_delta(a, b));
  }

  const MAX_EDGE_DELTA: u8 = 160;
  assert!(
    max_lr <= MAX_EDGE_DELTA,
    "unexpectedly large left/right edge delta for stitchTiles=\"stitch\" with offset filter region: max Δ={max_lr}"
  );
  assert!(
    max_tb <= MAX_EDGE_DELTA,
    "unexpectedly large top/bottom edge delta for stitchTiles=\"stitch\" with offset filter region: max Δ={max_tb}"
  );
}

#[test]
fn turbulence_midgray_displacement_map_is_nearly_identity_in_linear_rgb() {
  // Regression test for CIF=linearRGB pipelines where feTurbulence feeds feDisplacementMap.
  //
  // With baseFrequency=0, feTurbulence produces a constant mapped=0.5 everywhere. Under the
  // filter engine’s linearRGB model, generator primitives must encode that linear 0.5 as sRGB
  // bytes so later srgb_to_linear recovers (approximately) 0.5 instead of ~0.214 (mid-gray in
  // sRGB), which would cause a large displacement.
  //
  // The linear<->sRGB conversion is quantized through an 8-bit LUT, so we allow a small
  // per-channel tolerance instead of asserting exact identity.
  let filter = turbulence_displacement_filter(1.0);

  let mut pixmap = Pixmap::new(8, 8).unwrap();
  {
    let width = pixmap.width() as usize;
    let height = pixmap.height() as usize;
    let pixels = pixmap.pixels_mut();
    for y in 0..height {
      for x in 0..width {
        let is_white = (x + y) % 2 == 0;
        pixels[y * width + x] = if is_white {
          PremultipliedColorU8::from_rgba(255, 255, 255, 255).unwrap()
        } else {
          PremultipliedColorU8::from_rgba(0, 0, 0, 255).unwrap()
        };
      }
    }
  }
  let original = pixmap.clone();
  apply_filter(&filter, &mut pixmap);

  let mut max_delta = 0u8;
  let mut max_at = (0usize, 0usize, 'r', 0u8, 0u8);

  for (idx, (out_px, src_px)) in pixmap
    .pixels()
    .iter()
    .zip(original.pixels().iter())
    .enumerate()
  {
    let x = idx % pixmap.width() as usize;
    let y = idx / pixmap.width() as usize;
    for (name, out, src) in [
      ('r', out_px.red(), src_px.red()),
      ('g', out_px.green(), src_px.green()),
      ('b', out_px.blue(), src_px.blue()),
      ('a', out_px.alpha(), src_px.alpha()),
    ] {
      let delta = out.abs_diff(src);
      if delta > max_delta {
        max_delta = delta;
        max_at = (x, y, name, out, src);
      }
    }
  }

  assert!(
    max_delta <= 20,
    "expected displacement output to stay close to the source (max channel Δ <= 20), got Δ={max_delta} at ({},{}) channel {} (out={} src={})",
    max_at.0,
    max_at.1,
    max_at.2,
    max_at.3,
    max_at.4
  );
}
