use fastrender::debug::runtime::{with_runtime_toggles, RuntimeToggles};
use fastrender::ui::appearance::{
  AppearanceEnvOverrides, AppearanceSettings, DEFAULT_UI_SCALE, ENV_BROWSER_HIGH_CONTRAST,
  ENV_BROWSER_UI_SCALE, MAX_UI_SCALE, MIN_UI_SCALE,
};
use fastrender::ui::motion::{UiMotion, ENV_REDUCED_MOTION};
use fastrender::ui::theme_parsing::{BrowserTheme, ENV_BROWSER_THEME};
use std::collections::HashMap;
use std::sync::Arc;

#[test]
fn appearance_env_overrides_take_precedence_over_persisted_settings() {
  let toggles = RuntimeToggles::from_map(HashMap::from([
    (ENV_BROWSER_THEME.to_string(), "dark".to_string()),
    (ENV_BROWSER_UI_SCALE.to_string(), "2.0".to_string()),
    (ENV_BROWSER_HIGH_CONTRAST.to_string(), "1".to_string()),
    (ENV_REDUCED_MOTION.to_string(), "1".to_string()),
  ]));

  with_runtime_toggles(Arc::new(toggles), || {
    let persisted = AppearanceSettings {
      theme: BrowserTheme::Light,
      ui_scale: 1.25,
      high_contrast: false,
      reduced_motion: false,
    };

    let env = AppearanceEnvOverrides::from_env();
    let effective = persisted.with_env_overrides(env);

    assert_eq!(effective.theme, BrowserTheme::Dark);
    assert!((effective.ui_scale - 2.0).abs() < 1e-6);
    assert!(effective.high_contrast);
    assert!(effective.reduced_motion);
  });
}

#[test]
fn appearance_ui_scale_is_sanitized_and_clamped() {
  assert_eq!(
    AppearanceSettings {
      ui_scale: MAX_UI_SCALE + 10.0,
      ..AppearanceSettings::default()
    }
    .sanitized()
    .ui_scale,
    MAX_UI_SCALE
  );

  assert_eq!(
    AppearanceSettings {
      ui_scale: MIN_UI_SCALE - 0.1,
      ..AppearanceSettings::default()
    }
    .sanitized()
    .ui_scale,
    MIN_UI_SCALE
  );

  assert_eq!(
    AppearanceSettings {
      ui_scale: f32::NAN,
      ..AppearanceSettings::default()
    }
    .sanitized()
    .ui_scale,
    DEFAULT_UI_SCALE
  );
}

#[test]
fn reduced_motion_env_override_beats_settings() {
  with_runtime_toggles(
    Arc::new(RuntimeToggles::from_map(HashMap::from([(
      ENV_REDUCED_MOTION.to_string(),
      "1".to_string(),
    )]))),
    || {
      assert!(
        !UiMotion::from_settings(false).enabled,
        "expected env=true to force reduced motion, overriding settings"
      );
    },
  );

  with_runtime_toggles(
    Arc::new(RuntimeToggles::from_map(HashMap::from([(
      ENV_REDUCED_MOTION.to_string(),
      "0".to_string(),
    )]))),
    || {
      assert!(
        UiMotion::from_settings(true).enabled,
        "expected env=false to force motion enabled, overriding settings"
      );
    },
  );
}

