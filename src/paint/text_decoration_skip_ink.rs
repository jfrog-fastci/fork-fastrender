use crate::text::font_db::LoadedFont;
use crate::text::font_instance::{FontBBox, FontInstance};
use crate::text::pipeline::{RunRotation, ShapedRun};
use lru::LruCache;
use rustc_hash::FxHasher;
use std::cell::RefCell;
use std::hash::BuildHasherDefault;
use std::num::NonZeroUsize;
use std::sync::Arc;
use tiny_skia::Transform;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct UnderlineGlyphBboxKey {
  font_ptr: usize,
  font_index: u32,
  variations_hash: u64,
  glyph_id: u32,
}

const UNDERLINE_GLYPH_BBOX_CACHE_MAX_ENTRIES: NonZeroUsize =
  // Avoid `.unwrap()` in production code (see `xtask lint-no-panics`).
  //
  // The fallback branch is unreachable because the literal is non-zero, but keep it anyway so this
  // constant can never panic, even if edited incorrectly in the future.
  match NonZeroUsize::new(8192) {
    Some(v) => v,
    None => NonZeroUsize::MIN,
  };
type UnderlineGlyphBboxCacheHasher = BuildHasherDefault<FxHasher>;

thread_local! {
  static UNDERLINE_GLYPH_BBOX_CACHE: RefCell<
    LruCache<UnderlineGlyphBboxKey, Option<FontBBox>, UnderlineGlyphBboxCacheHasher>
  > = RefCell::new(LruCache::with_hasher(
    UNDERLINE_GLYPH_BBOX_CACHE_MAX_ENTRIES,
    UnderlineGlyphBboxCacheHasher::default(),
  ));
}

#[inline]
fn cached_glyph_bounding_box(
  instance: &FontInstance<'_>,
  font: &LoadedFont,
  variations_hash: u64,
  glyph_id: u32,
) -> Option<FontBBox> {
  let key = UnderlineGlyphBboxKey {
    font_ptr: Arc::as_ptr(&font.data) as usize,
    font_index: font.index,
    variations_hash,
    glyph_id,
  };

  UNDERLINE_GLYPH_BBOX_CACHE.with(|cache| {
    let mut cache = cache.borrow_mut();
    if let Some(cached) = cache.get(&key) {
      return *cached;
    }
    let bbox = instance.glyph_bounds(glyph_id);
    cache.put(key, bbox);
    bbox
  })
}

#[inline]
fn rotation_transform(rotation: RunRotation, origin_x: f32, origin_y: f32) -> Option<Transform> {
  crate::paint::text_rasterize::rotation_transform(rotation, origin_x, origin_y)
}

#[inline]
fn safe_skew(skew: f32) -> f32 {
  if skew.is_finite() {
    skew
  } else {
    0.0
  }
}

#[inline]
fn safe_bold_pad(bold: f32, coord_scale: f32) -> f32 {
  if bold.is_finite() {
    bold.abs() * coord_scale
  } else {
    0.0
  }
}

#[inline]
fn transform_point(transform: Transform, x: f32, y: f32) -> (f32, f32) {
  (
    x * transform.sx + y * transform.kx + transform.tx,
    x * transform.ky + y * transform.sy + transform.ty,
  )
}

#[inline]
fn push_interval(intervals: &mut Vec<(f32, f32)>, start: f32, end: f32) {
  if start.is_finite() && end.is_finite() && end > start {
    intervals.push((start, end));
  }
}

