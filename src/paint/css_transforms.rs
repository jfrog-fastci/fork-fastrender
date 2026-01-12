//! Shared helpers for CSS Transforms behavior during painting.
//!
//! CSS Transforms Level 2 defines that `transform-style: preserve-3d` is subject to
//! "3D flattening" boundaries: certain properties require the user agent to flatten
//! descendants before applying them, forcing the element's *used* `transform-style`
//! to `flat`.
//!
//! We centralize this logic so stacking-context construction and display list
//! building agree on when an element preserves a 3D rendering context.

use crate::style::position::Position;
use crate::style::types::{Isolation, MixBlendMode, Overflow, TransformStyle};
use crate::style::ComputedStyle;

pub(crate) fn used_transform_style(style: &ComputedStyle) -> TransformStyle {
  if is_3d_flattening_boundary(style) {
    TransformStyle::Flat
  } else {
    style.transform_style
  }
}

pub(crate) fn is_3d_flattening_boundary(style: &ComputedStyle) -> bool {
  if !style.filter.is_empty() || !style.backdrop_filter.is_empty() {
    return true;
  }
  if style.opacity < 1.0 - f32::EPSILON {
    return true;
  }
  if !matches!(style.clip_path, crate::style::types::ClipPath::None) {
    return true;
  }
  if matches!(style.position, Position::Absolute | Position::Fixed) && style.clip.is_some() {
    return true;
  }
  if overflow_axis_forces_3d_flattening(style.overflow_x)
    || overflow_axis_forces_3d_flattening(style.overflow_y)
  {
    return true;
  }
  if style.mask_layers.iter().any(|layer| layer.image.is_some()) {
    return true;
  }
  if style.mask_border.is_active() {
    return true;
  }
  if !matches!(style.mix_blend_mode, MixBlendMode::Normal) {
    return true;
  }
  if matches!(style.isolation, Isolation::Isolate) {
    return true;
  }
  if style.containment.isolates_paint() {
    return true;
  }
  false
}

fn overflow_axis_forces_3d_flattening(overflow: Overflow) -> bool {
  matches!(
    overflow,
    Overflow::Hidden | Overflow::Scroll | Overflow::Auto
  )
}
