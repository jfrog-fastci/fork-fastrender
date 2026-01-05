//! Differential (randomized) reference test for SVG `feTurbulence`.
//!
//! This compares FastRender's SVG filter executor against resvg across a deterministic sweep of
//! parameter combinations. It is intended to lock down subtle semantics such as coordinate mapping,
//! primitiveUnits/objectBoundingBox behavior, stitchTiles periods, and rounding.
//!
//! The test is `#[ignore]` by default because FastRender may temporarily diverge while the
//! turbulence implementation evolves. Run it manually with:
//! `cargo test --test svg_filter_turbulence_differential -- --ignored`
//!
//! Debug knobs:
//! - `FASTR_TURBULENCE_DIFF_SEED` (u32): RNG seed override.
//! - `FASTR_TURBULENCE_DIFF_CASES` (usize): number of cases (clamped 1..512).
//! - `FASTR_TURBULENCE_DIFF_ONLY` (usize): run a single case index.
//! - `FASTR_TURBULENCE_DIFF_START` (usize): start at a case index.
//! - `FASTR_TURBULENCE_DIFF_TOL` (u8, clamped 0..1): byte tolerance.
//! - `FASTR_TURBULENCE_DIFF_DUMP=1`: dump PNG+SVG artifacts on mismatch to
//!   `target/turbulence_differential/`.

use fastrender::geometry::Rect;
use fastrender::paint::svg_filter::{
  apply_svg_filter, ColorInterpolationFilters, FilterPrimitive, FilterStep, SvgFilter,
  SvgFilterRegion, SvgFilterUnits, SvgLength, TurbulenceType,
};
use std::env;
use std::fs;
use std::path::PathBuf;
use tiny_skia::{Pixmap, PremultipliedColorU8};

#[derive(Clone)]
struct XorShift32 {
  state: u32,
}

impl XorShift32 {
  fn new(seed: u32) -> Self {
    Self { state: seed }
  }

  fn next_u32(&mut self) -> u32 {
    // George Marsaglia's xorshift32.
    let mut x = self.state;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    self.state = x;
    x
  }

  fn next_bool(&mut self) -> bool {
    (self.next_u32() & 1) != 0
  }

  fn gen_range_u32(&mut self, min: u32, max_inclusive: u32) -> u32 {
    if min >= max_inclusive {
      return min;
    }
    let span = max_inclusive - min + 1;
    min + (self.next_u32() % span)
  }

  fn gen_range_f32(&mut self, min: f32, max: f32) -> f32 {
    if !min.is_finite() || !max.is_finite() || min >= max {
      return min;
    }
    let unit = (self.next_u32() as f64) / (u32::MAX as f64);
    (min as f64 + unit * (max as f64 - min as f64)) as f32
  }

  fn choose<T: Copy>(&mut self, options: &[T]) -> T {
    options[(self.next_u32() as usize) % options.len()]
  }
}

#[derive(Clone, Debug)]
struct TurbulenceCase {
  canvas_w: u32,
  canvas_h: u32,
  rect_x: f32,
  rect_y: f32,
  rect_w: f32,
  rect_h: f32,
  filter_x: f32,
  filter_y: f32,
  filter_w: f32,
  filter_h: f32,
  primitive_units: SvgFilterUnits,
  kind: TurbulenceType,
  base_frequency: (f32, f32),
  seed_attr: f32,
  seed: i32,
  octaves: u32,
  stitch_tiles: bool,
}

impl TurbulenceCase {
  fn seed_from_attr(seed_attr: f32) -> i32 {
    // Match FastRender's filter parser (`parse_fe_turbulence`): round to the nearest integer and
    // clamp negative/non-finite seeds to 0.
    if seed_attr.is_finite() {
      seed_attr.round().max(0.0) as i32
    } else {
      0
    }
  }

