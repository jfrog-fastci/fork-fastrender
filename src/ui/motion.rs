//! UI motion policy and animation helpers.
//!
//! The browser chrome uses subtle animations to feel responsive and premium. This module provides a
//! small policy abstraction so motion can be disabled (e.g. for accessibility / reduced-motion
//! preferences).
//!
//! Currently reduced motion is controlled via the `FASTR_BROWSER_REDUCED_MOTION` env var:
//! - unset / `0` / `false` → motion enabled (default)
//! - any other value (e.g. `1`) → reduced motion, animations disabled

use crate::debug::runtime::runtime_toggles;

/// Environment variable that disables micro-interaction animations when set to a truthy value.
pub const ENV_REDUCED_MOTION: &str = "FASTR_BROWSER_REDUCED_MOTION";

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

fn reduced_motion_from_runtime_toggles() -> bool {
  runtime_toggles()
    .get(ENV_REDUCED_MOTION)
    .map(parse_env_bool)
    .unwrap_or(false)
}

#[derive(Debug, Clone, Copy)]
pub struct UiMotionDurations {
  /// Fade in/out duration for hover highlights (seconds).
  pub hover_fade: f32,
  /// Transition duration for tab underline (seconds).
  pub tab_underline: f32,
  /// Fade/scale duration for popup menus (context menus, `<select>` dropdowns) (seconds).
  pub popup_open: f32,
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
      popup_open: 0.14,
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
    let reduced_motion = reduced_motion_from_runtime_toggles();
    Self::new(!reduced_motion)
  }

  pub fn new(enabled: bool) -> Self {
    Self {
      enabled,
      durations: UiMotionDurations::default(),
    }
  }

  /// Scale factor to apply for popup open animations (fade/scale).
  ///
  /// When motion is disabled this always returns `1.0`, so callers can keep drawing popups at their
  /// final size without introducing any motion.
  pub fn popup_open_scale(&self, t: f32) -> f32 {
    if !self.enabled {
      return 1.0;
    }
    // Subtle scale-in so popups feel responsive without being distracting.
    const MIN_SCALE: f32 = 0.98;
    let t = t.clamp(0.0, 1.0);
    MIN_SCALE + (1.0 - MIN_SCALE) * t
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
  use crate::debug::runtime::{with_runtime_toggles, RuntimeToggles};
  use super::{parse_env_bool, UiMotion, ENV_REDUCED_MOTION};
  use std::collections::HashMap;
  use std::sync::Arc;

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

  #[test]
  fn ui_motion_from_env_respects_reduced_motion_env_var() {
    with_runtime_toggles(Arc::new(RuntimeToggles::from_map(HashMap::new())), || {
      assert!(UiMotion::from_env().enabled, "motion should be enabled by default");
    });

    with_runtime_toggles(
      Arc::new(RuntimeToggles::from_map(HashMap::from([(
        ENV_REDUCED_MOTION.to_string(),
        "1".to_string(),
      )]))),
      || {
        assert!(
          !UiMotion::from_env().enabled,
          "motion should be disabled when reduced motion env var is truthy"
        );
      },
    );

    for value in ["0", ""] {
      with_runtime_toggles(
        Arc::new(RuntimeToggles::from_map(HashMap::from([(
          ENV_REDUCED_MOTION.to_string(),
          value.to_string(),
        )]))),
        || {
          assert!(
            UiMotion::from_env().enabled,
            "motion should be enabled when reduced motion env var is falsey ({value:?})"
          );
        },
      );
    }
  }

  #[test]
  fn popup_open_scale_is_identity_when_motion_disabled() {
    let motion = UiMotion::new(false);
    for t in [0.0_f32, 0.25, 0.5, 0.9, 1.0] {
      assert!(
        (motion.popup_open_scale(t) - 1.0).abs() < f32::EPSILON,
        "expected popup scale to be 1.0 when motion is disabled (t={t})"
      );
    }
  }

  #[test]
  fn popup_open_scale_interpolates_when_motion_enabled() {
    let motion = UiMotion::new(true);
    assert!(
      (motion.popup_open_scale(0.0) - 0.98).abs() < 1e-6,
      "expected scale at t=0 to start slightly smaller"
    );
    assert!(
      (motion.popup_open_scale(1.0) - 1.0).abs() < f32::EPSILON,
      "expected scale at t=1 to reach 1.0"
    );
    let mid = motion.popup_open_scale(0.5);
    assert!(mid > 0.98 && mid < 1.0, "expected mid scale to be in (0.98, 1.0)");
  }
}
