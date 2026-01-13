//! Text Rasterization Module
//!
//! Rasterizes text glyphs using font outlines and tiny-skia.
//!
//! # Overview
//!
//! This module converts shaped text (glyphs with positions) into pixels by:
//! 1. Extracting glyph outlines from font files using ttf-parser
//! 2. Converting outlines to tiny-skia paths
//! 3. Rendering paths to a pixmap with proper positioning and styling
//!
//! # Architecture
//!
//! ```text
//! ShapedRun (glyphs + font)
//!       ↓
//! GlyphOutlineBuilder (ttf-parser → path)
//!       ↓
//! GlyphCache (cached paths per font/glyph)
//!       ↓
//! TextRasterizer (render to pixmap)
//! ```
//!
//! # Example
//!
//! ```rust,ignore
//! use fastrender::paint::text_rasterize::TextRasterizer;
//! use fastrender::text::pipeline::ShapedRun;
//!
//! let mut rasterizer = TextRasterizer::new();
//! let pixmap = new_pixmap(800, 600).unwrap();
//!
//! rasterizer.render_shaped_run(
//!     &shaped_run,
//!     100.0,  // x position
//!     200.0,  // baseline y position
//!     Rgba::BLACK,
//!     &mut pixmap,
//! );
//! ```
//!
//! # CSS Specification
//!
//! Text rendering follows CSS specifications:
//! - CSS Fonts Module Level 4: Font rendering properties
//! - CSS Text Module Level 3: Text rendering
//!
//! # References
//!
//! - ttf-parser: <https://docs.rs/ttf-parser/>
//! - tiny-skia: <https://docs.rs/tiny-skia/>

use crate::debug::runtime;
use crate::error::{Error, RenderError, RenderStage, Result};
#[cfg(test)]
use crate::paint::pixmap::new_pixmap;
use crate::paint::pixmap::new_pixmap_with_context;
use crate::render_control::{check_active, check_active_periodic};
use crate::style::color::Rgba;
use crate::style::types::FontSmoothing;
use crate::text::color_fonts::{ColorFontRenderer, ColorGlyphRaster};
use crate::text::font_db::LoadedFont;
use crate::text::font_instance::{glyph_transform, FontInstance, GlyphOutlineMetrics};
use crate::text::pipeline::{
  record_text_rasterize, text_diagnostics_timer, GlyphPosition, ShapedRun, TextCacheStats,
  TextDiagnosticsStage,
};
use crate::text::variations::variation_hash;
use lru::LruCache;
use rustybuzz::Variation;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use tiny_skia::BlendMode as SkiaBlendMode;
use tiny_skia::Color;
use tiny_skia::FillRule;
use tiny_skia::Mask;
use tiny_skia::Paint;
use tiny_skia::Path;
use tiny_skia::Pixmap;
use tiny_skia::PixmapMut;
use tiny_skia::PixmapPaint;
use tiny_skia::Transform;

const DEFAULT_GLYPH_CACHE_ITEMS: usize = 2048;
const DEFAULT_GLYPH_CACHE_BYTES: usize = 32 * 1024 * 1024;
const ENV_GLYPH_CACHE_ITEMS: &str = "FASTR_GLYPH_CACHE_ITEMS";
const ENV_GLYPH_CACHE_BYTES: &str = "FASTR_GLYPH_CACHE_BYTES";
const DEFAULT_COLOR_GLYPH_CACHE_ITEMS: usize = 2048;
const DEFAULT_COLOR_GLYPH_CACHE_BYTES: usize = 16 * 1024 * 1024;
const ENV_COLOR_GLYPH_CACHE_ITEMS: &str = "FASTR_COLOR_GLYPH_CACHE_ITEMS";
const ENV_COLOR_GLYPH_CACHE_BYTES: &str = "FASTR_COLOR_GLYPH_CACHE_BYTES";
const ENV_TEXT_SUBPIXEL_AA_GAMMA: &str = "FASTR_TEXT_SUBPIXEL_AA_GAMMA";
const ENV_TEXT_SUBPIXEL_AA_DIAGNOSTICS: &str = "FASTR_TEXT_SUBPIXEL_AA_DIAGNOSTICS";
const ENV_TEXT_SNAP_GLYPH_POSITIONS: &str = "FASTR_TEXT_SNAP_GLYPH_POSITIONS";
const SCALE_QUANTIZATION: f32 = 512.0;
const DEADLINE_STRIDE: usize = 256;
const SUBPIXEL_AA_SCALE_X: u32 = 3;
const SUBPIXEL_AA_PAD_PX: i32 = 1;
// A small symmetric filter (in subpixel units) to reduce color fringes and better approximate
// browser LCD text rendering.
//
// This matches FreeType's `FT_LCD_FILTER_DEFAULT` weights (0x10,0x40,0x70,0x40,0x10). The sum is
// intentionally > 256; after the fixed-point divide-by-256 we clamp to 255 to avoid u8 wraparound.
const SUBPIXEL_AA_LCD_FILTER_WEIGHTS: [u32; 5] = [16, 64, 112, 64, 16];

#[inline]
fn subpixel_aa_lcd_filtered_alpha(row: &[u8], center_sub_x: i32, width_sub_i32: i32) -> u8 {
  let mut acc = 0u32;
  for (i, weight) in SUBPIXEL_AA_LCD_FILTER_WEIGHTS.iter().enumerate() {
    let offset = i as i32 - 2;
    let sub_x = center_sub_x + offset;
    if sub_x < 0 || sub_x >= width_sub_i32 {
      continue;
    }
    let idx = sub_x as usize * 4 + 3;
    acc += u32::from(row[idx]) * *weight;
  }
  let filtered = (acc + 128) >> 8;
  filtered.min(255) as u8
}

fn lcd_gamma_lut() -> &'static [u8; 256] {
  // Browsers typically apply a gamma/contrast adjustment to glyph coverages (especially for LCD
  // text) so strokes retain visual weight. Our subpixel AA path operates directly on tiny-skia's
  // coverage mask; without a light-weight remapping, glyph edges can appear too thin compared to
  // Chrome's FreeType/Skia pipeline, producing pervasive diffs on text-heavy fixtures.
  //
  // Approximate this by applying a simple gamma curve to the 0-255 coverage values. Tune this to
  // be conservative (avoid visibly over-darkening text) while still reducing the "washed out"
  // appearance of unadjusted coverages.
  const LCD_COVERAGE_GAMMA: f32 = 1.6;
  static LUT: OnceLock<[u8; 256]> = OnceLock::new();
  LUT.get_or_init(|| {
    let mut table = [0u8; 256];
    for (idx, slot) in table.iter_mut().enumerate() {
      let cov = idx as f32 / 255.0;
      let adjusted = (cov.powf(1.0 / LCD_COVERAGE_GAMMA) * 255.0).round();
      *slot = adjusted.clamp(0.0, 255.0) as u8;
    }
    table
  })
}

#[derive(Default)]
struct SubpixelAAScratch {
  pixmap: Option<Pixmap>,
}

impl std::fmt::Debug for SubpixelAAScratch {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("SubpixelAAScratch")
      .field(
        "allocated",
        &self.pixmap.as_ref().map(|p| (p.width(), p.height())),
      )
      .finish()
  }
}

impl SubpixelAAScratch {
  fn get_or_resize(&mut self, width: u32, height: u32) -> Result<&mut Pixmap> {
    let needs_new = match self.pixmap.as_ref() {
      Some(existing) => existing.width() != width || existing.height() != height,
      None => true,
    };
    if needs_new {
      self.pixmap = Some(new_pixmap_with_context(
        width,
        height,
        "text subpixel AA mask",
      )?);
    }
    if self.pixmap.is_none() {
      // We should only hit this when something unexpected cleared the cached pixmap. Retry the
      // allocation once and surface a deterministic error if the scratch is still missing.
      self.pixmap = Some(new_pixmap_with_context(width, height, "text subpixel AA mask")?);
    }

    let Some(pixmap) = self.pixmap.as_mut() else {
      return Err(Error::Render(RenderError::RasterizationFailed {
        reason: "text subpixel AA scratch pixmap missing after allocation".to_string(),
      }));
    };
    pixmap.data_mut().fill(0);
    Ok(pixmap)
  }
}

struct SubpixelAAGammaLut {
  gamma: f32,
  table: [u8; 256],
}

#[derive(Debug, Default, Clone, Copy)]
struct SubpixelAADiagnostics {
  skips_disabled: u64,
  skips_blend_mode: u64,
  skips_rotation: u64,
  skips_state_transform: u64,
  attempts: u64,
  successes: u64,
  failures_non_axis_aligned: u64,
  failures_clip_mask_mismatch: u64,
  failures_mask_overflow: u64,
  failures_other: u64,
}

impl std::fmt::Debug for SubpixelAAGammaLut {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("SubpixelAAGammaLut")
      .field("gamma", &self.gamma)
      .finish()
  }
}

impl SubpixelAAGammaLut {
  fn new(gamma: f32) -> Self {
    // Interpret `gamma` like a standard gamma value (>1 darkens edge coverage, <1 lightens).
    let exponent = if gamma.is_finite() && gamma > 0.0 {
      1.0 / gamma
    } else {
      1.0
    };
    let mut table = [0u8; 256];
    for (idx, entry) in table.iter_mut().enumerate() {
      let a = idx as f32 / 255.0;
      let corrected = a.powf(exponent);
      *entry = (corrected * 255.0).round().clamp(0.0, 255.0) as u8;
    }
    Self { gamma, table }
  }
}

/// Computes the advance width of a glyph with the given variation settings applied.
pub fn glyph_advance_with_variations(
  font: &LoadedFont,
  glyph_id: u32,
  font_size: f32,
  variations: &[Variation],
) -> Result<f32> {
  let instance =
    FontInstance::new(font, variations).ok_or_else(|| RenderError::RasterizationFailed {
      reason: "Unable to create font instance".into(),
    })?;
  let outline =
    instance
      .glyph_outline(glyph_id)
      .ok_or_else(|| RenderError::RasterizationFailed {
        reason: "Unable to build glyph outline".into(),
      })?;
  let scale = font_size / instance.units_per_em();
  Ok(outline.advance * scale)
}

/// Renders a single glyph with the provided variation settings into the target pixmap.
pub fn render_glyph_with_variations(
  font: &LoadedFont,
  glyph_id: u32,
  font_size: f32,
  x: f32,
  baseline_y: f32,
  color: Rgba,
  variations: &[Variation],
  pixmap: &mut Pixmap,
) -> Result<()> {
  let glyph = GlyphPosition {
    glyph_id,
    cluster: 0,
    x_offset: 0.0,
    y_offset: 0.0,
    x_advance: 0.0,
    y_advance: 0.0,
  };
  let mut rasterizer = TextRasterizer::default();
  rasterizer.render_glyph_run(
    &[glyph],
    font,
    font_size,
    0.0,
    0.0,
    0,
    &[],
    0,
    variations,
    None,
    x,
    baseline_y,
    color,
    TextRenderState::default(),
    pixmap,
  )?;
  Ok(())
}

// ============================================================================
// Glyph Cache
// ============================================================================

/// Cache key for glyph outlines.
///
/// `FontInstance::glyph_outline` returns paths in font design units (unscaled and
/// without synthetic oblique). The cache key therefore excludes draw-time
/// transforms like font size and skew unless hinting is enabled.
///
/// When hinting is enabled, glyph outlines depend on the font size (ppem) because
/// the hinting engine grid-fits to device pixels. In that case the cache key must
/// include the font size so we don't reuse outlines across incompatible sizes.
///
/// Using the font data pointer avoids comparing large font binaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct GlyphCacheKey {
  /// Pointer to font data (used as unique identifier)
  font_ptr: usize,
  /// Font face index within the file
  font_index: u32,
  /// Glyph ID within the font
  glyph_id: u32,
  /// Hash of variation coordinates applied to the face.
  variation_hash: u64,
  font_size_hundredths: u32,
  hinting: bool,
}

impl GlyphCacheKey {
  fn new(
    font: &LoadedFont,
    glyph_id: u32,
    variation_hash: u64,
    font_size: f32,
    hinting: bool,
  ) -> Self {
    let font_size_hundredths = if hinting && font_size.is_finite() && font_size > 0.0 {
      (font_size * 100.0)
        .round()
        .clamp(0.0, u32::MAX as f32)
        .trunc() as u32
    } else {
      0
    };
    Self {
      font_ptr: Arc::as_ptr(&font.data) as usize,
      font_index: font.index,
      glyph_id,
      variation_hash,
      font_size_hundredths,
      hinting,
    }
  }
}

/// Cached glyph path for efficient repeated rendering.
///
/// Contains a pre-built path that can be reused across multiple
/// render calls at different font sizes; scaling and skew are applied at draw time.
/// (Reserved for glyph caching optimization)
#[derive(Debug, Clone)]
struct CachedGlyph {
  /// The rendered path, or None if the glyph has no outline
  path: Option<Arc<Path>>,
  /// Horizontal advance for this glyph (font design units).
  advance: f32,
  /// LRU timestamp (monotonic counter)
  last_used: u64,
  /// Rough estimate of memory usage for budgeting/eviction
  estimated_size: usize,
}

/// Lightweight cache metrics for profiling.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GlyphCacheStats {
  /// Number of cache hits (outline reused)
  pub hits: u64,
  /// Number of cache misses (outline had to be built)
  pub misses: u64,
  /// Number of entries evicted by the cache policy
  pub evictions: u64,
  /// Current estimated bytes stored in the cache
  pub bytes: usize,
}

impl GlyphCacheStats {
  fn delta_from(&self, baseline: &GlyphCacheStats) -> GlyphCacheStats {
    GlyphCacheStats {
      hits: self.hits.saturating_sub(baseline.hits),
      misses: self.misses.saturating_sub(baseline.misses),
      evictions: self.evictions.saturating_sub(baseline.evictions),
      bytes: self.bytes,
    }
  }
}

/// Cache for rendered glyph paths.
///
/// Glyph outlines are cached in font design units (unpositioned) and
/// transformed at draw time. The cache key includes the font identity, face
/// index, glyph id, and variation coordinates.
///
/// The cache uses an LRU eviction policy with an optional memory budget
/// to keep footprint predictable while still delivering reuse on text-
/// heavy pages.
#[derive(Debug)]
pub struct GlyphCache {
  /// Cached glyph paths
  glyphs: HashMap<GlyphCacheKey, CachedGlyph>,
  /// Scratch slot used when caching is disabled (max entries/bytes set to 0).
  scratch: Option<CachedGlyph>,
  /// Usage order for LRU eviction (key + generation)
  usage_queue: VecDeque<(GlyphCacheKey, u64)>,
  /// Maximum cache size
  max_size: usize,
  /// Optional memory budget (bytes) for cached paths
  max_bytes: Option<usize>,
  /// Current estimated memory used by cached paths
  current_bytes: usize,
  /// Peak observed memory usage to report in diagnostics
  peak_bytes: usize,
  /// Monotonic counter used for LRU
  generation: u64,
  /// Cache hit count (for profiling)
  hits: u64,
  /// Cache miss count (for profiling)
  misses: u64,
  /// Number of evicted glyphs
  evictions: u64,
}

impl Default for GlyphCache {
  fn default() -> Self {
    Self::new()
  }
}

impl GlyphCache {
  /// Creates a new glyph cache with default size.
  pub fn new() -> Self {
    let max_size = DEFAULT_GLYPH_CACHE_ITEMS;
    Self {
      glyphs: HashMap::new(),
      scratch: None,
      max_size,
      max_bytes: Some(DEFAULT_GLYPH_CACHE_BYTES),
      usage_queue: VecDeque::new(),
      current_bytes: 0,
      peak_bytes: 0,
      generation: 0,
      hits: 0,
      misses: 0,
      evictions: 0,
    }
  }

  /// Creates a cache with custom maximum size.
  pub fn with_capacity(max_size: usize) -> Self {
    Self {
      glyphs: HashMap::with_capacity(max_size.min(256)),
      scratch: None,
      max_size,
      max_bytes: Some(DEFAULT_GLYPH_CACHE_BYTES),
      usage_queue: VecDeque::new(),
      current_bytes: 0,
      peak_bytes: 0,
      generation: 0,
      hits: 0,
      misses: 0,
      evictions: 0,
    }
  }

  /// Creates a cache with both glyph count and memory budget.
  pub fn with_limits(max_size: usize, max_bytes: Option<usize>) -> Self {
    Self {
      glyphs: HashMap::with_capacity(max_size.min(256)),
      scratch: None,
      max_size,
      max_bytes,
      usage_queue: VecDeque::new(),
      current_bytes: 0,
      peak_bytes: 0,
      generation: 0,
      hits: 0,
      misses: 0,
      evictions: 0,
    }
  }

  /// Updates the maximum number of cached glyphs.
  pub fn set_max_size(&mut self, max_size: usize) {
    self.max_size = max_size;
    if self.caching_disabled() {
      self.clear();
      return;
    }
    self.evict_if_needed();
  }

  /// Sets an optional memory budget (in bytes) for cached glyphs.
  pub fn set_max_bytes(&mut self, max_bytes: Option<usize>) {
    self.max_bytes = max_bytes;
    if self.caching_disabled() {
      self.clear();
      return;
    }
    self.evict_if_needed();
  }

  fn caching_disabled(&self) -> bool {
    self.max_size == 0 || matches!(self.max_bytes, Some(0))
  }

