use crate::error::RenderStage;
use crate::geometry::Rect;
use crate::paint::pixmap::new_pixmap;
use crate::render_control::{active_deadline, check_active, with_deadline};
use rayon::prelude::*;
use tiny_skia::{Pixmap, PremultipliedColorU8};

use super::{
  ColorInterpolationFilters, RenderResult, SvgFilterUnits, TurbulenceType, FILTER_DEADLINE_STRIDE,
};

// Ported from resvg's `filter::turbulence` implementation to match Chromium/Skia behaviour.
// Keep constants/naming aligned with the reference for easier comparison.
const RAND_M: i32 = 2_147_483_647; // 2**31 - 1
const RAND_A: i32 = 16_807; // 7**5; primitive root of m
const RAND_Q: i32 = 127_773; // m / a
const RAND_R: i32 = 2_836; // m % a

const B_SIZE: usize = 0x100;
const B_SIZE_I32: i32 = 0x100;
const B_LEN: usize = B_SIZE + B_SIZE + 2;
const BM: i32 = 0xff;
const PERLIN_N: i32 = 0x1000;
const CHANNELS: usize = 4;

// LinearRGB -> sRGB conversion table.
//
// Copied from resvg to keep `color-interpolation-filters="linearRGB"` byte-identical.
#[rustfmt::skip]
const LINEAR_RGB_TO_SRGB_TABLE: &[u8; 256] = &[
  0,  13,  22,  28,  34,  38,  42,  46,  50,  53,  56,  59,  61,  64,  66,  69,
  71,  73,  75,  77,  79,  81,  83,  85,  86,  88,  90,  92,  93,  95,  96,  98,
  99, 101, 102, 104, 105, 106, 108, 109, 110, 112, 113, 114, 115, 117, 118, 119,
  120, 121, 122, 124, 125, 126, 127, 128, 129, 130, 131, 132, 133, 134, 135, 136,
  137, 138, 139, 140, 141, 142, 143, 144, 145, 146, 147, 148, 148, 149, 150, 151,
  152, 153, 154, 155, 155, 156, 157, 158, 159, 159, 160, 161, 162, 163, 163, 164,
  165, 166, 167, 167, 168, 169, 170, 170, 171, 172, 173, 173, 174, 175, 175, 176,
  177, 178, 178, 179, 180, 180, 181, 182, 182, 183, 184, 185, 185, 186, 187, 187,
  188, 189, 189, 190, 190, 191, 192, 192, 193, 194, 194, 195, 196, 196, 197, 197,
  198, 199, 199, 200, 200, 201, 202, 202, 203, 203, 204, 205, 205, 206, 206, 207,
  208, 208, 209, 209, 210, 210, 211, 212, 212, 213, 213, 214, 214, 215, 215, 216,
  216, 217, 218, 218, 219, 219, 220, 220, 221, 221, 222, 222, 223, 223, 224, 224,
  225, 226, 226, 227, 227, 228, 228, 229, 229, 230, 230, 231, 231, 232, 232, 233,
  233, 234, 234, 235, 235, 236, 236, 237, 237, 238, 238, 238, 239, 239, 240, 240,
  241, 241, 242, 242, 243, 243, 244, 244, 245, 245, 246, 246, 246, 247, 247, 248,
  248, 249, 249, 250, 250, 251, 251, 251, 252, 252, 253, 253, 254, 254, 255, 255,
];

#[inline]
fn multiply_alpha_u8(channel: u8, alpha: u8) -> u8 {
  let a = alpha as f32 / 255.0;
  (channel as f32 * a + 0.5) as u8
}

#[inline]
fn demultiply_alpha_u8(channel: u8, alpha: u8) -> u8 {
  let a = alpha as f32 / 255.0;
  (channel as f32 / a + 0.5) as u8
}

#[derive(Clone, Copy, Debug)]
struct ResolvedRegion {
  x: u32,
  y: u32,
  width: u32,
  height: u32,
}

