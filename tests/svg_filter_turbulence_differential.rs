use fastrender::geometry::Rect;
use fastrender::paint::svg_filter::{
  apply_svg_filter, ColorInterpolationFilters, FilterPrimitive, FilterStep, SvgFilter,
  SvgFilterRegion, SvgFilterUnits, SvgLength, TurbulenceType,
};
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
    // Match FastRender's filter parser (`parse_fe_turbulence`):
    // truncate toward zero and preserve sign.
    seed_attr as i32
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
  <rect x="{rx}" y="{ry}" width="{rw}" height="{rh}" fill="white" filter="url(#f)"/>
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
  svg: &str,
  resvg: &[u8],
  fast: &[u8],
  width: u32,
) {
  assert_eq!(resvg.len(), fast.len(), "pixmaps must be same byte length");

  let mut max_delta = 0u8;
  let mut max_at = 0usize;
  let mut differing_bytes = 0usize;

  for (idx, (&a, &b)) in resvg.iter().zip(fast.iter()).enumerate() {
    let delta = a.abs_diff(b);
    if delta != 0 {
      differing_bytes += 1;
    }
    if delta > max_delta {
      max_delta = delta;
      max_at = idx;
    }
  }

  if max_delta <= 1 {
    return;
  }

  let pixel_idx = max_at / 4;
  let x = (pixel_idx % width as usize) as u32;
  let y = (pixel_idx / width as usize) as u32;
  let channel = match max_at % 4 {
    0 => "r",
    1 => "g",
    2 => "b",
    _ => "a",
  };

  panic!(
    "feTurbulence differential mismatch (tolerance<=1)\n  case={case_idx}\n  max Δ={max_delta} at ({x},{y}) channel {channel} (resvg={} fast={})\n  differing_bytes={differing_bytes} / {}\n  params={case:?}\n  svg=\n{svg}",
    resvg[max_at],
    fast[max_at],
    resvg.len()
  );
}

fn generate_cases() -> Vec<TurbulenceCase> {
  const CANVAS_W: u32 = 32;
  const CANVAS_H: u32 = 32;
  const CASES: usize = 128;

  let mut rng = XorShift32::new(0x2440_2440);

  let base_freq_choices: &[(f32, f32)] = &[
    (0.0, 0.0),
    (1e-4, 1e-4),
    (0.2, 0.2),
    (0.2, 1e-4),
    (1e-4, 0.2),
    (0.05, 0.08),
  ];

  let seed_choices: &[f32] = &[-3.6, -0.6, -0.4, 0.0, 1.0, 2.2, 7.0, 42.0, 1337.9];
  let seed_fracs: &[f32] = &[0.0, 0.2, 0.5, 0.7];

  let mut cases = Vec::with_capacity(CASES);

  for idx in 0..CASES {
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
      let fx = rng.gen_range_f32(0.0, 0.6);
      let fy = if rng.next_bool() {
        fx
      } else if rng.next_bool() {
        rng.gen_range_f32(0.0, 0.6)
      } else {
        rng.gen_range_f32(0.0, 0.0002)
      };
      (fx, fy)
    };

    let seed_attr = if idx < seed_choices.len() {
      seed_choices[idx]
    } else {
      let base = rng.gen_range_f32(-10.0, 100.0);
      base + rng.choose(seed_fracs)
    };
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
    let filter_w = rng.gen_range_f32(CANVAS_W as f32 * 0.75, CANVAS_W as f32 * 1.75);
    let filter_h = rng.gen_range_f32(CANVAS_H as f32 * 0.75, CANVAS_H as f32 * 1.75);
    let mut filter_x = rng.gen_range_f32(-(CANVAS_W as f32) * 0.75, CANVAS_W as f32 * 0.75);
    let mut filter_y = rng.gen_range_f32(-(CANVAS_H as f32) * 0.75, CANVAS_H as f32 * 0.75);

    // Ensure the region intersects the rasterized viewport so we get meaningful output.
    let viewport_x0 = 0.0;
    let viewport_y0 = 0.0;
    let viewport_x1 = CANVAS_W as f32;
    let viewport_y1 = CANVAS_H as f32;
    let inter_x0 = filter_x.max(viewport_x0);
    let inter_y0 = filter_y.max(viewport_y0);
    let inter_x1 = (filter_x + filter_w).min(viewport_x1);
    let inter_y1 = (filter_y + filter_h).min(viewport_y1);
    if inter_x1 <= inter_x0 || inter_y1 <= inter_y0 {
      filter_x = -(filter_w * 0.25);
      filter_y = -(filter_h * 0.25);
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
  let cases = generate_cases();

  for (case_idx, case) in cases.iter().enumerate() {
    let svg = case.as_svg();
    let resvg_pixmap = render_with_resvg(&svg, case.canvas_w, case.canvas_h);
    let fast_pixmap = render_with_fastrender(case);

    compare_pixmaps(
      case_idx,
      case,
      &svg,
      resvg_pixmap.data(),
      fast_pixmap.data(),
      case.canvas_w,
    );
  }
}