  /// Gets a cached glyph path or builds and caches it.
  fn get_or_build(
    &mut self,
    font: &LoadedFont,
    instance: &FontInstance,
    glyph_id: u32,
    font_size: f32,
    hinting: bool,
  ) -> Option<&CachedGlyph> {
    let key = GlyphCacheKey::new(
      font,
      glyph_id,
      instance.variation_hash(),
      font_size,
      hinting,
    );
    let generation = self.bump_generation();

    if self.caching_disabled() {
      self.misses += 1;
      self.scratch = None;
      let mut cached = self.build_glyph_path(font, instance, glyph_id, font_size, hinting)?;
      cached.last_used = generation;
      self.scratch = Some(cached);
      return self.scratch.as_ref();
    }

    // Ensure we don't retain a large outline in the scratch slot after caching has been enabled.
    self.scratch = None;

    if let Some(entry) = self.glyphs.get_mut(&key) {
      self.hits += 1;
      entry.last_used = generation;
    } else {
      self.misses += 1;
      let mut cached = self.build_glyph_path(font, instance, glyph_id, font_size, hinting)?;
      cached.last_used = generation;
      if let Some(max_bytes) = self.max_bytes {
        if max_bytes > 0 && cached.estimated_size > max_bytes {
          // Preserve correctness when a single glyph exceeds the configured byte budget: return the
          // rasterized outline but do not insert it into the cache (it would immediately evict
          // itself and we'd return `None`).
          self.scratch = Some(cached);
          return self.scratch.as_ref();
        }
      }
      self.current_bytes = self.current_bytes.saturating_add(cached.estimated_size);
      self.peak_bytes = self.peak_bytes.max(self.current_bytes);
      self.glyphs.insert(key, cached);
    }

    self.usage_queue.push_back((key, generation));
    self.evict_if_needed();
    self.glyphs.get(&key)
  }

  /// Builds a glyph path without caching.
  fn build_glyph_path(
    &self,
    font: &LoadedFont,
    instance: &FontInstance,
    glyph_id: u32,
    font_size: f32,
    hinting: bool,
  ) -> Option<CachedGlyph> {
    let outline = if hinting {
      instance.glyph_outline_hinted(font, glyph_id, font_size)?
    } else {
      instance.glyph_outline(glyph_id)?
    };
    let has_outline = outline.path.is_some();
    let estimated_size = if has_outline {
      estimate_glyph_size(&outline.metrics)
    } else {
      0
    };

    Some(CachedGlyph {
      path: outline.path.map(Arc::new),
      advance: outline.advance,
      last_used: 0,
      estimated_size,
    })
  }

  /// Returns the number of cached glyphs.
  #[inline]
  pub fn len(&self) -> usize {
    self.glyphs.len()
  }

  /// Returns whether the cache is empty.
  #[inline]
  pub fn is_empty(&self) -> bool {
    self.glyphs.is_empty()
  }

  /// Clears all cached glyphs.
  pub fn clear(&mut self) {
    self.glyphs.clear();
    self.scratch = None;
    self.usage_queue.clear();
    self.current_bytes = 0;
    self.peak_bytes = 0;
  }

  /// Returns cache statistics (hits, misses, evictions).
  pub fn stats(&self) -> GlyphCacheStats {
    GlyphCacheStats {
      hits: self.hits,
      misses: self.misses,
      evictions: self.evictions,
      bytes: self.current_bytes.max(self.peak_bytes),
    }
  }

  /// Resets cache statistics without clearing cached glyphs.
  pub fn reset_stats(&mut self) {
    self.hits = 0;
    self.misses = 0;
    self.evictions = 0;
    self.peak_bytes = self.current_bytes;
  }

  /// Bumps the generation counter used for LRU ordering.
  fn bump_generation(&mut self) -> u64 {
    self.generation = self.generation.wrapping_add(1);
    self.generation
  }

  /// Evicts old entries if cache is full.
  fn evict_if_needed(&mut self) {
    let max_bytes = self.max_bytes.unwrap_or(usize::MAX);

    while self.glyphs.len() > self.max_size || self.current_bytes > max_bytes {
      if let Some((key, generation)) = self.usage_queue.pop_front() {
        if let Some(entry) = self.glyphs.get(&key) {
          if entry.last_used == generation {
            if let Some(removed) = self.glyphs.remove(&key) {
              self.current_bytes = self.current_bytes.saturating_sub(removed.estimated_size);
              self.evictions += 1;
            }
          }
        }
      } else {
        break;
      }
    }
  }
}

// ============================================================================
// Color Glyph Cache
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ColorGlyphCacheKey {
  font_ptr: usize,
  font_index: u32,
  glyph_id: u32,
  font_size_hundredths: u32,
  palette_index: u16,
  variation_hash: u64,
  /// RGB signature of the text color. Alpha is applied at draw time so it is
  /// intentionally excluded to keep cached rasters reusable across opacity
  /// changes while still differentiating currentColor layers by hue.
  color_signature: u32,
  palette_override_hash: u64,
  synthetic_oblique_units: i32,
  transform_signature: u64,
}

impl ColorGlyphCacheKey {
  fn new(
    font: &LoadedFont,
    glyph_id: u32,
    font_size: f32,
    palette_index: u16,
    variations: &[Variation],
    color: Rgba,
    palette_override_hash: u64,
    synthetic_oblique: f32,
    transform_signature: u64,
  ) -> Self {
    Self {
      font_ptr: Arc::as_ptr(&font.data) as usize,
      font_index: font.index,
      glyph_id,
      font_size_hundredths: (font_size * 100.0) as u32,
      palette_index,
      variation_hash: variation_hash(variations),
      color_signature: rgb_signature(color),
      palette_override_hash,
      synthetic_oblique_units: quantize_skew(synthetic_oblique),
      transform_signature,
    }
  }
}

#[derive(Debug)]
pub(crate) struct ColorGlyphCache {
  glyphs: LruCache<ColorGlyphCacheKey, Option<ColorGlyphRaster>>,
  max_size: usize,
  max_bytes: usize,
  current_bytes: usize,
  peak_bytes: usize,
  hits: u64,
  misses: u64,
  evictions: u64,
}

impl Default for ColorGlyphCache {
  fn default() -> Self {
    Self::new()
  }
}

impl ColorGlyphCache {
  pub(crate) fn with_limits(max_size: usize, max_bytes: usize) -> Self {
    Self {
      glyphs: LruCache::unbounded(),
      max_size,
      max_bytes,
      current_bytes: 0,
      peak_bytes: 0,
      hits: 0,
      misses: 0,
      evictions: 0,
    }
  }

  pub(crate) fn new() -> Self {
    // The cache stores both successful rasters and negative lookups (`None`) so we avoid
    // repeatedly attempting expensive color-glyph rasterization for glyphs that fall back to
    // outlines. Negative entries are tiny (no pixmap bytes) but they still occupy an LRU slot, so
    // keep the default entry budget comfortably above the number of likely "text glyphs" to avoid
    // crowding out actual color rasters on pages that mix emoji fonts with normal text.
    Self::with_limits(
      DEFAULT_COLOR_GLYPH_CACHE_ITEMS,
      DEFAULT_COLOR_GLYPH_CACHE_BYTES,
    )
  }

  fn caching_disabled(&self) -> bool {
    self.max_size == 0 || self.max_bytes == 0
  }

  fn get(&mut self, key: &ColorGlyphCacheKey) -> Option<Option<ColorGlyphRaster>> {
    if self.caching_disabled() {
      self.misses += 1;
      return None;
    }
    if let Some(value) = self.glyphs.get(key).cloned() {
      self.hits += 1;
      return Some(value);
    }
    self.misses += 1;
    None
  }

  fn insert(&mut self, key: ColorGlyphCacheKey, value: Option<ColorGlyphRaster>) {
    if self.caching_disabled() {
      return;
    }
    if let Some(existing) = self.glyphs.peek(&key) {
      self.current_bytes = self
        .current_bytes
        .saturating_sub(color_glyph_entry_size(existing));
    }

    let glyph_bytes = color_glyph_entry_size(&value);
    self.current_bytes = self.current_bytes.saturating_add(glyph_bytes);
    self.peak_bytes = self.peak_bytes.max(self.current_bytes);
    self.glyphs.put(key, value);
    self.evict_if_needed();
  }

  fn stats(&self) -> GlyphCacheStats {
    GlyphCacheStats {
      hits: self.hits,
      misses: self.misses,
      evictions: self.evictions,
      bytes: self.current_bytes.max(self.peak_bytes),
    }
  }

  fn reset_stats(&mut self) {
    self.hits = 0;
    self.misses = 0;
    self.evictions = 0;
    self.peak_bytes = self.current_bytes;
  }

  fn evict_if_needed(&mut self) {
    while self.glyphs.len() > self.max_size || self.current_bytes > self.max_bytes {
      if let Some((_key, value)) = self.glyphs.pop_lru() {
        self.current_bytes = self
          .current_bytes
          .saturating_sub(color_glyph_entry_size(&value));
        self.evictions += 1;
      } else {
        break;
      }
    }
  }

  fn clear(&mut self) {
    self.glyphs.clear();
    self.current_bytes = 0;
    self.peak_bytes = 0;
    self.hits = 0;
    self.misses = 0;
    self.evictions = 0;
  }

  fn set_max_size(&mut self, max_size: usize) {
    self.max_size = max_size;
    if self.caching_disabled() {
      self.glyphs.clear();
      self.current_bytes = 0;
      self.peak_bytes = 0;
      return;
    }
    self.evict_if_needed();
  }

  fn set_max_bytes(&mut self, max_bytes: usize) {
    self.max_bytes = max_bytes;
    if self.caching_disabled() {
      self.glyphs.clear();
      self.current_bytes = 0;
      self.peak_bytes = 0;
      return;
    }
    self.evict_if_needed();
  }
}

fn rgb_signature(color: Rgba) -> u32 {
  ((color.r as u32) << 16) | ((color.g as u32) << 8) | color.b as u32
}

fn quantize_scale(scale: f32) -> u32 {
  if !scale.is_finite() || scale <= 0.0 {
    return 0;
  }
  let scaled = (scale * SCALE_QUANTIZATION).round();
  scaled.clamp(1.0, u32::MAX as f32).trunc() as u32
}

fn quantize_skew(skew: f32) -> i32 {
  if !skew.is_finite() {
    return 0;
  }

  if skew.abs() < 1e-4 {
    return 0;
  }

  let scaled = (skew * 10_000.0).round();
  scaled.clamp(i32::MIN as f32, i32::MAX as f32).trunc() as i32
}

fn transform_scale_components(transform: Transform) -> (f32, f32) {
  let scale_x = (transform.sx * transform.sx + transform.ky * transform.ky).sqrt();
  let scale_y = (transform.kx * transform.kx + transform.sy * transform.sy).sqrt();
  (scale_x, scale_y)
}

fn quantize_transform(transform: Transform) -> u64 {
  let (sx, sy) = transform_scale_components(transform);
  let sx_key = quantize_scale(sx.max(0.0));
  let sy_key = quantize_scale(sy.max(0.0));
  ((sx_key as u64) << 32) | sy_key as u64
}

fn color_glyph_size(glyph: &ColorGlyphRaster) -> usize {
  glyph
    .image
    .data()
    .len()
    .saturating_add(std::mem::size_of::<ColorGlyphRaster>())
}

fn color_glyph_entry_size(entry: &Option<ColorGlyphRaster>) -> usize {
  entry.as_ref().map(color_glyph_size).unwrap_or(0)
}

fn estimate_glyph_size(metrics: &GlyphOutlineMetrics) -> usize {
  let point_bytes = metrics.point_count * std::mem::size_of::<tiny_skia::Point>();
  // Each verb typically expands to a few bytes; use a small constant
  // so the estimate scales with command count without relying on
  // private tiny-skia details.
  let verb_bytes = metrics.verb_count * std::mem::size_of::<u8>();
  point_bytes.saturating_add(verb_bytes)
}

pub(crate) fn rotation_transform(
  rotation: crate::text::pipeline::RunRotation,
  origin_x: f32,
  origin_y: f32,
) -> Option<Transform> {
  let angle = match rotation {
    crate::text::pipeline::RunRotation::Ccw90 => -90.0_f32.to_radians(),
    crate::text::pipeline::RunRotation::Cw90 => 90.0_f32.to_radians(),
    crate::text::pipeline::RunRotation::None => return None,
  };

  let (sin, cos) = angle.sin_cos();
  // Rotate around the provided origin, matching previous behavior that
  // rotated around the run start and baseline position.
  let tx = origin_x - origin_x * cos + origin_y * sin;
  let ty = origin_y - origin_x * sin - origin_y * cos;
  Some(Transform::from_row(cos, sin, -sin, cos, tx, ty))
}

pub(crate) fn concat_transforms(a: Transform, b: Transform) -> Transform {
  Transform::from_row(
    a.sx * b.sx + a.kx * b.ky,
    a.ky * b.sx + a.sy * b.ky,
    a.sx * b.kx + a.kx * b.sy,
    a.ky * b.kx + a.sy * b.sy,
    a.sx * b.tx + a.kx * b.ty + a.tx,
    a.ky * b.tx + a.sy * b.ty + a.ty,
  )
}

#[inline]
fn is_translation_only_transform(transform: Transform) -> bool {
  // Quantization/numerical noise from matrix math can produce values like `0.99999994`.
  const EPS: f32 = 1e-6;
  (transform.sx - 1.0).abs() <= EPS
    && (transform.sy - 1.0).abs() <= EPS
    && transform.kx.abs() <= EPS
    && transform.ky.abs() <= EPS
}

#[inline]
fn snap_device_x(coord: f32, translation: f32) -> f32 {
  // Chrome's fixture baseline harness disables font subpixel positioning, meaning glyph origins
  // land on whole device pixels. Our text shaping/layout can produce fractional coordinates even
  // in otherwise axis-aligned scenes, so snap the glyph translation in device space to improve
  // fixture-vs-Chrome diffs.
  if !coord.is_finite() || !translation.is_finite() {
    return coord;
  }
  let device = coord + translation;
  if !device.is_finite() {
    return coord;
  }
  device.round() - translation
}

fn color_glyph_transform(skew: f32, glyph_x: f32, glyph_y: f32, left: f32, top: f32) -> Transform {
  // Color glyph rasters are already in device pixels with a Y-down origin, so we
  // only need to add the synthetic oblique shear in X and translate to the glyph
  // origin without snapping to integers.
  Transform::from_row(1.0, 0.0, -skew, 1.0, glyph_x + left, glyph_y + top)
}

fn cache_transform_signature(state: Transform, rotation: Option<Transform>) -> u64 {
  let mut transform = rotation.unwrap_or_else(Transform::identity);
  transform = concat_transforms(state, transform);
  quantize_transform(transform)
}

fn shared_glyph_cache_limits_from_env() -> (usize, Option<usize>) {
  let toggles = runtime::runtime_toggles();
  let max_items = toggles.usize_with_default(ENV_GLYPH_CACHE_ITEMS, DEFAULT_GLYPH_CACHE_ITEMS);
  let max_bytes = toggles.usize_with_default(ENV_GLYPH_CACHE_BYTES, DEFAULT_GLYPH_CACHE_BYTES);
  if max_items == 0 || max_bytes == 0 {
    return (0, Some(0));
  }
  (max_items, Some(max_bytes))
}

fn shared_color_glyph_cache_limits_from_env() -> (usize, usize) {
  let toggles = runtime::runtime_toggles();
  let max_items =
    toggles.usize_with_default(ENV_COLOR_GLYPH_CACHE_ITEMS, DEFAULT_COLOR_GLYPH_CACHE_ITEMS);
  let max_bytes =
    toggles.usize_with_default(ENV_COLOR_GLYPH_CACHE_BYTES, DEFAULT_COLOR_GLYPH_CACHE_BYTES);
  if max_items == 0 || max_bytes == 0 {
    return (0, 0);
  }
  (max_items, max_bytes)
}

pub(crate) fn shared_glyph_cache() -> Arc<Mutex<GlyphCache>> {
  static CACHE: OnceLock<Arc<Mutex<GlyphCache>>> = OnceLock::new();
  CACHE
    .get_or_init(|| {
      let (max_items, max_bytes) = shared_glyph_cache_limits_from_env();
      Arc::new(Mutex::new(GlyphCache::with_limits(max_items, max_bytes)))
    })
    .clone()
}

pub(crate) fn shared_color_cache() -> Arc<Mutex<ColorGlyphCache>> {
  static CACHE: OnceLock<Arc<Mutex<ColorGlyphCache>>> = OnceLock::new();
  CACHE
    .get_or_init(|| {
      let (max_items, max_bytes) = shared_color_glyph_cache_limits_from_env();
      Arc::new(Mutex::new(ColorGlyphCache::with_limits(
        max_items, max_bytes,
      )))
    })
    .clone()
}

pub(crate) fn shared_color_renderer() -> ColorFontRenderer {
  static RENDERER: OnceLock<ColorFontRenderer> = OnceLock::new();
  RENDERER.get_or_init(ColorFontRenderer::new).clone()
}

// ============================================================================
// Text Rasterizer
// ============================================================================

/// Rendering state applied to glyph rasterization.
#[derive(Debug, Clone, Copy)]
pub struct TextRenderState<'a> {
  /// Transform to apply after glyph positioning.
  pub transform: Transform,
  /// Optional clip mask to apply while painting.
  pub clip_mask: Option<&'a Mask>,
  /// Additional opacity multiplier.
  pub opacity: f32,
  /// Blend mode for the glyph draw.
  pub blend_mode: SkiaBlendMode,
  /// Whether subpixel AA is allowed for this draw (e.g. disabled for clip masks).
  pub allow_subpixel_aa: bool,
  /// Font smoothing mode (`-webkit-font-smoothing`, etc.).
  pub font_smoothing: FontSmoothing,
}

/// Optional stroke to apply when rasterizing text (e.g. `-webkit-text-stroke`).
#[derive(Debug, Clone, Copy)]
pub struct TextStroke {
  pub width: f32,
  pub color: Rgba,
}

impl<'a> Default for TextRenderState<'a> {
  fn default() -> Self {
    Self {
      transform: Transform::identity(),
      clip_mask: None,
      opacity: 1.0,
      blend_mode: SkiaBlendMode::SourceOver,
      allow_subpixel_aa: true,
      font_smoothing: FontSmoothing::Auto,
    }
  }
}

