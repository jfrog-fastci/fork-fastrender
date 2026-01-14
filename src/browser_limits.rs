//! Safety limits for viewport/DPR requested by browser-like embeddings.
//!
//! The desktop `browser` UI derives `viewport_css` from egui points and uses `dpr=pixels_per_point`.
//! On very large displays (or when a window is resized to extreme sizes), this can request
//! multi-gigabyte RGBA pixmaps from the renderer.
//!
//! This module provides deterministic clamping logic that can be shared by both UI and renderer
//! code so:
//! - the UI never requests absurd allocations from the worker, and
//! - the worker stays robust if other frontends send insane values.

use crate::debug::runtime;

/// Environment variable override for [`BrowserLimits::max_pixels`].
pub const ENV_MAX_PIXELS: &str = "FASTR_BROWSER_MAX_PIXELS";
/// Environment variable override for [`BrowserLimits::max_dim_px`].
pub const ENV_MAX_DIM_PX: &str = "FASTR_BROWSER_MAX_DIM_PX";
/// Environment variable override for [`BrowserLimits::max_dpr`].
pub const ENV_MAX_DPR: &str = "FASTR_BROWSER_MAX_DPR";

/// Default total device pixels (width_px * height_px) allowed for a single rendered frame.
///
/// Note: RGBA8 pixmaps are 4 bytes per pixel, so 50M pixels ≈ 200 MiB just for the raw buffer.
pub const DEFAULT_MAX_PIXELS: u64 = 50_000_000;
/// Default maximum width/height in device pixels for a rendered frame.
///
/// 8192 is a conservative upper bound that aligns with common GPU texture limits.
pub const DEFAULT_MAX_DIM_PX: u32 = 8192;

/// The renderer clamps the *effective* device pixel ratio internally.
///
/// Keep this in sync with `src/api.rs`'s `MIN_EFFECTIVE_SCALE`.
const RENDERER_MIN_DPR: f32 = 0.1;
/// Keep this in sync with `src/api.rs`'s `MAX_EFFECTIVE_SCALE`.
const RENDERER_MAX_DPR: f32 = 10.0;

#[derive(Debug, Clone, Copy)]
pub struct BrowserLimits {
  pub max_pixels: u64,
  pub max_dim_px: u32,
  pub max_dpr: f32,
}

impl Default for BrowserLimits {
  fn default() -> Self {
    Self {
      max_pixels: DEFAULT_MAX_PIXELS,
      max_dim_px: DEFAULT_MAX_DIM_PX,
      // Do not clamp further by default; the renderer already clamps to 10.0.
      max_dpr: RENDERER_MAX_DPR,
    }
  }
}

impl BrowserLimits {
  /// Load viewport limits from environment variables, falling back to defaults.
  ///
  /// Invalid / empty values are ignored (defaults remain in effect). Underscore separators are
  /// accepted (e.g. `50_000_000`).
  pub fn from_env() -> Self {
    let mut out = Self::default();
    let toggles = runtime::runtime_toggles();

    if let Some(v) = parse_env_u64(toggles.get(ENV_MAX_PIXELS)) {
      out.max_pixels = v.max(1);
    }
    if let Some(v) = parse_env_u64(toggles.get(ENV_MAX_DIM_PX)) {
      out.max_dim_px = (v.min(u32::MAX as u64) as u32).max(1);
    }
    if let Some(v) = parse_env_f32(toggles.get(ENV_MAX_DPR)) {
      // Clamp to the renderer's DPR range so UI/worker calculations match the renderer's behavior.
      out.max_dpr = v.clamp(RENDERER_MIN_DPR, RENDERER_MAX_DPR);
    }

    // Ensure invariants even when the env provided nonsense.
    out.max_pixels = out.max_pixels.max(1);
    out.max_dim_px = out.max_dim_px.max(1);
    out.max_dpr = out.max_dpr.clamp(RENDERER_MIN_DPR, RENDERER_MAX_DPR);
    out
  }

