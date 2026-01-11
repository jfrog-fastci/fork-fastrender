use crate::text::pipeline::ShapedRun;

const DEFAULT_FALLBACK_FONT_SIZE_PX: f32 = 16.0;
const FALLBACK_ADVANCE_PER_CHAR_MULTIPLIER: f32 = 0.6;

fn fallback_char_advance_px(runs: &[ShapedRun]) -> f32 {
  let font_size = runs
    .iter()
    .find_map(|run| (run.font_size.is_finite() && run.font_size > 0.0).then_some(run.font_size))
    .unwrap_or(DEFAULT_FALLBACK_FONT_SIZE_PX);
  (font_size * FALLBACK_ADVANCE_PER_CHAR_MULTIPLIER).max(0.0)
}

fn clamp_f32(value: f32, min: f32, max: f32) -> f32 {
  if !value.is_finite() {
    return min;
  }
  value.clamp(min, max)
}

fn char_boundary_byte_offsets(text: &str) -> Vec<usize> {
  let mut out: Vec<usize> = text.char_indices().map(|(idx, _)| idx).collect();
  out.push(text.len());
  out
}

fn byte_offset_for_char_idx(text: &str, char_idx: usize) -> Option<usize> {
  if char_idx == 0 {
    return Some(0);
  }
  let mut count = 0usize;
  for (byte_idx, _) in text.char_indices() {
    if count == char_idx {
      return Some(byte_idx);
    }
    count += 1;
  }
  if count == char_idx {
    return Some(text.len());
  }
  None
}

#[derive(Clone, Copy)]
struct RunVisualPosition<'a> {
  run: &'a ShapedRun,
  start_x: f32,
}

fn run_visual_positions(runs: &[ShapedRun]) -> Vec<RunVisualPosition<'_>> {
  let mut pen_x = 0.0f32;
  let mut out = Vec::with_capacity(runs.len());
  for run in runs {
    out.push(RunVisualPosition { run, start_x: pen_x });
    let advance = if run.advance.is_finite() { run.advance.max(0.0) } else { 0.0 };
    pen_x += advance;
  }
  out
}

fn prefix_width_for_local_byte(run: &ShapedRun, local_byte: usize) -> f32 {
  let mut width = 0.0f32;
  for glyph in &run.glyphs {
    if (glyph.cluster as usize) < local_byte {
      width += glyph.x_advance;
    }
  }
  if width.is_finite() { width.max(0.0) } else { 0.0 }
}

fn x_within_run_for_local_byte(run: &ShapedRun, local_byte: usize) -> f32 {
  let advance = if run.advance.is_finite() { run.advance.max(0.0) } else { 0.0 };
  let prefix = prefix_width_for_local_byte(run, local_byte);
  let mut x_local = if run.direction.is_rtl() {
    advance - prefix
  } else {
    prefix
  };
  x_local = clamp_f32(x_local, 0.0, advance);
  x_local
}

fn x_for_byte_offset(text: &str, runs: &[ShapedRun], byte_offset: usize) -> Option<f32> {
  let positions = run_visual_positions(runs);
  if positions.is_empty() {
    return None;
  }
  let text_len = text.len();
  let mut candidate_at_end: Option<RunVisualPosition<'_>> = None;
  for pos in positions {
    if byte_offset == text_len && pos.run.end == byte_offset {
      candidate_at_end = Some(pos);
    }
    if pos.run.start <= byte_offset && byte_offset < pos.run.end {
      let local_byte = byte_offset.saturating_sub(pos.run.start);
      let x = pos.start_x + x_within_run_for_local_byte(pos.run, local_byte);
      return Some(x);
    }
  }
  candidate_at_end.map(|pos| {
    let local_byte = byte_offset.saturating_sub(pos.run.start);
    pos.start_x + x_within_run_for_local_byte(pos.run, local_byte)
  })
}

/// Maps a logical caret boundary (character index in `text`) to an x-advance in local coordinates.
///
/// Returns `None` only when `char_idx` is out of range for `text`.
pub fn x_for_char_idx(text: &str, runs: &[ShapedRun], char_idx: usize) -> Option<f32> {
  let char_count = text.chars().count();
  if char_idx > char_count {
    return None;
  }

  if char_count == 0 {
    return Some(0.0);
  }

  if let Some(byte_offset) = byte_offset_for_char_idx(text, char_idx) {
    if let Some(x) = x_for_byte_offset(text, runs, byte_offset) {
      if x.is_finite() {
        return Some(x.max(0.0));
      }
    }
  }

  // Fallback: approximate fixed advance per character.
  let avg = fallback_char_advance_px(runs);
  Some(avg * char_idx as f32)
}