fn resolve_region(
  filter_region: Rect,
  output_width: u32,
  output_height: u32,
) -> Option<ResolvedRegion> {
  if output_width == 0 || output_height == 0 {
    return None;
  }

  let width = output_width as i32;
  let height = output_height as i32;

  let min_x = filter_region.min_x().floor() as i32;
  let min_y = filter_region.min_y().floor() as i32;
  let max_x = filter_region.max_x().ceil() as i32;
  let max_y = filter_region.max_y().ceil() as i32;

  let start_x = min_x.clamp(0, width);
  let start_y = min_y.clamp(0, height);
  let end_x = max_x.clamp(0, width);
  let end_y = max_y.clamp(0, height);

  if start_x >= end_x || start_y >= end_y {
    return None;
  }

  Some(ResolvedRegion {
    x: start_x as u32,
    y: start_y as u32,
    width: (end_x - start_x) as u32,
    height: (end_y - start_y) as u32,
  })
}

pub(super) fn render_turbulence(
  output_width: u32,
  output_height: u32,
  filter_region: Rect,
  base_frequency: (f32, f32),
  seed: i32,
  octaves: u32,
  stitch_tiles: bool,
  kind: TurbulenceType,
  color_interpolation_filters: ColorInterpolationFilters,
  primitive_units: SvgFilterUnits,
  _css_bbox: Rect,
  scale_x: f32,
  scale_y: f32,
  surface_origin_css: (f32, f32),
) -> RenderResult<Option<Pixmap>> {
  check_active(RenderStage::Paint)?;
  let region = match resolve_region(filter_region, output_width, output_height) {
    Some(region) => region,
    None => return Ok(new_pixmap(output_width, output_height)),
  };

  let mut pixmap = match new_pixmap(output_width, output_height) {
    Some(pixmap) => pixmap,
    None => return Ok(None),
  };

  let fractal_sum = matches!(kind, TurbulenceType::FractalNoise);

  let base_freq_x = if base_frequency.0.is_finite() && base_frequency.0 > 0.0 {
    base_frequency.0 as f64
  } else {
    0.0
  };
  let base_freq_y = if base_frequency.1.is_finite() && base_frequency.1 > 0.0 {
    base_frequency.1 as f64
  } else {
    0.0
  };

  // With baseFrequency=0, the noise is constant (all coordinates collapse to the origin in noise
  // space). Fast-path the output to avoid building tables/sampling per pixel while keeping the
  // output byte-identical to resvg/Chromium.
  if base_freq_x == 0.0 && base_freq_y == 0.0 {
    let channel = if fractal_sum { 128u8 } else { 0u8 };
    let a = channel;
    if a == 0 {
      return Ok(Some(pixmap));
    }

    let (r, g, b) = match color_interpolation_filters {
      ColorInterpolationFilters::SRGB => (
        multiply_alpha_u8(channel, a),
        multiply_alpha_u8(channel, a),
        multiply_alpha_u8(channel, a),
      ),
      ColorInterpolationFilters::LinearRGB => {
        let r_lin = multiply_alpha_u8(channel, a);
        let g_lin = multiply_alpha_u8(channel, a);
        let b_lin = multiply_alpha_u8(channel, a);

        let r_lin = demultiply_alpha_u8(r_lin, a);
        let g_lin = demultiply_alpha_u8(g_lin, a);
        let b_lin = demultiply_alpha_u8(b_lin, a);

        let r_srgb = LINEAR_RGB_TO_SRGB_TABLE[r_lin as usize];
        let g_srgb = LINEAR_RGB_TO_SRGB_TABLE[g_lin as usize];
        let b_srgb = LINEAR_RGB_TO_SRGB_TABLE[b_lin as usize];

        (
          multiply_alpha_u8(r_srgb, a),
          multiply_alpha_u8(g_srgb, a),
          multiply_alpha_u8(b_srgb, a),
        )
      }
    };
    let constant =
      PremultipliedColorU8::from_rgba(r, g, b, a).unwrap_or(PremultipliedColorU8::TRANSPARENT);

    let row_len = output_width as usize;
    let start_x = region.x as usize;
    let end_x = start_x + region.width as usize;
    let deadline = active_deadline();
    pixmap
      .pixels_mut()
      .par_chunks_mut(row_len)
      .enumerate()
      .skip(region.y as usize)
      .take(region.height as usize)
      .try_for_each(|(_, row)| {
        with_deadline(deadline.as_ref(), || -> RenderResult<()> {
          let row_slice = &mut row[start_x..end_x];
          for (x_offset, px) in row_slice.iter_mut().enumerate() {
            if x_offset % FILTER_DEADLINE_STRIDE == 0 {
              check_active(RenderStage::Paint)?;
            }
            *px = constant;
          }
          Ok(())
        })
      })?;

    return Ok(Some(pixmap));
  }

  let octaves = octaves.max(1);
  let tables = TurbulenceTables::new(seed);

  let scale_x = if scale_x.is_finite() && scale_x > 0.0 {
    scale_x
  } else {
    1.0
  };
  let scale_y = if scale_y.is_finite() && scale_y > 0.0 {
    scale_y
  } else {
    1.0
  };

  let origin_x = if surface_origin_css.0.is_finite() {
    surface_origin_css.0
  } else {
    0.0
  };
  let origin_y = if surface_origin_css.1.is_finite() {
    surface_origin_css.1
  } else {
    0.0
  };

  let to_units = |x_px: f32, y_px: f32| -> (f32, f32) {
    let x_user = origin_x + x_px / scale_x;
    let y_user = origin_y + y_px / scale_y;
    // Note: although SVG defines `primitiveUnits` for filter primitives, both Chromium/Skia and
    // resvg treat `feTurbulence` noise coordinates and `baseFrequency` as user-space values even
    // when `primitiveUnits="objectBoundingBox"`. Keep this behaviour to remain compatible with
    // those engines.
    match primitive_units {
      SvgFilterUnits::UserSpaceOnUse | SvgFilterUnits::ObjectBoundingBox => (x_user, y_user),
    }
  };

  // `stitchTiles="stitch"` is defined over the filter primitive subregion in the filter primitive
  // coordinate system. To keep scale-invariant output, convert the rendered pixel dimensions back
  // into that coordinate space (user/CSS units for `UserSpaceOnUse`).
  let (tile_x, tile_y) = to_units(region.x as f32, region.y as f32);
  let tile_x = tile_x as f64;
  let tile_y = tile_y as f64;
  let tile_width = region.width as f64 / scale_x as f64;
  let tile_height = region.height as f64 / scale_y as f64;
  let do_stitching = stitch_tiles
    && tile_width.is_finite()
    && tile_height.is_finite()
    && tile_width > 0.0
    && tile_height > 0.0;

  let row_len = output_width as usize;
  let start_x = region.x as usize;
  let end_x = start_x + region.width as usize;

  let deadline = active_deadline();
  pixmap
    .pixels_mut()
    .par_chunks_mut(row_len)
    .enumerate()
    .skip(region.y as usize)
    .take(region.height as usize)
    .try_for_each(|(y, row)| {
      with_deadline(deadline.as_ref(), || -> RenderResult<()> {
        let y_px = y as f32;
        let (_, y_coord) = to_units(0.0, y_px);
        let y_coord = y_coord as f64;
        let row_slice = &mut row[start_x..end_x];
        for (x_offset, px) in row_slice.iter_mut().enumerate() {
          if x_offset % FILTER_DEADLINE_STRIDE == 0 {
            check_active(RenderStage::Paint)?;
          }

          let x_px = (start_x + x_offset) as f32;
          let (x_coord, _) = to_units(x_px, 0.0);
          let x_coord = x_coord as f64;
          let turb_byte = |channel: usize| -> u8 {
            let n = turbulence(
              channel,
              x_coord,
              y_coord,
              tile_x,
              tile_y,
              tile_width,
              tile_height,
              base_freq_x,
              base_freq_y,
              octaves,
              fractal_sum,
              do_stitching,
              &tables,
            );
            let n = if fractal_sum {
              (n * 255.0 + 255.0) / 2.0
            } else {
              n * 255.0
            };

            // resvg clamps as f32 then rounds by `+0.5` before truncation.
            let clamped = if n.is_finite() {
              let n = n as f32;
              if n > 255.0 {
                255.0
              } else if n < 0.0 {
                0.0
              } else {
                n
              }
            } else {
              0.0
            };
            (clamped + 0.5) as u8
          };

          let a = turb_byte(3);
          if a == 0 {
            *px = PremultipliedColorU8::TRANSPARENT;
            continue;
          }

          let r = turb_byte(0);
          let g = turb_byte(1);
          let b = turb_byte(2);

          let (r, g, b) = match color_interpolation_filters {
            ColorInterpolationFilters::SRGB => (
              multiply_alpha_u8(r, a),
              multiply_alpha_u8(g, a),
              multiply_alpha_u8(b, a),
            ),
            // resvg stores LinearRGB pixels and converts the final image to sRGB before drawing,
            // which means conversion happens on a premultiplied buffer. Mirror that by performing:
            // premultiply (linear) -> demultiply -> linear->sRGB -> premultiply.
            ColorInterpolationFilters::LinearRGB => {
              let r_lin = multiply_alpha_u8(r, a);
              let g_lin = multiply_alpha_u8(g, a);
              let b_lin = multiply_alpha_u8(b, a);

              let r_lin = demultiply_alpha_u8(r_lin, a);
              let g_lin = demultiply_alpha_u8(g_lin, a);
              let b_lin = demultiply_alpha_u8(b_lin, a);

              let r_srgb = LINEAR_RGB_TO_SRGB_TABLE[r_lin as usize];
              let g_srgb = LINEAR_RGB_TO_SRGB_TABLE[g_lin as usize];
              let b_srgb = LINEAR_RGB_TO_SRGB_TABLE[b_lin as usize];

              (
                multiply_alpha_u8(r_srgb, a),
                multiply_alpha_u8(g_srgb, a),
                multiply_alpha_u8(b_srgb, a),
              )
            }
          };

          *px = PremultipliedColorU8::from_rgba(r, g, b, a)
            .unwrap_or(PremultipliedColorU8::TRANSPARENT);
        }
        Ok(())
      })
    })?;

  Ok(Some(pixmap))
}

