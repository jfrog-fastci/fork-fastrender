//! Caret positioning utilities for shaped text.
//!
//! FastRender shapes text into bidi-reordered [`ShapedRun`]s. Mapping between logical character
//! boundaries (character indices) and visual x positions is not always one-to-one:
//!
//! - Within a single run, a logical caret boundary maps to a single visual x.
//! - At LTR/RTL run boundaries a single logical boundary can have **two** valid visual caret
//!   positions (a "split caret"), one attached to the upstream run and one attached to the
//!   downstream run.
//!
//! This module provides:
//!
//! - `x_for_char_idx` / `char_idx_for_x` / `selection_segments_for_char_range`: helpers that pick a
//!   single x for a given logical boundary.
//! - `CaretAffinity` + `CaretStop` + `caret_stops_for_runs`: a full visual caret stop list that can
//!   represent split caret boundaries.

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

fn run_advance(run: &ShapedRun) -> f32 {
  if run.advance.is_finite() {
    run.advance.max(0.0)
  } else {
    0.0
  }
}

fn char_boundary_byte_offsets(text: &str) -> Vec<usize> {
  // `char_indices` already yields the 0 boundary for non-empty strings.
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
    out.push(RunVisualPosition {
      run,
      start_x: pen_x,
    });
    pen_x += run_advance(run);
  }
  out
}

fn x_within_run_for_local_byte(run: &ShapedRun, local_byte: usize) -> f32 {
  let advance = run_advance(run);
  let local_byte = local_byte.min(run.end.saturating_sub(run.start));

  if run.glyphs.is_empty() {
    let x = if run.direction.is_rtl() {
      if local_byte == 0 {
        advance
      } else {
        0.0
      }
    } else {
      if local_byte == 0 {
        0.0
      } else {
        advance
      }
    };
    return clamp_f32(x, 0.0, advance);
  }

  let mut x = 0.0f32;
  if run.direction.is_rtl() {
    // HarfBuzz emits RTL runs in visual order (left-to-right) with clusters in descending order.
    for glyph in &run.glyphs {
      let cluster = glyph.cluster as usize;
      if cluster < local_byte {
        break;
      }
      x += glyph.x_advance;
    }
  } else {
    // LTR: clusters are non-decreasing.
    for glyph in &run.glyphs {
      let cluster = glyph.cluster as usize;
      if cluster >= local_byte {
        break;
      }
      x += glyph.x_advance;
    }
  }

  if !x.is_finite() {
    x = 0.0;
  }
  clamp_f32(x, 0.0, advance)
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
    let run_advance = run_advance(run);
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

/// Which side of a logical caret boundary the caret is visually associated with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CaretAffinity {
  /// The caret is associated with the text *before* the boundary.
  Upstream,
  /// The caret is associated with the text *after* the boundary.
  Downstream,
}

impl Default for CaretAffinity {
  fn default() -> Self {
    Self::Downstream
  }
}

/// A single caret stop in visual (x) order.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CaretStop {
  /// Character boundary index (Unicode scalar value index, not bytes).
  pub char_idx: usize,
  /// X position in CSS px relative to the start of the shaped line (0 = left edge).
  pub x: f32,
  /// Affinity for this caret stop.
  pub affinity: CaretAffinity,
}

fn char_idx_for_byte(boundaries: &[usize], byte_idx: usize) -> usize {
  match boundaries.binary_search(&byte_idx) {
    Ok(idx) => idx,
    Err(idx) => idx,
  }
}

fn x_for_byte_in_run(run: &ShapedRun, target_byte: usize) -> f32 {
  let target_byte = target_byte.clamp(run.start, run.end);
  let local_byte = target_byte.saturating_sub(run.start);
  x_within_run_for_local_byte(run, local_byte)
}

