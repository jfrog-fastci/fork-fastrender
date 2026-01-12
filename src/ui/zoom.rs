//! Per-tab page zoom utilities.
//!
//! The browser UI implements "browser-like" zoom by scaling CSS pixels rather than the window:
//!
//! - The UI draws the worker's pixmap at a fixed physical size (in egui points).
//! - Increasing `zoom` decreases the CSS viewport size (`viewport_css`) while increasing the
//!   device-pixel ratio (`dpr`), keeping the resulting pixmap size roughly constant:
//!
//!   `pixmap_px ~= viewport_css * dpr ~= (available_points / zoom) * (ppp * zoom)`
//!
//! This preserves crisp text while making content appear larger/smaller.

/// Default zoom factor for new tabs.
pub const DEFAULT_ZOOM: f32 = 1.0;

/// Minimum allowed zoom factor for chrome shortcuts/UI.
pub const MIN_ZOOM: f32 = 0.5;

/// Maximum allowed zoom factor for chrome shortcuts/UI.
pub const MAX_ZOOM: f32 = 3.0;

/// Zoom multiplier applied for Ctrl/Cmd +/- shortcuts.
///
/// Most browsers use ~10% increments.
pub const ZOOM_STEP: f32 = 1.1;

pub fn clamp_zoom(zoom: f32) -> f32 {
  if !zoom.is_finite() {
    return DEFAULT_ZOOM;
  }
  zoom.clamp(MIN_ZOOM, MAX_ZOOM)
}

fn quantize_zoom(zoom: f32) -> f32 {
  // Avoid accumulating floating-point noise from repeated * / operations (e.g. 1.1^N).
  // Keep two decimal places: enough for stable UI display and viewport calculations.
  (zoom * 100.0).round() / 100.0
}

pub fn zoom_in(current: f32) -> f32 {
  quantize_zoom(clamp_zoom(current) * ZOOM_STEP).clamp(MIN_ZOOM, MAX_ZOOM)
}

pub fn zoom_out(current: f32) -> f32 {
  quantize_zoom(clamp_zoom(current) / ZOOM_STEP).clamp(MIN_ZOOM, MAX_ZOOM)
}

pub fn zoom_reset() -> f32 {
  DEFAULT_ZOOM
}

pub fn zoom_percent(zoom: f32) -> u32 {
  (clamp_zoom(zoom) * 100.0).round().max(1.0) as u32
}

/// Compute the viewport + DPR values to send to the render worker for a given zoom.
///
/// This follows the contract described in `instructions/browser_ui.md`:
/// - `viewport_css = (available_points / zoom).round().max(1)`
/// - `dpr = pixels_per_point * zoom`
pub fn viewport_css_and_dpr_for_zoom(
  available_points: (f32, f32),
  pixels_per_point: f32,
  zoom: f32,
) -> ((u32, u32), f32) {
  let zoom = clamp_zoom(zoom).max(1e-6);
  let ppp = if pixels_per_point.is_finite() && pixels_per_point > 0.0 {
    pixels_per_point
  } else {
    1.0
  };

  let avail_w = if available_points.0.is_finite() {
    available_points.0.max(0.0)
  } else {
    0.0
  };
  let avail_h = if available_points.1.is_finite() {
    available_points.1.max(0.0)
  } else {
    0.0
  };

  let viewport_css = (
    (avail_w / zoom).round().max(1.0) as u32,
    (avail_h / zoom).round().max(1.0) as u32,
  );
  let dpr = ppp * zoom;

  (viewport_css, dpr)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn zoom_clamps_and_quantizes() {
    assert_eq!(clamp_zoom(f32::NAN), DEFAULT_ZOOM);
    assert_eq!(clamp_zoom(0.0), MIN_ZOOM);
    assert_eq!(clamp_zoom(10.0), MAX_ZOOM);

    // Quantization should keep values stable.
    let z = DEFAULT_ZOOM;
    let z = zoom_in(z);
    let z = zoom_out(z);
    assert!((z - DEFAULT_ZOOM).abs() <= 0.02, "expected ~1.0, got {z}");
  }

  #[test]
  fn viewport_and_dpr_match_spec_formula() {
    let (viewport_css, dpr) = viewport_css_and_dpr_for_zoom((200.0, 120.0), 2.0, 1.0);
    assert_eq!(viewport_css, (200, 120));
    assert!((dpr - 2.0).abs() < 1e-6);

    let (viewport_css, dpr) = viewport_css_and_dpr_for_zoom((200.0, 120.0), 2.0, 2.0);
    assert_eq!(viewport_css, (100, 60));
    assert!((dpr - 4.0).abs() < 1e-6);
  }

  #[test]
  fn zoom_mapping_keeps_pixmap_size_constant_for_integer_points() {
    let available_points = (200.0, 120.0);
    let ppp = 2.0;

    let (vp1, dpr1) = viewport_css_and_dpr_for_zoom(available_points, ppp, 1.0);
    let (vp2, dpr2) = viewport_css_and_dpr_for_zoom(available_points, ppp, 2.0);

    let px1 = (
      (vp1.0 as f32 * dpr1).round() as i32,
      (vp1.1 as f32 * dpr1).round() as i32,
    );
    let px2 = (
      (vp2.0 as f32 * dpr2).round() as i32,
      (vp2.1 as f32 * dpr2).round() as i32,
    );
    assert_eq!(px1, px2);
  }
}