  pub fn clamp_viewport_and_dpr(&self, viewport_css: (u32, u32), dpr: f32) -> ViewportClamp {
    let requested_viewport_css = (viewport_css.0.max(1), viewport_css.1.max(1));
    let requested_dpr = sanitize_dpr(dpr).clamp(RENDERER_MIN_DPR, RENDERER_MAX_DPR);

    // Apply the browser-ui max-dpr knob before doing pixel math.
    let desired_dpr = requested_dpr.min(self.max_dpr);

    let desired_pixmap_px = pixmap_px_for(requested_viewport_css, desired_dpr);
    if self.pixmap_within_limits(desired_pixmap_px) {
      return ViewportClamp {
        requested_viewport_css,
        requested_dpr,
        viewport_css: requested_viewport_css,
        dpr: desired_dpr,
        pixmap_px: desired_pixmap_px,
      };
    }

    // Try to keep the CSS viewport stable, reducing DPR first. Only clamp viewport_css if we hit
    // the renderer's minimum DPR and still exceed limits.
    let mut viewport_css = requested_viewport_css;
    let mut clamped_dpr = desired_dpr;

    // Clamp DPR down to the maximum that satisfies our limits for this viewport size (may still be
    // below `RENDERER_MIN_DPR`, handled below).
    clamped_dpr = clamped_dpr.min(self.max_dpr_for_viewport(viewport_css));

    // If even the minimum DPR would exceed limits, reduce the CSS viewport until `RENDERER_MIN_DPR`
    // can fit.
    if clamped_dpr < RENDERER_MIN_DPR {
      viewport_css = self.clamp_viewport_for_fixed_dpr(viewport_css, RENDERER_MIN_DPR);
      clamped_dpr = desired_dpr.min(self.max_dpr_for_viewport(viewport_css));
    }

    clamped_dpr = clamped_dpr.clamp(RENDERER_MIN_DPR, self.max_dpr);

    // Final sanity loop: account for rounding (`round(viewport_css * dpr)`) by re-checking the
    // derived pixmap size and tightening as needed. This loop is deterministic and bounded.
    for _ in 0..8 {
      let pixmap_px = pixmap_px_for(viewport_css, clamped_dpr);
      if self.pixmap_within_limits(pixmap_px) {
        return ViewportClamp {
          requested_viewport_css,
          requested_dpr,
          viewport_css,
          dpr: clamped_dpr,
          pixmap_px,
        };
      }

      if clamped_dpr > RENDERER_MIN_DPR + 1e-6 {
        // Tighten DPR and retry.
        clamped_dpr = clamped_dpr.min(self.max_dpr_for_viewport(viewport_css));
        clamped_dpr = clamped_dpr.clamp(RENDERER_MIN_DPR, self.max_dpr);
        // If we didn't make progress (due to float/rounding), step down slightly.
        clamped_dpr = (clamped_dpr - 0.001).max(RENDERER_MIN_DPR);
        continue;
      }

      // DPR is at its minimum; clamp the CSS viewport further.
      viewport_css = self.clamp_viewport_for_fixed_dpr(viewport_css, RENDERER_MIN_DPR);
      clamped_dpr = desired_dpr.min(self.max_dpr_for_viewport(viewport_css));
      clamped_dpr = clamped_dpr.clamp(RENDERER_MIN_DPR, self.max_dpr);
    }

    // Should be unreachable, but keep a safe fallback.
    let viewport_css = self.clamp_viewport_for_fixed_dpr(viewport_css, RENDERER_MIN_DPR);
    let clamped_dpr = RENDERER_MIN_DPR.min(self.max_dpr);
    let pixmap_px = pixmap_px_for(viewport_css, clamped_dpr);
    ViewportClamp {
      requested_viewport_css,
      requested_dpr,
      viewport_css,
      dpr: clamped_dpr,
      pixmap_px,
    }
  }

  fn pixmap_within_limits(&self, pixmap_px: (u32, u32)) -> bool {
    let (w, h) = pixmap_px;
    if w > self.max_dim_px || h > self.max_dim_px {
      return false;
    }
    let total = (w as u64).saturating_mul(h as u64);
    total <= self.max_pixels
  }

  fn max_dpr_for_viewport(&self, viewport_css: (u32, u32)) -> f32 {
    let w_css = viewport_css.0.max(1) as f64;
    let h_css = viewport_css.1.max(1) as f64;

    let dim_limit_w = (self.max_dim_px as f64) / w_css;
    let dim_limit_h = (self.max_dim_px as f64) / h_css;
    let dim_limit = dim_limit_w.min(dim_limit_h);

    let area_css = w_css * h_css;
    let pixels_limit = (self.max_pixels as f64 / area_css).sqrt();

    let limit = dim_limit.min(pixels_limit).min(self.max_dpr as f64);
    if limit.is_finite() && limit > 0.0 {
      limit as f32
    } else {
      RENDERER_MIN_DPR
    }
  }