#[inline]
fn char_looks_ideographic(ch: char) -> bool {
  let cp = ch as u32;
  // `text-decoration-skip-ink: auto` is defined to be UA-dependent and, in practice, browsers
  // treat underline carving differently across writing systems. In particular, applying
  // descender-style "ink skipping" heuristics to CJK text often produces huge visible gaps.
  //
  // To better match browser behaviour and avoid over-carving, treat common East Asian scripts as
  // ideographic and exempt them from skip-ink exclusions in `auto` mode.
  (0x3400..=0x4DBF).contains(&cp) // CJK Unified Ideographs Extension A
    || (0x4E00..=0x9FFF).contains(&cp) // CJK Unified Ideographs
    || (0x20000..=0x2EBEF).contains(&cp) // CJK Unified Ideographs Extensions B..F
    || (0x30000..=0x3134F).contains(&cp) // CJK Unified Ideographs Extension G
    || (0xF900..=0xFAFF).contains(&cp) // CJK Compatibility Ideographs
    || (0x2F800..=0x2FA1F).contains(&cp) // CJK Compatibility Ideographs Supplement
    || (0x3000..=0x303F).contains(&cp) // CJK Symbols and Punctuation
    || (0x3040..=0x30FF).contains(&cp) // Hiragana + Katakana
    || (0x31F0..=0x31FF).contains(&cp) // Katakana Phonetic Extensions
    || (0x3100..=0x312F).contains(&cp) // Bopomofo
    || (0x31A0..=0x31BF).contains(&cp) // Bopomofo Extended
    || (0x1100..=0x11FF).contains(&cp) // Hangul Jamo
    || (0x3130..=0x318F).contains(&cp) // Hangul Compatibility Jamo
    || (0xA960..=0xA97F).contains(&cp) // Hangul Jamo Extended-A
    || (0xAC00..=0xD7AF).contains(&cp) // Hangul Syllables
    || (0xD7B0..=0xD7FF).contains(&cp) // Hangul Jamo Extended-B
    || (0xFF01..=0xFFEF).contains(&cp) // Halfwidth and Fullwidth Forms
}

#[inline]
fn cluster_looks_ideographic(run: &ShapedRun, cluster: u32) -> bool {
  let idx = cluster as usize;
  if idx >= run.text.len() {
    return false;
  }
  if !run.text.is_char_boundary(idx) {
    return false;
  }
  run
    .text
    .get(idx..)
    .and_then(|tail| tail.chars().next())
    .is_some_and(char_looks_ideographic)
}

fn glyph_aabb(
  instance: &FontInstance<'_>,
  font: &LoadedFont,
  variations_hash: u64,
  glyph_id: u32,
  glyph_x: f32,
  glyph_y: f32,
  scale: f32,
  skew: f32,
  rotation: Option<Transform>,
  pad_x: f32,
  pad_y: f32,
) -> Option<(f32, f32, f32, f32)> {
  if !glyph_x.is_finite() || !glyph_y.is_finite() || !scale.is_finite() || scale == 0.0 {
    return None;
  }

  let bbox = cached_glyph_bounding_box(instance, font, variations_hash, glyph_id)?;

  let mut min_x = f32::INFINITY;
  let mut max_x = f32::NEG_INFINITY;
  let mut min_y = f32::INFINITY;
  let mut max_y = f32::NEG_INFINITY;

  let corners = [
    (bbox.x_min, bbox.y_min),
    (bbox.x_min, bbox.y_max),
    (bbox.x_max, bbox.y_min),
    (bbox.x_max, bbox.y_max),
  ];
  for (px, py) in corners {
    let mut x = glyph_x + (px + skew * py) * scale;
    let mut y = glyph_y - py * scale;
    if let Some(t) = rotation {
      (x, y) = transform_point(t, x, y);
    }
    min_x = min_x.min(x);
    max_x = max_x.max(x);
    min_y = min_y.min(y);
    max_y = max_y.max(y);
  }

  if !min_x.is_finite() || !min_y.is_finite() || !max_x.is_finite() || !max_y.is_finite() {
    return None;
  }

  min_x -= pad_x;
  max_x += pad_x;
  min_y -= pad_y;
  max_y += pad_y;
  Some((min_x, max_x, min_y, max_y))
}