/// Build a list of visual caret stops from shaped runs.
///
/// The returned stops are ordered in the same visual order as the shaped runs: left-to-right in
/// physical x coordinates. Stops can share the same `char_idx` when a logical boundary has two
/// distinct visual positions (split caret).
pub fn caret_stops_for_runs(
  text: &str,
  runs: &[ShapedRun],
  fallback_advance: f32,
) -> Vec<CaretStop> {
  let boundaries = char_boundary_byte_offsets(text);
  let char_count = boundaries.len().saturating_sub(1);
  if char_count == 0 {
    return vec![CaretStop {
      char_idx: 0,
      x: 0.0,
      affinity: CaretAffinity::Downstream,
    }];
  }

  if runs.is_empty() {
    // No shaping: fall back to uniform advance across the available width.
    let total = if fallback_advance.is_finite() {
      fallback_advance.max(0.0)
    } else {
      0.0
    };
    let avg = if char_count > 0 {
      total / char_count as f32
    } else {
      0.0
    };
    return (0..=char_count)
      .map(|char_idx| CaretStop {
        char_idx,
        x: avg * char_idx as f32,
        affinity: if char_idx == char_count {
          CaretAffinity::Upstream
        } else {
          CaretAffinity::Downstream
        },
      })
      .collect();
  }

  let mut out: Vec<CaretStop> = Vec::new();
  let mut run_origin_x = 0.0f32;

  for run in runs {
    let start_char = char_idx_for_byte(&boundaries, run.start);
    let end_char = char_idx_for_byte(&boundaries, run.end);
    let (min_char, max_char) = if start_char <= end_char {
      (start_char, end_char)
    } else {
      (end_char, start_char)
    };

    let mut push_boundary = |char_idx: usize, x_local: f32, is_end: bool| {
      let affinity = if is_end {
        CaretAffinity::Upstream
      } else {
        CaretAffinity::Downstream
      };
      let x = run_origin_x + x_local;
      if let Some(prev) = out.last() {
        if prev.char_idx == char_idx && (prev.x - x).abs() <= 1e-3 {
          // Collapse identical duplicate stops (e.g. font fallback splitting a run without changing
          // direction/level). Keep the existing stop to preserve stable ordering.
          return;
        }
      }
      out.push(CaretStop {
        char_idx,
        x,
        affinity,
      });
    };

    if run.direction.is_rtl() {
      for char_idx in (min_char..=max_char).rev() {
        let byte_idx = boundaries
          .get(char_idx)
          .copied()
          .unwrap_or(run.end)
          .clamp(run.start, run.end);
        let x_local = x_for_byte_in_run(run, byte_idx);
        push_boundary(char_idx, x_local, byte_idx == run.end);
      }
    } else {
      for char_idx in min_char..=max_char {
        let byte_idx = boundaries
          .get(char_idx)
          .copied()
          .unwrap_or(run.end)
          .clamp(run.start, run.end);
        let x_local = x_for_byte_in_run(run, byte_idx);
        push_boundary(char_idx, x_local, byte_idx == run.end);
      }
    }

    run_origin_x += run_advance(run);
  }

  out
}

/// Find the caret stop index matching the given logical caret position.
///
/// This prefers an exact `(char_idx, affinity)` match. If none exists (e.g. callers provide a
/// default affinity at a boundary that only has an upstream stop), it falls back to a downstream
/// stop, then to the first stop with the same `char_idx`.
pub fn caret_stop_index(
  stops: &[CaretStop],
  char_idx: usize,
  affinity: CaretAffinity,
) -> Option<usize> {
  if stops.is_empty() {
    return None;
  }

  let mut fallback_downstream: Option<usize> = None;
  let mut fallback_any: Option<usize> = None;

  for (idx, stop) in stops.iter().enumerate() {
    if stop.char_idx != char_idx {
      continue;
    }
    fallback_any.get_or_insert(idx);
    if stop.affinity == CaretAffinity::Downstream {
      fallback_downstream.get_or_insert(idx);
    }
    if stop.affinity == affinity {
      return Some(idx);
    }
  }

  fallback_downstream.or(fallback_any)
}

/// Resolve an x coordinate for the given caret position.
pub fn caret_x_for_position(
  stops: &[CaretStop],
  char_idx: usize,
  affinity: CaretAffinity,
) -> Option<f32> {
  let idx = caret_stop_index(stops, char_idx, affinity)?;
  Some(stops[idx].x)
}

#[cfg(test)]
mod tests {
  use super::*;

  use crate::style::color::Rgba;
  use crate::text::font_db::{FontStretch, FontWeight, LoadedFont};
  use crate::text::pipeline::{Direction, GlyphPosition, RunRotation, ShapedRun};
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
      rotation: RunRotation::None,
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
      Direction::RightToLeft,
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
    let ltr = run(text, 0, 2, Direction::LeftToRight, 1.0, vec![0, 1]);
    let rtl = run(text, 2, 4, Direction::RightToLeft, 1.0, vec![1, 0]);
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
    let rtl = run(text, 2, 4, Direction::RightToLeft, 1.0, vec![1, 0]);
    let ltr = run(text, 0, 2, Direction::LeftToRight, 1.0, vec![0, 1]);
    let runs = vec![rtl, ltr];

