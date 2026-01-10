//! Generic CSS alignment code that is shared between both the Flexbox and CSS Grid algorithms.
use crate::style::AlignContent;

/// Implement fallback alignment.
///
/// In addition to the spec at https://www.w3.org/TR/css-align-3/ this implementation follows
/// the resolution of https://github.com/w3c/csswg-drafts/issues/10154
pub(crate) fn apply_alignment_fallback(
  free_space: f32,
  num_items: usize,
  mut alignment_mode: AlignContent,
  mut is_safe: bool,
) -> AlignContent {
  let free_space = if free_space.is_finite() { free_space } else { 0.0 };

  // Fallback occurs in two cases:

  // 1. If there is only a single item being aligned and alignment is a distributed alignment keyword
  //    https://www.w3.org/TR/css-align-3/#distribution-values
  if num_items <= 1 || free_space <= 0.0 {
    (alignment_mode, is_safe) = match alignment_mode {
      AlignContent::Stretch => (AlignContent::FlexStart, true),
      AlignContent::SpaceBetween => (AlignContent::FlexStart, true),
      AlignContent::SpaceAround => (AlignContent::Center, true),
      AlignContent::SpaceEvenly => (AlignContent::Center, true),
      _ => (alignment_mode, is_safe),
    }
  };

  // 2. If free space is negative the "safe" alignment variants all fallback to Start alignment
  if free_space <= 0.0 && is_safe {
    alignment_mode = AlignContent::Start;
  }

  alignment_mode
}

/// Generic alignment function that is used:
///   - For both align-content and justify-content alignment
///   - For both the Flexbox and CSS Grid algorithms
///
/// CSS Grid does not apply gaps as part of alignment, so the gap parameter should
/// always be set to zero for CSS Grid.
pub(crate) fn compute_alignment_offset(
  mut free_space: f32,
  num_items: usize,
  mut gap: f32,
  alignment_mode: AlignContent,
  layout_is_flex_reversed: bool,
  is_first: bool,
) -> f32 {
  if !free_space.is_finite() {
    free_space = 0.0;
  }
  if !gap.is_finite() || gap < 0.0 {
    gap = 0.0;
  }
  if num_items == 0 {
    return 0.0;
  }

  let offset = if is_first {
    match alignment_mode {
      AlignContent::Start => 0.0,
      AlignContent::FlexStart => {
        if layout_is_flex_reversed {
          free_space
        } else {
          0.0
        }
      }
      AlignContent::End => free_space,
      AlignContent::FlexEnd => {
        if layout_is_flex_reversed {
          0.0
        } else {
          free_space
        }
      }
      AlignContent::Center => free_space / 2.0,
      AlignContent::Stretch => 0.0,
      AlignContent::SpaceBetween => 0.0,
      AlignContent::SpaceAround => {
        if free_space >= 0.0 {
          (free_space / num_items as f32) / 2.0
        } else {
          free_space / 2.0
        }
      }
      AlignContent::SpaceEvenly => {
        if free_space >= 0.0 {
          free_space / (num_items + 1) as f32
        } else {
          free_space / 2.0
        }
      }
    }
  } else {
    let free_space = free_space.max(0.0);
    gap
      + match alignment_mode {
        AlignContent::Start => 0.0,
        AlignContent::FlexStart => 0.0,
        AlignContent::End => 0.0,
        AlignContent::FlexEnd => 0.0,
        AlignContent::Center => 0.0,
        AlignContent::Stretch => 0.0,
        AlignContent::SpaceBetween => free_space / (num_items - 1) as f32,
        AlignContent::SpaceAround => free_space / num_items as f32,
        AlignContent::SpaceEvenly => free_space / (num_items + 1) as f32,
      }
  };

  if offset.is_finite() { offset } else { 0.0 }
}

#[cfg(test)]
mod tests {
  use super::compute_alignment_offset;
  use crate::style::AlignContent;

  #[test]
  fn compute_alignment_offset_infinite_free_space_matches_zero_free_space() {
    let num_items = 3;
    let gap = 5.0;
    let layout_is_flex_reversed = false;

    for alignment_mode in [
      AlignContent::Start,
      AlignContent::FlexStart,
      AlignContent::Center,
      AlignContent::SpaceBetween,
    ] {
      let offset_inf_first = compute_alignment_offset(
        f32::INFINITY,
        num_items,
        gap,
        alignment_mode,
        layout_is_flex_reversed,
        true,
      );
      let offset_zero_first = compute_alignment_offset(
        0.0,
        num_items,
        gap,
        alignment_mode,
        layout_is_flex_reversed,
        true,
      );
      assert!(offset_inf_first.is_finite());
      assert_eq!(offset_inf_first, offset_zero_first);

      let offset_inf_non_first = compute_alignment_offset(
        f32::INFINITY,
        num_items,
        gap,
        alignment_mode,
        layout_is_flex_reversed,
        false,
      );
      let offset_zero_non_first = compute_alignment_offset(
        0.0,
        num_items,
        gap,
        alignment_mode,
        layout_is_flex_reversed,
        false,
      );
      assert!(offset_inf_non_first.is_finite());
      assert_eq!(offset_inf_non_first, offset_zero_non_first);
    }
  }

  #[test]
  fn compute_alignment_offset_nan_free_space_is_finite() {
    let offset = compute_alignment_offset(
      f32::NAN,
      3,
      5.0,
      AlignContent::Center,
      false,
      true,
    );
    assert!(offset.is_finite());
  }

  #[test]
  fn compute_alignment_offset_nan_and_negative_gap_are_treated_as_zero() {
    let num_items = 3;
    let free_space = 100.0;
    let layout_is_flex_reversed = false;

    let offset_nan_gap = compute_alignment_offset(
      free_space,
      num_items,
      f32::NAN,
      AlignContent::SpaceBetween,
      layout_is_flex_reversed,
      false,
    );
    let offset_zero_gap = compute_alignment_offset(
      free_space,
      num_items,
      0.0,
      AlignContent::SpaceBetween,
      layout_is_flex_reversed,
      false,
    );
    assert!(offset_nan_gap.is_finite());
    assert_eq!(offset_nan_gap, offset_zero_gap);

    let offset_negative_gap = compute_alignment_offset(
      free_space,
      num_items,
      -5.0,
      AlignContent::SpaceBetween,
      layout_is_flex_reversed,
      false,
    );
    assert!(offset_negative_gap.is_finite());
    assert_eq!(offset_negative_gap, offset_zero_gap);
  }

  #[test]
  fn apply_alignment_fallback_non_finite_free_space_matches_zero_free_space() {
    for (free_space, label) in [(f32::NAN, "NaN"), (f32::INFINITY, "INFINITY")] {
      let fallback = super::apply_alignment_fallback(free_space, 1, AlignContent::SpaceBetween, true);
      let expected = super::apply_alignment_fallback(0.0, 1, AlignContent::SpaceBetween, true);
      assert_eq!(fallback, expected, "{label}");
    }
  }
}