/// Collect underline exclusion intervals for a horizontal underline.
///
/// All inputs/outputs are in the caller's coordinate space. Pass `coord_scale` to scale
/// glyph metrics (stored in CSS px) into that space:
/// - `coord_scale = 1.0` for CSS px (display-list builder)
/// - `coord_scale = dpr` for device px (legacy painter)
///
/// Use `extra_pad_px` to further inflate glyph bounds in the caller's coordinate space (e.g. to
/// account for `-webkit-text-stroke` ink; typically `stroke_width_px * 0.5 * coord_scale`).
pub(crate) fn collect_underline_exclusions(
  runs: &[ShapedRun],
  line_start: f32,
  baseline_y: f32,
  band_top: f32,
  band_bottom: f32,
  skip_all: bool,
  coord_scale: f32,
  extra_pad_px: f32,
) -> Vec<(f32, f32)> {
  let mut intervals = Vec::new();
  // Small inflation to account for antialiasing without swallowing the entire line.
  let tolerance = 0.5 * coord_scale;
  let extra_pad_px = if extra_pad_px.is_finite() {
    extra_pad_px.max(0.0)
  } else {
    0.0
  };

  let mut pen_x = line_start;
  for run in runs {
    let Some(instance) = FontInstance::new(&run.font, &run.variations) else {
      pen_x += run.advance * coord_scale;
      continue;
    };
    let units_per_em = instance.units_per_em();
    if units_per_em <= 0.0 || !units_per_em.is_finite() {
      pen_x += run.advance * coord_scale;
      continue;
    }

    // Match the text rasterizer: the run origin is the current pen position regardless of
    // direction. HarfBuzz encodes RTL advancement in the sign of `x_advance`/`run.advance`.
    let run_advance = run.advance * coord_scale;
    let origin_x = pen_x;
    let origin_y = baseline_y;
    let rotation = rotation_transform(run.rotation, origin_x, origin_y);
    let scale = run.font_size * run.scale * coord_scale / units_per_em;
    let skew = safe_skew(run.synthetic_oblique);
    let bold_pad = safe_bold_pad(run.synthetic_bold, coord_scale);
    let pad_x = bold_pad + tolerance + extra_pad_px;
    let pad_y = bold_pad + tolerance + extra_pad_px;
    let variations_hash = crate::text::variations::variation_hash(&run.variations);

    let mut cursor_x = origin_x;
    for glyph in &run.glyphs {
      if !skip_all && cluster_looks_ideographic(run, glyph.cluster) {
        cursor_x += glyph.x_advance * coord_scale;
        continue;
      }

      // Match `TextRasterizer::render_glyph_run` positioning (with y-axis inversion).
      let glyph_x = cursor_x + glyph.x_offset * coord_scale;
      let glyph_y = origin_y - glyph.y_offset * coord_scale;

      if let Some((min_x, max_x, min_y, max_y)) = glyph_aabb(
        &instance,
        &run.font,
        variations_hash,
        glyph.glyph_id,
        glyph_x,
        glyph_y,
        scale,
        skew,
        rotation,
        pad_x,
        pad_y,
      ) {
        if skip_all || (max_y >= band_top && min_y <= band_bottom) {
          push_interval(&mut intervals, min_x, max_x);
        }
      }

      cursor_x += glyph.x_advance * coord_scale;
    }

    pen_x += run_advance;
  }

  intervals
}

