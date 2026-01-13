//! Browser UI → renderer media preference plumbing.
//!
//! The windowed browser UI exposes appearance/accessibility settings (theme, high contrast, reduced
//! motion). These should influence the page’s media-query surface (`prefers-*`) so sites can adapt
//! to the user’s preferences.
//!
//! Precedence:
//! - Explicit renderer overrides via `FASTR_PREFERS_*` env vars (captured in `RuntimeToggles`) win.
//! - Browser UI settings act as defaults only when those overrides are unset.

use crate::debug::runtime::{MediaOverrides, RuntimeToggles};
use crate::style::media::{ColorScheme, ContrastPreference};
use crate::ui::appearance::AppearanceSettings;
use crate::ui::messages::BrowserMediaPreferences;
use crate::ui::theme_parsing::BrowserTheme;
use std::sync::Arc;

/// Lightweight system theme snapshot used for mapping `BrowserTheme::System` to a concrete
/// light/dark preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemTheme {
  Light,
  Dark,
  Unknown,
}

impl SystemTheme {
  /// Convert from the optional winit system theme value.
  #[cfg(feature = "browser_ui")]
  pub fn from_winit(theme: Option<winit::window::Theme>) -> Self {
    match theme {
      Some(winit::window::Theme::Dark) => SystemTheme::Dark,
      Some(winit::window::Theme::Light) => SystemTheme::Light,
      None => SystemTheme::Unknown,
    }
  }
}

/// Map persisted browser appearance settings to page media preference defaults.
pub fn media_prefs_from_appearance(
  appearance: &AppearanceSettings,
  system_theme: SystemTheme,
) -> BrowserMediaPreferences {
  let prefers_color_scheme = match appearance.theme {
    BrowserTheme::Light => ColorScheme::Light,
    BrowserTheme::Dark => ColorScheme::Dark,
    BrowserTheme::System => match system_theme {
      SystemTheme::Dark => ColorScheme::Dark,
      SystemTheme::Light | SystemTheme::Unknown => ColorScheme::Light,
    },
  };

  let prefers_contrast = if appearance.high_contrast {
    // Map the browser’s high-contrast chrome mode to a "more contrast" page preference.
    ContrastPreference::More
  } else {
    ContrastPreference::NoPreference
  };

  BrowserMediaPreferences {
    prefers_color_scheme,
    prefers_contrast,
    prefers_reduced_motion: appearance.reduced_motion,
  }
}

/// Merge browser-provided defaults with explicit renderer overrides from `RuntimeToggles`.
///
/// The returned preferences are the *effective* values that should be exposed to pages.
pub fn merge_media_prefs_with_env_overrides(
  env_overrides: &MediaOverrides,
  ui_defaults: BrowserMediaPreferences,
) -> BrowserMediaPreferences {
  BrowserMediaPreferences {
    prefers_color_scheme: env_overrides
      .prefers_color_scheme
      .unwrap_or(ui_defaults.prefers_color_scheme),
    prefers_contrast: env_overrides
      .prefers_contrast
      .unwrap_or(ui_defaults.prefers_contrast),
    prefers_reduced_motion: env_overrides
      .prefers_reduced_motion
      .unwrap_or(ui_defaults.prefers_reduced_motion),
  }
}

/// Construct a derived `RuntimeToggles` instance that applies the given browser defaults while
/// preserving all other `FASTR_*` configuration.
pub(crate) fn runtime_toggles_with_browser_media_prefs(
  base: &Arc<RuntimeToggles>,
  ui_defaults: BrowserMediaPreferences,
) -> Arc<RuntimeToggles> {
  let effective = merge_media_prefs_with_env_overrides(&base.config().media, ui_defaults);

  let mut raw = base.raw_clone();
  raw.insert(
    "FASTR_PREFERS_COLOR_SCHEME".to_string(),
    effective.prefers_color_scheme.to_string(),
  );
  raw.insert(
    "FASTR_PREFERS_CONTRAST".to_string(),
    effective.prefers_contrast.to_string(),
  );
  raw.insert(
    "FASTR_PREFERS_REDUCED_MOTION".to_string(),
    if effective.prefers_reduced_motion {
      "reduce".to_string()
    } else {
      "no-preference".to_string()
    },
  );

  Arc::new(RuntimeToggles::from_map(raw))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::debug::runtime::RuntimeToggles;
  use std::collections::HashMap;

  #[test]
  fn appearance_mapping_theme_and_accessibility() {
    let base = AppearanceSettings::default();
    assert_eq!(
      media_prefs_from_appearance(&base, SystemTheme::Light),
      BrowserMediaPreferences {
        prefers_color_scheme: ColorScheme::Light,
        prefers_contrast: ContrastPreference::NoPreference,
        prefers_reduced_motion: false,
      }
    );

    let mut dark = base.clone();
    dark.theme = BrowserTheme::Dark;
    assert_eq!(
      media_prefs_from_appearance(&dark, SystemTheme::Light).prefers_color_scheme,
      ColorScheme::Dark
    );

    let mut sys = base.clone();
    sys.theme = BrowserTheme::System;
    assert_eq!(
      media_prefs_from_appearance(&sys, SystemTheme::Dark).prefers_color_scheme,
      ColorScheme::Dark
    );
    assert_eq!(
      media_prefs_from_appearance(&sys, SystemTheme::Unknown).prefers_color_scheme,
      ColorScheme::Light
    );

    let mut hc = base.clone();
    hc.high_contrast = true;
    assert_eq!(
      media_prefs_from_appearance(&hc, SystemTheme::Light).prefers_contrast,
      ContrastPreference::More
    );

    let mut rm = base.clone();
    rm.reduced_motion = true;
    assert!(media_prefs_from_appearance(&rm, SystemTheme::Light).prefers_reduced_motion);
  }

  #[test]
  fn env_overrides_take_precedence_over_ui_defaults() {
    let env = MediaOverrides {
      prefers_color_scheme: Some(ColorScheme::Dark),
      prefers_contrast: Some(ContrastPreference::Less),
      prefers_reduced_motion: Some(false),
      ..MediaOverrides::default()
    };

    let ui = BrowserMediaPreferences {
      prefers_color_scheme: ColorScheme::Light,
      prefers_contrast: ContrastPreference::More,
      prefers_reduced_motion: true,
    };

    let merged = merge_media_prefs_with_env_overrides(&env, ui);
    assert_eq!(merged.prefers_color_scheme, ColorScheme::Dark);
    assert_eq!(merged.prefers_contrast, ContrastPreference::Less);
    assert!(!merged.prefers_reduced_motion);
  }

  #[test]
  fn runtime_toggles_helper_preserves_existing_keys() {
    let base = Arc::new(RuntimeToggles::from_map(HashMap::from([(
      "FASTR_SOME_OTHER_FLAG".to_string(),
      "1".to_string(),
    )])));

    let ui = BrowserMediaPreferences {
      prefers_color_scheme: ColorScheme::Dark,
      prefers_contrast: ContrastPreference::More,
      prefers_reduced_motion: true,
    };

    let derived = runtime_toggles_with_browser_media_prefs(&base, ui);
    assert_eq!(derived.get("FASTR_SOME_OTHER_FLAG"), Some("1"));
    assert_eq!(
      derived.config().media.prefers_color_scheme,
      Some(ColorScheme::Dark)
    );
    assert_eq!(
      derived.config().media.prefers_contrast,
      Some(ContrastPreference::More)
    );
    assert_eq!(derived.config().media.prefers_reduced_motion, Some(true));
  }
}
