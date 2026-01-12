//! UI motion policy and animation helpers.
//!
//! The browser chrome uses subtle animations to feel responsive and premium. This module provides a
//! small policy abstraction so motion can be disabled (e.g. for accessibility / reduced-motion
//! preferences).
//!
//! Currently reduced motion is controlled via the `FASTR_BROWSER_REDUCED_MOTION` env var:
//! - unset / `0` / `false` → motion enabled (default)
//! - any other value (e.g. `1`) → reduced motion, animations disabled

use std::sync::atomic::{AtomicU8, Ordering};

/// Environment variable that disables micro-interaction animations when set to a truthy value.
pub const ENV_REDUCED_MOTION: &str = "FASTR_BROWSER_REDUCED_MOTION";

// 0 = false, 1 = true, 2 = unknown/uninitialized.
static REDUCED_MOTION_ENV_CACHE: AtomicU8 = AtomicU8::new(2);

fn parse_env_bool(value: &str) -> bool {
  // Treat any non-empty, non-falsey string as true. This is intentionally permissive so
  // `FASTR_BROWSER_REDUCED_MOTION=1`, `true`, `yes`, etc. work as expected.
  let v = value.trim();
  if v.is_empty() {
    return false;
  }
  !(v.eq_ignore_ascii_case("0")
    || v.eq_ignore_ascii_case("false")
    || v.eq_ignore_ascii_case("no")
    || v.eq_ignore_ascii_case("off"))
}

fn reduced_motion_from_env() -> bool {
  std::env::var(ENV_REDUCED_MOTION)
    .ok()
    .map(|v| parse_env_bool(&v))
    .unwrap_or(false)
}

fn reduced_motion_cached() -> bool {
  match REDUCED_MOTION_ENV_CACHE.load(Ordering::Relaxed) {
    0 => false,
    1 => true,
    _ => {
      let resolved = reduced_motion_from_env();
      REDUCED_MOTION_ENV_CACHE.store(if resolved { 1 } else { 0 }, Ordering::Relaxed);
      resolved
    }
  }
}

#[derive(Debug, Clone, Copy)]
pub struct UiMotionDurations {
  /// Fade in/out duration for hover highlights (seconds).
  pub hover_fade: f32,
  /// Transition duration for tab underline (seconds).
  pub tab_underline: f32,
  /// Fade/expand duration for the address bar focus ring (seconds).
  pub focus_ring: f32,
  /// Fade in/out duration for the loading progress indicator (seconds).
  pub progress_fade: f32,
}

impl Default for UiMotionDurations {
  fn default() -> Self {
    Self {
      hover_fade: 0.12,
      tab_underline: 0.16,
      focus_ring: 0.14,
      progress_fade: 0.18,
    }
  }
}

/// Motion policy used by the browser UI.
#[derive(Debug, Clone, Copy)]
pub struct UiMotion {
  pub enabled: bool,
  pub durations: UiMotionDurations,
}

impl UiMotion {
  /// Construct the motion policy from environment configuration.
  pub fn from_env() -> Self {
    let reduced_motion = reduced_motion_cached();
    Self::new(!reduced_motion)
  }

  pub fn new(enabled: bool) -> Self {
    Self {
      enabled,
      durations: UiMotionDurations::default(),
    }
  }

  #[cfg(feature = "browser_ui")]
  pub fn animate_bool(
    &self,
    ctx: &egui::Context,
    id: egui::Id,
    target: bool,
    duration: f32,
  ) -> f32 {
    if !self.enabled || duration <= 0.0 {
      return if target { 1.0 } else { 0.0 };
    }
    ctx.animate_value_with_time(id, if target { 1.0 } else { 0.0 }, duration)
  }

  #[cfg(feature = "browser_ui")]
  pub fn animate_f32(
    &self,
    ctx: &egui::Context,
    id: egui::Id,
    target: f32,
    duration: f32,
  ) -> f32 {
    if !self.enabled || duration <= 0.0 {
      return target;
    }
    ctx.animate_value_with_time(id, target, duration)
  }
}

#[cfg(test)]
mod tests {
  use super::parse_env_bool;

  #[test]
  fn parse_env_bool_falsey_values() {
    for v in ["", "0", "false", "FALSE", "no", "off", " 0 ", " false "] {
      assert!(
        !parse_env_bool(v),
        "expected {v:?} to be treated as false"
      );
    }
  }

  #[test]
  fn parse_env_bool_truthy_values() {
    for v in ["1", "true", "yes", "on", "anything", " 1 ", " TRUE "] {
      assert!(parse_env_bool(v), "expected {v:?} to be treated as true");
    }
  }
}