  fn primitive_units_attr(&self) -> &'static str {
    match self.primitive_units {
      SvgFilterUnits::UserSpaceOnUse => "userSpaceOnUse",
      SvgFilterUnits::ObjectBoundingBox => "objectBoundingBox",
    }
  }

  fn kind_attr(&self) -> &'static str {
    match self.kind {
      TurbulenceType::Turbulence => "turbulence",
      TurbulenceType::FractalNoise => "fractalNoise",
    }
  }

  fn stitch_attr(&self) -> &'static str {
    if self.stitch_tiles {
      "stitch"
    } else {
      "noStitch"
    }
  }

  fn as_svg(&self) -> String {
    let w = self.canvas_w;
    let h = self.canvas_h;
    format!(
      r#"<svg width="{w}" height="{h}" viewBox="0 0 {w} {h}" xmlns="http://www.w3.org/2000/svg">
  <defs>
    <filter id="f" filterUnits="userSpaceOnUse" primitiveUnits="{prim_units}" x="{fx}" y="{fy}" width="{fw}" height="{fh}" color-interpolation-filters="linearRGB">
      <feTurbulence type="{kind}" baseFrequency="{bfx} {bfy}" seed="{seed_attr}" numOctaves="{octaves}" stitchTiles="{stitch}"/>
    </filter>
  </defs>
  <g transform="translate({rx} {ry})">
    <rect x="0" y="0" width="{rw}" height="{rh}" fill="white" filter="url(#f)"/>
  </g>
</svg>"#,
      prim_units = self.primitive_units_attr(),
      fx = fmt_num(self.filter_x),
      fy = fmt_num(self.filter_y),
      fw = fmt_num(self.filter_w),
      fh = fmt_num(self.filter_h),
      kind = self.kind_attr(),
      bfx = fmt_num(self.base_frequency.0),
      bfy = fmt_num(self.base_frequency.1),
      seed_attr = fmt_num(self.seed_attr),
      octaves = self.octaves,
      stitch = self.stitch_attr(),
      rx = fmt_num(self.rect_x),
      ry = fmt_num(self.rect_y),
      rw = fmt_num(self.rect_w),
      rh = fmt_num(self.rect_h),
    )
  }
}

fn fmt_num(value: f32) -> String {
  if !value.is_finite() {
    return "0".to_string();
  }
  // Avoid scientific notation so the SVG stays readable and stable across toolchains.
  let mut s = format!("{value:.6}");
  while s.contains('.') && s.ends_with('0') {
    s.pop();
  }
  if s.ends_with('.') {
    s.pop();
  }
  if s == "-0" {
    s = "0".to_string();
  }
  s
}

fn quantize_svg_f32(value: f32) -> f32 {
  fmt_num(value).parse::<f32>().unwrap_or(0.0)
}

fn quantize_svg_pair(values: (f32, f32)) -> (f32, f32) {
  (quantize_svg_f32(values.0), quantize_svg_f32(values.1))
}

fn env_usize(name: &str) -> Option<usize> {
  env::var(name).ok()?.parse::<usize>().ok()
}

fn env_u32(name: &str) -> Option<u32> {
  env::var(name).ok()?.parse::<u32>().ok()
}

fn env_u8(name: &str) -> Option<u8> {
  env::var(name).ok()?.parse::<u8>().ok()
}

fn env_bool(name: &str) -> bool {
  match env::var(name).ok().as_deref() {
    Some("1") | Some("true") | Some("True") | Some("TRUE") | Some("yes") | Some("Yes")
    | Some("YES") => true,
    _ => false,
  }
}

fn render_with_resvg(svg: &str, width: u32, height: u32) -> resvg::tiny_skia::Pixmap {
  use resvg::usvg;

  let mut options = usvg::Options::default();
  options.resources_dir = None;
  let tree = usvg::Tree::from_str(svg, &options).expect("resvg should parse the generated SVG");

  let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height).expect("pixmap allocation");
  let size = tree.size();
  let scale_x = width as f32 / size.width();
  let scale_y = height as f32 / size.height();
  let transform = resvg::tiny_skia::Transform::from_scale(scale_x, scale_y);
  resvg::render(&tree, transform, &mut pixmap.as_mut());
  pixmap
}