/// Maps an x coordinate (local) to the nearest caret boundary (character index).
///
/// Tie-breaking is stable: on equal distance, choose the visually-leftmost caret stop.
pub fn char_idx_for_x(text: &str, runs: &[ShapedRun], x: f32) -> usize {
  let char_count = text.chars().count();
  if char_count == 0 {
    return 0;
  }

  let x = if x.is_finite() { x } else { 0.0 };
  let avg = fallback_char_advance_px(runs);

  // When shaping is missing, approximate with fixed-width carets.
  if runs.is_empty() {
    if avg <= f32::EPSILON {
      return 0;
    }
    let idx = (x / avg).round().clamp(0.0, char_count as f32) as usize;
    return idx;
  }

  let boundaries = char_boundary_byte_offsets(text);
  let positions = run_visual_positions(runs);

  let mut best_idx = 0usize;
  let mut best_x = f32::INFINITY;
  let mut best_dist = f32::INFINITY;

  let mut seen_any = false;
  for (idx, &byte_offset) in boundaries.iter().enumerate() {
    let caret_x = x_for_byte_offset_with_positions(text.len(), &positions, byte_offset)
      .unwrap_or_else(|| avg * idx as f32);
    let dist = (caret_x - x).abs();
    if !seen_any
      || dist < best_dist - f32::EPSILON
      || ((dist - best_dist).abs() <= f32::EPSILON && (caret_x < best_x - f32::EPSILON))
      || ((dist - best_dist).abs() <= f32::EPSILON
        && (caret_x - best_x).abs() <= f32::EPSILON
        && idx < best_idx)
    {
      best_idx = idx.min(char_count);
      best_x = caret_x;
      best_dist = dist;
      seen_any = true;
    }
  }

  best_idx
}

