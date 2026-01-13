use crate::debug::runtime;
use crate::error::{RenderError, RenderStage};
use crate::geometry::Point;
#[cfg(test)]
use crate::paint::pixmap::new_pixmap;
use crate::paint::pixmap::new_pixmap_uninitialized;
use crate::render_control::{
  active_deadline, check_active, check_active_periodic, with_deadline, RenderDeadline,
};
use crate::style::color::Rgba;
use lru::LruCache;
use rayon::prelude::*;
use rustc_hash::FxHashMap;
use rustc_hash::FxHasher;
use std::hash::BuildHasherDefault;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tiny_skia::{ColorU8, Pixmap, PixmapMut, PremultipliedColorU8, SpreadMode};

const DEADLINE_PIXELS_STRIDE: usize = 16 * 1024;
const GRADIENT_PARALLEL_THRESHOLD_PIXELS: usize = 1_000_000;
const GRADIENT_PARALLEL_MIN_TIMEOUT: Duration = Duration::from_millis(500);

#[inline(always)]
fn gradient_allow_parallel(deadline: Option<&RenderDeadline>) -> bool {
  deadline
    .and_then(RenderDeadline::timeout_limit)
    .map_or(true, |limit| limit >= GRADIENT_PARALLEL_MIN_TIMEOUT)
}

const DEFAULT_GRADIENT_PIXMAP_CACHE_ITEMS: usize = 64;
// Rasterized gradients can be extremely large (e.g. full-size mask/border-image gradients on
// long-scrolling pages). Keep the cache modest and bounded via LRU eviction to avoid runaway
// memory usage when a page triggers unique gradients.
const DEFAULT_GRADIENT_PIXMAP_CACHE_BYTES: usize = 64 * 1024 * 1024;
const ENV_GRADIENT_PIXMAP_CACHE_ITEMS: &str = "FASTR_GRADIENT_PIXMAP_CACHE_ITEMS";
const ENV_GRADIENT_PIXMAP_CACHE_BYTES: &str = "FASTR_GRADIENT_PIXMAP_CACHE_BYTES";

// Ordered dither matrix used by Chrome/Skia when quantizing gradients to 8-bit channels.
//
// Values are in the range 0-15. Indexing is `idx = (y & 3) * 4 + (x & 3)`.
//
// We keep this as a small integer table and derive the floating dither offset on demand:
// `dither = (m + 0.5) / 16.0`.
pub(crate) const BAYER_4X4_XY: [u8; 16] = [
  0, 12, 3, 15, //
  8, 4, 11, 7, //
  2, 14, 1, 13, //
  10, 6, 9, 5, //
];

// Chrome/Skia switches to an 8×8 ordered dither pattern for large gradients. Empirically, the
// *exact* 8×8 matrix depends on how many device pixels correspond to a single 8-bit step within the
// gradient (i.e. how banding-prone the slope is). We model this by selecting between a few
// observed 8×8 matrices.
//
// Values are in the range 0-63 and were extracted from Chrome output.
//
// Indexing is `idx = (y & 7) * 8 + (x & 7)` and the floating dither offset is:
// `dither = (m + 0.5) / 64.0`.
pub(crate) static BAYER_8X8_XY: [u8; 64] = [
  15, 48, 0, 60, 12, 51, 3, 63, //
  44, 19, 35, 31, 47, 16, 32, 28, //
  7, 56, 8, 52, 4, 59, 11, 55, //
  36, 27, 43, 23, 39, 24, 40, 20, //
  13, 50, 2, 62, 14, 49, 1, 61, //
  46, 17, 33, 29, 45, 18, 34, 30, //
  5, 58, 10, 54, 6, 57, 9, 53, //
  38, 25, 41, 21, 37, 26, 42, 22, //
];

// 8×8 dither matrix used when the gradient is steeper (fewer pixels per 8-bit step). Extracted
// from Chrome output for a 0→1 ramp over 512px (`tests/pages/fixtures/gradient_dither_ramp_small_delta_width_512`).
pub(crate) static BAYER_8X8_MEDIUM_XY: [u8; 64] = [
  12, 48, 3, 60, 15, 51, 0, 63, //
  32, 28, 44, 19, 35, 31, 47, 16, //
  4, 56, 11, 52, 7, 59, 8, 55, //
  40, 20, 36, 27, 43, 23, 39, 24, //
  14, 50, 1, 62, 13, 49, 2, 61, //
  34, 30, 46, 17, 33, 29, 45, 18, //
  6, 58, 9, 54, 5, 57, 10, 53, //
  42, 22, 38, 25, 41, 21, 37, 26, //
];

// Classic 8×8 Bayer matrix, used by Chrome for even steeper ramps (e.g. 0→1 over 128px).
pub(crate) static BAYER_8X8_CLASSIC_XY: [u8; 64] = [
  0, 48, 12, 60, 3, 51, 15, 63, //
  32, 16, 44, 28, 35, 19, 47, 31, //
  8, 56, 4, 52, 11, 59, 7, 55, //
  40, 24, 36, 20, 43, 27, 39, 23, //
  2, 50, 14, 62, 1, 49, 13, 61, //
  34, 18, 46, 30, 33, 17, 45, 29, //
  10, 58, 6, 54, 9, 57, 5, 53, //
  42, 26, 38, 22, 41, 25, 37, 21, //
];

#[inline(always)]
fn linear_gradient_use_8x8_dither(bucket: u16) -> bool {
  // `gradient_bucket` returns at least 64. Empirically, Chrome sticks to a 4×4 matrix for small
  // gradients (bucket == 64) but switches to an 8×8 matrix once the rasterized gradient is larger.
  bucket > 64
}

#[inline(always)]
fn linear_gradient_select_8x8_dither_table(
  lut: &GradientLut,
  gradient_len: f32,
) -> &'static [u8; 64] {
  // Heuristic based on observed Chrome output:
  //
  // - Very gentle ramps (e.g. 0→1 over ~1024px) use `BAYER_8X8_XY`
  // - Medium ramps (e.g. 0→1 over ~512px, or 0→2 over ~1024px) use `BAYER_8X8_MEDIUM_XY`
  // - Steeper ramps use the classic Bayer 8×8 matrix.
  //
  // We estimate the maximum number of device pixels per 8-bit step by inspecting each stop
  // segment's span and its maximum per-channel delta (in premultiplied space).
  let mut max_pixels_per_step = 0.0f32;
  if gradient_len.is_finite() && gradient_len > 0.0 {
    let stop_count = lut.stop_positions.len().min(lut.stop_colors.len());
    if stop_count >= 2 {
      for i in 0..stop_count - 1 {
        let span = (lut.stop_positions[i + 1] - lut.stop_positions[i]).abs();
        if !span.is_finite() || span <= 0.0 {
          continue;
        }
        let seg_len = gradient_len * span;
        if !seg_len.is_finite() || seg_len <= 0.0 {
          continue;
        }
        let c0 = lut.stop_colors[i];
        let c1 = lut.stop_colors[i + 1];
        let delta = (c1[0] - c0[0])
          .abs()
          .max((c1[1] - c0[1]).abs())
          .max((c1[2] - c0[2]).abs());
        if !delta.is_finite() || delta <= 1e-6 {
          continue;
        }
        let pixels_per_step = seg_len / delta;
        if pixels_per_step.is_finite() {
          max_pixels_per_step = max_pixels_per_step.max(pixels_per_step);
        }
      }
    }
  }

  if !(max_pixels_per_step.is_finite() && max_pixels_per_step > 0.0) {
    // Degenerate/flat gradients (or missing metadata) don't have a meaningful slope; fall back to
    // the most conservative pattern.
    return &BAYER_8X8_XY;
  }

  let pps_bucket = gradient_bucket(max_pixels_per_step.ceil().clamp(0.0, u32::MAX as f32) as u32);

  if pps_bucket >= 1024 {
    &BAYER_8X8_XY
  } else if pps_bucket >= 512 {
    &BAYER_8X8_MEDIUM_XY
  } else {
    &BAYER_8X8_CLASSIC_XY
  }
}

#[inline(always)]
fn linear_gradient_dither_band_and_dom_delta(src: [f32; 4], start: [f32; 4]) -> (i32, f32) {
  let dr = src[0] - start[0];
  let dg = src[1] - start[1];
  let db = src[2] - start[2];

  let mut max_abs = dr.abs();
  let mut dom_delta = dr;
  let abs_g = dg.abs();
  if abs_g >= max_abs {
    max_abs = abs_g;
    dom_delta = dg;
  }
  let abs_b = db.abs();
  if abs_b >= max_abs {
    max_abs = abs_b;
    dom_delta = db;
  }
  (max_abs as i32, dom_delta)
}

#[inline(always)]
fn linear_gradient_8x8_dither_shift_x(
  dither_table: &[u8; 64],
  band: i32,
  seg: i32,
  dom_delta: f32,
  multi_stop: bool,
) -> (usize, bool) {
  if std::ptr::eq(dither_table, &BAYER_8X8_CLASSIC_XY) {
    if multi_stop {
      let shift = if dom_delta >= 0.0 {
        (((band + 5) >> 4) * 2) as usize
      } else {
        (4 + ((band >> 3) * 2)) as usize
      };
      (shift, false)
    } else {
      let step = if dom_delta >= 0.0 { 2 } else { 6 };
      (((band >> 3) * step) as usize, false)
    }
  } else {
    let shifted = ((band + seg) & 1) != 0;
    (if shifted { 4usize } else { 0usize }, shifted)
  }
}