fn render_with_fastrender(case: &TurbulenceCase) -> Pixmap {
  let mut pixmap = Pixmap::new(case.canvas_w, case.canvas_h).expect("pixmap allocation");

  let white = PremultipliedColorU8::from_rgba(255, 255, 255, 255).unwrap();
  let pixels = pixmap.pixels_mut();
  let w = case.canvas_w as usize;
  let h = case.canvas_h as usize;

  let min_x = case.rect_x.floor() as i32;
  let min_y = case.rect_y.floor() as i32;
  let max_x = (case.rect_x + case.rect_w).ceil() as i32;
  let max_y = (case.rect_y + case.rect_h).ceil() as i32;

  for y in min_y.max(0)..max_y.min(h as i32) {
    for x in min_x.max(0)..max_x.min(w as i32) {
      pixels[y as usize * w + x as usize] = white;
    }
  }

  let mut filter = SvgFilter {
    color_interpolation_filters: ColorInterpolationFilters::LinearRGB,
    steps: vec![FilterStep {
      result: None,
      color_interpolation_filters: None,
      primitive: FilterPrimitive::Turbulence {
        base_frequency: case.base_frequency,
        seed: case.seed,
        octaves: case.octaves,
        stitch_tiles: case.stitch_tiles,
        kind: case.kind,
      },
      region: None,
    }],
    region: SvgFilterRegion {
      x: SvgLength::Number(case.filter_x),
      y: SvgLength::Number(case.filter_y),
      width: SvgLength::Number(case.filter_w),
      height: SvgLength::Number(case.filter_h),
      units: SvgFilterUnits::UserSpaceOnUse,
    },
    filter_res: None,
    primitive_units: case.primitive_units,
    fingerprint: 0,
  };
  filter.refresh_fingerprint();

  let bbox = Rect::from_xywh(case.rect_x, case.rect_y, case.rect_w, case.rect_h);
  apply_svg_filter(&filter, &mut pixmap, 1.0, bbox).expect("apply_svg_filter");
  pixmap
}