fn x_for_byte_offset_with_positions(
  text_len: usize,
  positions: &[RunVisualPosition<'_>],
  byte_offset: usize,
) -> Option<f32> {
  let mut candidate_at_end: Option<RunVisualPosition<'_>> = None;
  for pos in positions {
    if byte_offset == text_len && pos.run.end == byte_offset {
      candidate_at_end = Some(*pos);
    }
    if pos.run.start <= byte_offset && byte_offset < pos.run.end {
      let local_byte = byte_offset.saturating_sub(pos.run.start);
      return Some(pos.start_x + x_within_run_for_local_byte(pos.run, local_byte));
    }
  }
  candidate_at_end.map(|pos| {
    let local_byte = byte_offset.saturating_sub(pos.run.start);
    pos.start_x + x_within_run_for_local_byte(pos.run, local_byte)
  })
}

/// Maps a logical selection range (`start..end` in character indices) to one or more visual
/// segments.
///
/// Returned segments are in local coordinates (0..total advance) and sorted in visual order.
pub fn selection_segments_for_char_range(
  text: &str,
  runs: &[ShapedRun],
  start: usize,
  end: usize,
) -> Vec<(f32, f32)> {
  let char_count = text.chars().count();
  let mut start = start.min(char_count);
  let mut end = end.min(char_count);
  if start > end {
    std::mem::swap(&mut start, &mut end);
  }
  if start == end {
    return Vec::new();
  }

  let avg = fallback_char_advance_px(runs);

  if runs.is_empty() {
    let x1 = avg * start as f32;
    let x2 = avg * end as f32;
    let left = x1.min(x2);
    let right = x1.max(x2);
    if right - left <= f32::EPSILON {
      return Vec::new();
    }
    return vec![(left, right)];
  }

  let boundaries = char_boundary_byte_offsets(text);
  let start_byte = *boundaries.get(start).unwrap_or(&0);
  let end_byte = *boundaries.get(end).unwrap_or(&text.len());

  let mut segments = Vec::new();
  let mut pen_x = 0.0f32;
  for run in runs {
    let run_start_x = pen_x;
    let run_advance = if run.advance.is_finite() { run.advance.max(0.0) } else { 0.0 };
    pen_x += run_advance;

    let overlap_start = start_byte.max(run.start);
    let overlap_end = end_byte.min(run.end);
    if overlap_start >= overlap_end {
      continue;
    }

    let local_start = overlap_start.saturating_sub(run.start);
    let local_end = overlap_end.saturating_sub(run.start);
    let x1 = run_start_x + x_within_run_for_local_byte(run, local_start);
    let x2 = run_start_x + x_within_run_for_local_byte(run, local_end);
    let left = x1.min(x2);
    let right = x1.max(x2);
    if right - left <= f32::EPSILON {
      continue;
    }
    segments.push((left, right));
  }

  segments
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::style::color::Rgba;
  use crate::text::font_db::{FontStretch, FontWeight, LoadedFont};
  use crate::text::pipeline::{Direction, GlyphPosition, ShapedRun};
  use rustybuzz::Feature;
  use std::sync::Arc;

  fn dummy_font() -> Arc<LoadedFont> {
    Arc::new(LoadedFont {
      id: None,
      data: Arc::new(Vec::new()),
      index: 0,
      face_metrics_overrides: Default::default(),
      face_settings: Default::default(),
      family: "Dummy".to_string(),
      weight: FontWeight::NORMAL,
      style: crate::text::font_db::FontStyle::Normal,
      stretch: FontStretch::Normal,
    })
  }

  fn run(
    text: &str,
    start: usize,
    end: usize,
    direction: Direction,
    glyph_adv: f32,
    glyph_clusters: Vec<u32>,
  ) -> ShapedRun {
    let glyphs = glyph_clusters
      .into_iter()
      .map(|cluster| GlyphPosition {
        glyph_id: 1,
        cluster,
        x_offset: 0.0,
        y_offset: 0.0,
        x_advance: glyph_adv,
        y_advance: 0.0,
      })
      .collect::<Vec<_>>();
    let advance = glyphs.iter().map(|g| g.x_advance).sum::<f32>();
    ShapedRun {
      text: text[start..end].to_string(),
      start,
      end,
      glyphs,
      direction,
      level: if direction.is_rtl() { 1 } else { 0 },
      advance,
      font: dummy_font(),
      font_size: 10.0,
      baseline_shift: 0.0,
      language: None,
      features: Arc::<[Feature]>::from(Vec::<Feature>::new().into_boxed_slice()),
      synthetic_bold: 0.0,
      synthetic_oblique: 0.0,
      rotation: crate::text::pipeline::RunRotation::None,
      palette_index: 0,
      palette_overrides: Arc::new(Vec::<(u16, Rgba)>::new()),
      palette_override_hash: 0,
      variations: Vec::new(),
      scale: 1.0,
    }
  }

  #[test]
  fn rtl_run_maps_logical_boundaries_to_visual_x() {
    let text = "abcd";
    // Simulate HarfBuzz output for RTL: glyph order reversed (clusters descending).
    let runs = vec![run(
      text,
      0,
      4,
      Direction::Rtl,
      10.0,
      vec![3, 2, 1, 0],
    )];

    assert_eq!(x_for_char_idx(text, &runs, 0), Some(40.0));
    assert_eq!(x_for_char_idx(text, &runs, 4), Some(0.0));
    assert_eq!(x_for_char_idx(text, &runs, 1), Some(30.0));
  }

  #[test]
  fn mixed_runs_map_non_monotonic_and_hit_test_round_trips() {
    let text = "abCD";
    let ltr = run(text, 0, 2, Direction::Ltr, 1.0, vec![0, 1]);
    let rtl = run(text, 2, 4, Direction::Rtl, 1.0, vec![1, 0]);
    let runs = vec![ltr, rtl];

    // Logical boundaries (0..=4) map to non-monotonic x positions due to the RTL run.
    let xs: Vec<f32> = (0..=4)
      .map(|idx| x_for_char_idx(text, &runs, idx).unwrap())
      .collect();
    assert_eq!(xs, vec![0.0, 1.0, 4.0, 3.0, 2.0]);

    assert_eq!(char_idx_for_x(text, &runs, 0.4), 0);
    assert_eq!(char_idx_for_x(text, &runs, 0.6), 1);
    assert_eq!(char_idx_for_x(text, &runs, 3.6), 2);
    assert_eq!(char_idx_for_x(text, &runs, 2.4), 4);
    assert_eq!(char_idx_for_x(text, &runs, 2.6), 3);
  }

  #[test]
  fn selection_segments_split_across_runs_in_visual_order() {
    let text = "abCD";
    // Visual order: RTL run first, then LTR run (start indices are non-monotonic).
    let rtl = run(text, 2, 4, Direction::Rtl, 1.0, vec![1, 0]);
    let ltr = run(text, 0, 2, Direction::Ltr, 1.0, vec![0, 1]);
    let runs = vec![rtl, ltr];

    // Select the logical range covering "bC" (char indices 1..3).
    let segments = selection_segments_for_char_range(text, &runs, 1, 3);
    // RTL run occupies x=[0,2], selects "C" -> segment (1,2). LTR run occupies x=[2,4],
    // selects "b" -> segment (3,4).
    assert_eq!(segments, vec![(1.0, 2.0), (3.0, 4.0)]);
  }
}