    // Select the logical range covering "bC" (char indices 1..3).
    let segments = selection_segments_for_char_range(text, &runs, 1, 3);
    // RTL run occupies x=[0,2], selects "C" -> segment (1,2). LTR run occupies x=[2,4],
    // selects "b" -> segment (3,4).
    assert_eq!(segments, vec![(1.0, 2.0), (3.0, 4.0)]);
  }

  #[test]
  fn caret_stops_for_empty_text_returns_single_stop() {
    let stops = caret_stops_for_runs("", &[], 0.0);
    assert_eq!(stops.len(), 1);
    assert_eq!(
      stops[0],
      CaretStop {
        char_idx: 0,
        x: 0.0,
        affinity: CaretAffinity::Downstream,
      }
    );
  }

  #[test]
  fn caret_stops_include_split_caret_for_mixed_direction_text() {
    use crate::style::ComputedStyle;
    use crate::text::font_loader::FontContext;
    use crate::text::pipeline::ShapingPipeline;

    let text = "ABC אבג";
    let style = ComputedStyle::default();
    let ctx = FontContext::new();
    let runs = ShapingPipeline::new()
      .shape(text, &style, &ctx)
      .expect("shape");
    let stops = caret_stops_for_runs(text, &runs, 0.0);

    let split: Vec<&CaretStop> = stops.iter().filter(|s| s.char_idx == 4).collect();
    assert!(
      split.len() >= 2,
      "expected split caret stops at boundary char_idx=4; stops={:?}",
      stops
    );

    let distinct_x: std::collections::HashSet<i32> =
      split.iter().map(|s| (s.x * 10.0).round() as i32).collect();
    assert!(
      distinct_x.len() >= 2,
      "expected split caret stops to have different x positions; split={split:?}"
    );
  }

  #[test]
  fn caret_stop_navigation_traverses_both_affinities() {
    use crate::style::ComputedStyle;
    use crate::text::font_loader::FontContext;
    use crate::text::pipeline::ShapingPipeline;

    let text = "ABC אבג";
    let style = ComputedStyle::default();
    let ctx = FontContext::new();
    let runs = ShapingPipeline::new()
      .shape(text, &style, &ctx)
      .expect("shape");
    let stops = caret_stops_for_runs(text, &runs, 0.0);

    let indices: Vec<usize> = stops
      .iter()
      .enumerate()
      .filter(|(_, s)| s.char_idx == 4)
      .map(|(idx, _)| idx)
      .collect();
    assert!(
      indices.len() >= 2,
      "expected at least two stops for char_idx=4"
    );

    // Starting from the leftmost stop at char_idx=4, walk forward until we reach another stop with
    // the same logical boundary. This mirrors ArrowRight repeatedly stepping through visual stops.
    let start = indices[0];
    let mut idx = start;
    let mut saw_second = false;
    while idx + 1 < stops.len() {
      idx += 1;
      if stops[idx].char_idx == 4 && (stops[idx].x - stops[start].x).abs() > 1e-3 {
        saw_second = true;
        break;
      }
    }
    assert!(
      saw_second,
      "expected visual caret navigation to encounter the second split caret stop; stops={:?}",
      stops
    );
  }

  #[test]
  fn split_caret_affinity_maps_to_distinct_x_positions_at_bidi_boundary() {
    let text = "ABCאבג";
    // Byte offsets:
    // - "ABC" => 0..3
    // - "אבג" => 3..text.len()
    let ltr = run(text, 0, 3, Direction::LeftToRight, 1.0, vec![0, 1, 2]);
    // Simulate HarfBuzz output for RTL: glyph order reversed (clusters descending in bytes).
    let rtl = run(
      text,
      3,
      text.len(),
      Direction::RightToLeft,
      1.0,
      vec![4, 2, 0],
    );
    let runs = vec![ltr, rtl];
    let stops = caret_stops_for_runs(text, &runs, 0.0);

    assert_eq!(
      caret_x_for_position(&stops, 3, CaretAffinity::Upstream),
      Some(3.0),
    );
    assert_eq!(
      caret_x_for_position(&stops, 3, CaretAffinity::Downstream),
      Some(6.0),
    );
  }
}