fn compare_pixmaps(
  case_idx: usize,
  case: &TurbulenceCase,
  seed: u32,
  total_cases: usize,
  tolerance: u8,
  dump_artifacts: bool,
  svg: &str,
  resvg: &[u8],
  fast: &[u8],
  width: u32,
  height: u32,
) {
  assert_eq!(resvg.len(), fast.len(), "pixmaps must be same byte length");
  assert_eq!(
    resvg.len(),
    (width as usize)
      .saturating_mul(height as usize)
      .saturating_mul(4),
    "pixmap byte length must match width*height*4"
  );

  let mut max_delta = 0u8;
  let mut max_at = (0u32, 0u32, 'r', 0u8, 0u8);
  let mut differing_channels = 0usize;

  let pixel_count = resvg.len() / 4;
  let row_len = width as usize;

  for pixel_idx in 0..pixel_count {
    let base = pixel_idx * 4;
    // tiny-skia pixmaps are stored as premultiplied BGRA; convert indices to premultiplied RGBA.
    for (channel, resvg_byte, fast_byte) in [
      ('r', resvg[base + 2], fast[base + 2]),
      ('g', resvg[base + 1], fast[base + 1]),
      ('b', resvg[base], fast[base]),
      ('a', resvg[base + 3], fast[base + 3]),
    ] {
      let delta = resvg_byte.abs_diff(fast_byte);
      if delta != 0 {
        differing_channels += 1;
      }
      if delta > max_delta {
        max_delta = delta;
        let x = (pixel_idx % row_len) as u32;
        let y = (pixel_idx / row_len) as u32;
        max_at = (x, y, channel, resvg_byte, fast_byte);
      }
    }
  }

  if max_delta <= tolerance {
    return;
  }

  let mut artifact_note = String::new();
  if dump_artifacts {
    let out_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/turbulence_differential");
    if let Err(err) = fs::create_dir_all(&out_dir) {
      artifact_note = format!("\n  (failed to create artifact dir {out_dir:?}: {err})");
    } else {
      let resvg_path = out_dir.join(format!("case_{case_idx:04}_resvg.png"));
      let fast_path = out_dir.join(format!("case_{case_idx:04}_fastrender.png"));
      let diff_path = out_dir.join(format!("case_{case_idx:04}_diff.png"));
      let svg_path = out_dir.join(format!("case_{case_idx:04}.svg"));

      let to_rgba_image = |data: &[u8]| -> image::RgbaImage {
        // Convert premultiplied BGRA (tiny-skia) into straight RGBA for PNG dumps.
        let mut rgba = image::RgbaImage::new(width, height);
        for (dst, src) in rgba.as_mut().chunks_exact_mut(4).zip(data.chunks_exact(4)) {
          let b = src[0];
          let g = src[1];
          let r = src[2];
          let a = src[3];
          if a == 0 {
            dst.copy_from_slice(&[0, 0, 0, 0]);
            continue;
          }
          let alpha = a as f32 / 255.0;
          dst[0] = ((r as f32 / alpha).min(255.0)) as u8;
          dst[1] = ((g as f32 / alpha).min(255.0)) as u8;
          dst[2] = ((b as f32 / alpha).min(255.0)) as u8;
          dst[3] = a;
        }
        rgba
      };
      let write_png = |path: &PathBuf, img: &image::RgbaImage| -> Result<(), String> {
        img
          .save(path)
          .map_err(|err| format!("failed to write {path:?}: {err}"))
      };

      // Grayscale diff image showing max per-channel delta per pixel.
      let diff_img = {
        let mut img = image::RgbaImage::new(width, height);
        for (idx, px) in img.pixels_mut().enumerate() {
          let base = idx * 4;
          let db = resvg[base].abs_diff(fast[base]);
          let dg = resvg[base + 1].abs_diff(fast[base + 1]);
          let dr = resvg[base + 2].abs_diff(fast[base + 2]);
          let da = resvg[base + 3].abs_diff(fast[base + 3]);
          let max = db.max(dg).max(dr).max(da);
          // Amplify small diffs so single-byte changes are visible.
          let v = max.saturating_mul(8);
          *px = image::Rgba([v, v, v, 255]);
        }
        img
      };

      let write_svg =
        fs::write(&svg_path, svg).map_err(|err| format!("failed to write {svg_path:?}: {err}"));

      let resvg_img = to_rgba_image(resvg);
      let fast_img = to_rgba_image(fast);
      let writes = write_png(&resvg_path, &resvg_img)
        .and_then(|_| write_png(&fast_path, &fast_img))
        .and_then(|_| write_png(&diff_path, &diff_img))
        .and_then(|_| write_svg);

      match writes {
        Ok(()) => {
          artifact_note = format!(
            "\n  artifacts:\n    resvg={resvg_path:?}\n    fastrender={fast_path:?}\n    diff={diff_path:?}\n    svg={svg_path:?}"
          );
        }
        Err(err) => {
          artifact_note = format!("\n  (failed to write artifacts in {out_dir:?}: {err})");
        }
      }
    }
  }

  let pixel_offset = (max_at.1 as usize * width as usize + max_at.0 as usize) * 4;
  let resvg_px = [
    resvg[pixel_offset + 2],
    resvg[pixel_offset + 1],
    resvg[pixel_offset],
    resvg[pixel_offset + 3],
  ];
  let fast_px = [
    fast[pixel_offset + 2],
    fast[pixel_offset + 1],
    fast[pixel_offset],
    fast[pixel_offset + 3],
  ];

  panic!(
    "feTurbulence differential mismatch (tolerance<={tolerance})\n  case={case_idx} / {total_cases} (seed={seed})\n  max Δ={max_delta} at ({},{}) channel {} (resvg={} fast={})\n  pixel premul RGBA: resvg={resvg_px:?} fast={fast_px:?}\n  differing_channels={differing_channels} / {}\n  rerun:\n    FASTR_TURBULENCE_DIFF_SEED={seed} FASTR_TURBULENCE_DIFF_CASES={total_cases} FASTR_TURBULENCE_DIFF_ONLY={case_idx} FASTR_TURBULENCE_DIFF_TOL={tolerance} cargo test --test svg_filter_turbulence_differential -- --ignored\n  rerun (with artifacts):\n    FASTR_TURBULENCE_DIFF_SEED={seed} FASTR_TURBULENCE_DIFF_CASES={total_cases} FASTR_TURBULENCE_DIFF_ONLY={case_idx} FASTR_TURBULENCE_DIFF_TOL={tolerance} FASTR_TURBULENCE_DIFF_DUMP=1 cargo test --test svg_filter_turbulence_differential -- --ignored\n  params={case:?}{artifact_note}\n  svg=\n{svg}",
    max_at.0,
    max_at.1,
    max_at.2,
    max_at.3,
    max_at.4,
    pixel_count * 4
  );
}

