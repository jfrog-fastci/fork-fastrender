use crate::error::RenderStage;
use crate::geometry::Rect;
use crate::paint::pixmap::new_pixmap;
use crate::render_control::{active_deadline, check_active, with_deadline};
use rayon::prelude::*;
use tiny_skia::{Pixmap, PremultipliedColorU8};

use super::{
  ColorInterpolationFilters, RenderResult, SvgFilterUnits, TurbulenceType, FILTER_DEADLINE_STRIDE,
};

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
  seed: u32,
  octaves: u32,
  stitch_tiles: bool,
  kind: TurbulenceType,
  color_interpolation_filters: ColorInterpolationFilters,
  primitive_units: SvgFilterUnits,
  css_bbox: Rect,
  scale_x: f32,
  scale_y: f32,
  surface_origin: (f32, f32),
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

  let perm_r = build_permutation(seed);
  let perm_g = build_permutation(seed.wrapping_add(1));
  let perm_b = build_permutation(seed.wrapping_add(2));

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
  let origin_x = if surface_origin.0.is_finite() {
    surface_origin.0
  } else {
    0.0
  };
  let origin_y = if surface_origin.1.is_finite() {
    surface_origin.1
  } else {
    0.0
  };

  let bbox = Rect::from_xywh(
    css_bbox.x() + origin_x,
    css_bbox.y() + origin_y,
    css_bbox.width(),
    css_bbox.height(),
  );

  let octaves = octaves.max(1);
  let normalization: f32 = (0..octaves).map(|i| 0.5_f32.powi(i as i32)).sum();

  let row_len = output_width as usize;
  let start_x = region.x as usize;
  let end_x = start_x + region.width as usize;
  let base_freq_x = base_frequency.0.abs();
  let base_freq_y = base_frequency.1.abs();
  let base_freq_x = if base_freq_x.is_finite() { base_freq_x } else { 0.0 };
  let base_freq_y = if base_freq_y.is_finite() { base_freq_y } else { 0.0 };

  let extent_px_x = region.width.saturating_sub(1) as f32;
  let extent_px_y = region.height.saturating_sub(1) as f32;

  let bbox_w = bbox.width().abs();
  let bbox_h = bbox.height().abs();
  let extent_x = match primitive_units {
    SvgFilterUnits::UserSpaceOnUse => extent_px_x / scale_x,
    SvgFilterUnits::ObjectBoundingBox => {
      if bbox_w > 0.0 {
        extent_px_x / scale_x / bbox_w
      } else {
        0.0
      }
    }
  };
  let extent_y = match primitive_units {
    SvgFilterUnits::UserSpaceOnUse => extent_px_y / scale_y,
    SvgFilterUnits::ObjectBoundingBox => {
      if bbox_h > 0.0 {
        extent_px_y / scale_y / bbox_h
      } else {
        0.0
      }
    }
  };

  let deadline = active_deadline();
  pixmap
    .pixels_mut()
    .par_chunks_mut(row_len)
    .enumerate()
    .skip(region.y as usize)
    .take(region.height as usize)
    .try_for_each(|(y, row)| {
      with_deadline(deadline.as_ref(), || -> RenderResult<()> {
        let row_slice = &mut row[start_x..end_x];
        for (x_offset, px) in row_slice.iter_mut().enumerate() {
          if x_offset % FILTER_DEADLINE_STRIDE == 0 {
            check_active(RenderStage::Paint)?;
          }
          let x = (start_x + x_offset) as f32;
          let y = y as f32;

          let css_x = origin_x + x / scale_x;
          let css_y = origin_y + y / scale_y;

          let (coord_x, coord_y) = match primitive_units {
            SvgFilterUnits::UserSpaceOnUse => (css_x, css_y),
            SvgFilterUnits::ObjectBoundingBox => {
              let nx = if bbox_w > 0.0 {
                (css_x - bbox.min_x()) / bbox_w
              } else {
                0.0
              };
              let ny = if bbox_h > 0.0 {
                (css_y - bbox.min_y()) / bbox_h
              } else {
                0.0
              };
              (nx, ny)
            }
          };

          let render_channel = |perm: &[u8; 512]| -> f32 {
            let mut freq_x = base_freq_x;
            let mut freq_y = base_freq_y;
            let mut amplitude = 1.0;
            let mut value = 0.0;

            for _ in 0..octaves {
              let (freq_x_adj, wrap_x) = adjust_frequency(freq_x, extent_x, stitch_tiles);
              let (freq_y_adj, wrap_y) = adjust_frequency(freq_y, extent_y, stitch_tiles);
              let noise = if freq_x_adj == 0.0 && freq_y_adj == 0.0 {
                0.0
              } else {
                perlin(coord_x * freq_x_adj, coord_y * freq_y_adj, perm, wrap_x, wrap_y)
              };
              let noise = match kind {
                TurbulenceType::FractalNoise => noise,
                TurbulenceType::Turbulence => noise.abs(),
              };
              value += noise * amplitude;
              freq_x *= 2.0;
              freq_y *= 2.0;
              amplitude *= 0.5;
            }

            if normalization > 0.0 {
              value / normalization
            } else {
              0.0
            }
          };

          let map_result = |v: f32| match kind {
            TurbulenceType::FractalNoise => v * 0.5 + 0.5,
            TurbulenceType::Turbulence => v,
          };

          let encode_rgb = |v: f32| -> u8 {
            let mapped = map_result(v).clamp(0.0, 1.0);
            let encoded = match color_interpolation_filters {
              ColorInterpolationFilters::SRGB => mapped,
              ColorInterpolationFilters::LinearRGB => super::linear_to_srgb(mapped),
            };
            (encoded * 255.0).round().clamp(0.0, 255.0) as u8
          };

          let r = encode_rgb(render_channel(&perm_r));
          let g = encode_rgb(render_channel(&perm_g));
          let b = encode_rgb(render_channel(&perm_b));

          *px = PremultipliedColorU8::from_rgba(r, g, b, 255)
            .unwrap_or(PremultipliedColorU8::TRANSPARENT);
        }
        Ok(())
      })
    })?;

  Ok(Some(pixmap))
}