/// Collect underline exclusion intervals for a vertical underline (vertical writing modes).
///
/// Intervals are returned along the physical Y axis. The underline "band" is a vertical strip
/// between `band_left` and `band_right` in physical X.
///
/// Use `extra_pad_px` to further inflate glyph bounds in the caller's coordinate space (e.g. to
/// account for `-webkit-text-stroke` ink; typically `stroke_width_px * 0.5 * coord_scale`).
pub(crate) fn collect_underline_exclusions_vertical(
  runs: &[ShapedRun],
  inline_start: f32,
  block_baseline: f32,
  band_left: f32,
  band_right: f32,
  skip_all: bool,
  coord_scale: f32,
  extra_pad_px: f32,
) -> Vec<(f32, f32)> {
  let mut intervals = Vec::new();
  let tolerance = 0.5 * coord_scale;
  let extra_pad_px = if extra_pad_px.is_finite() {
    extra_pad_px.max(0.0)
  } else {
    0.0
  };

  let mut pen_inline = inline_start;
  for run in runs {
    let Some(instance) = FontInstance::new(&run.font, &run.variations) else {
      pen_inline += run.advance * coord_scale;
      continue;
    };
    let units_per_em = instance.units_per_em();
    if units_per_em <= 0.0 || !units_per_em.is_finite() {
      pen_inline += run.advance * coord_scale;
      continue;
    }

    // Match rasterization: vertical runs are always advanced using the shaped advances; do not
    // reinterpret bidi direction here (vertical shaping uses TopToBottom direction internally).
    let run_advance = run.advance * coord_scale;
    let origin_y = pen_inline;
    let origin_x = block_baseline;
    let rotation = rotation_transform(run.rotation, origin_x, origin_y);
    let scale = run.font_size * run.scale * coord_scale / units_per_em;
    let skew = safe_skew(run.synthetic_oblique);
    let bold_pad = safe_bold_pad(run.synthetic_bold, coord_scale);
    let pad_x = bold_pad + tolerance + extra_pad_px;
    let pad_y = bold_pad + tolerance + extra_pad_px;
    let variations_hash = crate::text::variations::variation_hash(&run.variations);

    let mut cursor_x = origin_x;
    let mut cursor_y = 0.0_f32;
    for glyph in &run.glyphs {
      if !skip_all && cluster_looks_ideographic(run, glyph.cluster) {
        cursor_x += glyph.x_advance * coord_scale;
        cursor_y += glyph.y_advance * coord_scale;
        continue;
      }

      // Match `TextRasterizer::render_glyph_run` positioning (with y-axis inversion).
      let glyph_x = cursor_x + glyph.x_offset * coord_scale;
      let glyph_y = origin_y + cursor_y - glyph.y_offset * coord_scale;

      if let Some((min_x, max_x, min_y, max_y)) = glyph_aabb(
        &instance,
        &run.font,
        variations_hash,
        glyph.glyph_id,
        glyph_x,
        glyph_y,
        scale,
        skew,
        rotation,
        pad_x,
        pad_y,
      ) {
        if skip_all || (max_x >= band_left && min_x <= band_right) {
          push_interval(&mut intervals, min_y, max_y);
        }
      }

      cursor_x += glyph.x_advance * coord_scale;
      cursor_y += glyph.y_advance * coord_scale;
    }

    pen_inline += run_advance;
  }

  intervals
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::text::face_cache;
  use crate::text::font_db::{
    FontFaceMetricsOverrides, FontStretch, FontStyle, FontWeight, LoadedFont,
  };
  use rustybuzz::Variation;
  use std::path::PathBuf;
  use std::sync::Arc;

  #[test]
  fn glyph_bbox_cache_separates_variations() {
    let font_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fonts/RobotoFlex-VF.ttf");
    let data = Arc::new(std::fs::read(font_path).expect("read test font"));
    let font = LoadedFont {
      id: None,
      data,
      index: 0,
      face_metrics_overrides: FontFaceMetricsOverrides::default(),
      face_settings: Default::default(),
      family: "RobotoFlex".to_string(),
      weight: FontWeight::NORMAL,
      style: FontStyle::Normal,
      stretch: FontStretch::Normal,
    };

    let cached_face = face_cache::get_ttf_face(&font).expect("parse test font");
    let face = cached_face.face();
    let glyph_id = face.glyph_index('A').expect("expected glyph for A").0 as u32;

    let variations_a = [Variation {
      tag: ttf_parser::Tag::from_bytes(b"wght"),
      value: 400.0,
    }];
    let variations_b = [Variation {
      tag: ttf_parser::Tag::from_bytes(b"wght"),
      value: 700.0,
    }];
    let hash_a = crate::text::variations::variation_hash(&variations_a);
    let hash_b = crate::text::variations::variation_hash(&variations_b);
    assert_ne!(hash_a, hash_b, "variation hash should depend on value");

    let instance_a = FontInstance::new(&font, &variations_a).expect("instance A");
    let instance_b = FontInstance::new(&font, &variations_b).expect("instance B");

    UNDERLINE_GLYPH_BBOX_CACHE.with(|cache| cache.borrow_mut().clear());
    let _ = cached_glyph_bounding_box(&instance_a, &font, hash_a, glyph_id);
    let _ = cached_glyph_bounding_box(&instance_b, &font, hash_b, glyph_id);
    let cache_len = UNDERLINE_GLYPH_BBOX_CACHE.with(|cache| cache.borrow().len());
    assert_eq!(
      cache_len, 2,
      "glyph bbox cache should key entries by variation hash"
    );
  }

  #[test]
  fn glyph_bbox_cache_is_bounded_lru() {
    let font_path =
      PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fonts/DejaVuSans-subset.ttf");
    let data = Arc::new(std::fs::read(font_path).expect("read test font"));
    let font = LoadedFont {
      id: None,
      data,
      index: 0,
      face_metrics_overrides: FontFaceMetricsOverrides::default(),
      face_settings: Default::default(),
      family: "DejaVu Sans Subset".to_string(),
      weight: FontWeight::NORMAL,
      style: FontStyle::Normal,
      stretch: FontStretch::Normal,
    };

    let instance = FontInstance::new(&font, &[]).expect("instance");
    let variations_hash = instance.variation_hash();
    let font_ptr = Arc::as_ptr(&font.data) as usize;

    UNDERLINE_GLYPH_BBOX_CACHE.with(|cache| cache.borrow_mut().clear());

    // Use out-of-range glyph IDs so `glyph_bounds` returns early without having to read glyph data.
    let base_glyph = u16::MAX as u32 + 1;
    for i in 0..UNDERLINE_GLYPH_BBOX_CACHE_MAX_ENTRIES.get() {
      let _ = cached_glyph_bounding_box(&instance, &font, variations_hash, base_glyph + i as u32);
    }

    // Touch the oldest entry so it becomes MRU.
    let _ = cached_glyph_bounding_box(&instance, &font, variations_hash, base_glyph);

    // Push one more entry; the cache should evict a single LRU entry rather than clearing all.
    let _ = cached_glyph_bounding_box(
      &instance,
      &font,
      variations_hash,
      base_glyph + UNDERLINE_GLYPH_BBOX_CACHE_MAX_ENTRIES.get() as u32,
    );

    let cache_len = UNDERLINE_GLYPH_BBOX_CACHE.with(|cache| cache.borrow().len());
    assert_eq!(
      cache_len,
      UNDERLINE_GLYPH_BBOX_CACHE_MAX_ENTRIES.get(),
      "glyph bbox cache should stay at capacity via incremental LRU eviction"
    );

    let key_recent = UnderlineGlyphBboxKey {
      font_ptr,
      font_index: font.index,
      variations_hash,
      glyph_id: base_glyph,
    };
    let key_evicted = UnderlineGlyphBboxKey {
      glyph_id: base_glyph + 1,
      ..key_recent
    };

    UNDERLINE_GLYPH_BBOX_CACHE.with(|cache| {
      let mut cache = cache.borrow_mut();
      assert!(
        cache.get(&key_recent).is_some(),
        "recently used entry should remain cached after overflow"
      );
      assert!(
        cache.get(&key_evicted).is_none(),
        "least recently used entry should be evicted on overflow"
      );
    });
  }

  #[test]
  fn underline_exclusions_apply_rotation_transform() {
    let font_path =
      PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fonts/DejaVuSans-subset.ttf");
    let data = Arc::new(std::fs::read(font_path).expect("read test font"));
    let font = LoadedFont {
      id: None,
      data,
      index: 0,
      face_metrics_overrides: FontFaceMetricsOverrides::default(),
      face_settings: Default::default(),
      family: "DejaVu Sans Subset".to_string(),
      weight: FontWeight::NORMAL,
      style: FontStyle::Normal,
      stretch: FontStretch::Normal,
    };

    let cached_face = face_cache::get_ttf_face(&font).expect("parse test font");
    let face = cached_face.face();
    // `DejaVuSans-subset.ttf` only contains a small subset of glyphs; pick one by glyph ID rather
    // than relying on a particular Unicode mapping being present.
    let glyph_id = {
      let mut best = None;
      let mut best_diff = 0_i32;
      for gid in 0..face.number_of_glyphs() {
        let Some(bbox) = face.glyph_bounding_box(ttf_parser::GlyphId(gid)) else {
          continue;
        };
        let w = bbox.x_max as i32 - bbox.x_min as i32;
        let h = bbox.y_max as i32 - bbox.y_min as i32;
        let diff = (w - h).abs();
        if diff > best_diff {
          best_diff = diff;
          best = Some(gid);
        }
      }
      best.expect("expected glyph bbox in test font") as u32
    };

    let mk_run = |rotation: RunRotation| ShapedRun {
      text: "x".to_string(),
      start: 0,
      end: 1,
      glyphs: vec![crate::text::pipeline::GlyphPosition {
        glyph_id,
        cluster: 0,
        x_offset: 0.0,
        y_offset: 0.0,
        x_advance: 0.0,
        y_advance: 0.0,
      }],
      direction: crate::text::pipeline::Direction::LeftToRight,
      level: 0,
      advance: 0.0,
      font: Arc::new(font.clone()),
      font_size: 100.0,
      baseline_shift: 0.0,
      language: None,
      features: Arc::from(Vec::new()),
      synthetic_bold: 0.0,
      synthetic_oblique: 0.0,
      rotation,
      vertical: false,
      palette_index: 0,
      palette_overrides: Arc::new(Vec::new()),
      palette_override_hash: 0,
      variations: Vec::new(),
      scale: 1.0,
    };

    let runs_no = [mk_run(RunRotation::None)];
    let runs_rot = [mk_run(RunRotation::Cw90)];

    let excl_no = collect_underline_exclusions(&runs_no, 0.0, 0.0, -1000.0, 1000.0, true, 1.0, 0.0);
    let excl_rot =
      collect_underline_exclusions(&runs_rot, 0.0, 0.0, -1000.0, 1000.0, true, 1.0, 0.0);

    assert_eq!(excl_no.len(), 1);
    assert_eq!(excl_rot.len(), 1);
    let width_no = excl_no[0].1 - excl_no[0].0;
    let width_rot = excl_rot[0].1 - excl_rot[0].0;
    assert!(
      (width_no - width_rot).abs() > 0.1,
      "rotated glyph bbox should change exclusion width (got {} vs {})",
      width_no,
      width_rot
    );
  }

  #[test]
  fn underline_exclusions_expand_with_extra_padding() {
    let font_path =
      PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fonts/DejaVuSans-subset.ttf");
    let data = Arc::new(std::fs::read(font_path).expect("read test font"));
    let font = LoadedFont {
      id: None,
      data,
      index: 0,
      face_metrics_overrides: FontFaceMetricsOverrides::default(),
      face_settings: Default::default(),
      family: "DejaVu Sans Subset".to_string(),
      weight: FontWeight::NORMAL,
      style: FontStyle::Normal,
      stretch: FontStretch::Normal,
    };

    let cached_face = face_cache::get_ttf_face(&font).expect("parse test font");
    let face = cached_face.face();
    let glyph_id = (0..face.number_of_glyphs())
      .find(|gid| face.glyph_bounding_box(ttf_parser::GlyphId(*gid)).is_some())
      .expect("expected glyph bbox in test font") as u32;

    let run = ShapedRun {
      text: "x".to_string(),
      start: 0,
      end: 1,
      glyphs: vec![crate::text::pipeline::GlyphPosition {
        glyph_id,
        cluster: 0,
        x_offset: 0.0,
        y_offset: 0.0,
        x_advance: 0.0,
        y_advance: 0.0,
      }],
      direction: crate::text::pipeline::Direction::LeftToRight,
      level: 0,
      advance: 0.0,
      font: Arc::new(font),
      font_size: 40.0,
      baseline_shift: 0.0,
      language: None,
      features: Arc::from(Vec::new()),
      synthetic_bold: 0.0,
      synthetic_oblique: 0.0,
      rotation: RunRotation::None,
      vertical: false,
      palette_index: 0,
      palette_overrides: Arc::new(Vec::new()),
      palette_override_hash: 0,
      variations: Vec::new(),
      scale: 1.0,
    };

    let no_pad =
      collect_underline_exclusions(&[run.clone()], 0.0, 0.0, -1000.0, 1000.0, true, 1.0, 0.0);
    let with_pad = collect_underline_exclusions(&[run], 0.0, 0.0, -1000.0, 1000.0, true, 1.0, 6.0);

    assert_eq!(no_pad.len(), 1);
    assert_eq!(with_pad.len(), 1);
    let width_no = no_pad[0].1 - no_pad[0].0;
    let width_pad = with_pad[0].1 - with_pad[0].0;
    assert!(
      width_pad > width_no + 1.0,
      "expected exclusion width to grow with padding (no_pad={}, with_pad={})",
      width_no,
      width_pad
    );
  }
}