#[inline]
fn f32_to_canonical_bits(value: f32) -> u32 {
  if value == 0.0 {
    0.0f32.to_bits()
  } else {
    value.to_bits()
  }
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
pub enum SpreadModeKey {
  Pad,
  Repeat,
  Reflect,
}

impl From<SpreadMode> for SpreadModeKey {
  fn from(value: SpreadMode) -> Self {
    match value {
      SpreadMode::Pad => SpreadModeKey::Pad,
      SpreadMode::Repeat => SpreadModeKey::Repeat,
      SpreadMode::Reflect => SpreadModeKey::Reflect,
    }
  }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct GradientStats {
  pub pixels: u64,
  pub duration: Duration,
}

impl GradientStats {
  pub fn record(&mut self, pixels: u64, duration: Duration) {
    self.pixels = self.pixels.saturating_add(pixels);
    self.duration += duration;
  }

  pub fn merge(&mut self, other: &GradientStats) {
    self.pixels = self.pixels.saturating_add(other.pixels);
    self.duration += other.duration;
  }

  pub fn millis(&self) -> f64 {
    self.duration.as_secs_f64() * 1000.0
  }
}

type GradientPixmapHasher = BuildHasherDefault<FxHasher>;

#[derive(Clone, Copy, Debug)]
pub struct GradientPixmapCacheConfig {
  pub max_items: usize,
  pub max_bytes: usize,
}

impl GradientPixmapCacheConfig {
  pub fn from_env() -> Self {
    let toggles = runtime::runtime_toggles();
    let max_items = toggles.usize_with_default(
      ENV_GRADIENT_PIXMAP_CACHE_ITEMS,
      DEFAULT_GRADIENT_PIXMAP_CACHE_ITEMS,
    );
    let max_bytes = toggles.usize_with_default(
      ENV_GRADIENT_PIXMAP_CACHE_BYTES,
      DEFAULT_GRADIENT_PIXMAP_CACHE_BYTES,
    );
    if max_items == 0 || max_bytes == 0 {
      // Both knobs meaningfully bound memory usage; treat either one being set to 0 as a request
      // to disable caching entirely.
      return Self {
        max_items: 0,
        max_bytes: 0,
      };
    }
    Self {
      max_items,
      max_bytes,
    }
  }
}

impl Default for GradientPixmapCacheConfig {
  fn default() -> Self {
    Self::from_env()
  }
}

#[derive(Clone, Hash, PartialEq, Eq)]
pub struct GradientPixmapCacheKey {
  kind: GradientPixmapCacheKeyKind,
  width: u32,
  height: u32,
  params: Vec<u32>,
  lut_key: Option<GradientCacheKey>,
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
enum GradientPixmapCacheKeyKind {
  Linear,
  Conic,
  ConicScaled,
  Radial,
}

impl GradientPixmapCacheKey {
  pub fn linear(
    width: u32,
    height: u32,
    start: Point,
    end: Point,
    spread: SpreadMode,
    stops: &[(f32, Rgba)],
    bucket: u16,
    dither_phase: u8,
  ) -> Option<Self> {
    if width == 0 || height == 0 || stops.is_empty() {
      return None;
    }
    if !start.x.is_finite() || !start.y.is_finite() || !end.x.is_finite() || !end.y.is_finite() {
      return None;
    }
    let period = gradient_period(stops);
    Some(Self {
      kind: GradientPixmapCacheKeyKind::Linear,
      width,
      height,
      params: vec![
        f32_to_canonical_bits(start.x),
        f32_to_canonical_bits(start.y),
        f32_to_canonical_bits(end.x),
        f32_to_canonical_bits(end.y),
        u32::from(dither_phase),
      ],
      lut_key: Some(GradientCacheKey::new(stops, spread, period, bucket)),
    })
  }

  pub fn conic(
    width: u32,
    height: u32,
    center: Point,
    start_angle: f32,
    spread: SpreadMode,
    stops: &[(f32, Rgba)],
    bucket: u16,
  ) -> Option<Self> {
    if width == 0 || height == 0 || stops.is_empty() {
      return None;
    }
    if !center.x.is_finite() || !center.y.is_finite() || !start_angle.is_finite() {
      return None;
    }
    let period = gradient_period(stops);
    // Canonicalize angle so angles that differ by 2π share cache entries.
    let canonical_angle = start_angle.rem_euclid(std::f32::consts::PI * 2.0);
    Some(Self {
      kind: GradientPixmapCacheKeyKind::Conic,
      width,
      height,
      params: vec![
        f32_to_canonical_bits(center.x),
        f32_to_canonical_bits(center.y),
        f32_to_canonical_bits(canonical_angle),
      ],
      lut_key: Some(GradientCacheKey::new(stops, spread, period, bucket)),
    })
  }

  pub fn conic_scaled(
    width: u32,
    height: u32,
    center: Point,
    start_angle: f32,
    spread: SpreadMode,
    stops: &[(f32, Rgba)],
    bucket: u16,
    scale_x: f32,
    scale_y: f32,
  ) -> Option<Self> {
    if width == 0 || height == 0 || stops.is_empty() {
      return None;
    }
    if !center.x.is_finite()
      || !center.y.is_finite()
      || !start_angle.is_finite()
      || !scale_x.is_finite()
      || !scale_y.is_finite()
    {
      return None;
    }
    let period = gradient_period(stops);
    let canonical_angle = start_angle.rem_euclid(std::f32::consts::PI * 2.0);
    Some(Self {
      kind: GradientPixmapCacheKeyKind::ConicScaled,
      width,
      height,
      params: vec![
        f32_to_canonical_bits(center.x),
        f32_to_canonical_bits(center.y),
        f32_to_canonical_bits(canonical_angle),
        f32_to_canonical_bits(scale_x),
        f32_to_canonical_bits(scale_y),
      ],
      lut_key: Some(GradientCacheKey::new(stops, spread, period, bucket)),
    })
  }

  pub fn radial(
    width: u32,
    height: u32,
    center: Point,
    radii: Point,
    spread: SpreadMode,
    stops: &[(f32, Rgba)],
    dither_phase: u8,
  ) -> Option<Self> {
    if width == 0 || height == 0 || stops.is_empty() {
      return None;
    }
    if !center.x.is_finite()
      || !center.y.is_finite()
      || !radii.x.is_finite()
      || !radii.y.is_finite()
      || radii.x <= 0.0
      || radii.y <= 0.0
    {
      return None;
    }
    Some(Self {
      kind: GradientPixmapCacheKeyKind::Radial,
      width,
      height,
      params: vec![
        f32_to_canonical_bits(center.x),
        f32_to_canonical_bits(center.y),
        f32_to_canonical_bits(radii.x),
        f32_to_canonical_bits(radii.y),
        u32::from(dither_phase),
      ],
      lut_key: Some(GradientCacheKey::new(
        stops,
        spread,
        gradient_period(stops),
        0,
      )),
    })
  }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct GradientPixmapCacheStats {
  pub hits: u64,
  pub misses: u64,
  pub bytes: u64,
  pub items: usize,
}

struct GradientPixmapCacheInner {
  lru: LruCache<GradientPixmapCacheKey, Arc<Pixmap>, GradientPixmapHasher>,
  hits: u64,
  misses: u64,
  bytes: usize,
  config: GradientPixmapCacheConfig,
}

impl GradientPixmapCacheInner {
  fn new(config: GradientPixmapCacheConfig) -> Self {
    Self {
      lru: LruCache::unbounded_with_hasher(GradientPixmapHasher::default()),
      hits: 0,
      misses: 0,
      bytes: 0,
      config,
    }
  }

  fn evict(&mut self) {
    while (self.config.max_items > 0 && self.lru.len() > self.config.max_items)
      || (self.config.max_bytes > 0 && self.bytes > self.config.max_bytes)
    {
      if let Some((_key, value)) = self.lru.pop_lru() {
        self.bytes = self.bytes.saturating_sub(value.data().len());
      } else {
        break;
      }
    }
  }

  fn stats(&self) -> GradientPixmapCacheStats {
    GradientPixmapCacheStats {
      hits: self.hits,
      misses: self.misses,
      bytes: self.bytes as u64,
      items: self.lru.len(),
    }
  }
}

#[derive(Clone)]
pub struct GradientPixmapCache {
  inner: Arc<Mutex<GradientPixmapCacheInner>>,
}

impl Default for GradientPixmapCache {
  fn default() -> Self {
    Self::new(GradientPixmapCacheConfig::default())
  }
}

impl GradientPixmapCache {
  pub fn new(config: GradientPixmapCacheConfig) -> Self {
    Self {
      inner: Arc::new(Mutex::new(GradientPixmapCacheInner::new(config))),
    }
  }

  pub fn snapshot(&self) -> GradientPixmapCacheStats {
    let guard = self
      .inner
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.stats()
  }

  pub fn get_or_insert<F>(
    &self,
    key: GradientPixmapCacheKey,
    build: F,
  ) -> Result<Option<Arc<Pixmap>>, RenderError>
  where
    F: FnOnce() -> Result<Option<Pixmap>, RenderError>,
  {
    // Fast path: caching disabled.
    {
      let guard = self
        .inner
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
      if guard.config.max_items == 0 || guard.config.max_bytes == 0 {
        drop(guard);
        return Ok(build()?.map(Arc::new));
      }
    }

    {
      let mut guard = match self.inner.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
          let mut guard = poisoned.into_inner();
          // Cache is a performance optimization. If we panic while holding the lock, clear the
          // cache so we don't keep partially inserted entries around.
          guard.lru.clear();
          guard.hits = 0;
          guard.misses = 0;
          guard.bytes = 0;
          guard
        }
      };
      if let Some(found) = guard.lru.get(&key).cloned() {
        guard.hits = guard.hits.saturating_add(1);
        return Ok(Some(found));
      }
      guard.misses = guard.misses.saturating_add(1);
    }

    let Some(pixmap) = build()? else {
      return Ok(None);
    };
    let weight = pixmap.data().len();

    let arc = Arc::new(pixmap);

    let mut guard = match self.inner.lock() {
      Ok(guard) => guard,
      Err(poisoned) => {
        let mut guard = poisoned.into_inner();
        guard.lru.clear();
        guard.hits = 0;
        guard.misses = 0;
        guard.bytes = 0;
        guard
      }
    };

    // Another thread may have inserted while we were rasterizing.
    if let Some(found) = guard.lru.get(&key).cloned() {
      guard.hits = guard.hits.saturating_add(1);
      return Ok(Some(found));
    }

    if guard.config.max_bytes > 0 && weight > guard.config.max_bytes {
      return Ok(Some(arc));
    }

    if let Some(existing) = guard.lru.peek(&key) {
      guard.bytes = guard.bytes.saturating_sub(existing.data().len());
    }
    guard.bytes = guard.bytes.saturating_add(weight);
    guard.lru.put(key, arc.clone());
    guard.evict();
    Ok(Some(arc))
  }
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct GradientStopKey {
  pos_bits: u32,
  r: u8,
  g: u8,
  b: u8,
  a_bits: u32,
}

#[derive(Clone, Hash, PartialEq, Eq)]
pub struct GradientCacheKey {
  stops: Vec<GradientStopKey>,
  spread: SpreadModeKey,
  span_bits: u32,
  bucket: u16,
}

impl GradientCacheKey {
  pub fn new(stops: &[(f32, Rgba)], spread: SpreadMode, span: f32, bucket: u16) -> Self {
    Self {
      stops: stops
        .iter()
        .map(|(pos, color)| GradientStopKey {
          pos_bits: f32_to_canonical_bits(*pos),
          r: color.r,
          g: color.g,
          b: color.b,
          a_bits: f32_to_canonical_bits(color.a),
        })
        .collect(),
      spread: spread.into(),
      span_bits: f32_to_canonical_bits(span),
      bucket,
    }
  }
}

#[derive(Clone)]
pub struct GradientLut {
  colors: Arc<Vec<[f32; 4]>>,
  segments: Arc<Vec<u16>>,
  stop_positions: Arc<Vec<f32>>,
  stop_colors: Arc<Vec<[f32; 4]>>,
  spread: SpreadModeKey,
  offset: f32,
  span: f32,
  scale: f32,
  last_idx: usize,
  first: PremultipliedColorU8,
  last: PremultipliedColorU8,
}

impl GradientLut {
  #[inline(always)]
  fn sample_mapped_f32(&self, t: f32) -> [f32; 4] {
    debug_assert!(t.is_finite());
    debug_assert!(t >= 0.0);
    debug_assert!(self.last_idx > 0);

    let scaled = t * self.scale;
    let idx = scaled as usize;
    if idx >= self.last_idx {
      // SAFETY: idx >= last_idx implies the last entry exists because last_idx > 0.
      return unsafe { *self.colors.get_unchecked(self.last_idx) };
    }
    let frac = scaled - idx as f32;
    if frac <= 0.0 {
      // SAFETY: idx < last_idx implies idx is within the LUT.
      return unsafe { *self.colors.get_unchecked(idx) };
    }
    // SAFETY: idx < last_idx implies idx+1 is within the LUT.
    let c0 = unsafe { *self.colors.get_unchecked(idx) };
    let c1 = unsafe { *self.colors.get_unchecked(idx + 1) };

    // Avoid interpolating across different stop segments. When a stop falls between the two LUT
    // sample positions, blending `c0` and `c1` would incorrectly smear a sharp corner and can
    // produce visible color leakage (e.g. red bleeding into the subsequent segment).
    let seg0 = unsafe { *self.segments.get_unchecked(idx) };
    let seg1 = unsafe { *self.segments.get_unchecked(idx + 1) };
    if seg0 != seg1 {
      // Fast path: a single boundary between adjacent segments.
      let seg0 = seg0 as usize;
      let seg1 = seg1 as usize;
      if seg1 == seg0 + 1 && seg1 < self.stop_positions.len() && seg1 < self.stop_colors.len() {
        let stop_pos = unsafe { *self.stop_positions.get_unchecked(seg1) };
        let stop_color = unsafe { *self.stop_colors.get_unchecked(seg1) };
        // `stop_positions` are stored relative to `offset` (see `build_gradient_lut`), and `t` is
        // already in this same normalized coordinate space.
        let stop_scaled = stop_pos * self.scale;
        let boundary = (stop_scaled - idx as f32).clamp(0.0, 1.0);
        if boundary <= 0.0 {
          // Boundary aligns with the left endpoint.
          let inv = 1.0 - frac;
          return [
            stop_color[0] * inv + c1[0] * frac,
            stop_color[1] * inv + c1[1] * frac,
            stop_color[2] * inv + c1[2] * frac,
            stop_color[3] * inv + c1[3] * frac,
          ];
        }
        if boundary >= 1.0 {
          // Boundary aligns with the right endpoint.
          let inv = 1.0 - frac;
          return [
            c0[0] * inv + stop_color[0] * frac,
            c0[1] * inv + stop_color[1] * frac,
            c0[2] * inv + stop_color[2] * frac,
            c0[3] * inv + stop_color[3] * frac,
          ];
        }
        if frac < boundary {
          let local = (frac / boundary).clamp(0.0, 1.0);
          let inv = 1.0 - local;
          return [
            c0[0] * inv + stop_color[0] * local,
            c0[1] * inv + stop_color[1] * local,
            c0[2] * inv + stop_color[2] * local,
            c0[3] * inv + stop_color[3] * local,
          ];
        }
        let local = ((frac - boundary) / (1.0 - boundary)).clamp(0.0, 1.0);
        let inv = 1.0 - local;
        return [
          stop_color[0] * inv + c1[0] * local,
          stop_color[1] * inv + c1[1] * local,
          stop_color[2] * inv + c1[2] * local,
          stop_color[3] * inv + c1[3] * local,
        ];
      }

      // Rare fallback: multiple stop boundaries within one LUT interval (or missing metadata).
      return self.sample_exact_f32(t);
    }

    let inv = 1.0 - frac;
    [
      c0[0] * inv + c1[0] * frac,
      c0[1] * inv + c1[1] * frac,
      c0[2] * inv + c1[2] * frac,
      c0[3] * inv + c1[3] * frac,
    ]
  }

  #[inline(always)]
  fn sample_mapped_f32_with_segment(&self, t: f32) -> ([f32; 4], u16) {
    debug_assert!(t.is_finite());
    debug_assert!(t >= 0.0);
    debug_assert!(self.last_idx > 0);

    let scaled = t * self.scale;
    let idx = scaled as usize;
    if idx >= self.last_idx {
      let seg = unsafe { *self.segments.get_unchecked(self.last_idx) };
      // SAFETY: idx >= last_idx implies the last entry exists because last_idx > 0.
      return (unsafe { *self.colors.get_unchecked(self.last_idx) }, seg);
    }
    let seg0 = unsafe { *self.segments.get_unchecked(idx) };
    let frac = scaled - idx as f32;
    if frac <= 0.0 {
      // SAFETY: idx < last_idx implies idx is within the LUT.
      return (unsafe { *self.colors.get_unchecked(idx) }, seg0);
    }
    // SAFETY: idx < last_idx implies idx+1 is within the LUT.
    let c0 = unsafe { *self.colors.get_unchecked(idx) };
    let c1 = unsafe { *self.colors.get_unchecked(idx + 1) };

    let seg1 = unsafe { *self.segments.get_unchecked(idx + 1) };
    if seg0 != seg1 {
      let seg0_usize = seg0 as usize;
      let seg1_usize = seg1 as usize;
      if seg1_usize == seg0_usize + 1
        && seg1_usize < self.stop_positions.len()
        && seg1_usize < self.stop_colors.len()
      {
        let stop_pos = unsafe { *self.stop_positions.get_unchecked(seg1_usize) };
        let stop_color = unsafe { *self.stop_colors.get_unchecked(seg1_usize) };
        let stop_scaled = stop_pos * self.scale;
        let boundary = (stop_scaled - idx as f32).clamp(0.0, 1.0);
        if boundary <= 0.0 {
          let inv = 1.0 - frac;
          return (
            [
              stop_color[0] * inv + c1[0] * frac,
              stop_color[1] * inv + c1[1] * frac,
              stop_color[2] * inv + c1[2] * frac,
              stop_color[3] * inv + c1[3] * frac,
            ],
            seg1,
          );
        }
        if boundary >= 1.0 {
          let inv = 1.0 - frac;
          return (
            [
              c0[0] * inv + stop_color[0] * frac,
              c0[1] * inv + stop_color[1] * frac,
              c0[2] * inv + stop_color[2] * frac,
              c0[3] * inv + stop_color[3] * frac,
            ],
            seg0,
          );
        }
        if frac < boundary {
          let local = (frac / boundary).clamp(0.0, 1.0);
          let inv = 1.0 - local;
          return (
            [
              c0[0] * inv + stop_color[0] * local,
              c0[1] * inv + stop_color[1] * local,
              c0[2] * inv + stop_color[2] * local,
              c0[3] * inv + stop_color[3] * local,
            ],
            seg0,
          );
        }
        let local = ((frac - boundary) / (1.0 - boundary)).clamp(0.0, 1.0);
        let inv = 1.0 - local;
        return (
          [
            stop_color[0] * inv + c1[0] * local,
            stop_color[1] * inv + c1[1] * local,
            stop_color[2] * inv + c1[2] * local,
            stop_color[3] * inv + c1[3] * local,
          ],
          seg1,
        );
      }

      // Rare fallback: multiple stop boundaries within one LUT interval (or missing metadata).
      // `sample_exact_f32` already handles this correctly; we also need the segment index for `t`.
      let color = self.sample_exact_f32(t);
      if self.stop_positions.len() < 2 {
        return (color, seg0);
      }
      let last = self.stop_positions.len().saturating_sub(1);
      let t = t.clamp(self.stop_positions[0], self.stop_positions[last]);
      if t <= self.stop_positions[0] {
        return (color, 0);
      }
      if t >= self.stop_positions[last] {
        return (color, last as u16);
      }
      let mut lo = 0usize;
      let mut hi = last;
      while lo + 1 < hi {
        let mid = (lo + hi) / 2;
        if t < self.stop_positions[mid] {
          hi = mid;
        } else {
          lo = mid;
        }
      }
      return (color, lo as u16);
    }

    let inv = 1.0 - frac;
    (
      [
        c0[0] * inv + c1[0] * frac,
        c0[1] * inv + c1[1] * frac,
        c0[2] * inv + c1[2] * frac,
        c0[3] * inv + c1[3] * frac,
      ],
      seg0,
    )
  }

  #[inline]
  fn sample_exact_f32(&self, t: f32) -> [f32; 4] {
    if self.stop_positions.is_empty() || self.stop_colors.is_empty() {
      return [0.0, 0.0, 0.0, 0.0];
    }
    let last = self.stop_positions.len().saturating_sub(1);
    // `t` is already in LUT-local coordinates (offset-subtracted); stop positions are stored in
    // this same coordinate space (see `build_gradient_lut`).
    let t = t.clamp(self.stop_positions[0], self.stop_positions[last]);
    if t <= self.stop_positions[0] {
      return self.stop_colors[0];
    }
    if t >= self.stop_positions[last] {
      return self.stop_colors[last];
    }

    // Find the segment containing `t` (stop positions are assumed sorted ascending).
    let mut lo = 0usize;
    let mut hi = last;
    while lo + 1 < hi {
      let mid = (lo + hi) / 2;
      if t < self.stop_positions[mid] {
        hi = mid;
      } else {
        lo = mid;
      }
    }
    let p0 = self.stop_positions[lo];
    let p1 = self.stop_positions[hi];
    let c0 = self.stop_colors[lo];
    let c1 = self.stop_colors[hi];
    if (p1 - p0).abs() < f32::EPSILON {
      return c0;
    }
    let frac = ((t - p0) / (p1 - p0)).clamp(0.0, 1.0);
    let inv = 1.0 - frac;
    [
      c0[0] * inv + c1[0] * frac,
      c0[1] * inv + c1[1] * frac,
      c0[2] * inv + c1[2] * frac,
      c0[3] * inv + c1[3] * frac,
    ]
  }

  #[inline(always)]
  fn pm_u8_to_f32(color: PremultipliedColorU8) -> [f32; 4] {
    [
      color.red() as f32,
      color.green() as f32,
      color.blue() as f32,
      color.alpha() as f32,
    ]
  }

  #[inline(always)]
  fn sample_pad_f32(&self, t: f32) -> [f32; 4] {
    if self.last_idx == 0 || !t.is_finite() {
      return Self::pm_u8_to_f32(self.first);
    }
    let t = t - self.offset;
    if t < 0.0 {
      return Self::pm_u8_to_f32(self.first);
    }
    if t >= self.span {
      return Self::pm_u8_to_f32(self.last);
    }
    self.sample_mapped_f32(t)
  }

  #[inline(always)]
  fn sample_pad_f32_with_segment(&self, t: f32) -> ([f32; 4], u16) {
    if self.last_idx == 0 || !t.is_finite() {
      return (Self::pm_u8_to_f32(self.first), 0);
    }
    let t = t - self.offset;
    if t < 0.0 {
      return (Self::pm_u8_to_f32(self.first), 0);
    }
    if t >= self.span {
      let last = self.stop_colors.len().saturating_sub(1) as u16;
      return (Self::pm_u8_to_f32(self.last), last);
    }
    self.sample_mapped_f32_with_segment(t)
  }

  #[inline(always)]
  fn sample_repeat_f32(&self, mut t: f32) -> [f32; 4] {
    if self.last_idx == 0 || !t.is_finite() {
      return Self::pm_u8_to_f32(self.first);
    }
    t -= self.offset;
    let p = self.span;
    if p <= 0.0 {
      return Self::pm_u8_to_f32(self.first);
    }
    t = t % p;
    if t < 0.0 {
      t += p;
    }
    self.sample_mapped_f32(t)
  }

  #[inline(always)]
  fn sample_repeat_f32_with_segment(&self, mut t: f32) -> ([f32; 4], u16) {
    if self.last_idx == 0 || !t.is_finite() {
      return (Self::pm_u8_to_f32(self.first), 0);
    }
    t -= self.offset;
    let p = self.span;
    if p <= 0.0 {
      return (Self::pm_u8_to_f32(self.first), 0);
    }
    t = t % p;
    if t < 0.0 {
      t += p;
    }
    self.sample_mapped_f32_with_segment(t)
  }

  #[inline(always)]
  fn sample_reflect_f32(&self, mut t: f32) -> [f32; 4] {
    if self.last_idx == 0 || !t.is_finite() {
      return Self::pm_u8_to_f32(self.first);
    }
    t -= self.offset;
    let p = self.span;
    if p <= 0.0 {
      return Self::pm_u8_to_f32(self.first);
    }
    let two_p = p * 2.0;
    t = t % two_p;
    if t < 0.0 {
      t += two_p;
    }
    if t > p {
      t = two_p - t;
    }
    self.sample_mapped_f32(t)
  }

  #[inline(always)]
  fn sample_reflect_f32_with_segment(&self, mut t: f32) -> ([f32; 4], u16) {
    if self.last_idx == 0 || !t.is_finite() {
      return (Self::pm_u8_to_f32(self.first), 0);
    }
    t -= self.offset;
    let p = self.span;
    if p <= 0.0 {
      return (Self::pm_u8_to_f32(self.first), 0);
    }
    let two_p = p * 2.0;
    t = t % two_p;
    if t < 0.0 {
      t += two_p;
    }
    if t > p {
      t = two_p - t;
    }
    self.sample_mapped_f32_with_segment(t)
  }

  #[inline(always)]
  fn quantize_round(color: [f32; 4]) -> PremultipliedColorU8 {
    // Convert premultiplied f32 channels to premultiplied u8 with round-half-up semantics.
    let alpha = (color[3] + 0.5) as i32;
    let alpha = alpha.clamp(0, 255) as u8;
    let mut r = (color[0] + 0.5) as i32;
    let mut g = (color[1] + 0.5) as i32;
    let mut b = (color[2] + 0.5) as i32;
    r = r.clamp(0, 255);
    g = g.clamp(0, 255);
    b = b.clamp(0, 255);
    let r = (r as u8).min(alpha);
    let g = (g as u8).min(alpha);
    let b = (b as u8).min(alpha);
    PremultipliedColorU8::from_rgba(r, g, b, alpha).unwrap_or(PremultipliedColorU8::TRANSPARENT)
  }

  #[inline(always)]
  pub(crate) fn quantize_dither(color: [f32; 4], dither: f32) -> PremultipliedColorU8 {
    // Convert premultiplied f32 channels to premultiplied u8 using ordered dithering.
    //
    // The dither value is in (0, 1) and is added before truncation (floor for positive inputs).
    let alpha = (color[3] + dither) as i32;
    let alpha = alpha.clamp(0, 255) as u8;
    let mut r = (color[0] + dither) as i32;
    let mut g = (color[1] + dither) as i32;
    let mut b = (color[2] + dither) as i32;
    r = r.clamp(0, 255);
    g = g.clamp(0, 255);
    b = b.clamp(0, 255);
    let r = (r as u8).min(alpha);
    let g = (g as u8).min(alpha);
    let b = (b as u8).min(alpha);
    PremultipliedColorU8::from_rgba(r, g, b, alpha).unwrap_or(PremultipliedColorU8::TRANSPARENT)
  }

  #[inline(always)]
  fn sample_pad(&self, t: f32) -> PremultipliedColorU8 {
    if self.last_idx == 0 || !t.is_finite() {
      return self.first;
    }
    let t = t - self.offset;
    if t < 0.0 {
      return self.first;
    }
    if t >= self.span {
      return self.last;
    }
    Self::quantize_round(self.sample_mapped_f32(t))
  }

  #[inline(always)]
  fn sample_repeat(&self, mut t: f32) -> PremultipliedColorU8 {
    if self.last_idx == 0 || !t.is_finite() {
      return self.first;
    }
    t -= self.offset;
    let p = self.span;
    if p <= 0.0 {
      return self.first;
    }
    t = t % p;
    if t < 0.0 {
      t += p;
    }
    Self::quantize_round(self.sample_mapped_f32(t))
  }

  #[inline(always)]
  fn sample_reflect(&self, mut t: f32) -> PremultipliedColorU8 {
    if self.last_idx == 0 || !t.is_finite() {
      return self.first;
    }
    t -= self.offset;
    let p = self.span;
    if p <= 0.0 {
      return self.first;
    }
    let two_p = p * 2.0;
    t = t % two_p;
    if t < 0.0 {
      t += two_p;
    }
    if t > p {
      t = two_p - t;
    }
    Self::quantize_round(self.sample_mapped_f32(t))
  }

  /// Samples the LUT using its configured spread mode.
  #[inline(always)]
  pub(crate) fn sample(&self, t: f32) -> PremultipliedColorU8 {
    match self.spread {
      SpreadModeKey::Pad => self.sample_pad(t),
      SpreadModeKey::Repeat => self.sample_repeat(t),
      SpreadModeKey::Reflect => self.sample_reflect(t),
    }
  }

  #[inline(always)]
  fn sample_pad_dither(&self, t: f32, dither: f32) -> PremultipliedColorU8 {
    if self.last_idx == 0 || !t.is_finite() {
      return self.first;
    }
    let t = t - self.offset;
    if t < 0.0 {
      return self.first;
    }
    if t >= self.span {
      return self.last;
    }
    Self::quantize_dither(self.sample_mapped_f32(t), dither)
  }

  #[inline(always)]
  fn sample_repeat_dither(&self, mut t: f32, dither: f32) -> PremultipliedColorU8 {
    if self.last_idx == 0 || !t.is_finite() {
      return self.first;
    }
    t -= self.offset;
    let p = self.span;
    if p <= 0.0 {
      return self.first;
    }
    t = t % p;
    if t < 0.0 {
      t += p;
    }
    Self::quantize_dither(self.sample_mapped_f32(t), dither)
  }

  #[inline(always)]
  fn sample_reflect_dither(&self, mut t: f32, dither: f32) -> PremultipliedColorU8 {
    if self.last_idx == 0 || !t.is_finite() {
      return self.first;
    }
    t -= self.offset;
    let p = self.span;
    if p <= 0.0 {
      return self.first;
    }
    let two_p = p * 2.0;
    t = t % two_p;
    if t < 0.0 {
      t += two_p;
    }
    if t > p {
      t = two_p - t;
    }
    Self::quantize_dither(self.sample_mapped_f32(t), dither)
  }
}

#[derive(Clone, Default)]
pub struct GradientLutCache {
  inner: Arc<Mutex<FxHashMap<GradientCacheKey, Arc<GradientLut>>>>,
}

impl GradientLutCache {
  pub fn get_or_build<F>(&self, key: GradientCacheKey, build: F) -> Arc<GradientLut>
  where
    F: FnOnce() -> GradientLut,
  {
    let mut guard = match self.inner.lock() {
      Ok(guard) => guard,
      Err(poisoned) => {
        let mut guard = poisoned.into_inner();
        // This cache is a performance optimization. If a panic happened while holding the lock we
        // may have partially inserted state, so clear everything and rebuild entries on demand.
        guard.clear();
        guard
      }
    };
    if let Some(found) = guard.get(&key) {
      return found.clone();
    }
    let lut = Arc::new(build());
    guard.entry(key).or_insert_with(|| lut.clone()).clone()
  }
}

#[inline(always)]
fn premultiply_rgba(color: Rgba) -> PremultipliedColorU8 {
  let alpha_u8 = (color.a * 255.0).round().clamp(0.0, 255.0) as u8;
  ColorU8::from_rgba(color.r, color.g, color.b, alpha_u8).premultiply()
}

fn build_gradient_lut(
  stops: &[(f32, Rgba)],
  spread: SpreadMode,
  span: f32,
  bucket: u16,
) -> GradientLut {
  let max_idx = bucket.max(1) as usize;
  let step_count = max_idx + 1;
  let max_idx = max_idx as f32;
  let offset = stops.first().map(|(pos, _)| *pos).unwrap_or(0.0);
  let span = span.max(1e-6);
  let mut colors = Vec::with_capacity(step_count);
  let mut segments = Vec::with_capacity(step_count);
  // Store stop positions relative to the first stop (`offset`) so sampling code can normalize `t`
  // by subtracting `offset` once and operate in the normalized [0, span] space. This supports stop
  // ranges outside `[0, 1]`, including negative stop positions.
  let stop_positions: Vec<f32> = stops.iter().map(|(pos, _)| *pos - offset).collect();
  let stop_colors: Vec<[f32; 4]> = stops
    .iter()
    .map(|(_, c)| {
      let a = c.a.clamp(0.0, 1.0);
      [c.r as f32 * a, c.g as f32 * a, c.b as f32 * a, a * 255.0]
    })
    .collect();
  let mut window = stops.windows(2).peekable();
  let mut segment_idx = 0u16;
  for i in 0..step_count {
    let pos = (i as f32 / max_idx) * span;
    while let Some(segment) = window.peek() {
      if pos > segment[1].0 - offset {
        window.next();
        segment_idx = segment_idx.saturating_add(1);
      } else {
        break;
      }
    }
    // NOTE: Interpolate in *premultiplied* alpha space. Interpolating unpremultiplied RGB then
    // multiplying by the interpolated alpha incorrectly squares the alpha contribution (e.g.
    // white->transparent becomes gray->transparent).
    let (pr, pg, pb, a) = if let Some(segment) = window.peek() {
      let (p0, c0) = segment[0];
      let (p1, c1) = segment[1];
      let p0 = p0 - offset;
      let p1 = p1 - offset;
      let a0 = c0.a.clamp(0.0, 1.0);
      let a1 = c1.a.clamp(0.0, 1.0);
      let pr0 = c0.r as f32 * a0;
      let pg0 = c0.g as f32 * a0;
      let pb0 = c0.b as f32 * a0;
      let pr1 = c1.r as f32 * a1;
      let pg1 = c1.g as f32 * a1;
      let pb1 = c1.b as f32 * a1;
      if (p1 - p0).abs() < f32::EPSILON {
        (pr0, pg0, pb0, a0)
      } else {
        let frac = ((pos - p0) / (p1 - p0)).clamp(0.0, 1.0);
        (
          pr0 + (pr1 - pr0) * frac,
          pg0 + (pg1 - pg0) * frac,
          pb0 + (pb1 - pb0) * frac,
          (a0 + (a1 - a0) * frac).clamp(0.0, 1.0),
        )
      }
    } else if let Some((_, c)) = stops.last() {
      let a = c.a.clamp(0.0, 1.0);
      (c.r as f32 * a, c.g as f32 * a, c.b as f32 * a, a)
    } else {
      (0.0, 0.0, 0.0, 0.0)
    };
    let a255 = a * 255.0;
    colors.push([pr, pg, pb, a255]);
    segments.push(segment_idx);
  }

  let colors = Arc::new(colors);
  let segments = Arc::new(segments);
  let stop_positions = Arc::new(stop_positions);
  let stop_colors = Arc::new(stop_colors);
  let last_idx = colors.len().saturating_sub(1);
  // Pad gradients use `first/last` directly when clamping outside the stop range, so make sure
  // these match the actual terminal stop colors rather than the sampled LUT values. This matters
  // when there are duplicate stops at the terminal positions (e.g. a sharp edge at `t==end`),
  // where the LUT sample at the exact position can come from the preceding segment.
  let first = stops
    .first()
    .map(|(_, c)| premultiply_rgba(*c))
    .unwrap_or(PremultipliedColorU8::TRANSPARENT);
  let last = stops
    .last()
    .map(|(_, c)| premultiply_rgba(*c))
    .unwrap_or(first);
  let scale = max_idx / span;

  GradientLut {
    spread: spread.into(),
    offset,
    span,
    scale,
    last_idx,
    first,
    last,
    colors,
    segments,
    stop_positions,
    stop_colors,
  }
}

pub fn gradient_period(stops: &[(f32, Rgba)]) -> f32 {
  match (stops.first(), stops.last()) {
    (Some((start, _)), Some((end, _))) => (*end - *start).max(1e-6),
    _ => 1.0,
  }
}

pub fn gradient_bucket(max_dim: u32) -> u16 {
  let mut bucket = 64u32;
  let target = max_dim.max(64);
  while bucket < target {
    bucket *= 2;
    if bucket >= 4096 {
      bucket = 4096;
      break;
    }
  }
  bucket as u16
}

pub fn get_gradient_lut(
  cache: &GradientLutCache,
  stops: &[(f32, Rgba)],
  spread: SpreadMode,
  bucket: u16,
) -> Arc<GradientLut> {
  let span = gradient_period(stops);
  let key = GradientCacheKey::new(stops, spread, span, bucket);
  cache.get_or_build(key, || build_gradient_lut(stops, spread, span, bucket))
}

pub fn rasterize_linear_gradient(
  width: u32,
  height: u32,
  start: Point,
  end: Point,
  spread: SpreadMode,
  stops: &[(f32, Rgba)],
  cache: &GradientLutCache,
  bucket: u16,
) -> Result<Option<Pixmap>, RenderError> {
  rasterize_linear_gradient_with_phase(width, height, start, end, spread, stops, cache, bucket, 0)
}

fn rasterize_linear_gradient_with_phase(
  width: u32,
  height: u32,
  start: Point,
  end: Point,
  spread: SpreadMode,
  stops: &[(f32, Rgba)],
  cache: &GradientLutCache,
  bucket: u16,
  dither_phase: u8,
) -> Result<Option<Pixmap>, RenderError> {
  check_active(RenderStage::Paint)?;
  if width == 0 || height == 0 || stops.is_empty() {
    return Ok(None);
  }
  let period = gradient_period(stops);

  let dx = end.x - start.x;
  let dy = end.y - start.y;
  let denom = dx * dx + dy * dy;
  let mut pixmap = match new_pixmap_uninitialized(width, height) {
    Ok(pixmap) => pixmap,
    Err(err) => {
      if matches!(err, RenderError::StageAllocationBudgetExceeded { .. }) {
        return Err(err);
      }
      return Ok(None);
    }
  };
  let pixels_len = width as usize * height as usize;
  if denom.abs() <= f32::EPSILON {
    let color = premultiply_rgba(stops[0].1);
    let pixels = pixmap.pixels_mut();
    let deadline = active_deadline();
    if pixels_len >= GRADIENT_PARALLEL_THRESHOLD_PIXELS
      && gradient_allow_parallel(deadline.as_ref())
      && crate::rayon_global::ensure_global_pool().is_ok()
    {
      pixels
        .par_chunks_mut(DEADLINE_PIXELS_STRIDE)
        .try_for_each(|chunk| {
          with_deadline(deadline.as_ref(), || {
            let mut counter = 0usize;
            check_active_periodic(&mut counter, 1, RenderStage::Paint)?;
            chunk.fill(color);
            Ok(())
          })
        })?;
    } else {
      let mut deadline_counter = 0usize;
      for chunk in pixels.chunks_mut(DEADLINE_PIXELS_STRIDE) {
        check_active_periodic(&mut deadline_counter, 1, RenderStage::Paint)?;
        chunk.fill(color);
      }
    }
    return Ok(Some(pixmap));
  }

  let inv_len = 1.0 / denom;
  let step_x = dx * inv_len;
  let step_y = dy * inv_len;
  let start_dot = (0.5 - start.x) * dx + (0.5 - start.y) * dy;
  let row_start0 = start_dot * inv_len;
  let stride = width as usize;
  let pixels = pixmap.pixels_mut();
  let spread_key: SpreadModeKey = spread.into();
  let use_8x8_dither = linear_gradient_use_8x8_dither(bucket);
  let multi_stop = stops.len() > 2;
  let phase_x = (dither_phase & 7) as usize;
  let phase_y = ((dither_phase >> 3) & 7) as usize;

  let key = GradientCacheKey::new(stops, spread, period, bucket);
  let lut = cache.get_or_build(key, || build_gradient_lut(stops, spread, period, bucket));
  let gradient_len = denom.sqrt();
  let dither_table_8x8 = if use_8x8_dither {
    Some(linear_gradient_select_8x8_dither_table(
      lut.as_ref(),
      gradient_len,
    ))
  } else {
    None
  };
  let (dither_mask, dither_row_stride, dither_scale, dither_bias, phase_x, phase_y) =
    if use_8x8_dither {
      (
        7usize,
        8usize,
        0.015625f32,  // 1/64
        0.0078125f32, // 0.5/64
        phase_x,
        phase_y,
      )
    } else {
      (
        3usize,
        4usize,
        0.0625f32,  // 1/16
        0.03125f32, // 0.5/16
        phase_x & 3,
        phase_y & 3,
      )
    };
  let sample_row = |y: usize, row: &mut [PremultipliedColorU8]| -> Result<(), RenderError> {
    let mut t = row_start0 + y as f32 * step_y;
    let y_mod = (y + phase_y) & dither_mask;
    let mut deadline_counter = 0usize;
    match spread_key {
      SpreadModeKey::Pad => {
        let mut x = 0usize;
        for chunk in row.chunks_mut(DEADLINE_PIXELS_STRIDE) {
          check_active_periodic(&mut deadline_counter, 1, RenderStage::Paint)?;
          let mut x_mod = (x + phase_x) & dither_mask;
          let chunk_len = chunk.len();
          for pixel in chunk.iter_mut() {
            if let Some(dither_table) = dither_table_8x8 {
              let (src, seg) = lut.sample_pad_f32_with_segment(t);
              let seg = seg as usize;
              let start = unsafe { *lut.stop_colors.get_unchecked(seg) };
              let (band, dom_delta) = linear_gradient_dither_band_and_dom_delta(src, start);
              let (shift, m0_fix) = linear_gradient_8x8_dither_shift_x(
                dither_table,
                band,
                seg as i32,
                dom_delta,
                multi_stop,
              );
              let idx = y_mod * dither_row_stride + ((x_mod + shift) & 7);
              let mut m = unsafe { *dither_table.get_unchecked(idx) };
              if m0_fix && m == 0 {
                m = 1;
              }
              let dither = m as f32 * dither_scale + dither_bias;
              *pixel = GradientLut::quantize_dither(src, dither);
            } else {
              let idx = y_mod * dither_row_stride + x_mod;
              let m = unsafe { *BAYER_4X4_XY.get_unchecked(idx) } as f32;
              let dither = m * dither_scale + dither_bias;
              *pixel = lut.sample_pad_dither(t, dither);
            }
            t += step_x;
            x_mod = (x_mod + 1) & dither_mask;
          }
          x += chunk_len;
        }
      }
      SpreadModeKey::Repeat => {
        let mut x = 0usize;
        for chunk in row.chunks_mut(DEADLINE_PIXELS_STRIDE) {
          check_active_periodic(&mut deadline_counter, 1, RenderStage::Paint)?;
          let mut x_mod = (x + phase_x) & dither_mask;
          let chunk_len = chunk.len();
          for pixel in chunk.iter_mut() {
            if let Some(dither_table) = dither_table_8x8 {
              let (src, seg) = lut.sample_repeat_f32_with_segment(t);
              let seg = seg as usize;
              let start = unsafe { *lut.stop_colors.get_unchecked(seg) };
              let (band, dom_delta) = linear_gradient_dither_band_and_dom_delta(src, start);
              let (shift, m0_fix) = linear_gradient_8x8_dither_shift_x(
                dither_table,
                band,
                seg as i32,
                dom_delta,
                multi_stop,
              );
              let idx = y_mod * dither_row_stride + ((x_mod + shift) & 7);
              let mut m = unsafe { *dither_table.get_unchecked(idx) };
              if m0_fix && m == 0 {
                m = 1;
              }
              let dither = m as f32 * dither_scale + dither_bias;
              *pixel = GradientLut::quantize_dither(src, dither);
            } else {
              let idx = y_mod * dither_row_stride + x_mod;
              let m = unsafe { *BAYER_4X4_XY.get_unchecked(idx) } as f32;
              let dither = m * dither_scale + dither_bias;
              *pixel = lut.sample_repeat_dither(t, dither);
            }
            t += step_x;
            x_mod = (x_mod + 1) & dither_mask;
          }
          x += chunk_len;
        }
      }
      SpreadModeKey::Reflect => {
        let mut x = 0usize;
        for chunk in row.chunks_mut(DEADLINE_PIXELS_STRIDE) {
          check_active_periodic(&mut deadline_counter, 1, RenderStage::Paint)?;
          let mut x_mod = (x + phase_x) & dither_mask;
          let chunk_len = chunk.len();
          for pixel in chunk.iter_mut() {
            if let Some(dither_table) = dither_table_8x8 {
              let (src, seg) = lut.sample_reflect_f32_with_segment(t);
              let seg = seg as usize;
              let start = unsafe { *lut.stop_colors.get_unchecked(seg) };
              let (band, dom_delta) = linear_gradient_dither_band_and_dom_delta(src, start);
              let (shift, m0_fix) = linear_gradient_8x8_dither_shift_x(
                dither_table,
                band,
                seg as i32,
                dom_delta,
                multi_stop,
              );
              let idx = y_mod * dither_row_stride + ((x_mod + shift) & 7);
              let mut m = unsafe { *dither_table.get_unchecked(idx) };
              if m0_fix && m == 0 {
                m = 1;
              }
              let dither = m as f32 * dither_scale + dither_bias;
              *pixel = GradientLut::quantize_dither(src, dither);
            } else {
              let idx = y_mod * dither_row_stride + x_mod;
              let m = unsafe { *BAYER_4X4_XY.get_unchecked(idx) } as f32;
              let dither = m * dither_scale + dither_bias;
              *pixel = lut.sample_reflect_dither(t, dither);
            }
            t += step_x;
            x_mod = (x_mod + 1) & dither_mask;
          }
          x += chunk_len;
        }
      }
    };
    Ok(())
  };

  let deadline = active_deadline();
  if pixels_len >= GRADIENT_PARALLEL_THRESHOLD_PIXELS
    && gradient_allow_parallel(deadline.as_ref())
    && crate::rayon_global::ensure_global_pool().is_ok()
  {
    pixels
      .par_chunks_mut(stride)
      .enumerate()
      .try_for_each(|(y, row)| {
        with_deadline(deadline.as_ref(), || -> Result<(), RenderError> {
          sample_row(y, row)
        })
      })?;
  } else {
    for (y, row) in pixels.chunks_mut(stride).enumerate() {
      sample_row(y, row)?;
    }
  }

  Ok(Some(pixmap))
}

pub fn paint_linear_gradient_src_over(
  dest: &mut Pixmap,
  dest_x: i32,
  dest_y: i32,
  width: u32,
  height: u32,
  start: Point,
  end: Point,
  spread: SpreadMode,
  stops: &[(f32, Rgba)],
  cache: &GradientLutCache,
  bucket: u16,
  dither_phase: u8,
  clip: Option<&tiny_skia::Mask>,
) -> Result<(), RenderError> {
  let mut dest = dest.as_mut();
  paint_linear_gradient_src_over_mut(
    &mut dest,
    dest_x,
    dest_y,
    width,
    height,
    start,
    end,
    spread,
    stops,
    cache,
    bucket,
    dither_phase,
    clip,
  )
}

pub(crate) fn paint_linear_gradient_src_over_mut(
  dest: &mut PixmapMut<'_>,
  dest_x: i32,
  dest_y: i32,
  width: u32,
  height: u32,
  start: Point,
  end: Point,
  spread: SpreadMode,
  stops: &[(f32, Rgba)],
  cache: &GradientLutCache,
  bucket: u16,
  dither_phase: u8,
  clip: Option<&tiny_skia::Mask>,
) -> Result<(), RenderError> {
  check_active(RenderStage::Paint)?;
  if width == 0 || height == 0 || stops.is_empty() {
    return Ok(());
  }
  let dest_width = dest.width() as i32;
  let dest_height = dest.height() as i32;
  if dest_width <= 0 || dest_height <= 0 {
    return Ok(());
  }
  let Some(rect_x1) = dest_x.checked_add(width as i32) else {
    return Ok(());
  };
  let Some(rect_y1) = dest_y.checked_add(height as i32) else {
    return Ok(());
  };
  let x0 = dest_x.max(0);
  let y0 = dest_y.max(0);
  let x1 = rect_x1.min(dest_width);
  let y1 = rect_y1.min(dest_height);
  if x0 >= x1 || y0 >= y1 {
    return Ok(());
  }
  let local_x0 = (x0 - dest_x) as usize;
  let local_y0 = (y0 - dest_y) as usize;
  let span_x = (x1 - x0) as usize;
  let span_y = (y1 - y0) as usize;

  let period = gradient_period(stops);
  let dx = end.x - start.x;
  let dy = end.y - start.y;
  let denom = dx * dx + dy * dy;
  let spread_key: SpreadModeKey = spread.into();
  let use_8x8_dither = linear_gradient_use_8x8_dither(bucket);
  let multi_stop = stops.len() > 2;
  let phase_x = (dither_phase & 7) as usize;
  let phase_y = ((dither_phase >> 3) & 7) as usize;
  let key = GradientCacheKey::new(stops, spread, period, bucket);
  let lut = cache.get_or_build(key, || build_gradient_lut(stops, spread, period, bucket));
  let gradient_len = denom.sqrt();
  let dither_table_8x8 = if use_8x8_dither {
    Some(linear_gradient_select_8x8_dither_table(
      lut.as_ref(),
      gradient_len,
    ))
  } else {
    None
  };
  let (dither_mask, dither_row_stride, dither_scale, dither_bias, phase_x, phase_y) =
    if use_8x8_dither {
      (
        7usize,
        8usize,
        0.015625f32,  // 1/64
        0.0078125f32, // 0.5/64
        phase_x,
        phase_y,
      )
    } else {
      (
        3usize,
        4usize,
        0.0625f32,  // 1/16
        0.03125f32, // 0.5/16
        phase_x & 3,
        phase_y & 3,
      )
    };

  #[inline(always)]
  fn blend_src_over(dst: PremultipliedColorU8, src: [f32; 4]) -> [f32; 4] {
    let sa = (src[3] * (1.0 / 255.0)).clamp(0.0, 1.0);
    if sa <= 0.0 {
      return [
        dst.red() as f32,
        dst.green() as f32,
        dst.blue() as f32,
        dst.alpha() as f32,
      ];
    }
    if sa >= 1.0 {
      return src;
    }
    let inv = 1.0 - sa;
    [
      src[0] + dst.red() as f32 * inv,
      src[1] + dst.green() as f32 * inv,
      src[2] + dst.blue() as f32 * inv,
      src[3] + dst.alpha() as f32 * inv,
    ]
  }

  let (clip_data, clip_stride) = match clip {
    Some(mask) if mask.width() as i32 == dest_width && mask.height() as i32 == dest_height => {
      (Some(mask.data()), mask.width() as usize)
    }
    _ => (None, 0),
  };

  let stride = dest.width() as usize;
  let pixels = dest.pixels_mut();
  let row_start = y0 as usize * stride;
  let row_end = y1 as usize * stride;
  let pixels = &mut pixels[row_start..row_end];

  if denom.abs() <= f32::EPSILON {
    let solid = premultiply_rgba(stops[0].1);
    let src = [
      solid.red() as f32,
      solid.green() as f32,
      solid.blue() as f32,
      solid.alpha() as f32,
    ];
    let total_pixels = span_x.saturating_mul(span_y);
    let paint_row = |local_y: usize,
                     row: &mut [PremultipliedColorU8],
                     mask_row: Option<&[u8]>|
     -> Result<(), RenderError> {
      let y_mod = (local_y + phase_y) & dither_mask;
      let mut x = 0usize;
      let mut deadline_counter = 0usize;
      let inv_255 = 1.0 / 255.0;
      for chunk in row.chunks_mut(DEADLINE_PIXELS_STRIDE) {
        check_active_periodic(&mut deadline_counter, 1, RenderStage::Paint)?;
        let mut x_mod = (local_x0 + x + phase_x) & dither_mask;
        let chunk_len = chunk.len();
        if let Some(mask_row) = mask_row {
          let mask_chunk = &mask_row[x..x + chunk_len];
          for (pixel, &mask) in chunk.iter_mut().zip(mask_chunk.iter()) {
            if mask == 0 {
              x_mod = (x_mod + 1) & dither_mask;
              continue;
            }
            let out = if mask == 255 {
              blend_src_over(*pixel, src)
            } else {
              let mf = mask as f32 * inv_255;
              blend_src_over(*pixel, [src[0] * mf, src[1] * mf, src[2] * mf, src[3] * mf])
            };
            let idx = y_mod * dither_row_stride + x_mod;
            let m = if let Some(dither_table) = dither_table_8x8 {
              (unsafe { *dither_table.get_unchecked(idx) }) as f32
            } else {
              (unsafe { *BAYER_4X4_XY.get_unchecked(idx) }) as f32
            };
            let dither = m * dither_scale + dither_bias;
            *pixel = GradientLut::quantize_dither(out, dither);
            x_mod = (x_mod + 1) & dither_mask;
          }
        } else {
          for pixel in chunk.iter_mut() {
            let out = blend_src_over(*pixel, src);
            let idx = y_mod * dither_row_stride + x_mod;
            let m = if let Some(dither_table) = dither_table_8x8 {
              (unsafe { *dither_table.get_unchecked(idx) }) as f32
            } else {
              (unsafe { *BAYER_4X4_XY.get_unchecked(idx) }) as f32
            };
            let dither = m * dither_scale + dither_bias;
            *pixel = GradientLut::quantize_dither(out, dither);
            x_mod = (x_mod + 1) & dither_mask;
          }
        }
        x += chunk_len;
      }
      Ok(())
    };

    if total_pixels >= GRADIENT_PARALLEL_THRESHOLD_PIXELS {
      let deadline = active_deadline();
      pixels
        .par_chunks_mut(stride)
        .enumerate()
        .try_for_each(|(row_idx, full_row)| {
          with_deadline(deadline.as_ref(), || {
            let local_y = local_y0 + row_idx;
            let row = &mut full_row[x0 as usize..x1 as usize];
            let mask_row = clip_data.map(|data| {
              let global_y = y0 as usize + row_idx;
              let start = global_y * clip_stride + x0 as usize;
              &data[start..start + span_x]
            });
            paint_row(local_y, row, mask_row)
          })
        })?;
    } else {
      for (row_idx, full_row) in pixels.chunks_mut(stride).enumerate() {
        let local_y = local_y0 + row_idx;
        let row = &mut full_row[x0 as usize..x1 as usize];
        let mask_row = clip_data.map(|data| {
          let global_y = y0 as usize + row_idx;
          let start = global_y * clip_stride + x0 as usize;
          &data[start..start + span_x]
        });
        paint_row(local_y, row, mask_row)?;
      }
    }
    return Ok(());
  }

  let inv_len = 1.0 / denom;
  let step_x = dx * inv_len;
  let step_y = dy * inv_len;
  let start_dot = (0.5 - start.x) * dx + (0.5 - start.y) * dy;
  let row_start0 = start_dot * inv_len;
  let total_pixels = span_x.saturating_mul(span_y);

  let paint_row = |local_y: usize,
                   row: &mut [PremultipliedColorU8],
                   mask_row: Option<&[u8]>|
   -> Result<(), RenderError> {
    let y_mod = (local_y + phase_y) & dither_mask;
    let mut deadline_counter = 0usize;
    let mut t = row_start0 + local_y as f32 * step_y + local_x0 as f32 * step_x;
    let inv_255 = 1.0 / 255.0;

    match spread_key {
      SpreadModeKey::Pad => {
        let mut x = 0usize;
        for chunk in row.chunks_mut(DEADLINE_PIXELS_STRIDE) {
          check_active_periodic(&mut deadline_counter, 1, RenderStage::Paint)?;
          let mut x_mod = (local_x0 + x + phase_x) & dither_mask;
          let chunk_len = chunk.len();
          if let Some(mask_row) = mask_row {
            let mask_chunk = &mask_row[x..x + chunk_len];
            for (pixel, &mask) in chunk.iter_mut().zip(mask_chunk.iter()) {
              if mask == 0 {
                t += step_x;
                x_mod = (x_mod + 1) & dither_mask;
                continue;
              }
              if let Some(dither_table) = dither_table_8x8 {
                let (mut src, seg) = lut.sample_pad_f32_with_segment(t);
                let seg = seg as usize;
                let start = unsafe { *lut.stop_colors.get_unchecked(seg) };
                let (band, dom_delta) = linear_gradient_dither_band_and_dom_delta(src, start);
                let (shift, m0_fix) = linear_gradient_8x8_dither_shift_x(
                  dither_table,
                  band,
                  seg as i32,
                  dom_delta,
                  multi_stop,
                );
                if mask != 255 {
                  let mf = mask as f32 * inv_255;
                  src[0] *= mf;
                  src[1] *= mf;
                  src[2] *= mf;
                  src[3] *= mf;
                }
                let out = blend_src_over(*pixel, src);
                let idx = y_mod * dither_row_stride + ((x_mod + shift) & 7);
                let mut m = unsafe { *dither_table.get_unchecked(idx) };
                if m0_fix && m == 0 {
                  m = 1;
                }
                let dither = m as f32 * dither_scale + dither_bias;
                *pixel = GradientLut::quantize_dither(out, dither);
              } else {
                let mut src = lut.sample_pad_f32(t);
                if mask != 255 {
                  let mf = mask as f32 * inv_255;
                  src[0] *= mf;
                  src[1] *= mf;
                  src[2] *= mf;
                  src[3] *= mf;
                }
                let out = blend_src_over(*pixel, src);
                let idx = y_mod * dither_row_stride + x_mod;
                let m = unsafe { *BAYER_4X4_XY.get_unchecked(idx) } as f32;
                let dither = m * dither_scale + dither_bias;
                *pixel = GradientLut::quantize_dither(out, dither);
              }
              t += step_x;
              x_mod = (x_mod + 1) & dither_mask;
            }
          } else {
            for pixel in chunk.iter_mut() {
              if let Some(dither_table) = dither_table_8x8 {
                let (src, seg) = lut.sample_pad_f32_with_segment(t);
                let seg = seg as usize;
                let start = unsafe { *lut.stop_colors.get_unchecked(seg) };
                let (band, dom_delta) = linear_gradient_dither_band_and_dom_delta(src, start);
                let (shift, m0_fix) = linear_gradient_8x8_dither_shift_x(
                  dither_table,
                  band,
                  seg as i32,
                  dom_delta,
                  multi_stop,
                );
                let out = blend_src_over(*pixel, src);
                let idx = y_mod * dither_row_stride + ((x_mod + shift) & 7);
                let mut m = unsafe { *dither_table.get_unchecked(idx) };
                if m0_fix && m == 0 {
                  m = 1;
                }
                let dither = m as f32 * dither_scale + dither_bias;
                *pixel = GradientLut::quantize_dither(out, dither);
              } else {
                let src = lut.sample_pad_f32(t);
                let out = blend_src_over(*pixel, src);
                let idx = y_mod * dither_row_stride + x_mod;
                let m = unsafe { *BAYER_4X4_XY.get_unchecked(idx) } as f32;
                let dither = m * dither_scale + dither_bias;
                *pixel = GradientLut::quantize_dither(out, dither);
              }
              t += step_x;
              x_mod = (x_mod + 1) & dither_mask;
            }
          }
          x += chunk_len;
        }
      }
      SpreadModeKey::Repeat => {
        let mut x = 0usize;
        for chunk in row.chunks_mut(DEADLINE_PIXELS_STRIDE) {
          check_active_periodic(&mut deadline_counter, 1, RenderStage::Paint)?;
          let mut x_mod = (local_x0 + x + phase_x) & dither_mask;
          let chunk_len = chunk.len();
          if let Some(mask_row) = mask_row {
            let mask_chunk = &mask_row[x..x + chunk_len];
            for (pixel, &mask) in chunk.iter_mut().zip(mask_chunk.iter()) {
              if mask == 0 {
                t += step_x;
                x_mod = (x_mod + 1) & dither_mask;
                continue;
              }
              if let Some(dither_table) = dither_table_8x8 {
                let (mut src, seg) = lut.sample_repeat_f32_with_segment(t);
                let seg = seg as usize;
                let start = unsafe { *lut.stop_colors.get_unchecked(seg) };
                let (band, dom_delta) = linear_gradient_dither_band_and_dom_delta(src, start);
                let (shift, m0_fix) = linear_gradient_8x8_dither_shift_x(
                  dither_table,
                  band,
                  seg as i32,
                  dom_delta,
                  multi_stop,
                );
                if mask != 255 {
                  let mf = mask as f32 * inv_255;
                  src[0] *= mf;
                  src[1] *= mf;
                  src[2] *= mf;
                  src[3] *= mf;
                }
                let out = blend_src_over(*pixel, src);
                let idx = y_mod * dither_row_stride + ((x_mod + shift) & 7);
                let mut m = unsafe { *dither_table.get_unchecked(idx) };
                if m0_fix && m == 0 {
                  m = 1;
                }
                let dither = m as f32 * dither_scale + dither_bias;
                *pixel = GradientLut::quantize_dither(out, dither);
              } else {
                let mut src = lut.sample_repeat_f32(t);
                if mask != 255 {
                  let mf = mask as f32 * inv_255;
                  src[0] *= mf;
                  src[1] *= mf;
                  src[2] *= mf;
                  src[3] *= mf;
                }
                let out = blend_src_over(*pixel, src);
                let idx = y_mod * dither_row_stride + x_mod;
                let m = unsafe { *BAYER_4X4_XY.get_unchecked(idx) } as f32;
                let dither = m * dither_scale + dither_bias;
                *pixel = GradientLut::quantize_dither(out, dither);
              }
              t += step_x;
              x_mod = (x_mod + 1) & dither_mask;
            }
          } else {
            for pixel in chunk.iter_mut() {
              if let Some(dither_table) = dither_table_8x8 {
                let (src, seg) = lut.sample_repeat_f32_with_segment(t);
                let seg = seg as usize;
                let start = unsafe { *lut.stop_colors.get_unchecked(seg) };
                let (band, dom_delta) = linear_gradient_dither_band_and_dom_delta(src, start);
                let (shift, m0_fix) = linear_gradient_8x8_dither_shift_x(
                  dither_table,
                  band,
                  seg as i32,
                  dom_delta,
                  multi_stop,
                );
                let out = blend_src_over(*pixel, src);
                let idx = y_mod * dither_row_stride + ((x_mod + shift) & 7);
                let mut m = unsafe { *dither_table.get_unchecked(idx) };
                if m0_fix && m == 0 {
                  m = 1;
                }
                let dither = m as f32 * dither_scale + dither_bias;
                *pixel = GradientLut::quantize_dither(out, dither);
              } else {
                let src = lut.sample_repeat_f32(t);
                let out = blend_src_over(*pixel, src);
                let idx = y_mod * dither_row_stride + x_mod;
                let m = unsafe { *BAYER_4X4_XY.get_unchecked(idx) } as f32;
                let dither = m * dither_scale + dither_bias;
                *pixel = GradientLut::quantize_dither(out, dither);
              }
              t += step_x;
              x_mod = (x_mod + 1) & dither_mask;
            }
          }
          x += chunk_len;
        }
      }
      SpreadModeKey::Reflect => {
        let mut x = 0usize;
        for chunk in row.chunks_mut(DEADLINE_PIXELS_STRIDE) {
          check_active_periodic(&mut deadline_counter, 1, RenderStage::Paint)?;
          let mut x_mod = (local_x0 + x + phase_x) & dither_mask;
          let chunk_len = chunk.len();
          if let Some(mask_row) = mask_row {
            let mask_chunk = &mask_row[x..x + chunk_len];
            for (pixel, &mask) in chunk.iter_mut().zip(mask_chunk.iter()) {
              if mask == 0 {
                t += step_x;
                x_mod = (x_mod + 1) & dither_mask;
                continue;
              }
              if let Some(dither_table) = dither_table_8x8 {
                let (mut src, seg) = lut.sample_reflect_f32_with_segment(t);
                let seg = seg as usize;
                let start = unsafe { *lut.stop_colors.get_unchecked(seg) };
                let (band, dom_delta) = linear_gradient_dither_band_and_dom_delta(src, start);
                let (shift, m0_fix) = linear_gradient_8x8_dither_shift_x(
                  dither_table,
                  band,
                  seg as i32,
                  dom_delta,
                  multi_stop,
                );
                if mask != 255 {
                  let mf = mask as f32 * inv_255;
                  src[0] *= mf;
                  src[1] *= mf;
                  src[2] *= mf;
                  src[3] *= mf;
                }
                let out = blend_src_over(*pixel, src);
                let idx = y_mod * dither_row_stride + ((x_mod + shift) & 7);
                let mut m = unsafe { *dither_table.get_unchecked(idx) };
                if m0_fix && m == 0 {
                  m = 1;
                }
                let dither = m as f32 * dither_scale + dither_bias;
                *pixel = GradientLut::quantize_dither(out, dither);
              } else {
                let mut src = lut.sample_reflect_f32(t);
                if mask != 255 {
                  let mf = mask as f32 * inv_255;
                  src[0] *= mf;
                  src[1] *= mf;
                  src[2] *= mf;
                  src[3] *= mf;
                }
                let out = blend_src_over(*pixel, src);
                let idx = y_mod * dither_row_stride + x_mod;
                let m = unsafe { *BAYER_4X4_XY.get_unchecked(idx) } as f32;
                let dither = m * dither_scale + dither_bias;
                *pixel = GradientLut::quantize_dither(out, dither);
              }
              t += step_x;
              x_mod = (x_mod + 1) & dither_mask;
            }
          } else {
            for pixel in chunk.iter_mut() {
              if let Some(dither_table) = dither_table_8x8 {
                let (src, seg) = lut.sample_reflect_f32_with_segment(t);
                let seg = seg as usize;
                let start = unsafe { *lut.stop_colors.get_unchecked(seg) };
                let (band, dom_delta) = linear_gradient_dither_band_and_dom_delta(src, start);
                let (shift, m0_fix) = linear_gradient_8x8_dither_shift_x(
                  dither_table,
                  band,
                  seg as i32,
                  dom_delta,
                  multi_stop,
                );
                let out = blend_src_over(*pixel, src);
                let idx = y_mod * dither_row_stride + ((x_mod + shift) & 7);
                let mut m = unsafe { *dither_table.get_unchecked(idx) };
                if m0_fix && m == 0 {
                  m = 1;
                }
                let dither = m as f32 * dither_scale + dither_bias;
                *pixel = GradientLut::quantize_dither(out, dither);
              } else {
                let src = lut.sample_reflect_f32(t);
                let out = blend_src_over(*pixel, src);
                let idx = y_mod * dither_row_stride + x_mod;
                let m = unsafe { *BAYER_4X4_XY.get_unchecked(idx) } as f32;
                let dither = m * dither_scale + dither_bias;
                *pixel = GradientLut::quantize_dither(out, dither);
              }
              t += step_x;
              x_mod = (x_mod + 1) & dither_mask;
            }
          }
          x += chunk_len;
        }
      }
    }
    Ok(())
  };

  let deadline = active_deadline();
  if total_pixels >= GRADIENT_PARALLEL_THRESHOLD_PIXELS
    && gradient_allow_parallel(deadline.as_ref())
    && crate::rayon_global::ensure_global_pool().is_ok()
  {
    pixels
      .par_chunks_mut(stride)
      .enumerate()
      .try_for_each(|(row_idx, full_row)| {
        with_deadline(deadline.as_ref(), || {
          let local_y = local_y0 + row_idx;
          let row = &mut full_row[x0 as usize..x1 as usize];
          let mask_row = clip_data.map(|data| {
            let global_y = y0 as usize + row_idx;
            let start = global_y * clip_stride + x0 as usize;
            &data[start..start + span_x]
          });
          paint_row(local_y, row, mask_row)
        })
      })?;
  } else {
    for (row_idx, full_row) in pixels.chunks_mut(stride).enumerate() {
      let local_y = local_y0 + row_idx;
      let row = &mut full_row[x0 as usize..x1 as usize];
      let mask_row = clip_data.map(|data| {
        let global_y = y0 as usize + row_idx;
        let start = global_y * clip_stride + x0 as usize;
        &data[start..start + span_x]
      });
      paint_row(local_y, row, mask_row)?;
    }
  }

  Ok(())
}

pub fn rasterize_linear_gradient_cached(
  pixmap_cache: &GradientPixmapCache,
  width: u32,
  height: u32,
  start: Point,
  end: Point,
  spread: SpreadMode,
  stops: &[(f32, Rgba)],
  cache: &GradientLutCache,
  bucket: u16,
  dither_phase: u8,
) -> Result<Option<Arc<Pixmap>>, RenderError> {
  let Some(key) = GradientPixmapCacheKey::linear(
    width,
    height,
    start,
    end,
    spread,
    stops,
    bucket,
    dither_phase,
  ) else {
    return Ok(None);
  };
  pixmap_cache.get_or_insert(key, || {
    rasterize_linear_gradient_with_phase(
      width,
      height,
      start,
      end,
      spread,
      stops,
      cache,
      bucket,
      dither_phase,
    )
  })
}

pub fn rasterize_conic_gradient(
  width: u32,
  height: u32,
  center: Point,
  start_angle: f32,
  spread: SpreadMode,
  stops: &[(f32, Rgba)],
  cache: &GradientLutCache,
  bucket: u16,
) -> Result<Option<Pixmap>, RenderError> {
  check_active(RenderStage::Paint)?;
  if width == 0 || height == 0 || stops.is_empty() {
    return Ok(None);
  }

  let period = gradient_period(stops);
  let key = GradientCacheKey::new(stops, spread, period, bucket);
  let lut = cache.get_or_build(key, || build_gradient_lut(stops, spread, period, bucket));
  let mut pixmap = match new_pixmap_uninitialized(width, height) {
    Ok(pixmap) => pixmap,
    Err(err) => {
      if matches!(err, RenderError::StageAllocationBudgetExceeded { .. }) {
        return Err(err);
      }
      return Ok(None);
    }
  };

  let start_angle = start_angle.rem_euclid(std::f32::consts::PI * 2.0);
  // `dx.atan2(-dy)` yields an angle in [-π, π] where 0 points "up" and angles increase clockwise
  // (matching CSS conic gradients). Map it into a [0, 1) turn fraction. Importantly, we map the
  // full circle into [0, 1) regardless of the last stop position; for non-repeating conic
  // gradients, the terminal stop color should simply extend through the remainder of the circle
  // (e.g. stops ending at 0.5 should dominate angles in [0.5, 1.0)).
  let angle_scale = 0.5 / std::f32::consts::PI;
  let stride = width as usize;
  let pixels = pixmap.pixels_mut();
  let dx0 = 0.5 - center.x;
  let pixels_len = width as usize * height as usize;
  let spread_key: SpreadModeKey = spread.into();
  let sample_row = |y: usize, row: &mut [PremultipliedColorU8]| -> Result<(), RenderError> {
    let dy = y as f32 + 0.5 - center.y;
    let mut dx = dx0;
    let mut deadline_counter = 0usize;
    match spread_key {
      SpreadModeKey::Pad => {
        for chunk in row.chunks_mut(DEADLINE_PIXELS_STRIDE) {
          check_active_periodic(&mut deadline_counter, 1, RenderStage::Paint)?;
          for pixel in chunk {
            let mut t = (dx.atan2(-dy) + start_angle) * angle_scale;
            if t < 0.0 {
              t += 1.0;
            } else if t >= 1.0 {
              t -= 1.0;
            }
            *pixel = lut.sample_pad(t);
            dx += 1.0;
          }
        }
      }
      SpreadModeKey::Repeat => {
        for chunk in row.chunks_mut(DEADLINE_PIXELS_STRIDE) {
          check_active_periodic(&mut deadline_counter, 1, RenderStage::Paint)?;
          for pixel in chunk {
            let mut t = (dx.atan2(-dy) + start_angle) * angle_scale;
            if t < 0.0 {
              t += 1.0;
            } else if t >= 1.0 {
              t -= 1.0;
            }
            *pixel = lut.sample_repeat(t);
            dx += 1.0;
          }
        }
      }
      SpreadModeKey::Reflect => {
        for chunk in row.chunks_mut(DEADLINE_PIXELS_STRIDE) {
          check_active_periodic(&mut deadline_counter, 1, RenderStage::Paint)?;
          for pixel in chunk {
            let mut t = (dx.atan2(-dy) + start_angle) * angle_scale;
            if t < 0.0 {
              t += 1.0;
            } else if t >= 1.0 {
              t -= 1.0;
            }
            *pixel = lut.sample_reflect(t);
            dx += 1.0;
          }
        }
      }
    };
    Ok(())
  };

  let deadline = active_deadline();
  if pixels_len >= GRADIENT_PARALLEL_THRESHOLD_PIXELS
    && gradient_allow_parallel(deadline.as_ref())
    && crate::rayon_global::ensure_global_pool().is_ok()
  {
    pixels
      .par_chunks_mut(stride)
      .enumerate()
      .try_for_each(|(y, row)| {
        with_deadline(deadline.as_ref(), || -> Result<(), RenderError> {
          sample_row(y, row)
        })
      })?;
  } else {
    for (y, row) in pixels.chunks_mut(stride).enumerate() {
      sample_row(y, row)?;
    }
  }

  Ok(Some(pixmap))
}

pub fn rasterize_conic_gradient_cached(
  pixmap_cache: &GradientPixmapCache,
  width: u32,
  height: u32,
  center: Point,
  start_angle: f32,
  spread: SpreadMode,
  stops: &[(f32, Rgba)],
  cache: &GradientLutCache,
  bucket: u16,
) -> Result<Option<Arc<Pixmap>>, RenderError> {
  let Some(key) =
    GradientPixmapCacheKey::conic(width, height, center, start_angle, spread, stops, bucket)
  else {
    return Ok(None);
  };
  pixmap_cache.get_or_insert(key, || {
    rasterize_conic_gradient(
      width,
      height,
      center,
      start_angle,
      spread,
      stops,
      cache,
      bucket,
    )
  })
}

/// Rasterize a conic gradient into a pixmap where the sampling coordinate space is scaled.
///
/// This is useful when the resulting pixmap will be drawn with a non-uniform scale (e.g. as a
/// repeated pattern where the tile size is fractional in device pixels). The `scale_x/scale_y`
/// parameters describe how many destination (device) pixels correspond to a 1px step in the
/// rasterized pixmap.
pub fn rasterize_conic_gradient_scaled(
  width: u32,
  height: u32,
  center: Point,
  start_angle: f32,
  spread: SpreadMode,
  stops: &[(f32, Rgba)],
  cache: &GradientLutCache,
  bucket: u16,
  scale_x: f32,
  scale_y: f32,
) -> Result<Option<Pixmap>, RenderError> {
  check_active(RenderStage::Paint)?;
  if width == 0
    || height == 0
    || stops.is_empty()
    || !scale_x.is_finite()
    || !scale_y.is_finite()
    || scale_x <= 0.0
    || scale_y <= 0.0
  {
    return Ok(None);
  }

  let period = gradient_period(stops);
  let key = GradientCacheKey::new(stops, spread, period, bucket);
  let lut = cache.get_or_build(key, || build_gradient_lut(stops, spread, period, bucket));
  let mut pixmap = match new_pixmap_uninitialized(width, height) {
    Ok(pixmap) => pixmap,
    Err(err) => {
      if matches!(err, RenderError::StageAllocationBudgetExceeded { .. }) {
        return Err(err);
      }
      return Ok(None);
    }
  };

  let start_angle = start_angle.rem_euclid(std::f32::consts::PI * 2.0);
  let angle_scale = 0.5 / std::f32::consts::PI;
  let stride = width as usize;
  let pixels = pixmap.pixels_mut();
  let dx0 = (0.5 - center.x) * scale_x;
  let mut deadline_counter = 0usize;
  for y in 0..height as usize {
    let dy = (y as f32 + 0.5 - center.y) * scale_y;
    let mut dx = dx0;
    let row_base = y * stride;
    for chunk in pixels[row_base..row_base + stride].chunks_mut(DEADLINE_PIXELS_STRIDE) {
      check_active_periodic(&mut deadline_counter, 1, RenderStage::Paint)?;
      for pixel in chunk {
        let mut t = (dx.atan2(-dy) + start_angle) * angle_scale;
        if t < 0.0 {
          t += 1.0;
        } else if t >= 1.0 {
          t -= 1.0;
        }
        *pixel = lut.sample(t);
        dx += scale_x;
      }
    }
  }

  Ok(Some(pixmap))
}

pub fn rasterize_conic_gradient_scaled_cached(
  pixmap_cache: &GradientPixmapCache,
  width: u32,
  height: u32,
  center: Point,
  start_angle: f32,
  spread: SpreadMode,
  stops: &[(f32, Rgba)],
  cache: &GradientLutCache,
  bucket: u16,
  scale_x: f32,
  scale_y: f32,
) -> Result<Option<Arc<Pixmap>>, RenderError> {
  let Some(key) = GradientPixmapCacheKey::conic_scaled(
    width,
    height,
    center,
    start_angle,
    spread,
    stops,
    bucket,
    scale_x,
    scale_y,
  ) else {
    return Ok(None);
  };
  pixmap_cache.get_or_insert(key, || {
    rasterize_conic_gradient_scaled(
      width,
      height,
      center,
      start_angle,
      spread,
      stops,
      cache,
      bucket,
      scale_x,
      scale_y,
    )
  })
}

pub fn rasterize_radial_gradient(
  width: u32,
  height: u32,
  center: Point,
  radii: Point,
  spread: SpreadMode,
  stops: &[(f32, Rgba)],
  cache: &GradientLutCache,
  bucket: u16,
  dither_phase: u8,
) -> Result<Option<Pixmap>, RenderError> {
  check_active(RenderStage::Paint)?;
  if width == 0 || height == 0 || stops.is_empty() {
    return Ok(None);
  }
  if !center.x.is_finite()
    || !center.y.is_finite()
    || !radii.x.is_finite()
    || !radii.y.is_finite()
    || radii.x <= 0.0
    || radii.y <= 0.0
  {
    return Ok(None);
  }

  let period = gradient_period(stops);
  let key = GradientCacheKey::new(stops, spread, period, bucket);
  let lut = cache.get_or_build(key, || build_gradient_lut(stops, spread, period, bucket));

  let mut pixmap = match new_pixmap_uninitialized(width, height) {
    Ok(pixmap) => pixmap,
    Err(err) => {
      if matches!(err, RenderError::StageAllocationBudgetExceeded { .. }) {
        return Err(err);
      }
      return Ok(None);
    }
  };

  let inv_rx = 1.0 / radii.x;
  let inv_ry = 1.0 / radii.y;
  if !inv_rx.is_finite() || !inv_ry.is_finite() {
    return Ok(None);
  }

  // Precompute X^2 terms for the row loops; this avoids recomputing `(dx / rx)^2` per pixel.
  let mut dx2: Vec<f32> = Vec::with_capacity(width as usize);
  let dx0 = (0.5 - center.x) * inv_rx;
  for x in 0..width as usize {
    let dx = dx0 + x as f32 * inv_rx;
    dx2.push(dx * dx);
  }

  let stride = width as usize;
  let pixels = pixmap.pixels_mut();
  let total_pixels = width as usize * height as usize;
  let spread_key: SpreadModeKey = spread.into();
  let use_8x8_dither = linear_gradient_use_8x8_dither(bucket);
  let multi_stop = stops.len() > 2;
  let phase_x = (dither_phase & 7) as usize;
  let phase_y = ((dither_phase >> 3) & 7) as usize;
  // Approximate the length of a 0→1 ramp in device pixels. For a circular radial gradient this is
  // simply the radius; for ellipses, use the major axis. This value is only used to select between
  // Skia's observed 8×8 dither tables.
  let gradient_len = radii.x.max(radii.y);
  let dither_table_8x8 = if use_8x8_dither {
    Some(linear_gradient_select_8x8_dither_table(
      lut.as_ref(),
      gradient_len,
    ))
  } else {
    None
  };
  let (dither_mask, dither_row_stride, dither_scale, dither_bias, phase_x, phase_y) =
    if use_8x8_dither {
      (
        7usize,
        8usize,
        0.015625f32,  // 1/64
        0.0078125f32, // 0.5/64
        phase_x,
        phase_y,
      )
    } else {
      (
        3usize,
        4usize,
        0.0625f32,  // 1/16
        0.03125f32, // 0.5/16
        phase_x & 3,
        phase_y & 3,
      )
    };

  let sample_row = |y: usize, row: &mut [PremultipliedColorU8]| -> Result<(), RenderError> {
    let dy = (y as f32 + 0.5 - center.y) * inv_ry;
    let dy2 = dy * dy;
    let y_mod = (y + phase_y) & dither_mask;
    let mut deadline_counter = 0usize;
    match spread_key {
      SpreadModeKey::Pad => {
        let mut x = 0usize;
        for chunk in row.chunks_mut(DEADLINE_PIXELS_STRIDE) {
          check_active_periodic(&mut deadline_counter, 1, RenderStage::Paint)?;
          let mut x_mod = (x + phase_x) & dither_mask;
          let chunk_len = chunk.len();
          for (idx, pixel) in chunk.iter_mut().enumerate() {
            let t = (dx2[x + idx] + dy2).sqrt();
            if let Some(dither_table) = dither_table_8x8 {
              let (src, seg) = lut.sample_pad_f32_with_segment(t);
              let seg = seg as usize;
              let start = unsafe { *lut.stop_colors.get_unchecked(seg) };
              let (band, dom_delta) = linear_gradient_dither_band_and_dom_delta(src, start);
              let (shift, m0_fix) = linear_gradient_8x8_dither_shift_x(
                dither_table,
                band,
                seg as i32,
                dom_delta,
                multi_stop,
              );
              let idx = y_mod * dither_row_stride + ((x_mod + shift) & 7);
              let mut m = unsafe { *dither_table.get_unchecked(idx) };
              if m0_fix && m == 0 {
                m = 1;
              }
              let dither = m as f32 * dither_scale + dither_bias;
              *pixel = GradientLut::quantize_dither(src, dither);
            } else {
              let idx = y_mod * dither_row_stride + x_mod;
              let m = unsafe { *BAYER_4X4_XY.get_unchecked(idx) } as f32;
              let dither = m * dither_scale + dither_bias;
              *pixel = lut.sample_pad_dither(t, dither);
            }
            x_mod = (x_mod + 1) & dither_mask;
          }
          x += chunk_len;
        }
      }
      SpreadModeKey::Repeat => {
        let mut x = 0usize;
        for chunk in row.chunks_mut(DEADLINE_PIXELS_STRIDE) {
          check_active_periodic(&mut deadline_counter, 1, RenderStage::Paint)?;
          let mut x_mod = (x + phase_x) & dither_mask;
          let chunk_len = chunk.len();
          for (idx, pixel) in chunk.iter_mut().enumerate() {
            let t = (dx2[x + idx] + dy2).sqrt();
            if let Some(dither_table) = dither_table_8x8 {
              let (src, seg) = lut.sample_repeat_f32_with_segment(t);
              let seg = seg as usize;
              let start = unsafe { *lut.stop_colors.get_unchecked(seg) };
              let (band, dom_delta) = linear_gradient_dither_band_and_dom_delta(src, start);
              let (shift, m0_fix) = linear_gradient_8x8_dither_shift_x(
                dither_table,
                band,
                seg as i32,
                dom_delta,
                multi_stop,
              );
              let idx = y_mod * dither_row_stride + ((x_mod + shift) & 7);
              let mut m = unsafe { *dither_table.get_unchecked(idx) };
              if m0_fix && m == 0 {
                m = 1;
              }
              let dither = m as f32 * dither_scale + dither_bias;
              *pixel = GradientLut::quantize_dither(src, dither);
            } else {
              let idx = y_mod * dither_row_stride + x_mod;
              let m = unsafe { *BAYER_4X4_XY.get_unchecked(idx) } as f32;
              let dither = m * dither_scale + dither_bias;
              *pixel = lut.sample_repeat_dither(t, dither);
            }
            x_mod = (x_mod + 1) & dither_mask;
          }
          x += chunk_len;
        }
      }
      SpreadModeKey::Reflect => {
        let mut x = 0usize;
        for chunk in row.chunks_mut(DEADLINE_PIXELS_STRIDE) {
          check_active_periodic(&mut deadline_counter, 1, RenderStage::Paint)?;
          let mut x_mod = (x + phase_x) & dither_mask;
          let chunk_len = chunk.len();
          for (idx, pixel) in chunk.iter_mut().enumerate() {
            let t = (dx2[x + idx] + dy2).sqrt();
            if let Some(dither_table) = dither_table_8x8 {
              let (src, seg) = lut.sample_reflect_f32_with_segment(t);
              let seg = seg as usize;
              let start = unsafe { *lut.stop_colors.get_unchecked(seg) };
              let (band, dom_delta) = linear_gradient_dither_band_and_dom_delta(src, start);
              let (shift, m0_fix) = linear_gradient_8x8_dither_shift_x(
                dither_table,
                band,
                seg as i32,
                dom_delta,
                multi_stop,
              );
              let idx = y_mod * dither_row_stride + ((x_mod + shift) & 7);
              let mut m = unsafe { *dither_table.get_unchecked(idx) };
              if m0_fix && m == 0 {
                m = 1;
              }
              let dither = m as f32 * dither_scale + dither_bias;
              *pixel = GradientLut::quantize_dither(src, dither);
            } else {
              let idx = y_mod * dither_row_stride + x_mod;
              let m = unsafe { *BAYER_4X4_XY.get_unchecked(idx) } as f32;
              let dither = m * dither_scale + dither_bias;
              *pixel = lut.sample_reflect_dither(t, dither);
            }
            x_mod = (x_mod + 1) & dither_mask;
          }
          x += chunk_len;
        }
      }
    };
    Ok(())
  };

  let deadline = active_deadline();
  if total_pixels >= GRADIENT_PARALLEL_THRESHOLD_PIXELS
    && gradient_allow_parallel(deadline.as_ref())
    && crate::rayon_global::ensure_global_pool().is_ok()
  {
    pixels
      .par_chunks_mut(stride)
      .enumerate()
      .try_for_each(|(y, row)| {
        with_deadline(deadline.as_ref(), || -> Result<(), RenderError> {
          sample_row(y, row)
        })
      })?;
  } else {
    for (y, row) in pixels.chunks_mut(stride).enumerate() {
      sample_row(y, row)?;
    }
  }

  Ok(Some(pixmap))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::debug::runtime::{self, RuntimeToggles};
  use crate::render_control::{with_deadline, RenderDeadline};
  use std::collections::HashMap;
  use std::sync::Arc;
  use std::time::Instant;

  #[test]
  fn gradient_cache_keys_canonicalize_negative_zero() {
    let stops = &[(0.0, Rgba::WHITE), (1.0, Rgba::BLACK)];
    let stops_neg = &[(-0.0, Rgba::WHITE), (1.0, Rgba::BLACK)];
    let span = gradient_period(stops);
    let key = GradientCacheKey::new(stops, SpreadMode::Pad, span, 0);
    let key_neg = GradientCacheKey::new(stops_neg, SpreadMode::Pad, span, 0);
    assert!(key == key_neg);

    let start = Point::new(0.0, 0.0);
    let end = Point::new(100.0, 100.0);
    let start_neg = Point::new(-0.0, -0.0);
    let end_neg = Point::new(100.0, 100.0);
    let pixmap_key =
      GradientPixmapCacheKey::linear(10, 10, start, end, SpreadMode::Pad, stops, 0, 0).unwrap();
    let pixmap_key_neg =
      GradientPixmapCacheKey::linear(10, 10, start_neg, end_neg, SpreadMode::Pad, stops, 0, 0)
        .unwrap();
    assert!(pixmap_key == pixmap_key_neg);
  }

  #[test]
  fn gradient_pixmap_cache_config_honors_env_overrides() {
    let toggles = Arc::new(RuntimeToggles::from_map(HashMap::from([
      (
        ENV_GRADIENT_PIXMAP_CACHE_ITEMS.to_string(),
        "12".to_string(),
      ),
      (
        ENV_GRADIENT_PIXMAP_CACHE_BYTES.to_string(),
        "3456".to_string(),
      ),
    ])));

    runtime::with_runtime_toggles(toggles, || {
      let config = GradientPixmapCacheConfig::default();
      assert_eq!(config.max_items, 12);
      assert_eq!(config.max_bytes, 3456);
    });
  }

  #[test]
  fn gradient_lut_interpolates_premultiplied_alpha() {
    // White fading to transparent should fade out white (decreasing alpha) rather than darkening.
    // This requires interpolating premultiplied RGB, not unpremultiplied RGB.
    let stops = &[(0.0, Rgba::WHITE), (1.0, Rgba::TRANSPARENT)];
    let lut = build_gradient_lut(stops, SpreadMode::Pad, 1.0, 16);
    let mid = lut.sample_pad(0.5);
    let expected =
      PremultipliedColorU8::from_rgba(128, 128, 128, 128).expect("valid premultiplied color");
    assert_eq!(mid, expected);
  }

  #[test]
  fn gradient_lut_supports_negative_stop_positions() {
    // CSS gradients allow stop positions outside [0, 1]. When the first stop is < 0, sample points
    // with `t < 0` can still fall *inside* the stop range and must interpolate instead of being
    // clamped to the first stop.
    let stops = &[(-0.2, Rgba::BLACK), (0.3, Rgba::WHITE)];
    let span = gradient_period(stops);
    let lut = build_gradient_lut(stops, SpreadMode::Pad, span, 16);

    let within = lut.sample_pad(-0.1);
    let expected =
      PremultipliedColorU8::from_rgba(51, 51, 51, 255).expect("valid premultiplied color");
    assert_eq!(within, expected);

    let mid = lut.sample_pad(0.0);
    let expected =
      PremultipliedColorU8::from_rgba(102, 102, 102, 255).expect("valid premultiplied color");
    assert_eq!(mid, expected);

    let before = lut.sample_pad(-0.25);
    let expected =
      PremultipliedColorU8::from_rgba(0, 0, 0, 255).expect("valid premultiplied color");
    assert_eq!(before, expected);

    let after = lut.sample_pad(0.4);
    let expected =
      PremultipliedColorU8::from_rgba(255, 255, 255, 255).expect("valid premultiplied color");
    assert_eq!(after, expected);
  }

  #[test]
  fn linear_gradient_dither_matrix_matches_expected() {
    // Guardrail: ordered-dither orientation/phase should remain stable.
    //
    // This 4×4 case is small enough to be easy to reason about while still exercising every entry
    // in the dither matrix. The expected values were computed directly from the current quantization
    // formula: `floor(lerp(black, white, t) + dither)`, with `t = (x + 0.5) / width` and the
    // per-pixel dither value derived from the matrix at `(x & 3, y & 3)`.
    let stops = vec![(0.0, Rgba::BLACK), (1.0, Rgba::WHITE)];
    let cache = GradientLutCache::default();
    let width = 4;
    let height = 4;
    let start = Point::new(0.0, 0.0);
    let end = Point::new(width as f32, 0.0);
    let pixmap = rasterize_linear_gradient(
      width,
      height,
      start,
      end,
      SpreadMode::Pad,
      &stops,
      &cache,
      gradient_bucket(width.max(height)),
    )
    .expect("rasterize")
    .expect("pixmap");

    let expected: [[u8; 4]; 4] = [
      [31, 96, 159, 224],
      [32, 95, 160, 223],
      [32, 96, 159, 223],
      [32, 96, 159, 223],
    ];
    let stride = width as usize;
    let pixels = pixmap.pixels();
    for (y, row) in expected.iter().enumerate() {
      for (x, v) in row.iter().enumerate() {
        let expected_px =
          PremultipliedColorU8::from_rgba(*v, *v, *v, 255).expect("valid premultiplied color");
        assert_eq!(pixels[y * stride + x], expected_px, "pixel {x},{y}");
      }
    }
  }

  #[test]
  fn linear_gradient_dither_matrix_matches_expected_large_8x8() {
    // Guardrail: for very gentle ramps, Chrome uses a specific 8×8 ordered-dither matrix.
    //
    // Use a low-contrast 0→1 gradient so each pixel channel is either 0 or 1 and the expected
    // pattern is easy to compute from the dither table.
    let stops = vec![
      (0.0, Rgba::BLACK),
      (
        1.0,
        Rgba {
          r: 1,
          g: 1,
          b: 1,
          a: 1.0,
        },
      ),
    ];
    let cache = GradientLutCache::default();
    let width = 1024;
    let height = 8;
    let start = Point::new(0.0, 0.0);
    let end = Point::new(width as f32, 0.0);
    let bucket = gradient_bucket(width.max(height));
    assert!(
      bucket >= 1024,
      "test must select the gentle-ramp 8×8 matrix"
    );
    let pixmap = rasterize_linear_gradient(
      width,
      height,
      start,
      end,
      SpreadMode::Pad,
      &stops,
      &cache,
      bucket,
    )
    .expect("rasterize")
    .expect("pixmap");
    let stride = width as usize;
    let pixels = pixmap.pixels();

    for y in 0..height as usize {
      for x in 0..width as usize {
        let t = (x as f32 + 0.5) / width as f32;
        let idx = (y & 7) * 8 + (x & 7);
        let m = BAYER_8X8_XY[idx] as f32;
        let dither = m * 0.015625 + 0.0078125;
        let v = (t + dither) as i32;
        let v = v.clamp(0, 255) as u8;
        let expected =
          PremultipliedColorU8::from_rgba(v, v, v, 255).expect("valid premultiplied color");
        assert_eq!(pixels[y * stride + x], expected, "pixel {x},{y}");
      }
    }
  }

  #[test]
  fn linear_gradient_dither_matrix_matches_expected_medium_8x8() {
    // Chrome uses a different 8×8 matrix for steeper ramps (e.g. 0→1 over 512px).
    let stops = vec![
      (0.0, Rgba::BLACK),
      (
        1.0,
        Rgba {
          r: 1,
          g: 1,
          b: 1,
          a: 1.0,
        },
      ),
    ];
    let cache = GradientLutCache::default();
    let width = 512;
    let height = 8;
    let start = Point::new(0.0, 0.0);
    let end = Point::new(width as f32, 0.0);
    let bucket = gradient_bucket(width.max(height));
    assert_eq!(bucket, 512);
    let pixmap = rasterize_linear_gradient(
      width,
      height,
      start,
      end,
      SpreadMode::Pad,
      &stops,
      &cache,
      bucket,
    )
    .expect("rasterize")
    .expect("pixmap");
    let stride = width as usize;
    let pixels = pixmap.pixels();
    for y in 0..height as usize {
      for x in 0..width as usize {
        let t = (x as f32 + 0.5) / width as f32;
        let idx = (y & 7) * 8 + (x & 7);
        let m = BAYER_8X8_MEDIUM_XY[idx] as f32;
        let dither = m * 0.015625 + 0.0078125;
        let v = (t + dither) as i32;
        let v = v.clamp(0, 255) as u8;
        let expected =
          PremultipliedColorU8::from_rgba(v, v, v, 255).expect("valid premultiplied color");
        assert_eq!(pixels[y * stride + x], expected, "pixel {x},{y}");
      }
    }
  }

  #[test]
  fn linear_gradient_dither_band_shifts_phase_for_multi_step_ramp() {
    // Regression: a 0→2 ramp spans multiple 8-bit steps, and Chrome shifts the dither phase (by 4
    // columns) each time the ramp crosses an integer boundary.
    let stops = vec![
      (0.0, Rgba::BLACK),
      (
        1.0,
        Rgba {
          r: 2,
          g: 2,
          b: 2,
          a: 1.0,
        },
      ),
    ];
    let cache = GradientLutCache::default();
    let width = 1024;
    let height = 8;
    let start = Point::new(0.0, 0.0);
    let end = Point::new(width as f32, 0.0);
    let bucket = gradient_bucket(width.max(height));
    assert_eq!(bucket, 1024);
    let pixmap = rasterize_linear_gradient(
      width,
      height,
      start,
      end,
      SpreadMode::Pad,
      &stops,
      &cache,
      bucket,
    )
    .expect("rasterize")
    .expect("pixmap");
    let stride = width as usize;
    let pixels = pixmap.pixels();
    for y in 0..height as usize {
      for x in 0..width as usize {
        let t = (x as f32 + 0.5) / width as f32;
        let val = 2.0 * t;
        let ip = val as i32;
        let frac = val - ip as f32;
        let shifted = ip & 1 == 1;
        let shift = if shifted { 4usize } else { 0usize };
        let idx = (y & 7) * 8 + ((x + shift) & 7);
        let mut m = BAYER_8X8_MEDIUM_XY[idx] as i32;
        if shifted && m == 0 {
          m = 1;
        }
        let dither = m as f32 * 0.015625 + 0.0078125;
        let v = (ip as f32 + frac + dither) as i32;
        let v = v.clamp(0, 255) as u8;
        let expected =
          PremultipliedColorU8::from_rgba(v, v, v, 255).expect("valid premultiplied color");
        assert_eq!(pixels[y * stride + x], expected, "pixel {x},{y}");
      }
    }
  }

  #[test]
  fn linear_gradient_dither_block_shifts_phase_for_large_range_ramp() {
    let stops = vec![
      (0.0, Rgba::BLACK),
      (
        1.0,
        Rgba {
          r: 32,
          g: 32,
          b: 32,
          a: 1.0,
        },
      ),
    ];
    let cache = GradientLutCache::default();
    let width = 1024;
    let height = 8;
    let start = Point::new(0.0, 0.0);
    let end = Point::new(width as f32, 0.0);
    let bucket = gradient_bucket(width.max(height));
    assert_eq!(bucket, 1024);
    let pixmap = rasterize_linear_gradient(
      width,
      height,
      start,
      end,
      SpreadMode::Pad,
      &stops,
      &cache,
      bucket,
    )
    .expect("rasterize")
    .expect("pixmap");
    let stride = width as usize;
    let pixels = pixmap.pixels();
    for y in 0..height as usize {
      for x in 0..width as usize {
        let t = (x as f32 + 0.5) / width as f32;
        let val = 32.0 * t;
        let ip = val as i32;
        let frac = val - ip as f32;
        let shift = ((ip >> 3) * 2) as usize;
        let idx = (y & 7) * 8 + ((x + shift) & 7);
        let m = BAYER_8X8_CLASSIC_XY[idx] as f32;
        let dither = m * 0.015625 + 0.0078125;
        let v = (ip as f32 + frac + dither) as i32;
        let v = v.clamp(0, 255) as u8;
        let expected =
          PremultipliedColorU8::from_rgba(v, v, v, 255).expect("valid premultiplied color");
        assert_eq!(pixels[y * stride + x], expected, "pixel {x},{y}");
      }
    }
  }

  #[test]
  fn linear_gradient_dither_classic_multi_stop_shift_matches_expected() {
    let dither_table = &BAYER_8X8_CLASSIC_XY;

    // Multi-stop gradients use a different phase-shift schedule in Chrome.
    let (shift, m0_fix) = linear_gradient_8x8_dither_shift_x(dither_table, 10, 0, 1.0, true);
    assert_eq!(shift, 0);
    assert!(!m0_fix);

    let (shift, _) = linear_gradient_8x8_dither_shift_x(dither_table, 11, 0, 1.0, true);
    assert_eq!(shift, 2);

    let (shift, _) = linear_gradient_8x8_dither_shift_x(dither_table, 27, 0, 1.0, true);
    assert_eq!(shift, 4);

    let (shift, _) = linear_gradient_8x8_dither_shift_x(dither_table, 0, 0, -1.0, true);
    assert_eq!(shift, 4);

    let (shift, _) = linear_gradient_8x8_dither_shift_x(dither_table, 8, 0, -1.0, true);
    assert_eq!(shift, 6);

    let (shift, _) = linear_gradient_8x8_dither_shift_x(dither_table, 16, 0, -1.0, true);
    assert_eq!(shift, 8);
  }

  #[test]
  fn gradient_lut_stop_positions_are_relative_to_first_stop() {
    // `GradientLut` stores samples in the coordinate space where `t` is shifted by the first stop.
    // Ensure the stop metadata used for sharp-boundary handling is expressed in that same space.
    //
    // Regression test: when stop positions were stored in absolute coordinates while `t` was
    // sampled relative to the first stop, the boundary math would clamp and the midpoint stop
    // would incorrectly blend with the segment endpoints.
    let stops = &[
      (10.0, Rgba::BLACK),
      (11.0, Rgba::WHITE),
      (12.0, Rgba::BLACK),
    ];
    let span = gradient_period(stops);
    // Use a tiny bucket so the LUT endpoints don't capture the middle stop; the sharp-stop logic
    // must kick in to preserve the boundary.
    let lut = build_gradient_lut(stops, SpreadMode::Pad, span, 1);
    let mid = lut.sample_pad(11.0);
    let expected =
      PremultipliedColorU8::from_rgba(255, 255, 255, 255).expect("valid premultiplied color");
    assert_eq!(mid, expected);
  }

  #[test]
  fn radial_gradient_samples_match_expected() {
    let stops = &[(0.0, Rgba::BLACK), (1.0, Rgba::WHITE)];
    let cache = GradientLutCache::default();
    let pixmap = rasterize_radial_gradient(
      3,
      3,
      Point::new(1.5, 1.5),
      Point::new(2.0, 2.0),
      SpreadMode::Pad,
      stops,
      &cache,
      gradient_bucket(3),
      0,
    )
    .expect("rasterize")
    .expect("pixmap");
    let pixels = pixmap.pixels();
    let stride = 3usize;
    let px = |x: usize, y: usize| pixels[y * stride + x];

    let center = PremultipliedColorU8::from_rgba(0, 0, 0, 255).unwrap();
    assert_eq!(px(1, 1), center);

    let half = PremultipliedColorU8::from_rgba(128, 128, 128, 255).unwrap();
    assert_eq!(px(0, 1), half);

    let corner = PremultipliedColorU8::from_rgba(180, 180, 180, 255).unwrap();
    assert_eq!(px(0, 0), corner);
  }

  #[test]
  fn radial_gradient_dither_matrix_matches_expected_4x4() {
    // Guardrail: radial gradients should apply the same ordered dithering used by Chrome/Skia,
    // including honoring the dither phase.
    let stops = vec![
      (0.0, Rgba::BLACK),
      (
        1.0,
        Rgba {
          r: 1,
          g: 1,
          b: 1,
          a: 1.0,
        },
      ),
    ];
    let cache = GradientLutCache::default();
    let width = 4;
    let height = 4;
    let bucket = gradient_bucket(width.max(height));
    assert_eq!(bucket, 64);
    let center = Point::new(2.0, 2.0);
    let radii = Point::new(10.0, 10.0);
    let dither_phase = (2u8 << 3) | 1u8;
    let phase_x = (dither_phase & 7) as usize;
    let phase_y = ((dither_phase >> 3) & 7) as usize;

    let pixmap = rasterize_radial_gradient(
      width,
      height,
      center,
      radii,
      SpreadMode::Pad,
      &stops,
      &cache,
      bucket,
      dither_phase,
    )
    .expect("rasterize")
    .expect("pixmap");
    let pixels = pixmap.pixels();
    let stride = width as usize;
    let lut = get_gradient_lut(&cache, &stops, SpreadMode::Pad, bucket);

    for y in 0..height as usize {
      let dy = (y as f32 + 0.5 - center.y) / radii.y;
      let dy2 = dy * dy;
      let y_mod = (y + phase_y) & 3;
      for x in 0..width as usize {
        let dx = (x as f32 + 0.5 - center.x) / radii.x;
        let t = (dx * dx + dy2).sqrt();
        let src = lut.sample_pad_f32(t);
        let x_mod = (x + phase_x) & 3;
        let idx = y_mod * 4 + x_mod;
        let m = BAYER_4X4_XY[idx] as f32;
        let dither = m * 0.0625 + 0.03125;
        let expected = GradientLut::quantize_dither(src, dither);
        assert_eq!(pixels[y * stride + x], expected, "pixel {x},{y}");
      }
    }
  }

  #[test]
  fn radial_gradient_dither_matrix_matches_expected_8x8() {
    // Guardrail: large radial gradients should switch to an 8×8 ordered dither pattern.
    let stops = vec![
      (0.0, Rgba::BLACK),
      (
        1.0,
        Rgba {
          r: 1,
          g: 1,
          b: 1,
          a: 1.0,
        },
      ),
    ];
    let cache = GradientLutCache::default();
    let width = 128;
    let height = 8;
    let bucket = gradient_bucket(width.max(height));
    assert_eq!(bucket, 128);
    let center = Point::new(0.0, 0.0);
    let radii = Point::new(1024.0, 1024.0);
    let dither_phase = (3u8 << 3) | 5u8;
    let phase_x = (dither_phase & 7) as usize;
    let phase_y = ((dither_phase >> 3) & 7) as usize;

    let pixmap = rasterize_radial_gradient(
      width,
      height,
      center,
      radii,
      SpreadMode::Pad,
      &stops,
      &cache,
      bucket,
      dither_phase,
    )
    .expect("rasterize")
    .expect("pixmap");
    let pixels = pixmap.pixels();
    let stride = width as usize;
    let lut = get_gradient_lut(&cache, &stops, SpreadMode::Pad, bucket);
    let gradient_len = radii.x.max(radii.y);
    let dither_table = linear_gradient_select_8x8_dither_table(lut.as_ref(), gradient_len);
    let multi_stop = false;

    for y in 0..height as usize {
      let dy = (y as f32 + 0.5 - center.y) / radii.y;
      let dy2 = dy * dy;
      let y_mod = (y + phase_y) & 7;
      for x in 0..width as usize {
        let dx = (x as f32 + 0.5 - center.x) / radii.x;
        let t = (dx * dx + dy2).sqrt();
        let (src, seg) = lut.sample_pad_f32_with_segment(t);
        let seg = seg as usize;
        let start = lut.stop_colors[seg];
        let (band, dom_delta) = linear_gradient_dither_band_and_dom_delta(src, start);
        let (shift, m0_fix) = linear_gradient_8x8_dither_shift_x(
          dither_table,
          band,
          seg as i32,
          dom_delta,
          multi_stop,
        );
        let x_mod = (x + phase_x) & 7;
        let idx = y_mod * 8 + ((x_mod + shift) & 7);
        let mut m = dither_table[idx];
        if m0_fix && m == 0 {
          m = 1;
        }
        let dither = m as f32 * 0.015625 + 0.0078125;
        let expected = GradientLut::quantize_dither(src, dither);
        assert_eq!(pixels[y * stride + x], expected, "pixel {x},{y}");
      }
    }
  }

  #[test]
  fn radial_gradient_pixmap_cache_key_includes_dither_phase() {
    let stops = &[(0.0, Rgba::BLACK), (1.0, Rgba::WHITE)];
    let key0 = GradientPixmapCacheKey::radial(
      10,
      10,
      Point::new(1.0, 2.0),
      Point::new(3.0, 4.0),
      SpreadMode::Pad,
      stops,
      0,
    )
    .expect("key0");
    let key1 = GradientPixmapCacheKey::radial(
      10,
      10,
      Point::new(1.0, 2.0),
      Point::new(3.0, 4.0),
      SpreadMode::Pad,
      stops,
      1,
    )
    .expect("key1");
    assert!(key0 != key1);
  }

  fn naive_conic(
    width: u32,
    height: u32,
    center: Point,
    start_angle: f32,
    stops: &[(f32, Rgba)],
    spread: SpreadMode,
  ) -> Pixmap {
    let period = gradient_period(stops);
    let Some(mut pixmap) = new_pixmap(width, height) else {
      panic!("pixmap allocation failed");
    };
    let stride = width as usize;
    let pixels = pixmap.pixels_mut();
    let inv_two_pi = 0.5 / std::f32::consts::PI;
    for y in 0..height as usize {
      let dy = y as f32 + 0.5 - center.y;
      for x in 0..width as usize {
        let dx = x as f32 + 0.5 - center.x;
        let angle = dx.atan2(-dy) + start_angle;
        let mut pos = (angle * inv_two_pi).rem_euclid(1.0) * period;
        match spread {
          SpreadMode::Repeat => {
            pos = pos.rem_euclid(period);
          }
          SpreadMode::Pad => pos = pos.clamp(0.0, period),
          SpreadMode::Reflect => {
            let two_p = period * 2.0;
            let mut v = pos.rem_euclid(two_p);
            if v > period {
              v = two_p - v;
            }
            pos = v;
          }
        }
        let color = sample_stop_color(stops, pos, period, spread);
        pixels[y * stride + x] = PremultipliedColorU8::from_rgba(
          color.r,
          color.g,
          color.b,
          (color.a * 255.0).round().clamp(0.0, 255.0) as u8,
        )
        .unwrap();
      }
    }
    pixmap
  }

  fn sample_stop_color(stops: &[(f32, Rgba)], t: f32, period: f32, spread: SpreadMode) -> Rgba {
    if stops.is_empty() {
      return Rgba::TRANSPARENT;
    }
    let mut pos = match spread {
      SpreadMode::Pad => t.clamp(0.0, period),
      SpreadMode::Repeat => t.rem_euclid(period),
      SpreadMode::Reflect => {
        let two_p = period * 2.0;
        let mut v = t.rem_euclid(two_p);
        if v > period {
          v = two_p - v;
        }
        v
      }
    };
    if pos <= stops[0].0 {
      return stops[0].1;
    }
    if pos >= stops.last().unwrap().0 {
      return stops.last().unwrap().1;
    }
    for window in stops.windows(2) {
      let (p0, c0) = window[0];
      let (p1, c1) = window[1];
      if pos >= p0 && pos <= p1 {
        let frac = ((pos - p0) / (p1 - p0)).clamp(0.0, 1.0);
        return Rgba {
          r: (c0.r as f32 + (c1.r as f32 - c0.r as f32) * frac)
            .round()
            .clamp(0.0, 255.0) as u8,
          g: (c0.g as f32 + (c1.g as f32 - c0.g as f32) * frac)
            .round()
            .clamp(0.0, 255.0) as u8,
          b: (c0.b as f32 + (c1.b as f32 - c0.b as f32) * frac)
            .round()
            .clamp(0.0, 255.0) as u8,
          a: c0.a + (c1.a - c0.a) * frac,
        };
      }
    }
    stops.last().unwrap().1
  }

  #[test]
  fn gradient_lut_cache_recovers_from_poisoned_lock() {
    let cache = GradientLutCache::default();

    let result = std::panic::catch_unwind(|| {
      let _guard = cache.inner.lock().unwrap();
      panic!("poison gradient LUT cache lock");
    });
    assert!(result.is_err(), "expected panic to be caught");
    assert!(
      cache.inner.is_poisoned(),
      "expected LUT cache mutex to be poisoned"
    );

    let stops = [(0.0, Rgba::BLACK), (1.0, Rgba::WHITE)];
    let key = GradientCacheKey::new(&stops, SpreadMode::Pad, 1.0, 16);
    let lut = cache.get_or_build(key, || build_gradient_lut(&stops, SpreadMode::Pad, 1.0, 16));
    assert_eq!(lut.span, 1.0);
    assert!(!lut.colors.is_empty());
  }

  #[test]
  fn gradient_pixmap_cache_hits_on_second_render() {
    let lut_cache = GradientLutCache::default();
    let pixmap_cache = GradientPixmapCache::default();
    let stops = vec![(0.0, Rgba::RED), (1.0, Rgba::BLUE)];
    let width = 64;
    let height = 32;
    let bucket = gradient_bucket(width.max(height));
    let start = Point::new(0.0, 0.0);
    let end = Point::new(width as f32, 0.0);

    let first = rasterize_linear_gradient_cached(
      &pixmap_cache,
      width,
      height,
      start,
      end,
      SpreadMode::Pad,
      &stops,
      &lut_cache,
      bucket,
      0,
    )
    .expect("first rasterize")
    .expect("first pixmap");
    let after_first = pixmap_cache.snapshot();
    assert_eq!(after_first.misses, 1);
    assert_eq!(after_first.hits, 0);

    let second = rasterize_linear_gradient_cached(
      &pixmap_cache,
      width,
      height,
      start,
      end,
      SpreadMode::Pad,
      &stops,
      &lut_cache,
      bucket,
      0,
    )
    .expect("second rasterize")
    .expect("second pixmap");
    let after_second = pixmap_cache.snapshot();
    assert_eq!(after_second.misses, 1);
    assert_eq!(after_second.hits, 1);
    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(first.data(), second.data());
  }

  fn max_diff(a: &Pixmap, b: &Pixmap) -> u8 {
    a.data()
      .iter()
      .zip(b.data())
      .map(|(x, y)| x.abs_diff(*y))
      .max()
      .unwrap_or(0)
  }

  #[test]
  fn conic_lut_matches_naive_with_low_error() {
    let stops = vec![(0.0, Rgba::RED), (0.5, Rgba::GREEN), (1.0, Rgba::BLUE)];
    let cache = GradientLutCache::default();
    let width = 64;
    let height = 64;
    let center = Point::new(width as f32 / 2.0, height as f32 / 2.0);
    let lut_pixmap = rasterize_conic_gradient(
      width,
      height,
      center,
      0.0,
      SpreadMode::Repeat,
      &stops,
      &cache,
      gradient_bucket(width.max(height).saturating_mul(2)),
    )
    .expect("lut rasterize")
    .expect("lut pixmap");
    let naive = naive_conic(width, height, center, 0.0, &stops, SpreadMode::Repeat);
    assert!(max_diff(&lut_pixmap, &naive) <= 1);
  }

  fn naive_conic_scaled(
    width: u32,
    height: u32,
    center: Point,
    start_angle: f32,
    stops: &[(f32, Rgba)],
    spread: SpreadMode,
    scale_x: f32,
    scale_y: f32,
  ) -> Pixmap {
    let period = gradient_period(stops);
    let Some(mut pixmap) = new_pixmap(width, height) else {
      panic!("pixmap allocation failed");
    };
    let stride = width as usize;
    let pixels = pixmap.pixels_mut();
    let inv_two_pi = 0.5 / std::f32::consts::PI;
    for y in 0..height as usize {
      let dy = (y as f32 + 0.5 - center.y) * scale_y;
      for x in 0..width as usize {
        let dx = (x as f32 + 0.5 - center.x) * scale_x;
        let angle = dx.atan2(-dy) + start_angle;
        let mut pos = (angle * inv_two_pi).rem_euclid(1.0) * period;
        match spread {
          SpreadMode::Repeat => {
            pos = pos.rem_euclid(period);
          }
          SpreadMode::Pad => pos = pos.clamp(0.0, period),
          SpreadMode::Reflect => {
            let two_p = period * 2.0;
            let mut v = pos.rem_euclid(two_p);
            if v > period {
              v = two_p - v;
            }
            pos = v;
          }
        }
        let color = sample_stop_color(stops, pos, period, spread);
        pixels[y * stride + x] = PremultipliedColorU8::from_rgba(
          color.r,
          color.g,
          color.b,
          (color.a * 255.0).round().clamp(0.0, 255.0) as u8,
        )
        .unwrap();
      }
    }
    pixmap
  }

  #[test]
  fn linear_lut_matches_naive_with_low_error() {
    let stops = vec![(0.0, Rgba::RED), (1.0, Rgba::BLUE)];
    let cache = GradientLutCache::default();
    let width = 32;
    let height = 8;
    let start = Point::new(0.0, 0.0);
    let end = Point::new(width as f32, 0.0);
    let lut_pixmap = rasterize_linear_gradient(
      width,
      height,
      start,
      end,
      SpreadMode::Pad,
      &stops,
      &cache,
      gradient_bucket(width.max(height)),
    )
    .expect("lut rasterize")
    .expect("lut pixmap");

    let mut naive = new_pixmap(width, height).expect("pixmap");
    let denom = (end.x - start.x) * (end.x - start.x) + (end.y - start.y) * (end.y - start.y);
    let inv = 1.0 / denom;
    let stride = width as usize;
    let pixels = naive.pixels_mut();
    for y in 0..height as usize {
      for x in 0..width as usize {
        let px = x as f32 + 0.5;
        let py = y as f32 + 0.5;
        let t = ((px - start.x) * (end.x - start.x) + (py - start.y) * (end.y - start.y)) * inv;
        let pos = t.clamp(0.0, 1.0);
        let color = sample_stop_color(&stops, pos, 1.0, SpreadMode::Pad);
        pixels[y * stride + x] = PremultipliedColorU8::from_rgba(
          color.r,
          color.g,
          color.b,
          (color.a * 255.0).round().clamp(0.0, 255.0) as u8,
        )
        .unwrap();
      }
    }

    assert!(max_diff(&lut_pixmap, &naive) <= 1);
  }

  #[test]
  fn linear_multi_stop_lut_matches_naive_with_low_error() {
    let stops = vec![(0.0, Rgba::RED), (0.33, Rgba::GREEN), (1.0, Rgba::BLUE)];
    let cache = GradientLutCache::default();
    let width = 64;
    let height = 8;
    let start = Point::new(0.0, 0.0);
    let end = Point::new(width as f32, 0.0);
    let lut_pixmap = rasterize_linear_gradient(
      width,
      height,
      start,
      end,
      SpreadMode::Pad,
      &stops,
      &cache,
      gradient_bucket(width.max(height)),
    )
    .expect("lut rasterize")
    .expect("lut pixmap");

    let mut naive = new_pixmap(width, height).expect("pixmap");
    let denom = (end.x - start.x) * (end.x - start.x) + (end.y - start.y) * (end.y - start.y);
    let inv = 1.0 / denom;
    let stride = width as usize;
    let pixels = naive.pixels_mut();
    for y in 0..height as usize {
      for x in 0..width as usize {
        let px = x as f32 + 0.5;
        let py = y as f32 + 0.5;
        let t = ((px - start.x) * (end.x - start.x) + (py - start.y) * (end.y - start.y)) * inv;
        let pos = t.clamp(0.0, 1.0);
        let color = sample_stop_color(&stops, pos, 1.0, SpreadMode::Pad);
        pixels[y * stride + x] = PremultipliedColorU8::from_rgba(
          color.r,
          color.g,
          color.b,
          (color.a * 255.0).round().clamp(0.0, 255.0) as u8,
        )
        .unwrap();
      }
    }

    assert!(max_diff(&lut_pixmap, &naive) <= 1);
  }

  #[test]
  fn linear_gradient_premultiplies_semi_transparent_stops() {
    let color = Rgba {
      r: 0,
      g: 255,
      b: 0,
      a: 0.5,
    };
    let stops = vec![(0.0, color), (1.0, color)];
    let cache = GradientLutCache::default();
    let pixmap = rasterize_linear_gradient(
      1,
      1,
      Point::new(0.0, 0.0),
      Point::new(0.0, 0.0),
      SpreadMode::Pad,
      &stops,
      &cache,
      1,
    )
    .expect("gradient rasterize")
    .expect("gradient pixmap");
    let px = pixmap.pixel(0, 0).expect("pixel");
    assert_eq!(px.red(), 0);
    assert_eq!(px.green(), 128);
    assert_eq!(px.blue(), 0);
    assert_eq!(px.alpha(), 128);
  }

  #[test]
  fn linear_gradient_src_over_dithers_after_blend() {
    let mut dest = new_pixmap(4, 1).expect("pixmap");
    let dst = PremultipliedColorU8::from_rgba(200, 10, 30, 255).expect("premultiplied dst");
    dest.pixels_mut().fill(dst);

    let faint_red = Rgba {
      r: 193,
      g: 0,
      b: 0,
      a: 1.0 / 255.0,
    };
    let stops = vec![(0.0, faint_red), (1.0, faint_red)];
    let cache = GradientLutCache::default();
    let bucket = gradient_bucket(1);

    let start = Point::new(0.0, 0.0);
    let end = Point::new(1.0, 0.0);

    // This pixel uses the first Bayer matrix entry, which yields the smallest dither offset.
    let dither = BAYER_4X4_XY[0] as f32 * 0.0625 + 0.03125;
    let lut = build_gradient_lut(&stops, SpreadMode::Pad, gradient_period(&stops), bucket);
    let src = lut.sample_pad_f32(0.5);

    let sa = (src[3] * (1.0 / 255.0)).clamp(0.0, 1.0);
    let inv = 1.0 - sa;
    let out = [
      src[0] + dst.red() as f32 * inv,
      src[1] + dst.green() as f32 * inv,
      src[2] + dst.blue() as f32 * inv,
      src[3] + dst.alpha() as f32 * inv,
    ];
    let expected = GradientLut::quantize_dither(out, dither);

    // Ensure the test setup is sensitive to whether dithering happens before or after blending.
    let src_pre = GradientLut::quantize_dither(src, dither);
    let sa_pre = (src_pre.alpha() as f32) * (1.0 / 255.0);
    let inv_pre = 1.0 - sa_pre;
    let out_pre = [
      src_pre.red() as f32 + dst.red() as f32 * inv_pre,
      src_pre.green() as f32 + dst.green() as f32 * inv_pre,
      src_pre.blue() as f32 + dst.blue() as f32 * inv_pre,
      src_pre.alpha() as f32 + dst.alpha() as f32 * inv_pre,
    ];
    let expected_preblend = GradientLut::quantize_dither(out_pre, dither);
    assert_ne!(
      expected, expected_preblend,
      "expected the test pixel to change when dithering is applied before blending"
    );

    paint_linear_gradient_src_over(
      &mut dest,
      0,
      0,
      1,
      1,
      start,
      end,
      SpreadMode::Pad,
      &stops,
      &cache,
      bucket,
      0,
      None,
    )
    .expect("paint src-over");

    let actual = dest.pixel(0, 0).expect("pixel");
    assert_eq!(actual, expected);
  }

  #[test]
  fn linear_gradient_src_over_matches_opaque_pattern_subrect() {
    let tile_w = 8u32;
    let tile_h = 8u32;
    let origin_x = 2i32;
    let origin_y = 3i32;
    let dest_x = 4i32;
    let dest_y = 5i32;
    let sub_w = 3u32;
    let sub_h = 2u32;

    let stops = vec![(0.0, Rgba::RED), (1.0, Rgba::BLUE)];
    let cache = GradientLutCache::default();
    let bucket = gradient_bucket(tile_w.max(tile_h));

    // Reference output: rasterize the whole tile with the pattern's origin-based dither phase, then
    // copy the relevant sub-rectangle into the destination surface.
    let tile_dither_phase = (((origin_y & 7) as u8) << 3) | ((origin_x & 7) as u8);
    let tile = rasterize_linear_gradient_with_phase(
      tile_w,
      tile_h,
      Point::new(0.0, 0.0),
      Point::new(0.0, tile_h as f32),
      SpreadMode::Pad,
      &stops,
      &cache,
      bucket,
      tile_dither_phase,
    )
    .expect("tile rasterize")
    .expect("tile pixmap");

    let bg = PremultipliedColorU8::from_rgba(10, 20, 30, 255).expect("bg premultiplied");
    let mut expected = new_pixmap(20, 20).expect("expected pixmap");
    expected.pixels_mut().fill(bg);
    let mut actual = new_pixmap(20, 20).expect("actual pixmap");
    actual.pixels_mut().fill(bg);

    let src_x = (dest_x - origin_x) as usize;
    let src_y = (dest_y - origin_y) as usize;
    let dst_x = dest_x as usize;
    let dst_y = dest_y as usize;
    let copy_w = sub_w as usize;
    let copy_h = sub_h as usize;

    let src_stride = tile_w as usize * 4;
    let dst_stride = expected.width() as usize * 4;
    let row_bytes = copy_w * 4;
    let tile_data = tile.data();
    let expected_data = expected.data_mut();
    for row in 0..copy_h {
      let src_off = (src_y + row) * src_stride + src_x * 4;
      let dst_off = (dst_y + row) * dst_stride + dst_x * 4;
      expected_data[dst_off..dst_off + row_bytes]
        .copy_from_slice(&tile_data[src_off..src_off + row_bytes]);
    }

    // Now paint the same sub-rect directly, using the local start/end coordinates that correspond
    // to sampling the (single) visible tile.
    let offset_x = (dest_x - origin_x) as f32;
    let offset_y = (dest_y - origin_y) as f32;
    let start = Point::new(-offset_x, -offset_y);
    let end = Point::new(-offset_x, tile_h as f32 - offset_y);
    let sub_dither_phase = (((dest_y & 7) as u8) << 3) | ((dest_x & 7) as u8);
    paint_linear_gradient_src_over(
      &mut actual,
      dest_x,
      dest_y,
      sub_w,
      sub_h,
      start,
      end,
      SpreadMode::Pad,
      &stops,
      &cache,
      bucket,
      sub_dither_phase,
      None,
    )
    .expect("paint src-over");

    assert_eq!(actual.data(), expected.data());
  }

  #[test]
  fn conic_lut_scaled_matches_naive_with_low_error() {
    let stops = vec![(0.0, Rgba::RED), (0.5, Rgba::GREEN), (1.0, Rgba::BLUE)];
    let cache = GradientLutCache::default();
    let width = 64;
    let height = 32;
    let center = Point::new(width as f32 / 2.0, height as f32 / 2.0);
    let scale_x = 0.75;
    let scale_y = 1.25;
    let lut_pixmap = rasterize_conic_gradient_scaled(
      width,
      height,
      center,
      0.0,
      SpreadMode::Repeat,
      &stops,
      &cache,
      gradient_bucket(width.max(height)),
      scale_x,
      scale_y,
    )
    .expect("lut rasterize")
    .expect("lut pixmap");
    let naive = naive_conic_scaled(
      width,
      height,
      center,
      0.0,
      &stops,
      SpreadMode::Repeat,
      scale_x,
      scale_y,
    );
    let diff = max_diff(&lut_pixmap, &naive);
    assert!(
      diff <= 2,
      "expected scaled conic LUT raster to be close to naive; max_diff={diff}"
    );
  }

  #[test]
  fn gradient_rasterizers_timeout_under_tiny_deadline() {
    let stops = vec![(0.0, Rgba::RED), (1.0, Rgba::BLUE)];
    let cache = GradientLutCache::default();
    // Warm up the rayon thread pool so this test measures cooperative deadline handling rather
    // than one-time thread pool initialization overhead (which can be large on CI).
    rayon::join(|| {}, || {});

    let width = 1024;
    let height = 1024;
    let center = Point::new(width as f32 / 2.0, height as f32 / 2.0);
    let deadline = RenderDeadline::new(Some(Duration::from_millis(1)), None);
    let start = Instant::now();
    let result = with_deadline(Some(&deadline), || {
      rasterize_conic_gradient(
        width,
        height,
        center,
        0.0,
        SpreadMode::Repeat,
        &stops,
        &cache,
        gradient_bucket(width.max(height).saturating_mul(2)),
      )
    });
    let elapsed = start.elapsed();
    let timeout_elapsed = match &result {
      Err(RenderError::Timeout { elapsed, .. }) => Some(*elapsed),
      _ => None,
    };
    assert!(
      matches!(
        result,
        Err(RenderError::Timeout {
          stage: RenderStage::Paint,
          ..
        })
      ),
      "expected timeout, got {result:?}"
    );
    assert!(
      elapsed < Duration::from_millis(250),
      "timeout should be cooperative (elapsed {elapsed:?}, error_elapsed={timeout_elapsed:?})"
    );
  }

  #[test]
  fn gradient_output_unchanged_with_deadline_enabled() {
    let cache = GradientLutCache::default();
    let deadline = RenderDeadline::new(Some(Duration::from_secs(60)), None);

    let linear_stops = vec![(0.0, Rgba::RED), (0.35, Rgba::GREEN), (1.0, Rgba::BLUE)];
    let width = 256;
    let height = 128;
    let start = Point::new(0.0, 0.0);
    let end = Point::new(width as f32, height as f32);
    let bucket = gradient_bucket(width.max(height));
    let base = rasterize_linear_gradient(
      width,
      height,
      start,
      end,
      SpreadMode::Pad,
      &linear_stops,
      &cache,
      bucket,
    )
    .expect("linear rasterize")
    .expect("linear pixmap");
    let with_deadline_pixmap = with_deadline(Some(&deadline), || {
      rasterize_linear_gradient(
        width,
        height,
        start,
        end,
        SpreadMode::Pad,
        &linear_stops,
        &cache,
        bucket,
      )
    })
    .expect("linear rasterize with deadline")
    .expect("linear pixmap with deadline");
    assert_eq!(base.data(), with_deadline_pixmap.data());

    let conic_stops = vec![(0.0, Rgba::BLACK), (0.5, Rgba::WHITE), (1.0, Rgba::BLACK)];
    let size = 192u32;
    let center = Point::new(size as f32 / 2.0, size as f32 / 2.0);
    let bucket = gradient_bucket(size.saturating_mul(2));
    let base = rasterize_conic_gradient(
      size,
      size,
      center,
      0.4,
      SpreadMode::Repeat,
      &conic_stops,
      &cache,
      bucket,
    )
    .expect("conic rasterize")
    .expect("conic pixmap");
    let with_deadline_pixmap = with_deadline(Some(&deadline), || {
      rasterize_conic_gradient(
        size,
        size,
        center,
        0.4,
        SpreadMode::Repeat,
        &conic_stops,
        &cache,
        bucket,
      )
    })
    .expect("conic rasterize with deadline")
    .expect("conic pixmap with deadline");
    assert_eq!(base.data(), with_deadline_pixmap.data());
  }
}