#[derive(Clone, Copy, Debug)]
struct StitchInfo {
  width: i32,
  height: i32,
  wrap_x: i32,
  wrap_y: i32,
}

#[derive(Clone)]
struct TurbulenceTables {
  lattice_selector: [usize; B_LEN],
  gradient: [[[f64; 2]; B_LEN]; CHANNELS],
}

impl TurbulenceTables {
  fn new(seed: i32) -> Self {
    let mut seed = normalize_seed(seed);

    let mut lattice_selector = [0usize; B_LEN];
    let mut gradient = [[[0.0f64; 2]; B_LEN]; CHANNELS];

    for k in 0..CHANNELS {
      for i in 0..B_SIZE {
        lattice_selector[i] = i;
        for j in 0..2 {
          seed = random(seed);
          gradient[k][i][j] =
            ((seed % (B_SIZE_I32 + B_SIZE_I32)) - B_SIZE_I32) as f64 / B_SIZE_I32 as f64;
        }

        let s =
          (gradient[k][i][0] * gradient[k][i][0] + gradient[k][i][1] * gradient[k][i][1]).sqrt();
        gradient[k][i][0] /= s;
        gradient[k][i][1] /= s;
      }
    }

    for i in (1..B_SIZE).rev() {
      let k = lattice_selector[i];
      seed = random(seed);
      let j = (seed % B_SIZE_I32) as usize;
      lattice_selector[i] = lattice_selector[j];
      lattice_selector[j] = k;
    }

    for i in 0..B_SIZE + 2 {
      lattice_selector[B_SIZE + i] = lattice_selector[i];
      for k in 0..CHANNELS {
        gradient[k][B_SIZE + i][0] = gradient[k][i][0];
        gradient[k][B_SIZE + i][1] = gradient[k][i][1];
      }
    }

    Self {
      lattice_selector,
      gradient,
    }
  }
}