/// Main text rasterizer for rendering shaped text to pixels.
///
/// Converts shaped text runs (glyph IDs + positions) into rendered
/// pixels on a tiny-skia pixmap.
///
/// # Example
///
/// ```rust,ignore
/// let mut rasterizer = TextRasterizer::new();
///
/// // Render a shaped run
/// rasterizer.render_shaped_run(
///     &shaped_run,
///     10.0,   // x position
///     100.0,  // baseline y position
///     Rgba::BLACK,
///     &mut pixmap,
/// )?;
/// ```
///
/// # Thread Safety
///
/// TextRasterizer is not thread-safe (uses internal mutable cache).
/// Create one instance per thread, or use external synchronization.
#[derive(Debug)]
pub struct TextRasterizer {
  /// Glyph path cache
  cache: Arc<Mutex<GlyphCache>>,
  /// Color glyph cache
  color_cache: Arc<Mutex<ColorGlyphCache>>,
  /// Renderer for color glyph formats
  color_renderer: ColorFontRenderer,
  hinting_enabled: bool,
  snap_glyph_positions: bool,
  subpixel_aa_enabled: bool,
  subpixel_aa_gamma: Option<SubpixelAAGammaLut>,
  subpixel_scratch: SubpixelAAScratch,
  subpixel_aa_diagnostics: Option<SubpixelAADiagnostics>,
}

impl Default for TextRasterizer {
  fn default() -> Self {
    Self::new()
  }
}

impl Drop for TextRasterizer {
  fn drop(&mut self) {
    let Some(stats) = self.subpixel_aa_diagnostics else {
      return;
    };
    if stats.attempts == 0
      && stats.skips_disabled == 0
      && stats.skips_blend_mode == 0
      && stats.skips_rotation == 0
      && stats.skips_state_transform == 0
    {
      return;
    }
    eprintln!(
      "text_subpixel_aa: skips(disabled={}, blend_mode={}, rotation={}, state_transform={}) attempts={} ok={} failures(non_axis_aligned={}, clip_mismatch={}, mask_overflow={}, other={})",
      stats.skips_disabled,
      stats.skips_blend_mode,
      stats.skips_rotation,
      stats.skips_state_transform,
      stats.attempts,
      stats.successes,
      stats.failures_non_axis_aligned,
      stats.failures_clip_mask_mismatch,
      stats.failures_mask_overflow,
      stats.failures_other
    );
  }
}

impl TextRasterizer {
  /// Creates a new text rasterizer.
  pub fn new() -> Self {
    Self::with_caches(
      Arc::new(Mutex::new(GlyphCache::new())),
      ColorFontRenderer::new(),
      Arc::new(Mutex::new(ColorGlyphCache::new())),
    )
  }

  /// Creates a rasterizer with custom cache capacity.
  pub fn with_cache_capacity(capacity: usize) -> Self {
    Self::with_caches(
      Arc::new(Mutex::new(GlyphCache::with_capacity(capacity))),
      ColorFontRenderer::new(),
      Arc::new(Mutex::new(ColorGlyphCache::new())),
    )
  }

  /// Creates a rasterizer that reuses the provided color glyph cache and renderer.
  ///
  /// Sharing the color cache lets callers avoid re-rasterizing color glyphs across canvases
  /// (e.g., display-list tiles) while still keeping outline caches local to each rasterizer.
  pub fn with_color_resources(
    color_renderer: ColorFontRenderer,
    color_cache: Arc<Mutex<ColorGlyphCache>>,
  ) -> Self {
    Self::with_caches(
      Arc::new(Mutex::new(GlyphCache::new())),
      color_renderer,
      color_cache,
    )
  }

  /// Builds a rasterizer backed by shared caches to keep repeated renders warm.
  pub fn with_shared_caches() -> Self {
    Self::with_caches(
      shared_glyph_cache(),
      shared_color_renderer(),
      shared_color_cache(),
    )
  }

  pub(crate) fn with_caches(
    cache: Arc<Mutex<GlyphCache>>,
    color_renderer: ColorFontRenderer,
    color_cache: Arc<Mutex<ColorGlyphCache>>,
  ) -> Self {
    let toggles = runtime::runtime_toggles();
    let hinting_enabled = toggles.truthy("FASTR_TEXT_HINTING");
    let subpixel_aa_enabled = toggles.truthy("FASTR_TEXT_SUBPIXEL_AA");
    let default_snap_glyph_positions = toggles.truthy("FASTR_DETERMINISTIC_PAINT")
      && hinting_enabled
      && !subpixel_aa_enabled;
    let snap_glyph_positions =
      toggles.truthy_with_default(ENV_TEXT_SNAP_GLYPH_POSITIONS, default_snap_glyph_positions);
    let subpixel_aa_diagnostics = if toggles.truthy(ENV_TEXT_SUBPIXEL_AA_DIAGNOSTICS) {
      Some(SubpixelAADiagnostics::default())
    } else {
      None
    };
    let subpixel_aa_gamma = if subpixel_aa_enabled {
      toggles.f64(ENV_TEXT_SUBPIXEL_AA_GAMMA).and_then(|gamma| {
        let gamma = gamma as f32;
        if gamma.is_finite() && gamma > 0.0 && (gamma - 1.0).abs() > 1e-6 {
          Some(SubpixelAAGammaLut::new(gamma))
        } else {
          None
        }
      })
    } else {
      None
    };
    Self {
      cache,
      color_cache,
      color_renderer,
      hinting_enabled,
      snap_glyph_positions,
      subpixel_aa_enabled,
      subpixel_aa_gamma,
      subpixel_scratch: SubpixelAAScratch::default(),
      subpixel_aa_diagnostics,
    }
  }

  fn record_subpixel_aa_failure(&mut self, err: &Error) {
    let Some(stats) = self.subpixel_aa_diagnostics.as_mut() else {
      return;
    };

    match err {
      Error::Render(RenderError::RasterizationFailed { reason }) => {
        if reason.contains("non-axis-aligned") {
          stats.failures_non_axis_aligned = stats.failures_non_axis_aligned.saturating_add(1);
        } else if reason.contains("clip mask size mismatch") {
          stats.failures_clip_mask_mismatch = stats.failures_clip_mask_mismatch.saturating_add(1);
        } else if reason.contains("mask width overflow") {
          stats.failures_mask_overflow = stats.failures_mask_overflow.saturating_add(1);
        } else {
          stats.failures_other = stats.failures_other.saturating_add(1);
        }
      }
      _ => {
        stats.failures_other = stats.failures_other.saturating_add(1);
      }
    }
  }

  fn map_point(transform: Transform, x: f32, y: f32) -> (f32, f32) {
    (
      transform.sx * x + transform.kx * y + transform.tx,
      transform.ky * x + transform.sy * y + transform.ty,
    )
  }

  fn transformed_bounds(path: &Path, transform: Transform) -> Option<(f32, f32, f32, f32)> {
    let rect = path.bounds();
    let corners = [
      (rect.left(), rect.top()),
      (rect.right(), rect.top()),
      (rect.left(), rect.bottom()),
      (rect.right(), rect.bottom()),
    ];
    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    for (x, y) in corners {
      let (tx, ty) = Self::map_point(transform, x, y);
      min_x = min_x.min(tx);
      min_y = min_y.min(ty);
      max_x = max_x.max(tx);
      max_y = max_y.max(ty);
    }
    if min_x.is_finite() && min_y.is_finite() && max_x.is_finite() && max_y.is_finite() {
      Some((min_x, min_y, max_x, max_y))
    } else {
      None
    }
  }

  fn fill_path_subpixel_aa(
    &mut self,
    dst: &mut PixmapMut<'_>,
    path: &Path,
    transform: Transform,
    fill_alpha: f32,
    color: Rgba,
    clip_mask: Option<&Mask>,
  ) -> Result<()> {
    // Subpixel AA assumes the LCD subpixel grid is aligned with the target surface X axis.
    // Avoid applying it when the glyph is rotated/skewed, which would map "horizontal" subpixels
    // onto a non-horizontal edge.
    if transform.kx.abs() > 1e-6 || transform.ky.abs() > 1e-6 {
      return Err(
        RenderError::RasterizationFailed {
          reason: "subpixel AA unsupported for non-axis-aligned glyph transform".to_string(),
        }
        .into(),
      );
    }

    let Some((min_x, min_y, max_x, max_y)) = Self::transformed_bounds(path, transform) else {
      return Ok(());
    };
    if max_x <= min_x || max_y <= min_y {
      return Ok(());
    }

    let origin_x = (min_x.floor() as i32).saturating_sub(SUBPIXEL_AA_PAD_PX);
    let origin_y = (min_y.floor() as i32).saturating_sub(SUBPIXEL_AA_PAD_PX);
    let end_x = (max_x.ceil() as i32).saturating_add(SUBPIXEL_AA_PAD_PX);
    let end_y = (max_y.ceil() as i32).saturating_add(SUBPIXEL_AA_PAD_PX);
    let width_px_i32 = end_x.saturating_sub(origin_x);
    let height_px_i32 = end_y.saturating_sub(origin_y);
    if width_px_i32 <= 0 || height_px_i32 <= 0 {
      return Ok(());
    }
    let width_px = width_px_i32 as u32;
    let height_px = height_px_i32 as u32;
    let width_sub = width_px.checked_mul(SUBPIXEL_AA_SCALE_X).ok_or_else(|| {
      RenderError::RasterizationFailed {
        reason: "subpixel AA mask width overflow".to_string(),
      }
    })?;

    let mask_pixmap = self.subpixel_scratch.get_or_resize(width_sub, height_px)?;
    mask_pixmap.fill(Color::TRANSPARENT);

    let mut mask_paint = Paint::default();
    mask_paint.set_color_rgba8(255, 255, 255, 255);
    mask_paint.anti_alias = true;
    mask_paint.blend_mode = SkiaBlendMode::SourceOver;

    let subpixel_scale = Transform::from_scale(SUBPIXEL_AA_SCALE_X as f32, 1.0);
    let transform_sub = concat_transforms(subpixel_scale, transform);
    let local_translate = Transform::from_translate(
      -(origin_x as f32) * SUBPIXEL_AA_SCALE_X as f32,
      -(origin_y as f32),
    );
    let mask_transform = concat_transforms(local_translate, transform_sub);

    mask_pixmap.fill_path(path, &mask_paint, FillRule::Winding, mask_transform, None);

    let dst_w = dst.width() as i32;
    let dst_h = dst.height() as i32;
    let dst_w_usize = dst.width() as usize;
    let mask_data = mask_pixmap.data();
    let clip_data = match clip_mask {
      Some(mask) if mask.width() == dst.width() && mask.height() == dst.height() => {
        Some(mask.data())
      }
      Some(_) => {
        return Err(
          RenderError::RasterizationFailed {
            reason: "subpixel AA clip mask size mismatch".to_string(),
          }
          .into(),
        );
      }
      None => None,
    };
    let dst_data = dst.data_mut();
    let width_sub_i32 = width_sub as i32;
    let width_sub_usize = width_sub as usize;

    let gamma_lut = lcd_gamma_lut();
    let color_r = color.r as f32;
    let color_g = color.g as f32;
    let color_b = color.b as f32;
    let fill_alpha = fill_alpha.clamp(0.0, 1.0);

    for local_y in 0..height_px_i32 {
      let global_y = origin_y + local_y;
      if global_y < 0 || global_y >= dst_h {
        continue;
      }
      let global_y_usize = global_y as usize;
      let row_start = local_y as usize * width_sub_usize * 4;
      let row = &mask_data[row_start..row_start + width_sub_usize * 4];

      for local_x in 0..width_px_i32 {
        let global_x = origin_x + local_x;
        if global_x < 0 || global_x >= dst_w {
          continue;
        }
        let global_x_usize = global_x as usize;

        let clip_alpha = clip_data
          .as_ref()
          .map(|clip| clip[global_y_usize * dst_w_usize + global_x_usize])
          .unwrap_or(255);
        if clip_alpha == 0 {
          continue;
        }

        // `tiny-skia` treats pixel centers as integer coordinates. When we upscale X by 3 to model
        // LCD subpixels, the three subpixel centers for a device pixel at integer `x` are located
        // at `3*x-1`, `3*x`, and `3*x+1` in subpixel coordinates (rather than `3*x`, `3*x+1`,
        // `3*x+2`). Offset by one subpixel so the green coverage is centered on the device pixel.
        let base_sub = local_x * SUBPIXEL_AA_SCALE_X as i32 - 1;
        let mut cov_r = subpixel_aa_lcd_filtered_alpha(row, base_sub, width_sub_i32);
        let mut cov_g = subpixel_aa_lcd_filtered_alpha(row, base_sub + 1, width_sub_i32);
        let mut cov_b = subpixel_aa_lcd_filtered_alpha(row, base_sub + 2, width_sub_i32);

        cov_r = gamma_lut[cov_r as usize];
        cov_g = gamma_lut[cov_g as usize];
        cov_b = gamma_lut[cov_b as usize];

        if clip_alpha != 255 {
          // Multiply coverages by the clip alpha (device pixel resolution).
          cov_r = ((u16::from(cov_r) * u16::from(clip_alpha) + 127) / 255) as u8;
          cov_g = ((u16::from(cov_g) * u16::from(clip_alpha) + 127) / 255) as u8;
          cov_b = ((u16::from(cov_b) * u16::from(clip_alpha) + 127) / 255) as u8;
        }

        if let Some(gamma) = self.subpixel_aa_gamma.as_ref() {
          cov_r = gamma.table[cov_r as usize];
          cov_g = gamma.table[cov_g as usize];
          cov_b = gamma.table[cov_b as usize];
        }

        if cov_r == 0 && cov_g == 0 && cov_b == 0 {
          continue;
        }

        let dst_idx = (global_y_usize * dst_w_usize + global_x_usize) * 4;
        let dst_a = dst_data[dst_idx + 3];

        if dst_a != 255 {
          // Fallback: collapse to a single coverage value for non-opaque backdrops since our
          // surface format cannot represent per-channel alpha.
          let cov_avg = (u16::from(cov_r) + u16::from(cov_g) + u16::from(cov_b)) as f32 / 3.0;
          let src_a = (fill_alpha * cov_avg / 255.0).clamp(0.0, 1.0);
          if src_a <= 0.0 {
            continue;
          }

          let dst_a_f = dst_a as f32 / 255.0;
          let out_a_f = src_a + dst_a_f * (1.0 - src_a);
          let out_a_u8 = (out_a_f * 255.0).round().clamp(0.0, 255.0) as u8;
          if out_a_u8 == 0 {
            dst_data[dst_idx..dst_idx + 4].copy_from_slice(&[0, 0, 0, 0]);
            continue;
          }

          let src_r = color_r * src_a;
          let src_g = color_g * src_a;
          let src_b = color_b * src_a;
          let dst_r = dst_data[dst_idx] as f32;
          let dst_g = dst_data[dst_idx + 1] as f32;
          let dst_b = dst_data[dst_idx + 2] as f32;
          let inv = 1.0 - src_a;
          dst_data[dst_idx] = (src_r + dst_r * inv).round().clamp(0.0, 255.0) as u8;
          dst_data[dst_idx + 1] = (src_g + dst_g * inv).round().clamp(0.0, 255.0) as u8;
          dst_data[dst_idx + 2] = (src_b + dst_b * inv).round().clamp(0.0, 255.0) as u8;
          dst_data[dst_idx + 3] = out_a_u8;
          continue;
        }

        // Opaque backdrop: apply per-channel coverage. Keep alpha opaque.
        let dst_r = dst_data[dst_idx] as f32;
        let dst_g = dst_data[dst_idx + 1] as f32;
        let dst_b = dst_data[dst_idx + 2] as f32;

        let a_r = fill_alpha * (cov_r as f32 / 255.0);
        let a_g = fill_alpha * (cov_g as f32 / 255.0);
        let a_b = fill_alpha * (cov_b as f32 / 255.0);

        dst_data[dst_idx] = (color_r * a_r + dst_r * (1.0 - a_r))
          .round()
          .clamp(0.0, 255.0) as u8;
        dst_data[dst_idx + 1] = (color_g * a_g + dst_g * (1.0 - a_g))
          .round()
          .clamp(0.0, 255.0) as u8;
        dst_data[dst_idx + 2] = (color_b * a_b + dst_b * (1.0 - a_b))
          .round()
          .clamp(0.0, 255.0) as u8;
        dst_data[dst_idx + 3] = 255;
      }
    }

    Ok(())
  }

  /// Renders a shaped text run to a pixmap.
  ///
  /// # Arguments
  ///
  /// * `run` - The shaped run containing glyphs and font
  /// * `x` - X position for the start of the run
  /// * `baseline_y` - Y position of the text baseline
  /// * `color` - Text fill color
  /// * `pixmap` - Target pixmap to render to
  ///
  /// # Returns
  ///
  /// The total horizontal advance (width) of the rendered text.
  ///
  /// # Example
  ///
  /// ```rust,ignore
  /// let advance = rasterizer.render_shaped_run(
  ///     &run,
  ///     100.0,      // x
  ///     200.0,      // baseline y
  ///     Rgba::BLACK,
  ///     &mut pixmap,
  /// )?;
  /// println!("Rendered {} pixels wide", advance);
  /// ```
  pub fn render_shaped_run(
    &mut self,
    run: &ShapedRun,
    x: f32,
    baseline_y: f32,
    color: Rgba,
    pixmap: &mut Pixmap,
  ) -> Result<f32> {
    self.render_shaped_run_with_state(
      run,
      x,
      baseline_y,
      color,
      pixmap,
      TextRenderState::default(),
    )
  }

  /// Renders a shaped text run with explicit render state (transform/clip/blend).
  pub fn render_shaped_run_with_state(
    &mut self,
    run: &ShapedRun,
    x: f32,
    baseline_y: f32,
    color: Rgba,
    pixmap: &mut Pixmap,
    state: TextRenderState<'_>,
  ) -> Result<f32> {
    let mut pixmap = pixmap.as_mut();
    self.render_shaped_run_with_state_pixmap_mut(run, x, baseline_y, color, &mut pixmap, state)
  }

