//! Browser UI appearance + accessibility settings.
//!
//! This module is intentionally kept **outside** the `browser_ui` feature gate so we can:
//! - persist appearance settings in the browser session file without pulling in winit/wgpu/egui,
//! - unit test precedence / sanitization logic in lightweight test targets.

use crate::debug::runtime::runtime_toggles;
use crate::ui::motion;
use crate::ui::theme_parsing::{
  format_hex_color, parse_browser_accent_env, parse_hex_color, parse_browser_theme_env, BrowserTheme,
  RgbaColor, ENV_BROWSER_ACCENT, ENV_BROWSER_THEME,
};
use serde::{Deserialize, Serialize};

/// Environment variable override for [`AppearanceSettings::ui_scale`].
pub const ENV_BROWSER_UI_SCALE: &str = "FASTR_BROWSER_UI_SCALE";

/// Environment variable override for [`AppearanceSettings::high_contrast`].
pub const ENV_BROWSER_HIGH_CONTRAST: &str = "FASTR_BROWSER_HIGH_CONTRAST";

pub const DEFAULT_UI_SCALE: f32 = 1.0;
// Keep these in sync with `ui::theme::{MIN_UI_SCALE,MAX_UI_SCALE}`. This module is intentionally
// not behind the `browser_ui` feature gate, so it cannot depend on `ui::theme` directly.
pub const MIN_UI_SCALE: f32 = 0.75;
pub const MAX_UI_SCALE: f32 = 2.0;

fn default_ui_scale() -> f32 {
  DEFAULT_UI_SCALE
}

/// Clamp a UI scale multiplier into the supported range.
///
/// This also normalizes invalid values (`NaN`, `<= 0`, etc) back to [`DEFAULT_UI_SCALE`].
pub fn clamp_ui_scale(raw: f32) -> f32 {
  sanitize_ui_scale(raw)
}

fn is_default_theme(theme: &BrowserTheme) -> bool {
  matches!(theme, BrowserTheme::System)
}

fn is_default_ui_scale(scale: &f32) -> bool {
  (*scale - DEFAULT_UI_SCALE).abs() <= 1e-6
}

fn is_false(value: &bool) -> bool {
  !*value
}

fn parse_env_bool(raw: &str) -> bool {
  let v = raw.trim();
  if v.is_empty() {
    return false;
  }
  !(v.eq_ignore_ascii_case("0")
    || v.eq_ignore_ascii_case("false")
    || v.eq_ignore_ascii_case("no")
    || v.eq_ignore_ascii_case("off"))
}

fn parse_env_f32(raw: &str) -> Option<f32> {
  let raw = raw.trim();
  if raw.is_empty() {
    return None;
  }
  let raw = raw.replace('_', "");
  let value = raw.parse::<f32>().ok()?;
  (value.is_finite() && value > 0.0).then_some(value)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppearanceSettings {
  /// Browser chrome theme selection (System/Light/Dark).
  #[serde(default, skip_serializing_if = "is_default_theme")]
  pub theme: BrowserTheme,

  /// Optional browser chrome accent color override.
  ///
  /// Stored as a hex string (e.g. `#RRGGBB` or `#RRGGBBAA`) so the session file stays readable and
  /// does not require any UI toolkit types.
  ///
  /// Accepts `RGB` shorthand and optional leading `#` (case-insensitive). Invalid values are
  /// discarded during [`AppearanceSettings::sanitized`].
  #[serde(default, skip_serializing_if = "Option::is_none", alias = "accent")]
  pub accent_color: Option<String>,

  /// Browser chrome UI scale multiplier (separate from per-tab page zoom).
  #[serde(default = "default_ui_scale", skip_serializing_if = "is_default_ui_scale")]
  pub ui_scale: f32,

  /// High contrast chrome theme variant / stronger focus indicators.
  #[serde(default, skip_serializing_if = "is_false")]
  pub high_contrast: bool,

  /// Disable/reduce non-essential chrome animations.
  #[serde(default, skip_serializing_if = "is_false")]
  pub reduced_motion: bool,
}

impl Default for AppearanceSettings {
  fn default() -> Self {
    Self {
      theme: BrowserTheme::System,
      accent_color: None,
      ui_scale: DEFAULT_UI_SCALE,
      high_contrast: false,
      reduced_motion: false,
    }
  }
}

impl AppearanceSettings {
  pub fn sanitized(mut self) -> Self {
    self.ui_scale = sanitize_ui_scale(self.ui_scale);
    self.accent_color = self
      .accent_color
      .take()
      .and_then(|raw| parse_hex_color(&raw).map(format_hex_color));
    self
  }

  pub fn is_default(value: &Self) -> bool {
    value == &Self::default()
  }

  pub fn with_env_overrides(mut self, env: AppearanceEnvOverrides) -> Self {
    if let Some(theme) = env.theme {
      self.theme = theme;
    }
    if let Some(accent) = env.accent {
      self.accent_color = Some(format_hex_color(accent));
    }
    if let Some(scale) = env.ui_scale {
      self.ui_scale = scale;
    }
    if let Some(high_contrast) = env.high_contrast {
      self.high_contrast = high_contrast;
    }
    if let Some(reduced_motion) = env.reduced_motion {
      self.reduced_motion = reduced_motion;
    }
    self.sanitized()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn sanitized_drops_invalid_accent_color() {
    let settings = AppearanceSettings {
      accent_color: Some("not-a-color".to_string()),
      ..Default::default()
    };
    assert_eq!(settings.sanitized().accent_color, None);
  }

  #[test]
  fn sanitized_normalizes_valid_accent_color() {
    let settings = AppearanceSettings {
      accent_color: Some("  #0f0 ".to_string()),
      ..Default::default()
    };
    assert_eq!(
      settings.sanitized().accent_color,
      Some("#00ff00".to_string())
    );
  }

  #[test]
  fn serializes_without_accent_color_when_none() {
    let settings = AppearanceSettings {
      theme: BrowserTheme::Dark,
      accent_color: None,
      ..Default::default()
    };
    let json = serde_json::to_string(&settings).expect("serialize AppearanceSettings");
    assert!(
      !json.contains("accent_color"),
      "expected accent_color to be omitted when None, got: {json}"
    );
  }
}

fn sanitize_ui_scale(raw: f32) -> f32 {
  if !raw.is_finite() || raw <= 0.0 {
    return DEFAULT_UI_SCALE;
  }
  raw.clamp(MIN_UI_SCALE, MAX_UI_SCALE)
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct AppearanceEnvOverrides {
  pub theme: Option<BrowserTheme>,
  pub accent: Option<RgbaColor>,
  pub ui_scale: Option<f32>,
  pub high_contrast: Option<bool>,
  pub reduced_motion: Option<bool>,
}

impl AppearanceEnvOverrides {
  pub fn from_env() -> Self {
    let toggles = runtime_toggles();
    let theme = parse_browser_theme_env(toggles.get(ENV_BROWSER_THEME));
    let accent = parse_browser_accent_env(toggles.get(ENV_BROWSER_ACCENT));

    let ui_scale = toggles
      .get(ENV_BROWSER_UI_SCALE)
      .and_then(parse_env_f32)
      .map(sanitize_ui_scale);

    // `Some(false)` means the env var was explicitly set to a falsey value.
    let high_contrast = toggles.get(ENV_BROWSER_HIGH_CONTRAST).map(parse_env_bool);

    let reduced_motion = motion::reduced_motion_override_from_env();

    Self {
      theme,
      accent,
      ui_scale,
      high_contrast,
      reduced_motion,
    }
  }
}