fn normalize_seed(seed: i32) -> i32 {
  let mut seed = seed as i64;
  if seed <= 0 {
    seed = (-seed).rem_euclid((RAND_M - 1) as i64) + 1;
  }
  if seed > (RAND_M - 1) as i64 {
    seed = (RAND_M - 1) as i64;
  }
  seed as i32
}

fn random(seed: i32) -> i32 {
  let mut result = RAND_A * (seed % RAND_Q) - RAND_R * (seed / RAND_Q);
  if result <= 0 {
    result += RAND_M;
  }
  result
}

fn turbulence(
  color_channel: usize,
  mut x: f64,
  mut y: f64,
  tile_x: f64,
  tile_y: f64,
  tile_width: f64,
  tile_height: f64,
  mut base_freq_x: f64,
  mut base_freq_y: f64,
  num_octaves: u32,
  fractal_sum: bool,
  do_stitching: bool,
  tables: &TurbulenceTables,
) -> f64 {
  // Adjust the base frequencies if necessary for stitching.
  let mut stitch = if do_stitching {
    // When stitching tiled turbulence, the frequencies must be adjusted so that the tile borders
    // will be continuous.
    if base_freq_x != 0.0 {
      let lo_freq = (tile_width * base_freq_x).floor() / tile_width;
      let hi_freq = (tile_width * base_freq_x).ceil() / tile_width;
      if base_freq_x / lo_freq < hi_freq / base_freq_x {
        base_freq_x = lo_freq;
      } else {
        base_freq_x = hi_freq;
      }
    }

    if base_freq_y != 0.0 {
      let lo_freq = (tile_height * base_freq_y).floor() / tile_height;
      let hi_freq = (tile_height * base_freq_y).ceil() / tile_height;
      if base_freq_y / lo_freq < hi_freq / base_freq_y {
        base_freq_y = lo_freq;
      } else {
        base_freq_y = hi_freq;
      }
    }

    // Set up initial stitch values.
    let width = (tile_width * base_freq_x + 0.5) as i32;
    let height = (tile_height * base_freq_y + 0.5) as i32;
    let wrap_x = (tile_x * base_freq_x + PERLIN_N as f64 + width as f64).ceil() as i32;
    let wrap_y = (tile_y * base_freq_y + PERLIN_N as f64 + height as f64).ceil() as i32;
    Some(StitchInfo {
      width,
      height,
      wrap_x,
      wrap_y,
    })
  } else {
    None
  };

  let mut sum = 0.0;
  x *= base_freq_x;
  y *= base_freq_y;
  let mut ratio = 1.0;

  for _ in 0..num_octaves {
    let value = noise2(color_channel, x, y, tables, stitch);
    if fractal_sum {
      sum += value / ratio;
    } else {
      sum += value.abs() / ratio;
    }

    x *= 2.0;
    y *= 2.0;
    ratio *= 2.0;

    if let Some(ref mut stitch) = stitch {
      // Update stitch values. Subtracting PerlinN before the multiplication and adding it
      // afterward simplifies to subtracting it once.
      stitch.width *= 2;
      stitch.wrap_x = 2 * stitch.wrap_x - PERLIN_N;
      stitch.height *= 2;
      stitch.wrap_y = 2 * stitch.wrap_y - PERLIN_N;
    }
  }

  sum
}