  pub(crate) fn render_shaped_run_with_state_pixmap_mut(
    &mut self,
    run: &ShapedRun,
    x: f32,
    baseline_y: f32,
    color: Rgba,
    pixmap: &mut PixmapMut<'_>,
    state: TextRenderState<'_>,
  ) -> Result<f32> {
    let rotation = rotation_transform(run.rotation, x, baseline_y);
    self.render_glyph_run_internal(
      &run.glyphs,
      &run.font,
      run.font_size * run.scale,
      run.synthetic_bold,
      run.synthetic_oblique,
      run.palette_index,
      &run.palette_overrides,
      run.palette_override_hash,
      &run.variations,
      rotation,
      x,
      baseline_y,
      color,
      None,
      state,
      pixmap,
    )?;

    // Preserve previous behavior of returning the shaped run's advance.
    Ok(run.advance)
  }

  pub fn render_glyph_run(
    &mut self,
    glyphs: &[GlyphPosition],
    font: &LoadedFont,
    font_size: f32,
    synthetic_bold: f32,
    synthetic_oblique: f32,
    palette_index: u16,
    palette_overrides: &[(u16, Rgba)],
    palette_override_hash: u64,
    variations: &[Variation],
    rotation: Option<Transform>,
    x: f32,
    baseline_y: f32,
    color: Rgba,
    state: TextRenderState<'_>,
    pixmap: &mut Pixmap,
  ) -> Result<f32> {
    let mut pixmap = pixmap.as_mut();
    self.render_glyph_run_internal(
      glyphs,
      font,
      font_size,
      synthetic_bold,
      synthetic_oblique,
      palette_index,
      palette_overrides,
      palette_override_hash,
      variations,
      rotation,
      x,
      baseline_y,
      color,
      None,
      state,
      &mut pixmap,
    )
  }

  #[allow(clippy::too_many_arguments)]
  pub fn render_glyph_run_with_stroke(
    &mut self,
    glyphs: &[GlyphPosition],
    font: &LoadedFont,
    font_size: f32,
    synthetic_bold: f32,
    synthetic_oblique: f32,
    palette_index: u16,
    palette_overrides: &[(u16, Rgba)],
    palette_override_hash: u64,
    variations: &[Variation],
    rotation: Option<Transform>,
    x: f32,
    baseline_y: f32,
    color: Rgba,
    stroke: Option<TextStroke>,
    state: TextRenderState<'_>,
    pixmap: &mut Pixmap,
  ) -> Result<f32> {
    let mut pixmap = pixmap.as_mut();
    self.render_glyph_run_internal(
      glyphs,
      font,
      font_size,
      synthetic_bold,
      synthetic_oblique,
      palette_index,
      palette_overrides,
      palette_override_hash,
      variations,
      rotation,
      x,
      baseline_y,
      color,
      stroke,
      state,
      &mut pixmap,
    )
  }

  #[allow(clippy::too_many_arguments)]
  pub(crate) fn render_glyph_run_with_stroke_pixmap_mut(
    &mut self,
    glyphs: &[GlyphPosition],
    font: &LoadedFont,
    font_size: f32,
    synthetic_bold: f32,
    synthetic_oblique: f32,
    palette_index: u16,
    palette_overrides: &[(u16, Rgba)],
    palette_override_hash: u64,
    variations: &[Variation],
    rotation: Option<Transform>,
    x: f32,
    baseline_y: f32,
    color: Rgba,
    stroke: Option<TextStroke>,
    state: TextRenderState<'_>,
    pixmap: &mut PixmapMut<'_>,
  ) -> Result<f32> {
    self.render_glyph_run_internal(
      glyphs,
      font,
      font_size,
      synthetic_bold,
      synthetic_oblique,
      palette_index,
      palette_overrides,
      palette_override_hash,
      variations,
      rotation,
      x,
      baseline_y,
      color,
      stroke,
      state,
      pixmap,
    )
  }

  #[allow(clippy::too_many_arguments)]
  fn render_glyph_run_internal(
    &mut self,
    glyphs: &[GlyphPosition],
    font: &LoadedFont,
    font_size: f32,
    synthetic_bold: f32,
    synthetic_oblique: f32,
    palette_index: u16,
    palette_overrides: &[(u16, Rgba)],
    palette_override_hash: u64,
    variations: &[Variation],
    rotation: Option<Transform>,
    x: f32,
    baseline_y: f32,
    color: Rgba,
    stroke: Option<TextStroke>,
    state: TextRenderState<'_>,
    pixmap: &mut PixmapMut<'_>,
  ) -> Result<f32> {
    let raster_timer = text_diagnostics_timer(TextDiagnosticsStage::Rasterize);
    let diag_enabled = raster_timer.is_some();
    let mut color_glyph_rasters = 0usize;
    let mut deadline_counter = 0usize;
    // Fast path: when a render deadline is already expired, avoid doing any work (and avoid
    // burning ~DEADLINE_STRIDE iterations before the first periodic check trips).
    check_active(RenderStage::Paint).map_err(Error::Render)?;

    let opacity = state.opacity.clamp(0.0, 1.0);
    let fill_alpha = (color.a * opacity).clamp(0.0, 1.0);
    let draw_fill = fill_alpha > 0.0;
    let stroke = stroke.filter(|s| s.width.is_finite() && s.width.abs() > 0.0 && s.color.a > 0.0);
    let stroke_alpha = stroke
      .map(|s| (s.color.a * opacity).clamp(0.0, 1.0))
      .unwrap_or(0.0);
    let draw_stroke = stroke_alpha > 0.0;

    let glyph_run_advance = || {
      let mut cursor_x = x;
      let mut cursor_y = 0.0_f32;
      for glyph in glyphs {
        cursor_x += glyph.x_advance;
        cursor_y += glyph.y_advance;
      }
      if cursor_y.abs() > (cursor_x - x).abs() {
        cursor_y
      } else {
        cursor_x - x
      }
    };

    // `font-size: 0` is valid CSS and should simply suppress glyph painting (and yield
    // zero-sized glyph transforms). Avoid treating it as a paint-time failure.
    if glyphs.is_empty()
      || (!draw_fill && !draw_stroke)
      || !font_size.is_finite()
      || font_size <= 0.0
    {
      return Ok(glyph_run_advance());
    }

    // Note: The shared glyph/color caches are used from multiple threads when paint parallelism is
    // enabled. Computing cache deltas from "before" and "after" snapshots taken outside the cache
    // lock can double-count (other threads can bump the counters between snapshots). To keep
    // diagnostics stable and correct, accumulate per-lock deltas instead.
    let mut outline_hits = 0u64;
    let mut outline_misses = 0u64;
    let mut outline_evictions = 0u64;
    let mut outline_bytes = 0usize;
    let mut color_hits = 0u64;
    let mut color_misses = 0u64;
    let mut color_evictions = 0u64;
    let mut color_bytes = 0usize;

    let instance =
      FontInstance::new(font, variations).ok_or_else(|| RenderError::RasterizationFailed {
        reason: format!("Failed to parse font: {}", font.family),
      })?;

    let units_per_em = instance.units_per_em();
    if units_per_em == 0.0 {
      return Err(
        RenderError::RasterizationFailed {
          reason: format!("Font {} has invalid units_per_em", font.family),
        }
        .into(),
      );
    }

    // Font outlines are stored in font units, then mapped into device-space by `glyph_transform`
    // (which applies `scale = font_size / units_per_em`). tiny-skia applies that transform to both
    // the path geometry *and* the stroke width, meaning stroke widths must be expressed in the
    // same coordinate space as the path (font units) to achieve a desired device-pixel width.
    //
    // Without this conversion, `-webkit-text-stroke` ends up scaled by `scale` and becomes far too
    // thin for typical font sizes (notably netflix.com's Top 10 ranking numbers).
    let scale = font_size / units_per_em;
    if !scale.is_finite() || scale == 0.0 {
      // `scale == 0` can happen with `font-size: 0` (handled above) or extreme underflow. Treat it
      // like a no-op paint instead of crashing the entire render.
      return Ok(glyph_run_advance());
    }
    let inv_scale = 1.0 / scale.abs();

    let antialias_enabled = !matches!(state.font_smoothing, FontSmoothing::None);
    let allow_subpixel_aa = state.allow_subpixel_aa
      && matches!(
        state.font_smoothing,
        FontSmoothing::Auto | FontSmoothing::Subpixel
      );

    // Create paint with fill color
    let mut paint = Paint::default();
    paint.set_color_rgba8(
      color.r,
      color.g,
      color.b,
      (fill_alpha * 255.0).round().clamp(0.0, 255.0) as u8,
    );
    paint.anti_alias = antialias_enabled;
    paint.blend_mode = state.blend_mode;

    // Create paint/stroke params for the optional stroke.
    let mut stroke_paint = Paint::default();
    let mut stroke_style = tiny_skia::Stroke::default();
    if let Some(stroke) = stroke {
      stroke_paint.set_color_rgba8(
        stroke.color.r,
        stroke.color.g,
        stroke.color.b,
        (stroke_alpha * 255.0).round().clamp(0.0, 255.0) as u8,
      );
      stroke_paint.anti_alias = antialias_enabled;
      stroke_paint.blend_mode = state.blend_mode;
      stroke_style.width = (stroke.width.abs() * inv_scale).max(0.0);
      stroke_style.line_join = tiny_skia::LineJoin::Round;
      stroke_style.line_cap = tiny_skia::LineCap::Round;
    }
    let mut cursor_x = x;
    let mut cursor_y = 0.0_f32;
    let transform_signature = cache_transform_signature(state.transform, rotation);
    let glyph_opacity = color.a.clamp(0.0, 1.0);
    let color_for_glyph = Rgba { a: 1.0, ..color };
    let translation_only_state_transform = is_translation_only_transform(state.transform);
    // Font hinting is only safe when the post-positioning transform is translation-only.
    // Rotations (including vertical writing mode rotations) and additional scale/skew would change
    // the effective ppem/grid-fitting target; browsers typically disable hinting in those cases.
    let hinting = self.hinting_enabled && translation_only_state_transform && rotation.is_none();
    let snap_glyph_positions = self.snap_glyph_positions
      && rotation.is_none()
      && synthetic_oblique.abs() <= 1e-6
      && translation_only_state_transform;

    // Render each glyph
    for glyph in glyphs {
      check_active_periodic(&mut deadline_counter, DEADLINE_STRIDE, RenderStage::Paint)
        .map_err(Error::Render)?;
      // Calculate glyph position
      let mut glyph_x = cursor_x + glyph.x_offset;
      if snap_glyph_positions {
        glyph_x = snap_device_x(glyph_x, state.transform.tx);
      }
      let glyph_y = baseline_y + cursor_y + glyph.y_offset;

      // Color glyph (if available) is painted after the outline stroke/fill so the fill lands on top.
      let color_glyph = if draw_fill {
        // Strip CSS alpha from the cached raster and apply it once at draw time so
        // palette layers (including currentColor paints) pick up text/ancestor opacity
        // without double-multiplying.
        let color_key = ColorGlyphCacheKey::new(
          font,
          glyph.glyph_id,
          font_size,
          palette_index,
          variations,
          color_for_glyph,
          palette_override_hash,
          synthetic_oblique,
          transform_signature,
        );

        let cached_color_glyph = {
          let mut cache = self
            .color_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
          if diag_enabled {
            let before = cache.stats();
            let value = cache.get(&color_key);
            let after = cache.stats();
            let delta = after.delta_from(&before);
            color_hits = color_hits.saturating_add(delta.hits);
            color_misses = color_misses.saturating_add(delta.misses);
            color_evictions = color_evictions.saturating_add(delta.evictions);
            color_bytes = color_bytes.max(after.bytes);
            value
          } else {
            cache.get(&color_key)
          }
        };

        Some(match cached_color_glyph {
          Some(value) => value,
          None => {
            let rendered = self.color_renderer.render(
              font,
              &instance,
              glyph.glyph_id,
              font_size,
              palette_index,
              palette_overrides,
              palette_override_hash,
              color_for_glyph,
              0.0,
              variations,
              Some((pixmap.width(), pixmap.height())),
            );
            {
              let mut cache = self
                .color_cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
              if diag_enabled {
                let before = cache.stats();
                cache.insert(color_key, rendered.clone());
                let after = cache.stats();
                let delta = after.delta_from(&before);
                color_hits = color_hits.saturating_add(delta.hits);
                color_misses = color_misses.saturating_add(delta.misses);
                color_evictions = color_evictions.saturating_add(delta.evictions);
                color_bytes = color_bytes.max(after.bytes);
              } else {
                cache.insert(color_key, rendered.clone());
              }
            }
            rendered
          }
        })
      } else {
        None
      }
      .and_then(|glyph| glyph);

      let needs_outline = draw_stroke || (draw_fill && color_glyph.is_none());
      if needs_outline {
        let cached_path = {
          let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
          if diag_enabled {
            let before = cache.stats();
            let path = cache
              .get_or_build(
                font,
                &instance,
                glyph.glyph_id,
                font_size,
                hinting,
              )
              .and_then(|glyph| glyph.path.clone());
            let after = cache.stats();
            let delta = after.delta_from(&before);
            outline_hits = outline_hits.saturating_add(delta.hits);
            outline_misses = outline_misses.saturating_add(delta.misses);
            outline_evictions = outline_evictions.saturating_add(delta.evictions);
            outline_bytes = outline_bytes.max(after.bytes);
            path
          } else {
            cache
              .get_or_build(
                font,
                &instance,
                glyph.glyph_id,
                font_size,
                hinting,
              )
              .and_then(|glyph| glyph.path.clone())
          }
        };
        if let Some(path) = cached_path {
          let mut transform = glyph_transform(scale, synthetic_oblique, glyph_x, glyph_y);
          if let Some(rotation) = rotation {
            transform = concat_transforms(rotation, transform);
          }
          transform = concat_transforms(state.transform, transform);

          if draw_stroke {
            pixmap.stroke_path(
              path.as_ref(),
              &stroke_paint,
              &stroke_style,
              transform,
              state.clip_mask,
            );
          }

          if draw_fill && color_glyph.is_none() {
            // Chrome typically renders text with LCD/subpixel anti-aliasing, producing tinted edge
            // pixels. Our default tiny-skia path rasterization is grayscale AA, so allow enabling a
            // lightweight subpixel mode for fixture-chrome diffs.
            //
            // This is intentionally guarded by a runtime toggle so golden tests remain stable.
            let mut used_subpixel = false;
            if antialias_enabled
              && allow_subpixel_aa
              && self.subpixel_aa_enabled
              && state.blend_mode == SkiaBlendMode::SourceOver
              && rotation.is_none()
              && translation_only_state_transform
            {
              if let Some(stats) = self.subpixel_aa_diagnostics.as_mut() {
                stats.attempts = stats.attempts.saturating_add(1);
              }
              match self.fill_path_subpixel_aa(
                pixmap,
                path.as_ref(),
                transform,
                fill_alpha,
                color,
                state.clip_mask,
              ) {
                Ok(()) => {
                  used_subpixel = true;
                  if let Some(stats) = self.subpixel_aa_diagnostics.as_mut() {
                    stats.successes = stats.successes.saturating_add(1);
                  }
                }
                Err(err) => self.record_subpixel_aa_failure(&err),
              }
            } else if let Some(stats) = self.subpixel_aa_diagnostics.as_mut() {
              if !self.subpixel_aa_enabled {
                stats.skips_disabled = stats.skips_disabled.saturating_add(1);
              } else if state.blend_mode != SkiaBlendMode::SourceOver {
                stats.skips_blend_mode = stats.skips_blend_mode.saturating_add(1);
              } else if rotation.is_some() {
                stats.skips_rotation = stats.skips_rotation.saturating_add(1);
              } else if !translation_only_state_transform {
                stats.skips_state_transform = stats.skips_state_transform.saturating_add(1);
              }
            }

            if !used_subpixel {
              // Render the path fill (grayscale AA).
              pixmap.fill_path(
                path.as_ref(),
                &paint,
                FillRule::Winding,
                transform,
                state.clip_mask,
              );
            }
            if synthetic_bold > 0.0 {
              let mut stroke = tiny_skia::Stroke::default();
              stroke.width = (synthetic_bold * 2.0 * inv_scale).max(0.0);
              stroke.line_join = tiny_skia::LineJoin::Round;
              stroke.line_cap = tiny_skia::LineCap::Round;
              pixmap.stroke_path(path.as_ref(), &paint, &stroke, transform, state.clip_mask);
            }
          }
        }
      }

      if let Some(color_image) = color_glyph {
        let combined_opacity = (glyph_opacity * opacity).clamp(0.0, 1.0);
        if combined_opacity > 0.0 {
          if diag_enabled {
            color_glyph_rasters += 1;
          }
          let mut transform = color_glyph_transform(
            synthetic_oblique,
            glyph_x,
            glyph_y,
            color_image.left,
            color_image.top,
          );
          if let Some(rotation) = rotation {
            transform = concat_transforms(rotation, transform);
          }
          transform = concat_transforms(state.transform, transform);
          draw_color_glyph(
            pixmap,
            &color_image,
            transform,
            combined_opacity,
            state.blend_mode,
            state.clip_mask,
          );
        }
      }

      // Advance cursor (x_offset is already applied, x_advance is the main movement)
      cursor_x += glyph.x_advance;
      cursor_y += glyph.y_advance;
    }

    // Even if the glyph count is below the periodic stride (or the deadline expires during the
    // last chunk of work), ensure we still surface the paint timeout.
    check_active(RenderStage::Paint).map_err(Error::Render)?;

    if diag_enabled {
      // Preserve the previous behavior of reporting cache sizes even when this run happens to be
      // entirely color glyphs (or otherwise doesn't touch one of the caches).
      {
        let cache = self
          .cache
          .lock()
          .unwrap_or_else(|poisoned| poisoned.into_inner());
        outline_bytes = outline_bytes.max(cache.stats().bytes);
      }
      {
        let cache = self
          .color_cache
          .lock()
          .unwrap_or_else(|poisoned| poisoned.into_inner());
        color_bytes = color_bytes.max(cache.stats().bytes);
      }
      record_text_rasterize(
        raster_timer,
        color_glyph_rasters,
        TextCacheStats {
          hits: outline_hits,
          misses: outline_misses,
          evictions: outline_evictions,
          bytes: outline_bytes,
        },
        TextCacheStats {
          hits: color_hits,
          misses: color_misses,
          evictions: color_evictions,
          bytes: color_bytes,
        },
      );
    } else {
      record_text_rasterize(
        raster_timer,
        color_glyph_rasters,
        TextCacheStats::default(),
        TextCacheStats::default(),
      );
    }

    let advance = if cursor_y.abs() > (cursor_x - x).abs() {
      cursor_y
    } else {
      cursor_x - x
    };

    Ok(advance)
  }