fn generate_cases(seed: u32, case_count: usize) -> Vec<TurbulenceCase> {
  const CANVAS_W: u32 = 32;
  const CANVAS_H: u32 = 32;

  let case_count = case_count.clamp(1, 512);
  let mut rng = XorShift32::new(seed);

  let base_freq_choices: &[(f32, f32)] = &[
    (0.0, 0.0),
    (1e-4, 1e-4),
    (0.0, 0.2),
    (0.2, 0.0),
    (0.2, 0.2),
    (0.2, 1e-4),
    (1e-4, 0.2),
    (0.05, 0.08),
  ];

  let seed_choices: &[f32] = &[-3.6, -0.6, -0.4, 0.0, 1.0, 2.2, 7.0, 42.0, 1337.9];
  let seed_fracs: &[f32] = &[0.0, 0.2, 0.5, 0.7];

  let mut cases = Vec::with_capacity(case_count);

  for idx in 0..case_count {
    let kind = if idx % 2 == 0 {
      TurbulenceType::Turbulence
    } else {
      TurbulenceType::FractalNoise
    };

    let primitive_units = if idx % 3 == 0 {
      SvgFilterUnits::ObjectBoundingBox
    } else {
      if rng.next_bool() {
        SvgFilterUnits::UserSpaceOnUse
      } else {
        SvgFilterUnits::ObjectBoundingBox
      }
    };

    let base_frequency = if idx < base_freq_choices.len() {
      base_freq_choices[idx]
    } else if rng.next_bool() {
      rng.choose(base_freq_choices)
    } else {
      let fx = quantize_svg_f32(rng.gen_range_f32(0.0, 0.6));
      let fy = if rng.next_bool() {
        fx
      } else if rng.next_bool() {
        quantize_svg_f32(rng.gen_range_f32(0.0, 0.6))
      } else {
        quantize_svg_f32(rng.gen_range_f32(0.0, 0.0002))
      };
      (fx, fy)
    };
    let base_frequency = quantize_svg_pair(base_frequency);

    let seed_attr = if idx < seed_choices.len() {
      seed_choices[idx]
    } else {
      let base = rng.gen_range_f32(-10.0, 100.0);
      base + rng.choose(seed_fracs)
    };
    let seed_attr = quantize_svg_f32(seed_attr);
    let seed = TurbulenceCase::seed_from_attr(seed_attr);

    let octaves = rng.gen_range_u32(1, 4);
    let stitch_tiles = rng.next_bool();

    let (rect_x, rect_y, rect_w, rect_h) =
      if matches!(primitive_units, SvgFilterUnits::ObjectBoundingBox) {
        let min_size = 8u32;
        let rw = rng.gen_range_u32(min_size, CANVAS_W);
        let rh = rng.gen_range_u32(min_size, CANVAS_H);
        let rx = rng.gen_range_u32(0, CANVAS_W - rw);
        let ry = rng.gen_range_u32(0, CANVAS_H - rh);
        (rx as f32, ry as f32, rw as f32, rh as f32)
      } else {
        (0.0, 0.0, CANVAS_W as f32, CANVAS_H as f32)
      };

    // Make the filter region wander so we cover coordinate translation and viewport clipping.
    let filter_w =
      quantize_svg_f32(rng.gen_range_f32(CANVAS_W as f32 * 0.75, CANVAS_W as f32 * 1.75));
    let filter_h =
      quantize_svg_f32(rng.gen_range_f32(CANVAS_H as f32 * 0.75, CANVAS_H as f32 * 1.75));
    let mut filter_x =
      quantize_svg_f32(rng.gen_range_f32(-(CANVAS_W as f32) * 0.75, CANVAS_W as f32 * 0.75));
    let mut filter_y =
      quantize_svg_f32(rng.gen_range_f32(-(CANVAS_H as f32) * 0.75, CANVAS_H as f32 * 0.75));

    // Ensure the region intersects the rasterized viewport so we get meaningful output.
    let viewport_x0 = 0.0;
    let viewport_y0 = 0.0;
    let viewport_x1 = CANVAS_W as f32;
    let viewport_y1 = CANVAS_H as f32;
    // FastRender's `SvgFilterRegion` numeric `userSpaceOnUse` coordinates are resolved relative to
    // the filtered object's bbox origin. Mirror that by treating filter_x/y as local coordinates
    // (global = rect_x/y + filter_x/y) so the resvg SVG and direct `SvgFilter` construction match.
    let global_filter_x = rect_x + filter_x;
    let global_filter_y = rect_y + filter_y;
    let inter_x0 = global_filter_x.max(viewport_x0);
    let inter_y0 = global_filter_y.max(viewport_y0);
    let inter_x1 = (global_filter_x + filter_w).min(viewport_x1);
    let inter_y1 = (global_filter_y + filter_h).min(viewport_y1);
    if inter_x1 <= inter_x0 || inter_y1 <= inter_y0 {
      // Nudge the local region so that, after bbox-origin translation, it overlaps the viewport.
      filter_x = quantize_svg_f32(-(filter_w * 0.25) - rect_x);
      filter_y = quantize_svg_f32(-(filter_h * 0.25) - rect_y);
    }

    cases.push(TurbulenceCase {
      canvas_w: CANVAS_W,
      canvas_h: CANVAS_H,
      rect_x,
      rect_y,
      rect_w,
      rect_h,
      filter_x,
      filter_y,
      filter_w,
      filter_h,
      primitive_units,
      kind,
      base_frequency,
      seed_attr,
      seed,
      octaves,
      stitch_tiles,
    });
  }

  cases
}

