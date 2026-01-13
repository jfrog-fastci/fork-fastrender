//! Helpers for mapping browser UI appearance settings onto renderer media preferences.
//!
//! The windowed browser UI (chrome) has appearance/accessibility settings such as theme (light/dark),
//! high-contrast mode, and reduced-motion mode. Rendered web content should observe the same user
//! preferences via CSS media queries:
//! - `prefers-color-scheme`
//! - `prefers-contrast`
//! - `prefers-reduced-motion`
//!
//! The renderer reads these preferences from the active [`crate::debug::runtime::RuntimeToggles`]
//! (typically derived from `FASTR_*` environment variables). The browser UI installs a global
//! `RuntimeToggles` override that fills in missing `FASTR_PREFERS_*` values based on the resolved
//! chrome appearance while still allowing explicit `FASTR_PREFERS_*` env overrides to win.

/// `FASTR_*` env var name for overriding `prefers-color-scheme`.
pub const ENV_PREFERS_COLOR_SCHEME: &str = "FASTR_PREFERS_COLOR_SCHEME";
/// `FASTR_*` env var name for overriding `prefers-contrast`.
pub const ENV_PREFERS_CONTRAST: &str = "FASTR_PREFERS_CONTRAST";
/// `FASTR_*` env var name for overriding `prefers-reduced-motion`.
pub const ENV_PREFERS_REDUCED_MOTION: &str = "FASTR_PREFERS_REDUCED_MOTION";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedTheme {
  Light,
  Dark,
}

/// Map resolved browser appearance settings to `FASTR_PREFERS_*` env var key/value pairs.
///
/// Callers are expected to apply the returned pairs *only when* the corresponding keys are missing
/// from the process environment, so explicit renderer overrides still win.
pub fn prefers_env_vars_for_appearance(
  resolved_theme: ResolvedTheme,
  high_contrast: bool,
  reduced_motion: bool,
) -> [(&'static str, &'static str); 3] {
  let scheme = match resolved_theme {
    ResolvedTheme::Light => "light",
    ResolvedTheme::Dark => "dark",
  };

  // Per Media Queries Level 5, `prefers-contrast` uses `more|less|custom|no-preference`.
  let contrast = if high_contrast { "more" } else { "no-preference" };

  // Per Media Queries Level 5, `prefers-reduced-motion` uses `reduce|no-preference`.
  let motion = if reduced_motion { "reduce" } else { "no-preference" };

  [
    (ENV_PREFERS_COLOR_SCHEME, scheme),
    (ENV_PREFERS_CONTRAST, contrast),
    (ENV_PREFERS_REDUCED_MOTION, motion),
  ]
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::debug::runtime::RuntimeToggles;
  use crate::style::media::{ColorScheme, ContrastPreference};
  use std::collections::HashMap;

  #[test]
  fn maps_appearance_to_env_vars() {
    assert_eq!(
      prefers_env_vars_for_appearance(ResolvedTheme::Dark, true, true),
      [
        (ENV_PREFERS_COLOR_SCHEME, "dark"),
        (ENV_PREFERS_CONTRAST, "more"),
        (ENV_PREFERS_REDUCED_MOTION, "reduce"),
      ]
    );

    assert_eq!(
      prefers_env_vars_for_appearance(ResolvedTheme::Light, false, false),
      [
        (ENV_PREFERS_COLOR_SCHEME, "light"),
        (ENV_PREFERS_CONTRAST, "no-preference"),
        (ENV_PREFERS_REDUCED_MOTION, "no-preference"),
      ]
    );
  }

  #[test]
  fn inserted_env_vars_parse_into_runtime_toggles_media_overrides() {
    let mut raw: HashMap<String, String> = HashMap::new();
    for (k, v) in prefers_env_vars_for_appearance(ResolvedTheme::Dark, true, true) {
      raw.insert(k.to_string(), v.to_string());
    }

    let toggles = RuntimeToggles::from_map(raw);
    let media = &toggles.config().media;
    assert_eq!(media.prefers_color_scheme, Some(ColorScheme::Dark));
    assert_eq!(media.prefers_contrast, Some(ContrastPreference::More));
    assert_eq!(media.prefers_reduced_motion, Some(true));
  }
}