fn adjust_frequency(freq: f32, extent: f32, stitch: bool) -> (f32, Option<i32>) {
  if !stitch || extent <= f32::EPSILON || !extent.is_finite() || !freq.is_finite() {
    return (freq, None);
  }
  let mut wrap = (freq * extent).round() as i32;
  if wrap == 0 {
    if freq == 0.0 {
      return (0.0, None);
    }
    wrap = 1;
  }
  if wrap < 0 {
    wrap = -wrap;
  }
  let adjusted = wrap as f32 / extent;
  (adjusted, Some(wrap))
}

fn perlin(x: f32, y: f32, perm: &[u8; 512], wrap_x: Option<i32>, wrap_y: Option<i32>) -> f32 {
  let xi0 = x.floor() as i32;
  let yi0 = y.floor() as i32;
  let xf = x - xi0 as f32;
  let yf = y - yi0 as f32;

  let xi1 = xi0 + 1;
  let yi1 = yi0 + 1;

  let u = fade(xf);
  let v = fade(yf);

  let n00 = grad(hash(xi0, yi0, perm, wrap_x, wrap_y), xf, yf);
  let n10 = grad(hash(xi1, yi0, perm, wrap_x, wrap_y), xf - 1.0, yf);
  let n01 = grad(hash(xi0, yi1, perm, wrap_x, wrap_y), xf, yf - 1.0);
  let n11 = grad(hash(xi1, yi1, perm, wrap_x, wrap_y), xf - 1.0, yf - 1.0);

  let x1 = lerp(n00, n10, u);
  let x2 = lerp(n01, n11, u);
  lerp(x1, x2, v)
}

fn hash(xi: i32, yi: i32, perm: &[u8; 512], wrap_x: Option<i32>, wrap_y: Option<i32>) -> u8 {
  let xi = wrap_index(xi, wrap_x);
  let yi = wrap_index(yi, wrap_y);
  perm[(perm[xi] as usize + yi) & 255]
}

fn wrap_index(idx: i32, wrap: Option<i32>) -> usize {
  let value = match wrap {
    Some(period) if period > 0 => {
      let mut v = idx % period;
      if v < 0 {
        v += period;
      }
      v as usize
    }
    _ => (idx & 255) as usize,
  };
  value & 255
}

fn fade(t: f32) -> f32 {
  t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
  a + t * (b - a)
}

fn grad(hash: u8, x: f32, y: f32) -> f32 {
  let h = hash & 7;
  let u = if h < 4 { x } else { y };
  let v = if h < 4 { y } else { x };
  let a = if (h & 1) == 0 { u } else { -u };
  let b = if (h & 2) == 0 { v } else { -v };
  a + b
}

fn build_permutation(seed: u32) -> [u8; 512] {
  let mut source = [0u8; 256];
  for (i, v) in source.iter_mut().enumerate() {
    *v = i as u8;
  }
  let mut rng = XorShift32::new(seed);
  for i in (1..256).rev() {
    let j = (rng.next_u32() % ((i + 1) as u32)) as usize;
    source.swap(i, j);
  }
  let mut perm = [0u8; 512];
  for i in 0..512 {
    perm[i] = source[i & 255];
  }
  perm
}

#[derive(Clone)]
struct XorShift32 {
  state: u32,
}

impl XorShift32 {
  fn new(seed: u32) -> Self {
    let state = seed.wrapping_add(1).wrapping_mul(0x9e37_79b9);
    Self { state }
  }

  fn next_u32(&mut self) -> u32 {
    let mut x = self.state;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    self.state = x;
    x
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use rayon::ThreadPoolBuilder;

  fn render_bytes(
    seed: u32,
    color_interpolation_filters: super::super::ColorInterpolationFilters,
  ) -> Vec<u8> {
    const WIDTH: u32 = 32;
    const HEIGHT: u32 = 32;

    let filter_region = Rect::from_xywh(0.0, 0.0, WIDTH as f32, HEIGHT as f32);
    let pixmap = render_turbulence(
      WIDTH,
      HEIGHT,
      filter_region,
      (0.13, 0.27),
      seed,
      3,
      false,
      super::super::TurbulenceType::Turbulence,
      color_interpolation_filters,
      super::super::SvgFilterUnits::UserSpaceOnUse,
      filter_region,
      1.0,
      1.0,
      (0.0, 0.0),
    )
    .expect("render should succeed")
    .expect("expected pixmap");

    pixmap.data().to_vec()
  }

  #[test]
  fn turbulence_raster_is_deterministic_for_same_seed() {
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
  fn turbulence_different_seeds_produce_different_output() {
    let pool = ThreadPoolBuilder::new()
      .num_threads(4)
      .build()
      .expect("thread pool");
    let a = pool.install(|| render_bytes(1, super::super::ColorInterpolationFilters::SRGB));
    let b = pool.install(|| render_bytes(2, super::super::ColorInterpolationFilters::SRGB));
    assert_eq!(a.len(), b.len(), "expected same output size");
    assert!(
      a.iter().zip(&b).any(|(a, b)| a != b),
      "expected different seeds to affect output"
    );
  }
}