  fn clamp_viewport_for_fixed_dpr(&self, viewport_css: (u32, u32), dpr: f32) -> (u32, u32) {
    let dpr = sanitize_dpr(dpr).clamp(RENDERER_MIN_DPR, RENDERER_MAX_DPR);
    let mut w = viewport_css.0.max(1);
    let mut h = viewport_css.1.max(1);

    // Clamp individual dimensions by the max texture dimension.
    let max_w_css = ((self.max_dim_px as f64) / (dpr as f64)).floor().max(1.0) as u32;
    let max_h_css = ((self.max_dim_px as f64) / (dpr as f64)).floor().max(1.0) as u32;
    w = w.min(max_w_css).max(1);
    h = h.min(max_h_css).max(1);

    // Clamp area to satisfy the max total pixel budget.
    let max_area_css = (self.max_pixels as f64) / ((dpr as f64) * (dpr as f64));
    let area_css = (w as f64) * (h as f64);
    if area_css > max_area_css && max_area_css.is_finite() && max_area_css > 1.0 {
      let scale = (max_area_css / area_css).sqrt();
      w = ((w as f64) * scale).floor().max(1.0) as u32;
      h = ((h as f64) * scale).floor().max(1.0) as u32;
    }

    (w.max(1), h.max(1))
  }
}

#[derive(Debug, Clone, Copy)]
pub struct ViewportClamp {
  pub requested_viewport_css: (u32, u32),
  pub requested_dpr: f32,
  pub viewport_css: (u32, u32),
  pub dpr: f32,
  pub pixmap_px: (u32, u32),
}

impl ViewportClamp {
  pub fn is_clamped(&self) -> bool {
    self.viewport_css != self.requested_viewport_css || (self.dpr - self.requested_dpr).abs() > 1e-6
  }

  pub fn warning_text(&self, limits: &BrowserLimits) -> Option<String> {
    if !self.is_clamped() {
      return None;
    }
    Some(format!(
      "Viewport clamped: requested viewport_css={:?} dpr={:.3} → viewport_css={:?} dpr={:.3} (pixmap_px={}x{}; limits: max_dim_px={} max_pixels={})",
      self.requested_viewport_css,
      self.requested_dpr,
      self.viewport_css,
      self.dpr,
      self.pixmap_px.0,
      self.pixmap_px.1,
      limits.max_dim_px,
      limits.max_pixels
    ))
  }
}

fn sanitize_dpr(dpr: f32) -> f32 {
  if dpr.is_finite() && dpr > 0.0 {
    dpr
  } else {
    1.0
  }
}

fn pixmap_px_for(viewport_css: (u32, u32), dpr: f32) -> (u32, u32) {
  let dpr = sanitize_dpr(dpr);
  let w = ((viewport_css.0 as f64) * (dpr as f64)).round();
  let h = ((viewport_css.1 as f64) * (dpr as f64)).round();
  let w = w.max(1.0).min(u32::MAX as f64) as u32;
  let h = h.max(1.0).min(u32::MAX as f64) as u32;
  (w, h)
}

fn parse_env_u64(raw: Option<&str>) -> Option<u64> {
  let raw = raw?;
  let raw = raw.trim();
  if raw.is_empty() {
    return None;
  }
  let raw = raw.replace('_', "");
  let value = raw.parse::<u64>().ok()?;
  (value > 0).then_some(value)
}

fn parse_env_f32(raw: Option<&str>) -> Option<f32> {
  let raw = raw?;
  let raw = raw.trim();
  if raw.is_empty() {
    return None;
  }
  let raw = raw.replace('_', "");
  let value = raw.parse::<f32>().ok()?;
  (value.is_finite() && value > 0.0).then_some(value)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn clamp_noop_for_reasonable_values() {
    let limits = BrowserLimits::default();
    let clamp = limits.clamp_viewport_and_dpr((800, 600), 2.0);
    assert_eq!(clamp.viewport_css, (800, 600));
    assert!((clamp.dpr - 2.0).abs() < 1e-6);
    assert!(!clamp.is_clamped());
    assert_eq!(clamp.pixmap_px, (1600, 1200));
  }

  #[test]
  fn clamp_prevents_absurd_pixmap_sizes() {
    let limits = BrowserLimits::default();
    let clamp = limits.clamp_viewport_and_dpr((100_000, 100_000), 4.0);
    assert!(clamp.is_clamped());
    assert!(clamp.pixmap_px.0 <= limits.max_dim_px);
    assert!(clamp.pixmap_px.1 <= limits.max_dim_px);
    let total = (clamp.pixmap_px.0 as u64) * (clamp.pixmap_px.1 as u64);
    assert!(
      total <= limits.max_pixels,
      "expected total pixels <= {}, got {}",
      limits.max_pixels,
      total
    );
  }
}

