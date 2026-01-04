use crate::text::font_db::LoadedFont;
use crate::text::pipeline::{RunRotation, ShapedRun};
use rustc_hash::FxHashMap;
use std::cell::RefCell;
use std::sync::Arc;
use tiny_skia::Transform;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct UnderlineGlyphBboxKey {
  font_ptr: usize,
  font_index: u32,
  variations_hash: u64,
  glyph_id: u16,
}

const UNDERLINE_GLYPH_BBOX_CACHE_MAX_ENTRIES: usize = 8192;

thread_local! {
  static UNDERLINE_GLYPH_BBOX_CACHE: RefCell<FxHashMap<UnderlineGlyphBboxKey, Option<ttf_parser::Rect>>> =
    RefCell::new(FxHashMap::default());
}

#[inline]
fn cached_glyph_bounding_box(
  face: &ttf_parser::Face<'static>,
  font: &LoadedFont,
  variations_hash: u64,
  glyph_id: u16,
) -> Option<ttf_parser::Rect> {
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
    let bbox = face.glyph_bounding_box(ttf_parser::GlyphId(glyph_id));
    if cache.len() >= UNDERLINE_GLYPH_BBOX_CACHE_MAX_ENTRIES {
      cache.clear();
    }
    cache.insert(key, bbox);
    bbox
  })
}

#[inline]
fn rotation_transform(rotation: RunRotation, origin_x: f32, origin_y: f32) -> Option<Transform> {
  crate::paint::text_rasterize::rotation_transform(rotation, origin_x, origin_y)
}