  /// Renders multiple shaped runs.
  ///
  /// Convenience method for rendering a line of text with multiple runs.
  ///
  /// # Arguments
  ///
  /// * `runs` - Slice of shaped runs to render
  /// * `x` - X position for the start
  /// * `baseline_y` - Y position of the text baseline
  /// * `color` - Text fill color
  /// * `pixmap` - Target pixmap
  ///
  /// # Returns
  ///
  /// Total horizontal advance of all runs.
  pub fn render_runs(
    &mut self,
    runs: &[ShapedRun],
    x: f32,
    baseline_y: f32,
    color: Rgba,
    pixmap: &mut Pixmap,
  ) -> Result<f32> {
    let mut cursor_x = x;
    let mut cursor_y = 0.0_f32;

    for run in runs {
      let advance = self.render_shaped_run_with_state(
        run,
        cursor_x,
        baseline_y + cursor_y,
        color,
        pixmap,
        TextRenderState::default(),
      )?;
      if run.vertical {
        cursor_y += advance;
      } else {
        cursor_x += advance;
      }
    }

    Ok((cursor_x - x) + cursor_y)
  }

  /// Renders text with a specific font (low-level API).
  ///
  /// This is a lower-level method that renders individual glyph positions.
  /// Most users should use `render_shaped_run` instead.
  ///
  /// # Arguments
  ///
  /// * `glyphs` - Slice of glyph positions to render
  /// * `font` - Font to use for glyph outlines
  /// * `font_size` - Font size in pixels
  /// * `x` - X position
  /// * `baseline_y` - Baseline Y position
  /// * `color` - Fill color
  /// * `pixmap` - Target pixmap
  pub fn render_glyphs_with_state(
    &mut self,
    glyphs: &[GlyphPosition],
    font: &LoadedFont,
    font_size: f32,
    x: f32,
    baseline_y: f32,
    color: Rgba,
    synthetic_bold: f32,
    synthetic_oblique: f32,
    palette_index: u16,
    variations: &[Variation],
    rotation: Option<Transform>,
    state: TextRenderState<'_>,
    pixmap: &mut Pixmap,
  ) -> Result<f32> {
    self.render_glyph_run(
      glyphs,
      font,
      font_size,
      synthetic_bold,
      synthetic_oblique,
      palette_index,
      &[],
      0,
      variations,
      rotation,
      x,
      baseline_y,
      color,
      state,
      pixmap,
    )
  }

  pub fn render_glyphs(
    &mut self,
    glyphs: &[GlyphPosition],
    font: &LoadedFont,
    font_size: f32,
    x: f32,
    baseline_y: f32,
    color: Rgba,
    pixmap: &mut Pixmap,
  ) -> Result<f32> {
    self.render_glyphs_with_state(
      glyphs,
      font,
      font_size,
      x,
      baseline_y,
      color,
      0.0,
      0.0,
      0,
      &[],
      None,
      TextRenderState::default(),
      pixmap,
    )
  }

  /// Backwards-compatible wrapper that accepts explicit palette/variation parameters.
  pub fn render_glyphs_with_state_and_palette(
    &mut self,
    glyphs: &[GlyphPosition],
    font: &LoadedFont,
    font_size: f32,
    x: f32,
    baseline_y: f32,
    color: Rgba,
    synthetic_bold: f32,
    synthetic_oblique: f32,
    palette_index: u16,
    variations: &[Variation],
    rotation: Option<Transform>,
    state: TextRenderState<'_>,
    pixmap: &mut Pixmap,
  ) -> Result<f32> {
    self.render_glyphs_with_state(
      glyphs,
      font,
      font_size,
      x,
      baseline_y,
      color,
      synthetic_bold,
      synthetic_oblique,
      palette_index,
      variations,
      rotation,
      state,
      pixmap,
    )
  }

  /// Returns positioned glyph paths for the provided run using the outline cache.
  pub fn positioned_glyph_paths(
    &mut self,
    glyphs: &[GlyphPosition],
    font: &LoadedFont,
    font_size: f32,
    x: f32,
    baseline_y: f32,
    synthetic_oblique: f32,
    rotation: Option<Transform>,
    variations: &[Variation],
  ) -> Result<Vec<Path>> {
    check_active(RenderStage::Paint).map_err(Error::Render)?;
    let instance =
      FontInstance::new(font, variations).ok_or_else(|| RenderError::RasterizationFailed {
        reason: format!("Failed to parse font: {}", font.family),
      })?;

    let units_per_em = instance.units_per_em();
    if units_per_em == 0.0 {
      return Err(
        RenderError::RasterizationFailed {
          reason: format!("Font {} has invalid units_per_em", font.family),
        }
        .into(),
      );
    }

    let scale = font_size / units_per_em;
    let hinting = self.hinting_enabled && rotation.is_none();
    let mut paths = Vec::with_capacity(glyphs.len());
    let mut cursor_x = x;
    let mut cursor_y = 0.0_f32;
    let mut deadline_counter = 0usize;

    for glyph in glyphs {
      check_active_periodic(&mut deadline_counter, DEADLINE_STRIDE, RenderStage::Paint)
        .map_err(Error::Render)?;
      let glyph_x = cursor_x + glyph.x_offset;
      let glyph_y = baseline_y + cursor_y + glyph.y_offset;
      let cached_path = self
        .cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get_or_build(
          font,
          &instance,
          glyph.glyph_id,
          font_size,
          hinting,
        )
        .and_then(|glyph| glyph.path.clone());
      if let Some(path) = cached_path {
        let mut transform = glyph_transform(scale, synthetic_oblique, glyph_x, glyph_y);
        if let Some(rotation) = rotation {
          transform = concat_transforms(rotation, transform);
        }
        if let Some(transformed) = path.as_ref().clone().transform(transform) {
          paths.push(transformed);
        }
      }

      cursor_x += glyph.x_advance;
      cursor_y += glyph.y_advance;
    }

    check_active(RenderStage::Paint).map_err(Error::Render)?;

    Ok(paths)
  }

  /// Backwards-compatible wrapper that accepts rustybuzz variations explicitly.
  pub fn positioned_glyph_paths_with_variations(
    &mut self,
    glyphs: &[GlyphPosition],
    font: &LoadedFont,
    font_size: f32,
    x: f32,
    baseline_y: f32,
    synthetic_oblique: f32,
    rotation: Option<Transform>,
    variations: &[Variation],
  ) -> Result<Vec<Path>> {
    self.positioned_glyph_paths(
      glyphs,
      font,
      font_size,
      x,
      baseline_y,
      synthetic_oblique,
      rotation,
      variations,
    )
  }

  /// Clears the glyph cache.
  ///
  /// Call this when fonts are unloaded or memory pressure is high.
  pub fn clear_cache(&mut self) {
    self
      .cache
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .clear();
    self
      .color_cache
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .clear();
  }

  /// Sets the maximum number of cached glyph outlines.
  pub fn set_cache_capacity(&mut self, max_glyphs: usize) {
    self
      .cache
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .set_max_size(max_glyphs);
  }

  /// Sets an optional memory budget (in bytes) for cached outlines.
  pub fn set_cache_memory_budget(&mut self, max_bytes: Option<usize>) {
    self
      .cache
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .set_max_bytes(max_bytes);
    if let Some(bytes) = max_bytes {
      self
        .color_cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .set_max_bytes(bytes);
    }
  }

  /// Returns the number of cached glyph paths.
  #[inline]
  pub fn cache_size(&self) -> usize {
    let outline_len = self
      .cache
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .len();
    let color_len = self
      .color_cache
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .glyphs
      .len();
    outline_len + color_len
  }

  /// Returns glyph cache statistics (hits/misses/evictions).
  #[inline]
  pub fn cache_stats(&self) -> GlyphCacheStats {
    let outlines = self
      .cache
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .stats();
    let color = self
      .color_cache
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .stats();
    GlyphCacheStats {
      hits: outlines.hits + color.hits,
      misses: outlines.misses + color.misses,
      evictions: outlines.evictions + color.evictions,
      bytes: outlines.bytes.saturating_add(color.bytes),
    }
  }

  /// Resets cache statistics without dropping cached outlines.
  pub fn reset_cache_stats(&mut self) {
    self
      .cache
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .reset_stats();
    self
      .color_cache
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .reset_stats();
  }

  /// Returns a cached or freshly rendered color glyph raster for the given glyph.
  ///
  /// This reuses the shared color glyph cache so callers (e.g. shadow rendering) can
  /// obtain the glyph alpha mask without triggering redundant rasterization work.
  pub fn get_color_glyph(
    &mut self,
    font: &LoadedFont,
    glyph_id: u32,
    font_size: f32,
    palette_index: u16,
    variations: &[Variation],
    color: Rgba,
    synthetic_oblique: f32,
  ) -> Option<ColorGlyphRaster> {
    let instance = FontInstance::new(font, variations)?;
    let palette_overrides: &[(u16, Rgba)] = &[];
    let palette_override_hash = 0;
    let color_key = ColorGlyphCacheKey::new(
      font,
      glyph_id,
      font_size,
      palette_index,
      variations,
      color,
      palette_override_hash,
      synthetic_oblique,
      cache_transform_signature(Transform::identity(), None),
    );
    let cached = self
      .color_cache
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .get(&color_key);
    match cached {
      Some(value) => value,
      None => {
        let rendered = self.color_renderer.render(
          font,
          &instance,
          glyph_id,
          font_size,
          palette_index,
          palette_overrides,
          palette_override_hash,
          color,
          0.0,
          variations,
          None,
        );
        self
          .color_cache
          .lock()
          .unwrap_or_else(|poisoned| poisoned.into_inner())
          .insert(color_key, rendered.clone());
        rendered
      }
    }
  }
}

fn draw_color_glyph(
  target: &mut PixmapMut<'_>,
  glyph: &ColorGlyphRaster,
  base_transform: Transform,
  opacity: f32,
  blend_mode: SkiaBlendMode,
  clip_mask: Option<&Mask>,
) {
  let mut paint = PixmapPaint::default();
  paint.opacity = opacity.clamp(0.0, 1.0);
  paint.blend_mode = blend_mode;
  let pixmap_ref = glyph.image.as_ref().as_ref();
  target.draw_pixmap(0, 0, pixmap_ref, &paint, base_transform, clip_mask);
}

// ============================================================================
// Utility Functions
// ============================================================================

/// Converts an Rgba to a tiny-skia color.
#[inline]
pub fn to_skia_color(color: Rgba) -> tiny_skia::Color {
  tiny_skia::Color::from_rgba8(color.r, color.g, color.b, color.alpha_u8())
}

/// Renders a single glyph to a pixmap (standalone function).
///
/// This is a convenience function for simple use cases.
/// For rendering multiple glyphs, use `TextRasterizer` for caching benefits.
///
/// # Arguments
///
/// * `font` - The font containing the glyph
/// * `glyph_id` - ID of the glyph to render
/// * `font_size` - Font size in pixels
/// * `x` - X position
/// * `y` - Baseline Y position
/// * `color` - Fill color
/// * `pixmap` - Target pixmap
///
/// # Returns
///
/// The glyph's horizontal advance, or an error if rendering fails.
pub fn render_glyph(
  font: &LoadedFont,
  glyph_id: u32,
  font_size: f32,
  x: f32,
  y: f32,
  color: Rgba,
  pixmap: &mut Pixmap,
) -> Result<f32> {
  let instance = FontInstance::new(font, &[]).ok_or_else(|| RenderError::RasterizationFailed {
    reason: format!("Failed to parse font: {}", font.family),
  })?;

  let units_per_em = instance.units_per_em();
  let scale = font_size / units_per_em;

  let Some(outline) = instance.glyph_outline(glyph_id) else {
    return Ok(0.0);
  };

  // Render path if present.
  if let Some(path) = outline.path {
    let mut paint = Paint::default();
    paint.set_color_rgba8(color.r, color.g, color.b, color.alpha_u8());
    paint.anti_alias = true;

    let transform = glyph_transform(scale, 0.0, x, y);

    pixmap.fill_path(&path, &paint, FillRule::Winding, transform, None);
  }

  Ok(outline.advance * scale)
}

/// Gets the horizontal advance for a glyph.
///
/// Returns the distance to move the cursor after this glyph.
pub fn glyph_advance(font: &LoadedFont, glyph_id: u32, font_size: f32) -> Result<f32> {
  let instance = FontInstance::new(font, &[]).ok_or_else(|| RenderError::RasterizationFailed {
    reason: format!("Failed to parse font: {}", font.family),
  })?;
  let units_per_em = instance.units_per_em();
  let scale = font_size / units_per_em;
  Ok(
    instance
      .glyph_outline(glyph_id)
      .map(|o| o.advance * scale)
      .unwrap_or(0.0),
  )
}