fn noise2(
  color_channel: usize,
  x: f64,
  y: f64,
  tables: &TurbulenceTables,
  stitch_info: Option<StitchInfo>,
) -> f64 {
  let t = x + PERLIN_N as f64;
  let mut bx0 = t as i32;
  let mut bx1 = bx0 + 1;
  let rx0 = t - (t as i64) as f64;
  let rx1 = rx0 - 1.0;

  let t = y + PERLIN_N as f64;
  let mut by0 = t as i32;
  let mut by1 = by0 + 1;
  let ry0 = t - (t as i64) as f64;
  let ry1 = ry0 - 1.0;

  // If stitching, adjust lattice points accordingly.
  if let Some(info) = stitch_info {
    if bx0 > info.wrap_x {
      bx0 -= info.width;
    }
    if bx1 > info.wrap_x {
      bx1 -= info.width;
    }
    if by0 > info.wrap_y {
      by0 -= info.height;
    }
    if by1 > info.wrap_y {
      by1 -= info.height;
    }
  }

  bx0 &= BM;
  bx1 &= BM;
  by0 &= BM;
  by1 &= BM;

  let i = tables.lattice_selector[bx0 as usize];
  let j = tables.lattice_selector[bx1 as usize];
  let b00 = tables.lattice_selector[i + by0 as usize];
  let b10 = tables.lattice_selector[j + by0 as usize];
  let b01 = tables.lattice_selector[i + by1 as usize];
  let b11 = tables.lattice_selector[j + by1 as usize];

  let sx = s_curve(rx0);
  let sy = s_curve(ry0);

  let q = &tables.gradient[color_channel][b00];
  let u = rx0 * q[0] + ry0 * q[1];
  let q = &tables.gradient[color_channel][b10];
  let v = rx1 * q[0] + ry0 * q[1];
  let a = lerp(sx, u, v);

  let q = &tables.gradient[color_channel][b01];
  let u = rx0 * q[0] + ry1 * q[1];
  let q = &tables.gradient[color_channel][b11];
  let v = rx1 * q[0] + ry1 * q[1];
  let b = lerp(sx, u, v);

  lerp(sy, a, b)
}

