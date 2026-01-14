/// Apply browser-like Shift+wheel semantics (treat vertical wheel deltas as horizontal scrolling).
///
/// Many platforms report classic mouse wheels as vertical-only even with Shift held; mainstream
/// browsers commonly reinterpret that as horizontal scrolling.
///
/// Behaviour:
/// - When Shift is not held, the delta is returned unchanged.
/// - When Shift is held and the horizontal delta is effectively absent (≈0), a non-zero vertical
///   delta is moved into the horizontal axis (`dx += dy; dy = 0`).
/// - If a real horizontal delta is present (e.g. trackpad, tilt wheel), it is preserved.
#[must_use]
pub fn remap_wheel_delta_for_shift(delta: (f32, f32), shift: bool) -> (f32, f32) {
  if !shift {
    return delta;
  }

  let (dx, dy) = delta;
  if !dx.is_finite() || !dy.is_finite() || dy == 0.0 {
    return delta;
  }

  // Many scroll devices report horizontal deltas as exactly 0; keep a small epsilon to allow for
  // float noise introduced by coordinate conversions.
  const DX_EPSILON: f32 = 1e-3;
  if dx.abs() < DX_EPSILON {
    (dx + dy, 0.0)
  } else {
    delta
  }
}

#[cfg(test)]
mod tests {
  use super::remap_wheel_delta_for_shift;

  #[test]
  fn shift_wheel_maps_vertical_delta_to_horizontal() {
    assert_eq!(remap_wheel_delta_for_shift((0.0, 5.0), false), (0.0, 5.0));
    assert_eq!(remap_wheel_delta_for_shift((0.0, 5.0), true), (5.0, 0.0));
  }

  #[test]
  fn shift_wheel_preserves_existing_horizontal_delta() {
    assert_eq!(remap_wheel_delta_for_shift((3.0, 0.0), true), (3.0, 0.0));
    assert_eq!(remap_wheel_delta_for_shift((3.0, 7.0), true), (3.0, 7.0));
  }
}