/// Gets the horizontal advance for a glyph with variation coordinates applied.
// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod color_glyph_opacity_tests {
  use crate::image_compare::{compare_png, CompareConfig};
  use crate::style::color::Rgba;
  use crate::text::font_db::{FontStretch, FontStyle, FontWeight, LoadedFont};
  use crate::text::pipeline::GlyphPosition;
  use std::path::Path;
  use std::sync::Arc;
  use tiny_skia::{Color, Pixmap};

  fn load_color_font(path: &str, family: &str) -> LoadedFont {
    let full_path = Path::new(env!("CARGO_MANIFEST_DIR")).join(path);
    let data = std::fs::read(&full_path).expect("font bytes");
    LoadedFont {
      id: None,
      data: Arc::new(data),
      index: 0,
      family: family.to_string(),
      weight: FontWeight::NORMAL,
      style: FontStyle::Normal,
      stretch: FontStretch::Normal,
      face_metrics_overrides: Default::default(),
      face_settings: Default::default(),
    }
  }

  fn render_color_glyph(font: &LoadedFont, color: Rgba) -> Pixmap {
    let face = font.as_ttf_face().expect("parse font");
    let glyph_id = face.glyph_index('A').expect("glyph A").0 as u32;
    let glyphs = [GlyphPosition {
      glyph_id,
      cluster: 0,
      x_offset: 0.0,
      y_offset: 0.0,
      x_advance: 0.0,
      y_advance: 0.0,
    }];

    let mut pixmap = Pixmap::new(96, 96).expect("pixmap");
    pixmap.fill(Color::from_rgba8(0, 0, 0, 0));

    let mut rasterizer = super::TextRasterizer::new();
    rasterizer
      .render_glyphs(&glyphs, font, 64.0, 12.0, 72.0, color, &mut pixmap)
      .expect("render glyph");
    pixmap
  }

  fn assert_matches_golden(name: &str, png: &[u8]) {
    let golden_path = Path::new(env!("CARGO_MANIFEST_DIR"))
      .join("tests/fixtures/golden")
      .join(name);
    if std::env::var("UPDATE_GOLDENS").is_ok() {
      std::fs::write(&golden_path, png).expect("write golden");
      return;
    }
    let expected = std::fs::read(&golden_path).expect("read golden");
    let diff = compare_png(png, &expected, &CompareConfig::strict()).expect("compare");
    assert!(diff.is_match(), "{}", diff.summary());
  }

  #[test]
  fn palette_glyph_tracks_text_alpha() {
    let font = load_color_font("tests/fonts/ColorTestCOLR.ttf", "ColorTestCOLR");

    let full = render_color_glyph(&font, Rgba::BLACK);
    let quarter = render_color_glyph(
      &font,
      Rgba {
        a: 0.25,
        ..Rgba::BLACK
      },
    );

    assert_matches_golden("color_glyph_palette_full.png", &full.encode_png().unwrap());
    assert_matches_golden(
      "color_glyph_palette_quarter.png",
      &quarter.encode_png().unwrap(),
    );
  }

  #[test]
  fn current_color_alpha_applied_once() {
    let font = load_color_font(
      "tests/fonts/ColorTestCOLRCurrentColor.ttf",
      "ColorTestCOLRCurrentColor",
    );
    let color = Rgba::from_rgba8(0, 255, 0, 128);
    let pixmap = render_color_glyph(&font, color);
    assert_matches_golden(
      "color_glyph_current_color_half.png",
      &pixmap.encode_png().unwrap(),
    );

    let mut min_y = u32::MAX;
    for y in 0..pixmap.height() {
      for x in 0..pixmap.width() {
        if pixmap.pixel(x, y).unwrap().alpha() > 0 {
          min_y = min_y.min(y);
        }
      }
    }

    assert!(
      min_y < u32::MAX,
      "expected to find painted pixels for currentColor glyph"
    );

    let mut top_band_alpha = 0;
    for y in min_y..=min_y.saturating_add(2).min(pixmap.height() - 1) {
      for x in 0..pixmap.width() {
        top_band_alpha = top_band_alpha.max(pixmap.pixel(x, y).unwrap().alpha());
      }
    }

    assert!(
      top_band_alpha >= 120 && top_band_alpha <= 130,
      "top band should keep single application of text alpha, got {}",
      top_band_alpha
    );
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::render_control::{with_deadline, RenderDeadline};
  use crate::text::color_fonts::{color_font_render_count, ColorFontRenderCountGuard};
  use crate::text::face_cache;
  use crate::text::font_db::{FontStretch, FontStyle, FontWeight};
  use crate::text::font_loader::FontContext;
  use std::path::PathBuf;
  use std::sync::atomic::{AtomicUsize, Ordering};
  use std::time::Duration;

  fn get_test_font() -> Option<LoadedFont> {
    let ctx = FontContext::new();
    ctx.get_sans_serif()
  }

  #[test]
  fn lcd_gamma_lut_is_monotonic_and_lifts_midtones() {
    let lut = lcd_gamma_lut();
    assert_eq!(lut[0], 0);
    assert_eq!(lut[255], 255);
    assert!(
      lut[128] > 128,
      "expected gamma-adjusted coverages to lift midtones (got {})",
      lut[128]
    );
    for i in 1..256 {
      assert!(
        lut[i] >= lut[i - 1],
        "expected gamma LUT to be monotonic: lut[{}]={} < lut[{}]={}",
        i,
        lut[i],
        i - 1,
        lut[i - 1]
      );
    }
  }

  #[test]
  fn test_glyph_cache_creation() {
    let cache = GlyphCache::new();
    assert!(cache.is_empty());
    assert_eq!(cache.len(), 0);
  }

  #[test]
  fn test_glyph_cache_with_capacity() {
    let cache = GlyphCache::with_capacity(100);
    assert!(cache.is_empty());
  }

  #[test]
  fn test_text_rasterizer_creation() {
    let rasterizer = TextRasterizer::new();
    assert_eq!(rasterizer.cache_size(), 0);
  }

  #[test]
  fn test_text_rasterizer_with_capacity() {
    let rasterizer = TextRasterizer::with_cache_capacity(500);
    assert_eq!(rasterizer.cache_size(), 0);
  }

  #[test]
  fn text_rasterizer_enables_glyph_position_snapping_in_deterministic_hinted_mode() {
    let toggles = Arc::new(runtime::RuntimeToggles::from_map(HashMap::from([
      ("FASTR_DETERMINISTIC_PAINT".to_string(), "1".to_string()),
      ("FASTR_TEXT_HINTING".to_string(), "1".to_string()),
      ("FASTR_TEXT_SUBPIXEL_AA".to_string(), "0".to_string()),
    ])));
    runtime::with_runtime_toggles(toggles, || {
      let rasterizer = TextRasterizer::new();
      assert!(rasterizer.snap_glyph_positions);
    });
  }

  #[test]
  fn snap_device_x_aligns_to_integer_device_pixels() {
    for (coord, tx) in [
      (0.0, 0.0),
      (0.25, 0.0),
      (10.2, 0.3),
      (-5.8, 2.25),
      (1234.567, -0.125),
    ] {
      let snapped = snap_device_x(coord, tx);
      let device = snapped + tx;
      assert!(
        (device - device.round()).abs() < 1e-6,
        "expected snapped coord to land on device pixel: coord={coord} tx={tx} -> snapped={snapped} device={device}",
      );
    }
  }

  #[test]
  fn text_rasterizer_font_size_zero_does_not_error() {
    let font = FontContext::new()
      .get_sans_serif()
      .expect("expected bundled sans-serif font for tests");

    let glyphs = [GlyphPosition {
      glyph_id: 0,
      cluster: 0,
      x_offset: 0.0,
      y_offset: 0.0,
      x_advance: 12.0,
      y_advance: 0.0,
    }];

    let mut rasterizer = TextRasterizer::new();
    let mut pixmap = new_pixmap(4, 4).unwrap();
    let advance = rasterizer
      .render_glyph_run(
        &glyphs,
        &font,
        0.0,
        0.0,
        0.0,
        0,
        &[],
        0,
        &[],
        None,
        0.0,
        0.0,
        Rgba::BLACK,
        TextRenderState::default(),
        &mut pixmap,
      )
      .expect("font-size:0 should not error");
    assert_eq!(advance, 12.0);
  }

  #[test]
  fn test_render_glyph_basic() {
    let font = match get_test_font() {
      Some(f) => f,
      None => return, // Skip if no fonts available
    };

    let mut pixmap = new_pixmap(100, 100).unwrap();
    pixmap.fill(tiny_skia::Color::WHITE);

    // Get glyph ID for 'A'
    let face = font.as_ttf_face().unwrap();
    let glyph_id = face.glyph_index('A').map(|g| g.0 as u32).unwrap_or(0);

    // Render the glyph
    let result = render_glyph(&font, glyph_id, 16.0, 10.0, 80.0, Rgba::BLACK, &mut pixmap);

    assert!(result.is_ok());
    let advance = result.unwrap();
    assert!(advance > 0.0);
  }

  #[test]
  fn text_rasterizer_font_size_zero_is_noop() {
    let font = match get_test_font() {
      Some(f) => f,
      None => return, // Skip if no fonts available
    };

    let face = font.as_ttf_face().unwrap();
    let glyph_id = face.glyph_index('A').map(|g| g.0 as u32).unwrap_or(0);
    let glyphs = [GlyphPosition {
      glyph_id,
      cluster: 0,
      x_offset: 0.0,
      y_offset: 0.0,
      x_advance: 10.0,
      y_advance: 0.0,
    }];

    let mut pixmap = new_pixmap(64, 64).unwrap();
    pixmap.fill(tiny_skia::Color::WHITE);
    let before = pixmap.data().to_vec();

    let mut rasterizer = TextRasterizer::new();
    let advance =
      rasterizer.render_glyphs(&glyphs, &font, 0.0, 0.0, 32.0, Rgba::BLACK, &mut pixmap);

    assert!(advance.is_ok());
    assert!((advance.unwrap() - 10.0).abs() < f32::EPSILON);
    assert_eq!(pixmap.data(), &before[..]);

    // Repeat with a stroke to cover `-webkit-text-stroke` paths.
    let before = pixmap.data().to_vec();
    let advance = rasterizer.render_glyph_run_with_stroke(
      &glyphs,
      &font,
      0.0,
      0.0,
      0.0,
      0,
      &[],
      0,
      &[],
      None,
      0.0,
      32.0,
      Rgba::BLACK,
      Some(TextStroke {
        width: 1.0,
        color: Rgba::BLACK,
      }),
      TextRenderState::default(),
      &mut pixmap,
    );
    assert!(advance.is_ok());
    assert!((advance.unwrap() - 10.0).abs() < f32::EPSILON);
    assert_eq!(pixmap.data(), &before[..]);
  }

  #[test]
  fn test_render_glyph_space() {
    let font = match get_test_font() {
      Some(f) => f,
      None => return,
    };

    let mut pixmap = new_pixmap(100, 100).unwrap();
    pixmap.fill(tiny_skia::Color::WHITE);

    // Get glyph ID for space
    let face = font.as_ttf_face().unwrap();
    let glyph_id = face.glyph_index(' ').map(|g| g.0 as u32).unwrap_or(0);

    // Render (should succeed even though space has no outline)
    let result = render_glyph(&font, glyph_id, 16.0, 10.0, 80.0, Rgba::BLACK, &mut pixmap);

    assert!(result.is_ok());
    // Space should have positive advance
    let advance = result.unwrap();
    assert!(advance > 0.0);
  }

  #[test]
  fn test_glyph_advance() {
    let font = match get_test_font() {
      Some(f) => f,
      None => return,
    };

    let face = font.as_ttf_face().unwrap();
    let Some(glyph_id) = face.glyph_index('A').map(|g| g.0 as u32) else {
      return;
    };

    let advance = glyph_advance(&font, glyph_id, 16.0);
    assert!(advance.is_ok());
    assert!(advance.unwrap() > 0.0);
  }

  #[cfg(debug_assertions)]
  #[test]
  fn render_glyph_reuses_cached_face() {
    let font = match get_test_font() {
      Some(f) => f,
      None => return,
    };
    let Some(glyph_id) =
      face_cache::with_face(&font, |face| face.glyph_index('A').map(|g| g.0 as u32)).flatten()
    else {
      return;
    };

    let mut pixmap = new_pixmap(64, 64).unwrap();
    pixmap.fill(tiny_skia::Color::WHITE);

    let _guard = face_cache::FaceParseCountGuard::start();
    for _ in 0..3 {
      if render_glyph(&font, glyph_id, 16.0, 0.0, 32.0, Rgba::BLACK, &mut pixmap).is_err() {
        return;
      }
    }

    assert!(
      face_cache::face_parse_count() <= 1,
      "rasterizing repeated glyphs should not reparse faces"
    );
  }

  #[test]
  fn test_text_rasterizer_render_glyphs() {
    let font = match get_test_font() {
      Some(f) => f,
      None => return,
    };

    let face = font.as_ttf_face().unwrap();

    // Create some test glyphs
    let glyphs: Vec<GlyphPosition> = "ABC"
      .chars()
      .enumerate()
      .filter_map(|(i, c)| {
        let glyph_id = face.glyph_index(c)?.0 as u32;
        Some(GlyphPosition {
          glyph_id,
          cluster: i as u32,
          x_offset: 0.0,
          y_offset: 0.0,
          x_advance: 10.0, // Approximate
          y_advance: 0.0,
        })
      })
      .collect();

    let mut pixmap = new_pixmap(200, 100).unwrap();
    pixmap.fill(tiny_skia::Color::WHITE);

    let mut rasterizer = TextRasterizer::new();
    let result =
      rasterizer.render_glyphs(&glyphs, &font, 16.0, 10.0, 80.0, Rgba::BLACK, &mut pixmap);

    assert!(result.is_ok());
  }

  #[test]
  fn text_rasterize_respects_deadline_timeout_in_glyph_loop() {
    let font = FontContext::new()
      .get_sans_serif()
      .expect("expected bundled sans-serif font for tests");
    let face = font.as_ttf_face().expect("parse test font");
    let glyph_id = face
      .glyph_index(' ')
      .or_else(|| face.glyph_index('A'))
      .expect("resolve glyph for deadline timeout test")
      .0 as u32;

    let glyphs: Vec<GlyphPosition> = (0..10_000u32)
      .map(|cluster| GlyphPosition {
        glyph_id,
        cluster,
        x_offset: 0.0,
        y_offset: 0.0,
        x_advance: 0.0,
        y_advance: 0.0,
      })
      .collect();

    let checks = Arc::new(AtomicUsize::new(0));
    let checks_for_cb = Arc::clone(&checks);
    let cancel: Arc<crate::render_control::CancelCallback> = Arc::new(move || {
      let prev = checks_for_cb.fetch_add(1, Ordering::SeqCst);
      prev >= 1
    });
    let deadline = RenderDeadline::new(None, Some(cancel));
    let mut rasterizer = TextRasterizer::new();
    let mut pixmap = new_pixmap(4, 4).unwrap();

    let result = with_deadline(Some(&deadline), || {
      rasterizer.render_glyph_run(
        &glyphs,
        &font,
        16.0,
        0.0,
        0.0,
        0,
        &[],
        0,
        &[],
        None,
        0.0,
        0.0,
        Rgba::BLACK,
        TextRenderState::default(),
        &mut pixmap,
      )
    });

    assert!(
      checks.load(Ordering::SeqCst) >= 2,
      "expected render deadline to be checked more than once (entry + periodic), got {}",
      checks.load(Ordering::SeqCst)
    );
    assert!(
      matches!(
        result,
        Err(Error::Render(RenderError::Timeout {
          stage: RenderStage::Paint,
          ..
        }))
      ),
      "expected paint-stage timeout, got {result:?}"
    );
  }

  #[test]
  fn text_rasterize_times_out_immediately_when_deadline_already_expired() {
    let font = FontContext::new()
      .get_sans_serif()
      .expect("expected bundled sans-serif font for tests");
    let face = font.as_ttf_face().expect("parse test font");
    let glyph_id = face
      .glyph_index(' ')
      .or_else(|| face.glyph_index('A'))
      .expect("resolve glyph for deadline timeout test")
      .0 as u32;

    let glyphs = [GlyphPosition {
      glyph_id,
      cluster: 0,
      x_offset: 0.0,
      y_offset: 0.0,
      x_advance: 0.0,
      y_advance: 0.0,
    }];

    let deadline = RenderDeadline::new(Some(Duration::from_millis(0)), None);
    let mut rasterizer = TextRasterizer::new();
    let mut pixmap = new_pixmap(4, 4).unwrap();

    let result = with_deadline(Some(&deadline), || {
      rasterizer.render_glyph_run(
        &glyphs,
        &font,
        16.0,
        0.0,
        0.0,
        0,
        &[],
        0,
        &[],
        None,
        0.0,
        0.0,
        Rgba::BLACK,
        TextRenderState::default(),
        &mut pixmap,
      )
    });

    assert!(
      matches!(
        result,
        Err(Error::Render(RenderError::Timeout {
          stage: RenderStage::Paint,
          ..
        }))
      ),
      "expected paint-stage timeout, got {result:?}"
    );
  }

  #[test]
  fn positioned_glyph_paths_respects_deadline_timeout_in_glyph_loop() {
    let font = FontContext::new()
      .get_sans_serif()
      .expect("expected bundled sans-serif font for tests");
    let face = font.as_ttf_face().expect("parse test font");
    let glyph_id = face
      .glyph_index(' ')
      .or_else(|| face.glyph_index('A'))
      .expect("resolve glyph for deadline timeout test")
      .0 as u32;

    let glyphs: Vec<GlyphPosition> = (0..10_000u32)
      .map(|cluster| GlyphPosition {
        glyph_id,
        cluster,
        x_offset: 0.0,
        y_offset: 0.0,
        x_advance: 0.0,
        y_advance: 0.0,
      })
      .collect();

    let checks = Arc::new(AtomicUsize::new(0));
    let checks_for_cb = Arc::clone(&checks);
    let cancel: Arc<crate::render_control::CancelCallback> = Arc::new(move || {
      let prev = checks_for_cb.fetch_add(1, Ordering::SeqCst);
      prev >= 1
    });
    let deadline = RenderDeadline::new(None, Some(cancel));
    let mut rasterizer = TextRasterizer::new();

    let result = with_deadline(Some(&deadline), || {
      rasterizer.positioned_glyph_paths(&glyphs, &font, 16.0, 0.0, 0.0, 0.0, None, &[])
    });

    assert!(
      checks.load(Ordering::SeqCst) >= 2,
      "expected render deadline to be checked more than once (entry + periodic), got {}",
      checks.load(Ordering::SeqCst)
    );
    assert!(
      matches!(
        result,
        Err(Error::Render(RenderError::Timeout {
          stage: RenderStage::Paint,
          ..
        }))
      ),
      "expected paint-stage timeout, got {result:?}"
    );
  }

  #[test]
  fn text_rasterize_respects_deadline_timeout_for_short_runs() {
    let font = FontContext::new()
      .get_sans_serif()
      .expect("expected bundled sans-serif font for tests");
    let face = font.as_ttf_face().expect("parse test font");
    let glyph_id = face
      .glyph_index(' ')
      .or_else(|| face.glyph_index('A'))
      .expect("resolve glyph for deadline timeout test")
      .0 as u32;

    let glyphs = [GlyphPosition {
      glyph_id,
      cluster: 0,
      x_offset: 0.0,
      y_offset: 0.0,
      x_advance: 0.0,
      y_advance: 0.0,
    }];

    let checks = Arc::new(AtomicUsize::new(0));
    let checks_for_cb = Arc::clone(&checks);
    let cancel: Arc<crate::render_control::CancelCallback> = Arc::new(move || {
      let prev = checks_for_cb.fetch_add(1, Ordering::SeqCst);
      prev >= 1
    });
    let deadline = RenderDeadline::new(None, Some(cancel));
    let mut rasterizer = TextRasterizer::new();
    let mut pixmap = new_pixmap(4, 4).unwrap();

    let result = with_deadline(Some(&deadline), || {
      rasterizer.render_glyph_run(
        &glyphs,
        &font,
        16.0,
        0.0,
        0.0,
        0,
        &[],
        0,
        &[],
        None,
        0.0,
        0.0,
        Rgba::BLACK,
        TextRenderState::default(),
        &mut pixmap,
      )
    });

    assert!(
      checks.load(Ordering::SeqCst) >= 2,
      "expected entry + final deadline checks for short runs, got {}",
      checks.load(Ordering::SeqCst)
    );
    assert!(
      matches!(
        result,
        Err(Error::Render(RenderError::Timeout {
          stage: RenderStage::Paint,
          ..
        }))
      ),
      "expected paint-stage timeout, got {result:?}"
    );
  }

  #[test]
  fn positioned_glyph_paths_respects_deadline_timeout_for_short_runs() {
    let font = FontContext::new()
      .get_sans_serif()
      .expect("expected bundled sans-serif font for tests");
    let face = font.as_ttf_face().expect("parse test font");
    let glyph_id = face
      .glyph_index(' ')
      .or_else(|| face.glyph_index('A'))
      .expect("resolve glyph for deadline timeout test")
      .0 as u32;

    let glyphs = [GlyphPosition {
      glyph_id,
      cluster: 0,
      x_offset: 0.0,
      y_offset: 0.0,
      x_advance: 0.0,
      y_advance: 0.0,
    }];

    let checks = Arc::new(AtomicUsize::new(0));
    let checks_for_cb = Arc::clone(&checks);
    let cancel: Arc<crate::render_control::CancelCallback> = Arc::new(move || {
      let prev = checks_for_cb.fetch_add(1, Ordering::SeqCst);
      prev >= 1
    });
    let deadline = RenderDeadline::new(None, Some(cancel));
    let mut rasterizer = TextRasterizer::new();

    let result = with_deadline(Some(&deadline), || {
      rasterizer.positioned_glyph_paths(&glyphs, &font, 16.0, 0.0, 0.0, 0.0, None, &[])
    });

    assert!(
      checks.load(Ordering::SeqCst) >= 2,
      "expected entry + final deadline checks for short runs, got {}",
      checks.load(Ordering::SeqCst)
    );
    assert!(
      matches!(
        result,
        Err(Error::Render(RenderError::Timeout {
          stage: RenderStage::Paint,
          ..
        }))
      ),
      "expected paint-stage timeout, got {result:?}"
    );
  }

  #[test]
  fn test_to_skia_color() {
    let color = Rgba::from_rgba8(255, 128, 64, 200);
    let skia_color = to_skia_color(color);

    // tiny-skia Color methods return f32 in 0.0-1.0 range
    // from_rgba8 creates premultiplied colors, so values are normalized
    assert!(skia_color.red() > 0.0);
    assert!(skia_color.green() > 0.0);
    assert!(skia_color.blue() > 0.0);
    assert!(skia_color.alpha() > 0.0);
  }

  #[test]
  fn test_glyph_cache_key() {
    let font = match get_test_font() {
      Some(f) => f,
      None => return,
    };

    let variations: &[Variation] = &[];
    let var_hash = variation_hash(variations);

    let key1 = GlyphCacheKey::new(&font, 65, var_hash, 16.0, false);
    let key2 = GlyphCacheKey::new(&font, 65, var_hash, 32.0, false);
    let key3 = GlyphCacheKey::new(&font, 66, var_hash, 16.0, false);
    let key4 = GlyphCacheKey::new(&font, 65, var_hash.wrapping_add(1), 16.0, false);
    let key5 = GlyphCacheKey::new(&font, 65, var_hash, 16.0, true);
    let key6 = GlyphCacheKey::new(&font, 65, var_hash, 32.0, true);

    // Same font, glyph, and variation hash should be equal
    assert_eq!(key1, key2);

    // Different glyph should be different
    assert_ne!(key1, key3);

    // Different variation hash should be different
    assert_ne!(key1, key4);

    // When hinting is enabled, font size becomes part of the key.
    assert_ne!(key1, key5);
    assert_ne!(key5, key6);
  }

  #[test]
  fn test_glyph_cache_hit_miss_and_eviction() {
    let font = match get_test_font() {
      Some(f) => f,
      None => return,
    };

    let face = font.as_ttf_face().unwrap();
    let glyph_a = face.glyph_index('A').map(|g| g.0 as u32).unwrap_or(0);
    let glyph_b = face.glyph_index('B').map(|g| g.0 as u32).unwrap_or(1);

    let mut cache = GlyphCache::with_capacity(1);

    let variations: &[Variation] = &[];
    let instance = FontInstance::new(&font, variations).unwrap();

    assert!(cache
      .get_or_build(&font, &instance, glyph_a, 16.0, false)
      .is_some());
    let stats = cache.stats();
    assert_eq!(stats.misses, 1);
    assert_eq!(stats.hits, 0);

    assert!(cache
      .get_or_build(&font, &instance, glyph_a, 16.0, false)
      .is_some());
    let stats = cache.stats();
    assert_eq!(stats.hits, 1);

    // Insert another glyph to trigger eviction
    assert!(cache
      .get_or_build(&font, &instance, glyph_b, 16.0, false)
      .is_some());
    let stats = cache.stats();
    assert!(stats.evictions >= 1);
    assert!(cache.len() <= 1);
  }

  #[test]
  fn test_glyph_cache_clear() {
    let mut cache = GlyphCache::new();
    cache.clear();
    assert!(cache.is_empty());
  }

  #[test]
  fn test_text_rasterizer_clear_cache() {
    let mut rasterizer = TextRasterizer::new();
    rasterizer.clear_cache();
    assert_eq!(rasterizer.cache_size(), 0);
  }

  #[test]
  fn test_text_rasterizer_cache_hits_on_reuse() {
    let font = match get_test_font() {
      Some(f) => f,
      None => return,
    };

    let face = font.as_ttf_face().unwrap();
    let Some(glyph_id) = face.glyph_index('A').map(|g| g.0 as u32) else {
      return;
    };

    let glyphs = vec![GlyphPosition {
      glyph_id,
      cluster: 0,
      x_offset: 0.0,
      y_offset: 0.0,
      x_advance: 10.0,
      y_advance: 0.0,
    }];

    let mut rasterizer = TextRasterizer::new();
    rasterizer.reset_cache_stats();

    let mut pixmap = new_pixmap(50, 50).unwrap();
    rasterizer
      .render_glyphs(&glyphs, &font, 16.0, 10.0, 35.0, Rgba::BLACK, &mut pixmap)
      .unwrap();

    let stats_after_first = rasterizer.cache_stats();

    let mut pixmap2 = new_pixmap(50, 50).unwrap();
    rasterizer
      .render_glyphs(&glyphs, &font, 16.0, 20.0, 40.0, Rgba::BLACK, &mut pixmap2)
      .unwrap();

    let stats_after_second = rasterizer.cache_stats();
    assert_eq!(stats_after_second.misses, stats_after_first.misses);
    assert!(stats_after_second.hits > stats_after_first.hits);
  }

  #[test]
  fn text_rasterizer_cache_recovers_from_poisoned_lock() {
    let font = match get_test_font() {
      Some(f) => f,
      None => return,
    };

    let face = font.as_ttf_face().unwrap();
    let Some(glyph_id) = face.glyph_index('A').map(|g| g.0 as u32) else {
      return;
    };

    let glyphs = vec![GlyphPosition {
      glyph_id,
      cluster: 0,
      x_offset: 0.0,
      y_offset: 0.0,
      x_advance: 10.0,
      y_advance: 0.0,
    }];

    let mut rasterizer = TextRasterizer::new();
    rasterizer.reset_cache_stats();

    let mut pixmap = new_pixmap(50, 50).unwrap();
    rasterizer
      .render_glyphs(&glyphs, &font, 16.0, 10.0, 35.0, Rgba::BLACK, &mut pixmap)
      .unwrap();

    let stats_before = rasterizer.cache_stats();

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      let _guard = rasterizer.cache.lock().unwrap();
      panic!("poison text outline cache lock");
    }));
    assert!(result.is_err(), "expected panic to be caught");
    assert!(
      rasterizer.cache.is_poisoned(),
      "expected glyph outline cache mutex to be poisoned"
    );

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      let _guard = rasterizer.color_cache.lock().unwrap();
      panic!("poison text color cache lock");
    }));
    assert!(result.is_err(), "expected panic to be caught");
    assert!(
      rasterizer.color_cache.is_poisoned(),
      "expected color glyph cache mutex to be poisoned"
    );

    let mut pixmap2 = new_pixmap(50, 50).unwrap();
    rasterizer
      .render_glyphs(&glyphs, &font, 16.0, 20.0, 40.0, Rgba::BLACK, &mut pixmap2)
      .unwrap();

    let stats_after = rasterizer.cache_stats();
    assert_eq!(
      stats_after.misses, stats_before.misses,
      "second render should reuse cached entries without additional misses"
    );
    assert!(
      stats_after.hits > stats_before.hits,
      "second render should record cache hits after lock poison recovery"
    );
  }

  #[test]
  fn color_glyph_cache_caches_negative_results() {
    let font_path =
      PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fonts/colrv1-test.ttf");
    let font_bytes = std::fs::read(&font_path).expect("read colrv1-test font fixture");
    let font = LoadedFont {
      id: None,
      data: Arc::new(font_bytes),
      index: 0,
      face_metrics_overrides: crate::text::font_db::FontFaceMetricsOverrides::default(),
      face_settings: Default::default(),
      family: "Test COLRv1".to_string(),
      weight: FontWeight::NORMAL,
      style: FontStyle::Normal,
      stretch: FontStretch::Normal,
    };

    // The fixture is generated with glyph order ['.notdef', 'colr', 'box', 'triangle'] where only
    // the `colr` glyph is addressable via COLR paint graphs. The `box` outline is therefore a
    // stable non-color glyph that should always fall back to outline rendering.
    let glyph_id = 2_u32;

    // Sanity check: ensure this glyph does not render through the color font renderer.
    let instance = FontInstance::new(&font, &[]).expect("parse colrv1-test font");
    assert!(
      ColorFontRenderer::new()
        .render(
          &font,
          &instance,
          glyph_id,
          32.0,
          0,
          &[],
          0,
          Rgba::BLACK,
          0.0,
          &[],
          None,
        )
        .is_none(),
      "expected fixture glyph to fall back to outlines"
    );

    let _render_guard = ColorFontRenderCountGuard::start();
    let mut rasterizer = TextRasterizer::new();
    rasterizer.reset_cache_stats();

    let glyphs = vec![GlyphPosition {
      glyph_id,
      cluster: 0,
      x_offset: 0.0,
      y_offset: 0.0,
      x_advance: 10.0,
      y_advance: 0.0,
    }];

    let mut pixmap = new_pixmap(64, 64).unwrap();
    rasterizer
      .render_glyphs(&glyphs, &font, 32.0, 0.0, 48.0, Rgba::BLACK, &mut pixmap)
      .unwrap();

    let color_stats_first = rasterizer.color_cache.lock().unwrap().stats();
    assert_eq!(color_stats_first.misses, 1);
    assert_eq!(color_stats_first.hits, 0);
    assert_eq!(
      color_font_render_count(),
      1,
      "first render should attempt color rasterization once"
    );

    let mut pixmap2 = new_pixmap(64, 64).unwrap();
    rasterizer
      .render_glyphs(&glyphs, &font, 32.0, 0.0, 48.0, Rgba::BLACK, &mut pixmap2)
      .unwrap();

    let color_stats_second = rasterizer.color_cache.lock().unwrap().stats();
    assert_eq!(color_stats_second.misses, 1);
    assert_eq!(color_stats_second.hits, 1);
    assert_eq!(
      color_font_render_count(),
      1,
      "cached negative color lookup should skip subsequent color rasterization attempts"
    );
  }

  #[test]
  fn test_text_rasterizer_outline_cache_reused_across_font_sizes() {
    let font = match get_test_font() {
      Some(f) => f,
      None => return,
    };

    let face = font.as_ttf_face().unwrap();
    let glyph_id = face.glyph_index('A').map(|g| g.0 as u32).unwrap_or(0);

    let glyphs = vec![GlyphPosition {
      glyph_id,
      cluster: 0,
      x_offset: 0.0,
      y_offset: 0.0,
      x_advance: 10.0,
      y_advance: 0.0,
    }];

    let mut rasterizer = TextRasterizer::new();
    rasterizer.reset_cache_stats();

    rasterizer
      .positioned_glyph_paths(&glyphs, &font, 16.0, 0.0, 0.0, 0.0, None, &[])
      .unwrap();
    rasterizer
      .positioned_glyph_paths(&glyphs, &font, 32.0, 0.0, 0.0, 0.0, None, &[])
      .unwrap();

    let outline_stats = rasterizer
      .cache
      .lock()
      .map(|cache| cache.stats())
      .unwrap_or_default();
    assert_eq!(outline_stats.misses, 1);
    assert!(outline_stats.hits >= 1);
  }

  #[test]
  fn test_text_rasterizer_outline_cache_reused_across_synthetic_oblique() {
    let font = match get_test_font() {
      Some(f) => f,
      None => return,
    };

    let face = font.as_ttf_face().unwrap();
    let Some(glyph_id) = face.glyph_index('A').map(|g| g.0 as u32) else {
      return;
    };

    let glyphs = vec![GlyphPosition {
      glyph_id,
      cluster: 0,
      x_offset: 0.0,
      y_offset: 0.0,
      x_advance: 10.0,
      y_advance: 0.0,
    }];

    let mut rasterizer = TextRasterizer::new();
    rasterizer.reset_cache_stats();

    rasterizer
      .positioned_glyph_paths(&glyphs, &font, 16.0, 0.0, 0.0, 0.0, None, &[])
      .unwrap();
    rasterizer
      .positioned_glyph_paths(&glyphs, &font, 16.0, 0.0, 0.0, 0.25, None, &[])
      .unwrap();

    let outline_stats = rasterizer
      .cache
      .lock()
      .map(|cache| cache.stats())
      .unwrap_or_default();
    assert_eq!(outline_stats.misses, 1);
    assert!(outline_stats.hits >= 1);
  }

  #[test]
  fn test_text_rasterizer_outline_cache_reused_across_rotation() {
    let font = match get_test_font() {
      Some(f) => f,
      None => return,
    };

    let face = font.as_ttf_face().unwrap();
    let Some(glyph_id) = face.glyph_index('A').map(|g| g.0 as u32) else {
      return;
    };

    let glyphs = vec![GlyphPosition {
      glyph_id,
      cluster: 0,
      x_offset: 0.0,
      y_offset: 0.0,
      x_advance: 10.0,
      y_advance: 0.0,
    }];

    let mut rasterizer = TextRasterizer::new();
    rasterizer.reset_cache_stats();

    rasterizer
      .positioned_glyph_paths(&glyphs, &font, 16.0, 0.0, 0.0, 0.0, None, &[])
      .unwrap();
    rasterizer
      .positioned_glyph_paths(
        &glyphs,
        &font,
        16.0,
        0.0,
        0.0,
        0.0,
        Some(Transform::from_rotate(25.0)),
        &[],
      )
      .unwrap();

    let outline_stats = rasterizer
      .cache
      .lock()
      .map(|cache| cache.stats())
      .unwrap_or_default();
    assert_eq!(outline_stats.misses, 1);
    assert!(outline_stats.hits >= 1);
  }

  #[test]
  fn test_color_black() {
    let black = Rgba::BLACK;
    assert_eq!(black.r, 0);
    assert_eq!(black.g, 0);
    assert_eq!(black.b, 0);
    assert_eq!(black.a, 1.0);
  }

  #[test]
  fn test_glyph_outline_builder_metrics() {
    use ttf_parser::OutlineBuilder;

    let mut builder = crate::text::font_instance::PathOutlineBuilder::new();
    OutlineBuilder::move_to(&mut builder, 0.0, 0.0);
    OutlineBuilder::line_to(&mut builder, 10.0, 0.0);
    OutlineBuilder::quad_to(&mut builder, 15.0, 5.0, 20.0, 0.0);
    OutlineBuilder::curve_to(&mut builder, 20.0, 5.0, 25.0, 5.0, 30.0, 0.0);
    OutlineBuilder::close(&mut builder);

    let (_path, metrics) = builder.finish();
    assert_eq!(metrics.verb_count, 5);
    assert_eq!(metrics.point_count, 7);
  }

  #[test]
  fn test_glyph_transform_matrix() {
    let transform = glyph_transform(2.0, 0.25, 10.0, 20.0);
    assert!((transform.sx - 2.0).abs() < 1e-6);
    assert!((transform.kx - 0.5).abs() < 1e-6);
    assert!((transform.sy + 2.0).abs() < 1e-6);
    assert_eq!(transform.tx, 10.0);
    assert_eq!(transform.ty, 20.0);
  }

  mod font_smoothing_aa_tests {
    use super::*;

    fn glyph_for_char(font: &LoadedFont, c: char) -> Option<u32> {
      let face = font.as_ttf_face().ok()?;
      face.glyph_index(c).map(|g| g.0 as u32)
    }

    fn any_partial_alpha(pixmap: &Pixmap) -> bool {
      pixmap
        .data()
        .chunks_exact(4)
        .any(|px| px[3] > 0 && px[3] < 255)
    }

    fn any_ink(pixmap: &Pixmap) -> bool {
      pixmap.data().chunks_exact(4).any(|px| px[3] > 0)
    }

    fn any_color_fringes(pixmap: &Pixmap) -> bool {
      // Only meaningful when both the background and glyph color are grayscale (R==G==B). In that
      // case, any channel divergence indicates the LCD/subpixel branch ran.
      pixmap.data().chunks_exact(4).any(|px| px[0] != px[1] || px[1] != px[2])
    }

    fn render_single_glyph(
      font: &LoadedFont,
      glyph_id: u32,
      font_size: f32,
      x: f32,
      baseline_y: f32,
      color: Rgba,
      background: Option<tiny_skia::Color>,
      state: TextRenderState<'_>,
    ) -> (Pixmap, TextRasterizer) {
      let glyphs = [GlyphPosition {
        glyph_id,
        cluster: 0,
        x_offset: 0.0,
        y_offset: 0.0,
        x_advance: 0.0,
        y_advance: 0.0,
      }];

      let mut rasterizer = TextRasterizer::new();
      let mut pixmap = Pixmap::new(96, 96).expect("pixmap");
      if let Some(bg) = background {
        pixmap.fill(bg);
      }

      rasterizer
        .render_glyph_run(
          &glyphs,
          font,
          font_size,
          0.0,
          0.0,
          0,
          &[],
          0,
          &[],
          None,
          x,
          baseline_y,
          color,
          state,
          &mut pixmap,
        )
        .expect("render glyph");

      (pixmap, rasterizer)
    }

    #[test]
    fn font_smoothing_none_disables_antialiasing() {
      let toggles = Arc::new(runtime::RuntimeToggles::from_map(HashMap::from([(
        "FASTR_TEXT_SUBPIXEL_AA".to_string(),
        "0".to_string(),
      )])));
      runtime::with_runtime_toggles(toggles, || {
        let Some(font) = get_test_font() else {
          return;
        };
        let Some(glyph_id) = glyph_for_char(&font, 'A') else {
          return;
        };

        let (aa_pixmap, _rasterizer) = render_single_glyph(
          &font,
          glyph_id,
          64.0,
          10.25,
          72.5,
          Rgba::BLACK,
          None,
          TextRenderState {
            font_smoothing: FontSmoothing::Auto,
            ..TextRenderState::default()
          },
        );
        assert!(
          any_partial_alpha(&aa_pixmap),
          "expected grayscale AA to produce partially-covered pixels"
        );

        let (none_pixmap, _rasterizer) = render_single_glyph(
          &font,
          glyph_id,
          64.0,
          10.25,
          72.5,
          Rgba::BLACK,
          None,
          TextRenderState {
            font_smoothing: FontSmoothing::None,
            ..TextRenderState::default()
          },
        );
        assert!(any_ink(&none_pixmap), "expected glyph to draw");
        assert!(
          !any_partial_alpha(&none_pixmap),
          "FontSmoothing::None should disable anti-aliasing"
        );
      });
    }

    #[test]
    fn font_smoothing_grayscale_disables_subpixel_aa_branch() {
      let toggles = Arc::new(runtime::RuntimeToggles::from_map(HashMap::from([
        ("FASTR_TEXT_SUBPIXEL_AA".to_string(), "1".to_string()),
        (
          "FASTR_TEXT_SUBPIXEL_AA_DIAGNOSTICS".to_string(),
          "1".to_string(),
        ),
        // Keep glyph positions fractional so we reliably exercise LCD edge cases.
        ("FASTR_TEXT_SNAP_GLYPH_POSITIONS".to_string(), "0".to_string()),
      ])));
      runtime::with_runtime_toggles(toggles, || {
        let Some(font) = get_test_font() else {
          return;
        };
        let Some(glyph_id) = glyph_for_char(&font, 'I') else {
          return;
        };

        let (pixmap, rasterizer) = render_single_glyph(
          &font,
          glyph_id,
          64.0,
          10.25,
          72.5,
          Rgba::WHITE,
          Some(tiny_skia::Color::from_rgba8(128, 128, 128, 255)),
          TextRenderState {
            font_smoothing: FontSmoothing::Grayscale,
            ..TextRenderState::default()
          },
        );

        assert!(rasterizer.subpixel_aa_enabled);
        assert!(
          rasterizer.subpixel_aa_diagnostics.is_some(),
          "expected diagnostics to be enabled for test"
        );
        let stats = rasterizer.subpixel_aa_diagnostics.unwrap();
        assert_eq!(
          stats.attempts, 0,
          "FontSmoothing::Grayscale should skip LCD/subpixel AA attempts"
        );
        assert_eq!(stats.successes, 0);
        assert!(
          pixmap
            .data()
            .chunks_exact(4)
            .any(|px| px[0] != 128 || px[1] != 128 || px[2] != 128),
          "expected glyph draw to modify the backdrop"
        );
        assert!(
          !any_color_fringes(&pixmap),
          "grayscale AA should not introduce color fringes"
        );
      });
    }

    #[test]
    fn font_smoothing_subpixel_enables_subpixel_aa_branch() {
      let toggles = Arc::new(runtime::RuntimeToggles::from_map(HashMap::from([
        ("FASTR_TEXT_SUBPIXEL_AA".to_string(), "1".to_string()),
        (
          "FASTR_TEXT_SUBPIXEL_AA_DIAGNOSTICS".to_string(),
          "1".to_string(),
        ),
        ("FASTR_TEXT_SNAP_GLYPH_POSITIONS".to_string(), "0".to_string()),
      ])));
      runtime::with_runtime_toggles(toggles, || {
        let Some(font) = get_test_font() else {
          return;
        };
        let Some(glyph_id) = glyph_for_char(&font, 'I') else {
          return;
        };

        let (pixmap, rasterizer) = render_single_glyph(
          &font,
          glyph_id,
          64.0,
          10.25,
          72.5,
          Rgba::WHITE,
          Some(tiny_skia::Color::from_rgba8(128, 128, 128, 255)),
          TextRenderState {
            font_smoothing: FontSmoothing::Subpixel,
            ..TextRenderState::default()
          },
        );

        assert!(rasterizer.subpixel_aa_enabled);
        assert!(
          rasterizer.subpixel_aa_diagnostics.is_some(),
          "expected diagnostics to be enabled for test"
        );
        let stats = rasterizer.subpixel_aa_diagnostics.unwrap();
        assert_eq!(
          stats.attempts, 1,
          "FontSmoothing::Subpixel should attempt LCD/subpixel AA when enabled"
        );
        assert_eq!(
          stats.successes, 1,
          "FontSmoothing::Subpixel should successfully rasterize via the LCD path"
        );
        assert!(
          any_color_fringes(&pixmap),
          "expected LCD/subpixel AA to introduce color fringes on an opaque backdrop"
        );
      });
    }
  }

  #[test]
  fn subpixel_text_rasterization_produces_color_fringes_only_on_opaque_backdrops() {
    let toggles = Arc::new(runtime::RuntimeToggles::from_map(HashMap::from([(
      "FASTR_TEXT_SUBPIXEL_AA".to_string(),
      "1".to_string(),
    )])));
    runtime::with_runtime_toggles(toggles, || {
      let mut rasterizer = TextRasterizer::new();
      assert!(rasterizer.subpixel_aa_enabled);

      let mut pixmap = Pixmap::new(16, 4).unwrap();
      pixmap.fill(tiny_skia::Color::from_rgba8(128, 128, 128, 255));
      let rect = tiny_skia::Rect::from_xywh(0.0, 0.0, 5.1, 4.0).unwrap();
      let path = tiny_skia::PathBuilder::from_rect(rect);
      {
        let mut pixmap_mut = pixmap.as_mut();
        rasterizer
          .fill_path_subpixel_aa(
            &mut pixmap_mut,
            &path,
            Transform::identity(),
            1.0,
            Rgba::WHITE,
            None,
          )
          .unwrap();
      }

      let mut fringe = false;
      for px in pixmap.data().chunks_exact(4) {
        if px[3] != 255 {
          continue;
        }
        if px[0] == 128 && px[1] == 128 && px[2] == 128 {
          continue;
        }
        if px[0] == 255 && px[1] == 255 && px[2] == 255 {
          continue;
        }
        if px[0] != px[1] || px[1] != px[2] {
          fringe = true;
          break;
        }
      }
      assert!(fringe);

      let mut transparent = Pixmap::new(16, 4).unwrap();
      transparent.fill(tiny_skia::Color::from_rgba8(0, 0, 0, 0));
      {
        let mut pixmap_mut = transparent.as_mut();
        rasterizer
          .fill_path_subpixel_aa(
            &mut pixmap_mut,
            &path,
            Transform::identity(),
            1.0,
            Rgba::WHITE,
            None,
          )
          .unwrap();
      }
      assert!(transparent
        .data()
        .chunks_exact(4)
        .any(|px| px[3] > 0 && px[3] < 255));
      for px in transparent.data().chunks_exact(4) {
        if px[3] == 0 {
          continue;
        }
        assert_eq!(px[0], px[1]);
        assert_eq!(px[1], px[2]);
      }
    });
  }

  fn count_tinted_pixels(pixmap: &Pixmap) -> usize {
    pixmap
      .data()
      .chunks_exact(4)
      .filter(|px| px[3] != 0 && (px[0] != px[1] || px[1] != px[2]))
      .count()
  }

  #[test]
  fn subpixel_text_rasterization_is_disabled_for_scaled_transforms() {
    let font = match get_test_font() {
      Some(f) => f,
      None => return,
    };

    let face = font.as_ttf_face().unwrap();
    // Use a glyph with strong vertical stems so LCD/subpixel AA reliably produces colored fringes
    // when enabled (the identity-transform sanity check below depends on this).
    let Some(glyph_id) = face.glyph_index('H').map(|g| g.0 as u32) else {
      return;
    };

    let glyphs = [GlyphPosition {
      glyph_id,
      cluster: 0,
      x_offset: 0.0,
      y_offset: 0.0,
      x_advance: 0.0,
      y_advance: 0.0,
    }];

    let toggles = Arc::new(runtime::RuntimeToggles::from_map(HashMap::from([
      ("FASTR_TEXT_SUBPIXEL_AA".to_string(), "1".to_string()),
      // Ensure glyph positions remain fractional so the LCD/subpixel AA path would normally kick in.
      ("FASTR_TEXT_SNAP_GLYPH_POSITIONS".to_string(), "0".to_string()),
    ])));
    runtime::with_runtime_toggles(toggles, || {
      let mut rasterizer = TextRasterizer::new();
      assert!(rasterizer.subpixel_aa_enabled);

      let mut scaled = new_pixmap(200, 120).unwrap();
      scaled.fill(tiny_skia::Color::WHITE);

      let state = TextRenderState {
        transform: Transform::from_scale(1.25, 1.25),
        ..TextRenderState::default()
      };
      rasterizer
        .render_glyph_run_with_stroke(
          &glyphs,
          &font,
          32.0,
          0.0,
          0.0,
          0,
          &[],
          0,
          &[],
          None,
          10.3,
          64.0,
          Rgba::BLACK,
          None,
          state,
          &mut scaled,
        )
        .unwrap();

      assert_eq!(
        count_tinted_pixels(&scaled),
        0,
        "scaled text should fall back to grayscale AA (no RGB fringes)"
      );

      // Sanity: subpixel AA should still be used for translation-only transforms.
      let mut identity = new_pixmap(200, 120).unwrap();
      identity.fill(tiny_skia::Color::WHITE);
      rasterizer
        .render_glyph_run_with_stroke(
          &glyphs,
          &font,
          32.0,
          0.0,
          0.0,
          0,
          &[],
          0,
          &[],
          None,
          10.3,
          64.0,
          Rgba::BLACK,
          None,
          TextRenderState::default(),
          &mut identity,
        )
        .unwrap();
      assert!(
        count_tinted_pixels(&identity) > 0,
        "expected translation-only text to keep subpixel AA enabled for this test"
      );
    });
  }

  #[test]
  fn hinting_is_disabled_for_scaled_transforms_to_keep_outline_cache_reusable() {
    let font = match get_test_font() {
      Some(f) => f,
      None => return,
    };

    let face = font.as_ttf_face().unwrap();
    let Some(glyph_id) = face.glyph_index('H').map(|g| g.0 as u32) else {
      return;
    };

    let glyphs = [GlyphPosition {
      glyph_id,
      cluster: 0,
      x_offset: 0.0,
      y_offset: 0.0,
      x_advance: 0.0,
      y_advance: 0.0,
    }];

    let toggles = Arc::new(runtime::RuntimeToggles::from_map(HashMap::from([(
      "FASTR_TEXT_HINTING".to_string(),
      "1".to_string(),
    )])));
    runtime::with_runtime_toggles(toggles, || {
      let mut rasterizer = TextRasterizer::new();
      assert!(rasterizer.hinting_enabled);
      rasterizer.reset_cache_stats();

      let state = TextRenderState {
        transform: Transform::from_scale(1.25, 1.25),
        ..TextRenderState::default()
      };

      let mut pixmap = new_pixmap(200, 120).unwrap();
      pixmap.fill(tiny_skia::Color::WHITE);
      rasterizer
        .render_glyph_run_with_stroke(
          &glyphs,
          &font,
          16.0,
          0.0,
          0.0,
          0,
          &[],
          0,
          &[],
          None,
          10.0,
          64.0,
          Rgba::BLACK,
          None,
          state,
          &mut pixmap,
        )
        .unwrap();

      let mut pixmap2 = new_pixmap(200, 120).unwrap();
      pixmap2.fill(tiny_skia::Color::WHITE);
      rasterizer
        .render_glyph_run_with_stroke(
          &glyphs,
          &font,
          32.0,
          0.0,
          0.0,
          0,
          &[],
          0,
          &[],
          None,
          10.0,
          64.0,
          Rgba::BLACK,
          None,
          state,
          &mut pixmap2,
        )
        .unwrap();

      let outline_stats = rasterizer
        .cache
        .lock()
        .map(|cache| cache.stats())
        .unwrap_or_default();
      assert_eq!(
        outline_stats.misses, 1,
        "expected hinting to be suppressed under scale transforms so outlines reuse across font sizes"
      );
    });
  }

  fn subpixel_lcd_filter_clamps_above_255() {
    // FreeType's default LCD kernel sums to 272, so a fully-covered run of subpixels yields a value
    // > 255 after the fixed-point divide-by-256. Ensure we clamp instead of wrapping a u8.
    let width_sub_i32 = 9;
    let mut row = vec![0u8; width_sub_i32 as usize * 4];
    for i in 0..width_sub_i32 as usize {
      row[i * 4 + 3] = 255;
    }
    assert_eq!(subpixel_aa_lcd_filtered_alpha(&row, 4, width_sub_i32), 255);
  }

  #[test]
  fn subpixel_lcd_filter_matches_freetype_default_weights() {
    // With only the center subpixel fully covered, FreeType's default filter produces:
    // (255*112 + 128) >> 8 = 112.
    let width_sub_i32 = 7;
    let mut row = vec![0u8; width_sub_i32 as usize * 4];
    row[3 * 4 + 3] = 255;
    assert_eq!(subpixel_aa_lcd_filtered_alpha(&row, 3, width_sub_i32), 112);
  }

  #[test]
  fn subpixel_text_rasterization_uses_centered_subpixel_sampling() {
    // Regression test for LCD/subpixel AA alignment.
    //
    // When converting the 3×-upsampled alpha mask back into device pixels, each device pixel's
    // three samples must be centered on the device pixel center (`3*x-1`, `3*x`, `3*x+1` in the
    // upsampled grid). Sampling `3*x`, `3*x+1`, `3*x+2` shifts the effective coverage by 1/3 px and
    // produces asymmetric fringes on simple axis-aligned edges.
    let toggles = Arc::new(runtime::RuntimeToggles::from_map(HashMap::from([(
      "FASTR_TEXT_SUBPIXEL_AA".to_string(),
      "1".to_string(),
    )])));
    runtime::with_runtime_toggles(toggles, || {
      let mut rasterizer = TextRasterizer::new();
      assert!(rasterizer.subpixel_aa_enabled);

      let mut pixmap = Pixmap::new(16, 4).unwrap();
      pixmap.fill(tiny_skia::Color::from_rgba8(0, 0, 0, 255));

      // A wide white rectangle whose vertical edges pass through the centers of device pixels at
      // x=4 and x=12. Subpixel AA should therefore produce a monotonic channel ramp across each
      // edge: R<G<B on the left edge (coverage increasing to the right), and R>G>B on the right
      // edge.
      let rect = tiny_skia::Rect::from_xywh(4.0, -10.0, 8.0, 24.0).unwrap();
      let path = tiny_skia::PathBuilder::from_rect(rect);
      {
        let mut pixmap_mut = pixmap.as_mut();
        rasterizer
          .fill_path_subpixel_aa(
            &mut pixmap_mut,
            &path,
            Transform::identity(),
            1.0,
            Rgba::WHITE,
            None,
          )
          .unwrap();
      }

      let sample = |x: usize, y: usize| -> (u8, u8, u8) {
        let idx = (y * pixmap.width() as usize + x) * 4;
        (
          pixmap.data()[idx],
          pixmap.data()[idx + 1],
          pixmap.data()[idx + 2],
        )
      };

      // Sanity: interior pixel should be fully covered.
      assert_eq!(sample(8, 1), (255, 255, 255));

      let (r_l, g_l, b_l) = sample(4, 1);
      assert!(
        r_l < g_l && g_l < b_l,
        "expected left edge to ramp R<G<B, got ({r_l},{g_l},{b_l})"
      );

      let (r_r, g_r, b_r) = sample(12, 1);
      assert!(
        r_r > g_r && g_r > b_r,
        "expected right edge to ramp R>G>B, got ({r_r},{g_r},{b_r})"
      );
    });
  }

  #[test]
  fn hinting_is_disabled_for_rotated_runs_to_keep_outline_cache_reusable() {
    let font = match get_test_font() {
      Some(f) => f,
      None => return,
    };

    let face = font.as_ttf_face().unwrap();
    let Some(glyph_id) = face.glyph_index('H').map(|g| g.0 as u32) else {
      return;
    };

    let glyphs = [GlyphPosition {
      glyph_id,
      cluster: 0,
      x_offset: 0.0,
      y_offset: 0.0,
      x_advance: 0.0,
      y_advance: 0.0,
    }];

    let toggles = Arc::new(runtime::RuntimeToggles::from_map(HashMap::from([(
      "FASTR_TEXT_HINTING".to_string(),
      "1".to_string(),
    )])));
    runtime::with_runtime_toggles(toggles, || {
      let mut rasterizer = TextRasterizer::new();
      assert!(rasterizer.hinting_enabled);
      rasterizer.reset_cache_stats();

      let rotation = Some(Transform::from_rotate(25.0));

      let mut pixmap = new_pixmap(200, 120).unwrap();
      pixmap.fill(tiny_skia::Color::WHITE);
      rasterizer
        .render_glyph_run_with_stroke(
          &glyphs,
          &font,
          16.0,
          0.0,
          0.0,
          0,
          &[],
          0,
          &[],
          rotation,
          10.0,
          64.0,
          Rgba::BLACK,
          None,
          TextRenderState::default(),
          &mut pixmap,
        )
        .unwrap();

      let mut pixmap2 = new_pixmap(200, 120).unwrap();
      pixmap2.fill(tiny_skia::Color::WHITE);
      rasterizer
        .render_glyph_run_with_stroke(
          &glyphs,
          &font,
          32.0,
          0.0,
          0.0,
          0,
          &[],
          0,
          &[],
          rotation,
          10.0,
          64.0,
          Rgba::BLACK,
          None,
          TextRenderState::default(),
          &mut pixmap2,
        )
        .unwrap();

      let outline_stats = rasterizer
        .cache
        .lock()
        .map(|cache| cache.stats())
        .unwrap_or_default();
      assert_eq!(
        outline_stats.misses, 1,
        "expected hinting to be suppressed for rotated runs so outlines reuse across font sizes"
      );
    });
  }

  #[test]
  fn hinting_is_disabled_for_rotated_positioned_glyph_paths_to_keep_outline_cache_reusable() {
    let font = match get_test_font() {
      Some(f) => f,
      None => return,
    };

    let face = font.as_ttf_face().unwrap();
    let Some(glyph_id) = face.glyph_index('H').map(|g| g.0 as u32) else {
      return;
    };

    let glyphs = [GlyphPosition {
      glyph_id,
      cluster: 0,
      x_offset: 0.0,
      y_offset: 0.0,
      x_advance: 0.0,
      y_advance: 0.0,
    }];

    let toggles = Arc::new(runtime::RuntimeToggles::from_map(HashMap::from([(
      "FASTR_TEXT_HINTING".to_string(),
      "1".to_string(),
    )])));
    runtime::with_runtime_toggles(toggles, || {
      let mut rasterizer = TextRasterizer::new();
      assert!(rasterizer.hinting_enabled);
      rasterizer.reset_cache_stats();

      let rotation = Some(Transform::from_rotate(25.0));

      rasterizer
        .positioned_glyph_paths(&glyphs, &font, 16.0, 0.0, 0.0, 0.0, rotation, &[])
        .unwrap();
      rasterizer
        .positioned_glyph_paths(&glyphs, &font, 32.0, 0.0, 0.0, 0.0, rotation, &[])
        .unwrap();

      let outline_stats = rasterizer
        .cache
        .lock()
        .map(|cache| cache.stats())
        .unwrap_or_default();
      assert_eq!(
        outline_stats.misses, 1,
        "expected hinting to be suppressed for rotated positioned_glyph_paths so outlines reuse across font sizes"
      );
    });
  }
}
