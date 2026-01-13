#![cfg(feature = "browser_ui")]

//! WCAG-style contrast helpers for egui `Color32`.
//!
//! These helpers are intentionally small/pure so they can be used in unit tests to guard the
//! browser chrome theme palette against regressions that would reduce readability or make focus
//! indicators hard to see.

use egui::Color32;

#[inline]
fn srgb_to_linear(value: f32) -> f32 {
  if value <= 0.04045 {
    value / 12.92
  } else {
    ((value + 0.055) / 1.055).powf(2.4)
  }
}

/// Compute the WCAG relative luminance of a color (alpha ignored).
///
/// Formula: <https://www.w3.org/TR/WCAG21/#dfn-relative-luminance>
pub fn relative_luminance(color: Color32) -> f32 {
  let r = srgb_to_linear(color.r() as f32 / 255.0);
  let g = srgb_to_linear(color.g() as f32 / 255.0);
  let b = srgb_to_linear(color.b() as f32 / 255.0);
  0.2126 * r + 0.7152 * g + 0.0722 * b
}

/// Composite `foreground` over `background` using non-premultiplied alpha.
///
/// This matches standard "source over" alpha blending and rounds the resulting channels back to
/// 8-bit sRGBA, keeping tests deterministic and in the same discrete space as `egui::Color32`.
pub fn composite_over(foreground: Color32, background: Color32) -> Color32 {
  let fg_a = foreground.a() as f32 / 255.0;
  let bg_a = background.a() as f32 / 255.0;
  let out_a = fg_a + bg_a * (1.0 - fg_a);
  if out_a <= 0.0 {
    return Color32::TRANSPARENT;
  }

  let fg_r = foreground.r() as f32;
  let fg_g = foreground.g() as f32;
  let fg_b = foreground.b() as f32;

  let bg_r = background.r() as f32;
  let bg_g = background.g() as f32;
  let bg_b = background.b() as f32;

  let r = (fg_r * fg_a + bg_r * bg_a * (1.0 - fg_a)) / out_a;
  let g = (fg_g * fg_a + bg_g * bg_a * (1.0 - fg_a)) / out_a;
  let b = (fg_b * fg_a + bg_b * bg_a * (1.0 - fg_a)) / out_a;

  Color32::from_rgba_unmultiplied(
    r.round().clamp(0.0, 255.0) as u8,
    g.round().clamp(0.0, 255.0) as u8,
    b.round().clamp(0.0, 255.0) as u8,
    (out_a * 255.0).round().clamp(0.0, 255.0) as u8,
  )
}

/// Compute the WCAG contrast ratio between `foreground` and `background`.
///
/// If the foreground is translucent, this composites it over the background first so the reported
/// ratio reflects the actual displayed color (deterministically, without needing access to any OS
/// theme settings).
///
/// Formula: <https://www.w3.org/TR/WCAG21/#dfn-contrast-ratio>
pub fn contrast_ratio(foreground: Color32, background: Color32) -> f32 {
  let effective_fg = if foreground.a() == 255 {
    foreground
  } else {
    composite_over(foreground, background)
  };

  let l_fg = relative_luminance(effective_fg);
  let l_bg = relative_luminance(background);
  let (l1, l2) = if l_fg >= l_bg { (l_fg, l_bg) } else { (l_bg, l_fg) };
  (l1 + 0.05) / (l2 + 0.05)
}