#[inline]
fn s_curve(t: f64) -> f64 {
  t * t * (3.0 - 2.0 * t)
}

#[inline]
fn lerp(t: f64, a: f64, b: f64) -> f64 {
  a + t * (b - a)
}

#[cfg(test)]
mod tests {
  use super::*;
  use rayon::ThreadPoolBuilder;

  fn render_bytes(
    seed: i32,
    color_interpolation_filters: super::super::ColorInterpolationFilters,
  ) -> Vec<u8> {
    const WIDTH: u32 = 32;
    const HEIGHT: u32 = 32;

    let filter_region = Rect::from_xywh(0.0, 0.0, WIDTH as f32, HEIGHT as f32);
    let css_bbox = filter_region;
    let pixmap = render_turbulence(
      WIDTH,
      HEIGHT,
      filter_region,
      (0.08, 0.13),
      seed,
      3,
      false,
      super::super::TurbulenceType::Turbulence,
      color_interpolation_filters,
      super::super::SvgFilterUnits::UserSpaceOnUse,
      css_bbox,
      1.0,
      1.0,
      (0.0, 0.0),
    )
    .expect("render should succeed")
    .expect("expected pixmap");

    pixmap.data().to_vec()
  }

  #[test]
  fn turbulence_render_is_byte_identical_for_same_seed() {
    let pool = ThreadPoolBuilder::new()
      .num_threads(4)
      .build()
      .expect("thread pool");
    let first = pool.install(|| render_bytes(0, super::super::ColorInterpolationFilters::SRGB));
    let second = pool.install(|| render_bytes(0, super::super::ColorInterpolationFilters::SRGB));
    assert_eq!(first, second);
  }

  #[test]
  fn turbulence_raster_is_deterministic_across_thread_counts() {
    let single = ThreadPoolBuilder::new()
      .num_threads(1)
      .build()
      .expect("thread pool");
    let parallel = ThreadPoolBuilder::new()
      .num_threads(4)
      .build()
      .expect("thread pool");

    let single_bytes =
      single.install(|| render_bytes(42, super::super::ColorInterpolationFilters::LinearRGB));
    let parallel_bytes =
      parallel.install(|| render_bytes(42, super::super::ColorInterpolationFilters::LinearRGB));

    assert_eq!(single_bytes, parallel_bytes);
  }

  #[test]
  fn turbulence_render_differs_for_different_seeds() {
    let pool = ThreadPoolBuilder::new()
      .num_threads(4)
      .build()
      .expect("thread pool");
    let a = pool.install(|| render_bytes(0, super::super::ColorInterpolationFilters::SRGB));
    let b = pool.install(|| render_bytes(2, super::super::ColorInterpolationFilters::SRGB));
    assert_eq!(a.len(), b.len(), "expected same output size");
    assert!(
      a.iter().zip(&b).any(|(a, b)| a != b),
      "expected different seeds to affect output"
    );
  }

  #[test]
  fn turbulence_render_is_deterministic_under_rayon_pool() {
    let baseline = render_bytes(0, super::super::ColorInterpolationFilters::SRGB);
    let pool = ThreadPoolBuilder::new()
      .num_threads(4)
      .build()
      .expect("thread pool");

    pool.install(|| {
      let outputs: Vec<Vec<u8>> = (0..16usize)
        .into_par_iter()
        .map(|_| render_bytes(0, super::super::ColorInterpolationFilters::SRGB))
        .collect();
      for (idx, output) in outputs.iter().enumerate() {
        assert_eq!(output, &baseline, "output differed at iteration {idx}");
      }
    });
  }
}
