//! High-contrast mode parsing + theme tuning helpers.
//!
//! The browser UI uses this to opt into a stronger-contrast theme (borders, selection highlight,
//! focus rings) for accessibility. Keep this module free of egui/winit dependencies so it can be
//! unit-tested without enabling the `browser_ui` feature.

/// Environment variable that enables a high-contrast chrome theme.
pub const ENV_BROWSER_HIGH_CONTRAST: &str = "FASTR_BROWSER_HIGH_CONTRAST";

/// Parse a truthy/falsey environment variable value.
///
/// This matches the permissive parsing used by other browser UI env bools:
/// - Unset / empty / `0` / `false` / `no` / `off` => false
/// - Anything else => true
pub fn parse_env_bool(value: &str) -> bool {
  let v = value.trim();
  if v.is_empty() {
    return false;
  }

  !(v.eq_ignore_ascii_case("0")
    || v.eq_ignore_ascii_case("false")
    || v.eq_ignore_ascii_case("no")
    || v.eq_ignore_ascii_case("off"))
}

/// Parse an optional env var string using [`parse_env_bool`].
pub fn parse_env_bool_opt(value: Option<&str>) -> bool {
  value.is_some_and(parse_env_bool)
}

/// Parse `FASTR_BROWSER_HIGH_CONTRAST` (default: false).
pub fn parse_high_contrast_env(raw: Option<&str>) -> bool {
  parse_env_bool_opt(raw)
}

/// Read `FASTR_BROWSER_HIGH_CONTRAST` from the process environment (default: false).
pub fn high_contrast_enabled_from_env() -> bool {
  let raw = std::env::var(ENV_BROWSER_HIGH_CONTRAST).ok();
  parse_high_contrast_env(raw.as_deref())
}

/// Theme tuning parameters used by `src/ui/theme.rs` when high-contrast mode is enabled.
///
/// These are intentionally expressed in primitive types (no egui types) so unit tests can compare
/// them deterministically without pulling in `egui`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HighContrastThemeTuning {
  /// Base widget/background stroke width (used for borders).
  pub bg_stroke_width: f32,
  /// Focus ring stroke width.
  pub focus_stroke_width: f32,
  /// Alpha used for the focus ring color.
  pub focus_stroke_alpha: u8,
  /// Alpha used for selection background fills (e.g. selected rows).
  pub selection_bg_alpha: u8,
  /// Alpha used for hovered widget border strokes.
  pub hover_stroke_alpha: u8,
}

/// Resolve the theme tuning values for the given high-contrast setting.
pub fn theme_tuning(high_contrast: bool) -> HighContrastThemeTuning {
  if high_contrast {
    HighContrastThemeTuning {
      bg_stroke_width: 2.0,
      focus_stroke_width: 2.0,
      focus_stroke_alpha: 255,
      selection_bg_alpha: 140,
      hover_stroke_alpha: 220,
    }
  } else {
    HighContrastThemeTuning {
      bg_stroke_width: 1.0,
      focus_stroke_width: 1.0,
      focus_stroke_alpha: 230,
      selection_bg_alpha: 90,
      hover_stroke_alpha: 180,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::{parse_env_bool, parse_high_contrast_env, theme_tuning};

  #[test]
  fn high_contrast_env_parsing_truthy_and_falsey_values() {
    for v in [
      None,
      Some(""),
      Some("   "),
      Some("0"),
      Some("false"),
      Some("FALSE"),
      Some("no"),
      Some("off"),
      Some(" 0 "),
      Some(" off "),
    ] {
      assert!(!parse_high_contrast_env(v), "expected {v:?} to be parsed as false");
    }

    for v in [
      Some("1"),
      Some("true"),
      Some("TRUE"),
      Some("yes"),
      Some("on"),
      Some("anything"),
      Some(" 1 "),
    ] {
      assert!(parse_high_contrast_env(v), "expected {v:?} to be parsed as true");
    }

    // `parse_env_bool` is the primitive used by the env parser; keep its trimming behaviour covered.
    assert!(!parse_env_bool("  "));
    assert!(!parse_env_bool("  false "));
    assert!(parse_env_bool("  true "));
  }

  #[test]
  fn high_contrast_theme_tuning_increases_contrast_and_focus_strength() {
    let normal = theme_tuning(false);
    let high = theme_tuning(true);

    assert!(
      high.bg_stroke_width > normal.bg_stroke_width,
      "expected high-contrast bg_stroke_width to be stronger ({} > {})",
      high.bg_stroke_width,
      normal.bg_stroke_width
    );
    assert!(
      high.focus_stroke_width > normal.focus_stroke_width,
      "expected high-contrast focus_stroke_width to be stronger ({} > {})",
      high.focus_stroke_width,
      normal.focus_stroke_width
    );
    assert!(
      high.focus_stroke_alpha > normal.focus_stroke_alpha,
      "expected high-contrast focus_stroke_alpha to be higher ({} > {})",
      high.focus_stroke_alpha,
      normal.focus_stroke_alpha
    );
    assert!(
      high.selection_bg_alpha > normal.selection_bg_alpha,
      "expected high-contrast selection_bg_alpha to be higher ({} > {})",
      high.selection_bg_alpha,
      normal.selection_bg_alpha
    );
    assert!(
      high.hover_stroke_alpha > normal.hover_stroke_alpha,
      "expected high-contrast hover_stroke_alpha to be higher ({} > {})",
      high.hover_stroke_alpha,
      normal.hover_stroke_alpha
    );
  }
}