#[inline]
fn safe_skew(skew: f32) -> f32 {
  if skew.is_finite() { skew } else { 0.0 }
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

fn glyph_aabb(
  face: &ttf_parser::Face<'static>,
  font: &LoadedFont,
  variations_hash: u64,
  glyph_id: u16,
  glyph_x: f32,
  glyph_y: f32,
  scale: f32,
  skew: f32,
  rotation: Option<Transform>,
  pad_x: f32,
  pad_y: f32,
  fallback_advance_x: f32,
  ascent: f32,
  descent: f32,
) -> Option<(f32, f32, f32, f32)> {
  if !glyph_x.is_finite() || !glyph_y.is_finite() || !scale.is_finite() || scale == 0.0 {
    return None;
  }

  let bbox = cached_glyph_bounding_box(face, font, variations_hash, glyph_id);

  let mut min_x = f32::INFINITY;
  let mut max_x = f32::NEG_INFINITY;
  let mut min_y = f32::INFINITY;
  let mut max_y = f32::NEG_INFINITY;

  if let Some(bbox) = bbox {
    let corners = [
      (bbox.x_min as f32, bbox.y_min as f32),
      (bbox.x_min as f32, bbox.y_max as f32),
      (bbox.x_max as f32, bbox.y_min as f32),
      (bbox.x_max as f32, bbox.y_max as f32),
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
  } else {
    // Conservative fallback when no bounding box is available (color fonts, missing glyphs, etc.).
    let x0 = glyph_x.min(glyph_x + fallback_advance_x);
    let x1 = glyph_x.max(glyph_x + fallback_advance_x);
    let y0 = glyph_y - ascent;
    let y1 = glyph_y + descent;
    let corners = [(x0, y0), (x0, y1), (x1, y0), (x1, y1)];
    for (mut x, mut y) in corners {
      if let Some(t) = rotation {
        (x, y) = transform_point(t, x, y);
      }
      min_x = min_x.min(x);
      max_x = max_x.max(x);
      min_y = min_y.min(y);
      max_y = max_y.max(y);
    }
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
pub(crate) fn collect_underline_exclusions(
  runs: &[ShapedRun],
  line_start: f32,
  baseline_y: f32,
  band_top: f32,
  band_bottom: f32,
  skip_all: bool,
  coord_scale: f32,
) -> Vec<(f32, f32)> {
  let mut intervals = Vec::new();
  // Small inflation to account for antialiasing without swallowing the entire line.
  let tolerance = 0.5 * coord_scale;

  let mut pen_x = line_start;
  for run in runs {
    let Some(cached_face) = crate::text::face_cache::get_ttf_face(&run.font) else {
      pen_x += run.advance * coord_scale;
      continue;
    };
    let owned_face = (!run.variations.is_empty()).then(|| {
      let mut face = cached_face.clone_face();
      crate::text::apply_rustybuzz_variations(&mut face, &run.variations);
      face
    });
    let face: &ttf_parser::Face<'static> = owned_face.as_ref().unwrap_or_else(|| cached_face.face());
    let units_per_em = face.units_per_em() as f32;
    if units_per_em <= 0.0 || !units_per_em.is_finite() {
      pen_x += run.advance * coord_scale;
      continue;
    }

    let run_advance = run.advance * coord_scale;
    let origin_x = if run.direction.is_rtl() {
      pen_x + run_advance
    } else {
      pen_x
    };
    let origin_y = baseline_y;
    let rotation = rotation_transform(run.rotation, origin_x, origin_y);
    let scale = run.font_size * run.scale * coord_scale / units_per_em;
    let skew = safe_skew(run.synthetic_oblique);
    let bold_pad = safe_bold_pad(run.synthetic_bold, coord_scale);
    let pad_x = bold_pad + tolerance;
    let pad_y = bold_pad + tolerance;
    let variations_hash = crate::text::variations::variation_hash(&run.variations);

    // Match the painter/backend behavior: RTL runs advance in the negative inline direction.
    let dir_sign = if run.direction.is_rtl() { -1.0 } else { 1.0 };
    let mut cursor_x = origin_x;
    for glyph in &run.glyphs {
      let glyph_x = cursor_x + dir_sign * glyph.x_offset * coord_scale;
      let glyph_y = origin_y - glyph.y_offset * coord_scale;
      let fallback_advance_x = dir_sign * glyph.x_advance * coord_scale;
      let effective_font_size = run.font_size * run.scale * coord_scale;
      let ascent = effective_font_size;
      let descent = effective_font_size * 0.25;

      if let Ok(glyph_id) = u16::try_from(glyph.glyph_id) {
        if let Some((min_x, max_x, min_y, max_y)) = glyph_aabb(
          face,
          &run.font,
          variations_hash,
          glyph_id,
          glyph_x,
          glyph_y,
          scale,
          skew,
          rotation,
          pad_x,
          pad_y,
          fallback_advance_x,
          ascent,
          descent,
        ) {
          if skip_all || (max_y >= band_top && min_y <= band_bottom) {
            push_interval(&mut intervals, min_x, max_x);
          }
        }
      }

      cursor_x += dir_sign * glyph.x_advance * coord_scale;
    }

    pen_x += run_advance;
  }

  intervals
}

/// Collect underline exclusion intervals for a vertical underline (vertical writing modes).
///
/// Intervals are returned along the physical Y axis. The underline "band" is a vertical strip
/// between `band_left` and `band_right` in physical X.
pub(crate) fn collect_underline_exclusions_vertical(
  runs: &[ShapedRun],
  inline_start: f32,
  block_baseline: f32,
  band_left: f32,
  band_right: f32,
  skip_all: bool,
  coord_scale: f32,
) -> Vec<(f32, f32)> {
  let mut intervals = Vec::new();
  let tolerance = 0.5 * coord_scale;

  let mut pen_inline = inline_start;
  for run in runs {
    let Some(cached_face) = crate::text::face_cache::get_ttf_face(&run.font) else {
      pen_inline += run.advance * coord_scale;
      continue;
    };
    let owned_face = (!run.variations.is_empty()).then(|| {
      let mut face = cached_face.clone_face();
      crate::text::apply_rustybuzz_variations(&mut face, &run.variations);
      face
    });
    let face: &ttf_parser::Face<'static> = owned_face.as_ref().unwrap_or_else(|| cached_face.face());
    let units_per_em = face.units_per_em() as f32;
    if units_per_em <= 0.0 || !units_per_em.is_finite() {
      pen_inline += run.advance * coord_scale;
      continue;
    }

    let run_advance = run.advance * coord_scale;
    let origin_y = if run.direction.is_rtl() {
      pen_inline + run_advance
    } else {
      pen_inline
    };
    let origin_x = block_baseline;
    let rotation = rotation_transform(run.rotation, origin_x, origin_y);
    let scale = run.font_size * run.scale * coord_scale / units_per_em;
    let skew = safe_skew(run.synthetic_oblique);
    let bold_pad = safe_bold_pad(run.synthetic_bold, coord_scale);
    let pad_x = bold_pad + tolerance;
    let pad_y = bold_pad + tolerance;
    let variations_hash = crate::text::variations::variation_hash(&run.variations);

    let dir_sign = if run.direction.is_rtl() { -1.0 } else { 1.0 };
    let mut cursor_y = 0.0_f32;
    let mut cursor_x = 0.0_f32;
    for glyph in &run.glyphs {
      // Match display-list rendering: vertical text runs are positioned with the run origin at
      // (block_baseline, inline_origin) and then advanced by the glyph's x/y advances.
      //
      // `y_offset` is in the opposite sign convention to tiny-skia's coordinate system, so it is
      // subtracted to match `GlyphInstance { y_offset: -glyph.y_offset }`.
      let glyph_x = origin_x + cursor_x + glyph.x_offset * coord_scale;
      let glyph_y = origin_y + cursor_y - glyph.y_offset * coord_scale;
      let fallback_advance_x = glyph.x_advance * coord_scale;
      let effective_font_size = run.font_size * run.scale * coord_scale;
      let ascent = effective_font_size;
      let descent = effective_font_size * 0.25;

      if let Ok(glyph_id) = u16::try_from(glyph.glyph_id) {
        if let Some((min_x, max_x, min_y, max_y)) = glyph_aabb(
          face,
          &run.font,
          variations_hash,
          glyph_id,
          glyph_x,
          glyph_y,
          scale,
          skew,
          rotation,
          pad_x,
          pad_y,
          fallback_advance_x,
          ascent,
          descent,
        ) {
          if skip_all || (max_x >= band_left && min_x <= band_right) {
            push_interval(&mut intervals, min_y, max_y);
          }
        }
      }

      cursor_x += dir_sign * glyph.x_advance * coord_scale;
      cursor_y += dir_sign * glyph.y_advance * coord_scale;
    }

    pen_inline += run_advance;
  }

  intervals
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::text::face_cache;
  use crate::text::font_db::{FontFaceMetricsOverrides, FontStretch, FontStyle, FontWeight, LoadedFont};
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
      family: "RobotoFlex".to_string(),
      weight: FontWeight::NORMAL,
      style: FontStyle::Normal,
      stretch: FontStretch::Normal,
    };

    let cached_face = face_cache::get_ttf_face(&font).expect("parse test font");
    let face = cached_face.face();
    let glyph_id = face
      .glyph_index('A')
      .expect("expected glyph for A")
      .0;

    let hash_a = crate::text::variations::variation_hash(&[Variation {
      tag: ttf_parser::Tag::from_bytes(b"wght"),
      value: 400.0,
    }]);
    let hash_b = crate::text::variations::variation_hash(&[Variation {
      tag: ttf_parser::Tag::from_bytes(b"wght"),
      value: 700.0,
    }]);
    assert_ne!(hash_a, hash_b, "variation hash should depend on value");

    UNDERLINE_GLYPH_BBOX_CACHE.with(|cache| cache.borrow_mut().clear());
    let _ = cached_glyph_bounding_box(face, &font, hash_a, glyph_id);
    let _ = cached_glyph_bounding_box(face, &font, hash_b, glyph_id);
    let cache_len = UNDERLINE_GLYPH_BBOX_CACHE.with(|cache| cache.borrow().len());
    assert_eq!(
      cache_len, 2,
      "glyph bbox cache should key entries by variation hash"
    );
  }

  #[test]
  fn underline_exclusions_apply_rotation_transform() {
    let font_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
      .join("tests/fixtures/fonts/DejaVuSans-subset.ttf");
    let data = Arc::new(std::fs::read(font_path).expect("read test font"));
    let font = LoadedFont {
      id: None,
      data,
      index: 0,
      face_metrics_overrides: FontFaceMetricsOverrides::default(),
      family: "DejaVu Sans Subset".to_string(),
      weight: FontWeight::NORMAL,
      style: FontStyle::Normal,
      stretch: FontStretch::Normal,
    };

    let cached_face = face_cache::get_ttf_face(&font).expect("parse test font");
    let face = cached_face.face();
    let glyph_id = face
      .glyph_index('W')
      .or_else(|| face.glyph_index('A'))
      .expect("expected glyph in test font")
      .0 as u32;

    let mk_run = |rotation: RunRotation| ShapedRun {
      text: "W".to_string(),
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
      font_size: 20.0,
      baseline_shift: 0.0,
      language: None,
      synthetic_bold: 0.0,
      synthetic_oblique: 0.0,
      rotation,
      palette_index: 0,
      palette_overrides: Arc::new(Vec::new()),
      palette_override_hash: 0,
      variations: Vec::new(),
      scale: 1.0,
    };

    let runs_no = [mk_run(RunRotation::None)];
    let runs_rot = [mk_run(RunRotation::Cw90)];

    let excl_no = collect_underline_exclusions(&runs_no, 0.0, 0.0, -1000.0, 1000.0, true, 1.0);
    let excl_rot = collect_underline_exclusions(&runs_rot, 0.0, 0.0, -1000.0, 1000.0, true, 1.0);

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
}
