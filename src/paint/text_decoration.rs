use crate::style::types::TextUnderlinePosition;
use crate::style::types::WritingMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnderlineSide {
  Left,
  Right,
}

pub(crate) fn resolve_underline_side(
  writing_mode: WritingMode,
  underline_position: TextUnderlinePosition,
) -> UnderlineSide {
  match underline_position {
    TextUnderlinePosition::Left | TextUnderlinePosition::UnderLeft => UnderlineSide::Left,
    TextUnderlinePosition::Right | TextUnderlinePosition::UnderRight => UnderlineSide::Right,
    TextUnderlinePosition::Auto
    | TextUnderlinePosition::FromFont
    | TextUnderlinePosition::Under => match writing_mode {
      WritingMode::VerticalRl | WritingMode::SidewaysRl => UnderlineSide::Right,
      WritingMode::VerticalLr | WritingMode::SidewaysLr => UnderlineSide::Left,
      WritingMode::HorizontalTb => UnderlineSide::Right,
    },
  }
}

/// Compute a dash offset for a segment that starts at `segment_start` so that
/// the dash pattern phase matches what it would be for a single continuous
/// stroke starting at 0.
pub(super) fn dash_offset_for_segment(dash_array: &[f32], segment_start: f32) -> f32 {
  if !segment_start.is_finite() {
    return 0.0;
  }

  let pattern_len: f32 = dash_array.iter().copied().sum();
  if !pattern_len.is_finite() || pattern_len <= 0.0 {
    return 0.0;
  }

  segment_start.rem_euclid(pattern_len)
}

/// Compute the wavy underline "phase" for a segment that starts at
/// `segment_start`, where `segment_start` is relative to the decoration's
/// overall start.
///
/// Returns `(cursor_start, up)` where:
/// - `cursor_start` is the start of the current wave segment (relative to the
///   decoration's start), and
/// - `up` is the boolean used by the renderer to decide whether the wave
///   control point is on the "up" or "down" side.
pub(super) fn wavy_phase_for_segment(wavelength: f32, segment_start: f32) -> (f32, bool) {
  if !wavelength.is_finite() || wavelength <= 0.0 || !segment_start.is_finite() {
    return (0.0, true);
  }

  let phase = segment_start.rem_euclid(wavelength);
  let cursor_start = segment_start - phase;
  // `cursor_start / wavelength` should be an integer, but use rounding to avoid
  // float noise around boundaries.
  let idx = (cursor_start / wavelength).round() as i64;
  let up = idx.rem_euclid(2) == 0;
  (cursor_start, up)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn dash_offset_for_segment_preserves_phase() {
    let t = 2.0;
    let dash = [3.0 * t, t];
    let pattern_len: f32 = dash.iter().copied().sum();
    assert!((pattern_len - 8.0).abs() < 1e-6);

    assert!((dash_offset_for_segment(&dash, 0.0) - 0.0).abs() < 1e-6);
    assert!((dash_offset_for_segment(&dash, 1.0) - 1.0).abs() < 1e-6);
    assert!((dash_offset_for_segment(&dash, pattern_len + 1.0) - 1.0).abs() < 1e-6);
    assert!((dash_offset_for_segment(&dash, pattern_len * 2.0 + 1.0) - 1.0).abs() < 1e-6);
  }

  #[test]
  fn wavy_phase_for_segment_toggles_each_wavelength() {
    let w = 10.0;

    let (cursor, up) = wavy_phase_for_segment(w, 0.0);
    assert!((cursor - 0.0).abs() < 1e-6);
    assert!(up);

    let (cursor, up) = wavy_phase_for_segment(w, 5.0);
    assert!((cursor - 0.0).abs() < 1e-6);
    assert!(up);

    let (cursor, up) = wavy_phase_for_segment(w, 10.0);
    assert!((cursor - 10.0).abs() < 1e-6);
    assert!(!up);

    let (cursor, up) = wavy_phase_for_segment(w, 15.0);
    assert!((cursor - 10.0).abs() < 1e-6);
    assert!(!up);

    let (cursor, up) = wavy_phase_for_segment(w, 20.0);
    assert!((cursor - 20.0).abs() < 1e-6);
    assert!(up);
  }
}