#[test]
#[ignore = "Differential reference test against resvg; expected to fail until the feTurbulence rewrite lands."]
fn svg_filter_turbulence_differential_against_resvg() {
  let seed = env_u32("FASTR_TURBULENCE_DIFF_SEED").unwrap_or(0x2440_2440);
  let case_count = env_usize("FASTR_TURBULENCE_DIFF_CASES").unwrap_or(128);
  let only_case = env_usize("FASTR_TURBULENCE_DIFF_ONLY");
  let start_case = env_usize("FASTR_TURBULENCE_DIFF_START").unwrap_or(0);
  let tolerance = env_u8("FASTR_TURBULENCE_DIFF_TOL").unwrap_or(0);
  let tolerance = tolerance.min(1);
  let dump_artifacts = env_bool("FASTR_TURBULENCE_DIFF_DUMP");

  let cases = generate_cases(seed, case_count);
  let end_case = cases.len();

  let range = if let Some(only) = only_case {
    only..(only + 1)
  } else {
    start_case..end_case
  };

  for case_idx in range {
    let Some(case) = cases.get(case_idx) else {
      panic!(
        "requested case index {case_idx} but only {} cases were generated (seed={seed})",
        cases.len()
      );
    };
    let svg = case.as_svg();
    let resvg_pixmap = render_with_resvg(&svg, case.canvas_w, case.canvas_h);
    let fast_pixmap = render_with_fastrender(case);

    compare_pixmaps(
      case_idx,
      case,
      seed,
      cases.len(),
      tolerance,
      dump_artifacts,
      &svg,
      resvg_pixmap.data(),
      fast_pixmap.data(),
      case.canvas_w,
      case.canvas_h,
    );
  }
}
