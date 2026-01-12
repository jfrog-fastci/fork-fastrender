#![cfg(feature = "browser_ui")]

use crate::text::font_db::{FontDatabase, FontStyle, FontWeight, GenericFamily, LoadedFont};
use crate::ui::high_contrast;
use egui::{Color32, FontData, FontDefinitions, FontFamily, FontId, Stroke, Style};
use egui::epaint::Shadow;

pub const ENV_BROWSER_THEME: &str = "FASTR_BROWSER_THEME";
pub const ENV_BROWSER_ACCENT: &str = "FASTR_BROWSER_ACCENT";

/// Environment variable for overriding the browser chrome UI scale.
///
/// This affects only the egui-based chrome (font sizes / widget sizing) and intentionally does
/// **not** affect per-tab page zoom.
pub const ENV_UI_SCALE: &str = "FASTR_BROWSER_UI_SCALE";

/// Default UI scale factor when unset.
pub const DEFAULT_UI_SCALE: f32 = 1.0;

/// Minimum allowed UI scale factor.
pub const MIN_UI_SCALE: f32 = 0.75;

/// Maximum allowed UI scale factor.
pub const MAX_UI_SCALE: f32 = 2.0;

pub fn clamp_ui_scale(ui_scale: f32) -> f32 {
  if !ui_scale.is_finite() {
    return DEFAULT_UI_SCALE;
  }
  ui_scale.clamp(MIN_UI_SCALE, MAX_UI_SCALE)
}

pub fn ui_scale_from_str(raw: &str) -> Option<f32> {
  let raw = raw.trim();
  if raw.is_empty() {
    return None;
  }
  let raw = raw.replace('_', "");
  let value = raw.parse::<f32>().ok()?;
  (value.is_finite()).then_some(clamp_ui_scale(value))
}

/// Returns `Some(scale)` when the env var is set to a valid float, otherwise `None`.
pub fn ui_scale_from_env() -> Option<f32> {
  let raw = std::env::var(ENV_UI_SCALE).ok()?;
  ui_scale_from_str(&raw)
}

pub fn resolve_ui_scale(env_override: Option<f32>, session: Option<f32>) -> f32 {
  clamp_ui_scale(env_override.or(session).unwrap_or(DEFAULT_UI_SCALE))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeMode {
  System,
  Light,
  Dark,
}

impl std::str::FromStr for ThemeMode {
  type Err = String;

  fn from_str(raw: &str) -> Result<Self, Self::Err> {
    let value = raw.trim();
    if value.eq_ignore_ascii_case("system") {
      Ok(Self::System)
    } else if value.eq_ignore_ascii_case("light") {
      Ok(Self::Light)
    } else if value.eq_ignore_ascii_case("dark") {
      Ok(Self::Dark)
    } else {
      Err(format!("expected system|light|dark, got {raw:?}"))
    }
  }
}

#[derive(Debug, Clone)]
pub struct BrowserThemeColors {
  pub bg: Color32,
  pub surface: Color32,
  pub raised: Color32,
  pub text_primary: Color32,
  pub text_secondary: Color32,
  pub border: Color32,
  pub accent: Color32,
  pub danger: Color32,
  pub warn: Color32,
}

#[derive(Debug, Clone)]
pub struct BrowserThemeSizing {
  pub corner_radius: f32,
  pub padding: f32,
  pub stroke_width: f32,
}

#[derive(Debug, Clone)]
pub struct BrowserThemeTypography {
  pub base_font_size: f32,
  pub monospace_font_size: f32,
}

#[derive(Debug, Clone)]
pub struct BrowserTheme {
  pub mode: ThemeMode,
  pub high_contrast: bool,
  pub colors: BrowserThemeColors,
  pub sizing: BrowserThemeSizing,
  pub typography: BrowserThemeTypography,
}

impl BrowserTheme {
  pub fn light(accent: Option<Color32>) -> Self {
    Self::light_with_contrast(accent, false)
  }

  pub fn light_high_contrast(accent: Option<Color32>) -> Self {
    Self::light_with_contrast(accent, true)
  }

  fn light_with_contrast(accent: Option<Color32>, high_contrast_enabled: bool) -> Self {
    let accent = accent.unwrap_or_else(|| Color32::from_rgb(0x3B, 0x82, 0xF6)); // blue-500
    let tuning = high_contrast::theme_tuning(high_contrast_enabled);
    Self {
      mode: ThemeMode::Light,
      high_contrast: high_contrast_enabled,
      colors: BrowserThemeColors {
        bg: Color32::from_rgb(0xF5, 0xF6, 0xF8),
        surface: Color32::from_rgb(0xFF, 0xFF, 0xFF),
        raised: Color32::from_rgb(0xFF, 0xFF, 0xFF),
        text_primary: Color32::from_rgb(0x11, 0x18, 0x27),
        text_secondary: Color32::from_rgb(0x6B, 0x72, 0x80),
        border: if high_contrast_enabled {
          Color32::from_rgb(0x9C, 0xA3, 0xAF) // gray-400
        } else {
          Color32::from_rgb(0xE5, 0xE7, 0xEB) // gray-200
        },
        accent,
        danger: Color32::from_rgb(0xEF, 0x44, 0x44), // red-500
        warn: Color32::from_rgb(0xF5, 0x9E, 0x0B),   // amber-500
      },
      sizing: BrowserThemeSizing {
        corner_radius: 7.0,
        padding: 8.0,
        stroke_width: tuning.bg_stroke_width,
      },
      typography: BrowserThemeTypography {
        base_font_size: 14.0,
        monospace_font_size: 13.0,
      },
    }
  }

  pub fn dark(accent: Option<Color32>) -> Self {
    Self::dark_with_contrast(accent, false)
  }

  pub fn dark_high_contrast(accent: Option<Color32>) -> Self {
    Self::dark_with_contrast(accent, true)
  }

  fn dark_with_contrast(accent: Option<Color32>, high_contrast_enabled: bool) -> Self {
    let accent = accent.unwrap_or_else(|| Color32::from_rgb(0x60, 0xA5, 0xFA)); // blue-400
    let tuning = high_contrast::theme_tuning(high_contrast_enabled);
    Self {
      mode: ThemeMode::Dark,
      high_contrast: high_contrast_enabled,
      colors: BrowserThemeColors {
        bg: Color32::from_rgb(0x0B, 0x0F, 0x14),
        surface: Color32::from_rgb(0x11, 0x18, 0x27),
        raised: Color32::from_rgb(0x1F, 0x29, 0x37),
        text_primary: Color32::from_rgb(0xF9, 0xFA, 0xFB),
        text_secondary: Color32::from_rgb(0x9C, 0xA3, 0xAF),
        border: if high_contrast_enabled {
          Color32::from_rgb(0x6B, 0x72, 0x80) // gray-500
        } else {
          Color32::from_rgb(0x37, 0x41, 0x51)
        },
        accent,
        danger: Color32::from_rgb(0xF8, 0x71, 0x71), // red-400
        warn: Color32::from_rgb(0xFB, 0xBF, 0x24),   // amber-400
      },
      sizing: BrowserThemeSizing {
        corner_radius: 7.0,
        padding: 8.0,
        stroke_width: tuning.bg_stroke_width,
      },
      typography: BrowserThemeTypography {
        base_font_size: 14.0,
        monospace_font_size: 13.0,
      },
    }
  }
}

fn env_flag(var: &str) -> Option<bool> {
  std::env::var(var).ok().map(|v| {
    !matches!(v.as_str(), "0" | "false" | "False" | "FALSE" | "")
      && !v.eq_ignore_ascii_case("off")
  })
}

pub fn theme_mode_override_from_env() -> Option<ThemeMode> {
  let raw = std::env::var(ENV_BROWSER_THEME).ok()?;
  if raw.trim().is_empty() {
    return None;
  }
  match raw.parse::<ThemeMode>() {
    Ok(mode) => Some(mode),
    Err(err) => {
      eprintln!("{ENV_BROWSER_THEME}: {err}");
      None
    }
  }
}

fn parse_hex_color(raw: &str) -> Option<Color32> {
  let value = raw.trim();
  let value = value.strip_prefix('#').unwrap_or(value);
  let decode = |s: &str| u8::from_str_radix(s, 16).ok();

  match value.len() {
    3 => {
      let mut chars = value.chars();
      let r = chars.next()?;
      let g = chars.next()?;
      let b = chars.next()?;
      let rr = decode(&format!("{r}{r}"))?;
      let gg = decode(&format!("{g}{g}"))?;
      let bb = decode(&format!("{b}{b}"))?;
      Some(Color32::from_rgb(rr, gg, bb))
    }
    6 => {
      let rr = decode(value.get(0..2)?)?;
      let gg = decode(value.get(2..4)?)?;
      let bb = decode(value.get(4..6)?)?;
      Some(Color32::from_rgb(rr, gg, bb))
    }
    8 => {
      let rr = decode(value.get(0..2)?)?;
      let gg = decode(value.get(2..4)?)?;
      let bb = decode(value.get(4..6)?)?;
      let aa = decode(value.get(6..8)?)?;
      Some(Color32::from_rgba_unmultiplied(rr, gg, bb, aa))
    }
    _ => None,
  }
}

pub fn accent_color_override_from_env() -> Option<Color32> {
  let raw = std::env::var(ENV_BROWSER_ACCENT).ok()?;
  if raw.trim().is_empty() {
    return None;
  }
  match parse_hex_color(&raw) {
    Some(color) => Some(color),
    None => {
      eprintln!(
        "{ENV_BROWSER_ACCENT}: expected hex like #RRGGBB or #RRGGBBAA, got {raw:?}"
      );
      None
    }
  }
}

fn resolve_theme_mode_from_system_theme(
  system_theme: Option<winit::window::Theme>,
  override_mode: Option<ThemeMode>,
) -> ThemeMode {
  match override_mode.unwrap_or(ThemeMode::System) {
    ThemeMode::Light => ThemeMode::Light,
    ThemeMode::Dark => ThemeMode::Dark,
    ThemeMode::System => match system_theme {
      Some(winit::window::Theme::Dark) => ThemeMode::Dark,
      Some(winit::window::Theme::Light) => ThemeMode::Light,
      None => ThemeMode::Light,
    },
  }
}

pub fn resolve_theme_mode(
  window: &winit::window::Window,
  override_mode: Option<ThemeMode>,
) -> ThemeMode {
  resolve_theme_mode_from_system_theme(window.theme(), override_mode)
}

fn rgba_with_alpha(color: Color32, alpha: u8) -> Color32 {
  Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha)
}

fn try_load_face(db: &FontDatabase, families: &[&str]) -> Option<LoadedFont> {
  for family in families {
    if let Some(id) = db.query(family, FontWeight::NORMAL, FontStyle::Normal) {
      if let Some(font) = db.load_font(id) {
        return Some(font);
      }
    }
  }
  None
}

fn apply_fonts_to_definitions(
  definitions: &mut FontDefinitions,
  ui_font: &LoadedFont,
  mono_font: &LoadedFont,
) {
  const UI_KEY: &str = "fastr_ui";
  const MONO_KEY: &str = "fastr_mono";

  definitions.font_data.insert(
    UI_KEY.to_string(),
    FontData {
      font: std::borrow::Cow::Owned((*ui_font.data).clone()),
      index: ui_font.index,
      tweak: Default::default(),
    },
  );
  definitions.font_data.insert(
    MONO_KEY.to_string(),
    FontData {
      font: std::borrow::Cow::Owned((*mono_font.data).clone()),
      index: mono_font.index,
      tweak: Default::default(),
    },
  );

  definitions
    .families
    .entry(FontFamily::Proportional)
    .or_default()
    .insert(0, UI_KEY.to_string());
  definitions
    .families
    .entry(FontFamily::Monospace)
    .or_default()
    .insert(0, MONO_KEY.to_string());
}

fn build_font_definitions_from_dbs(
  system_db: Option<&FontDatabase>,
  bundled_db: &FontDatabase,
) -> FontDefinitions {
  let mut definitions = FontDefinitions::default();

  let ui_candidates: [&str; 3] = ["system-ui", "ui-sans-serif", "sans-serif"];
  let mut ui_fallbacks: Vec<&str> = Vec::new();
  ui_fallbacks.extend(GenericFamily::SystemUi.fallback_families());
  ui_fallbacks.extend(GenericFamily::UiSansSerif.fallback_families());
  // Explicit bundled faces (when a system UI face is unavailable).
  ui_fallbacks.extend(["Roboto Flex", "Noto Sans", "DejaVu Sans"]);
  // Prefer the explicit generic names first so fontdb generic overrides (when present) win.
  let mut ui_families: Vec<&str> = Vec::new();
  ui_families.extend(ui_candidates);
  ui_families.extend(ui_fallbacks);

  let mono_candidates: [&str; 2] = ["ui-monospace", "monospace"];
  let mut mono_families: Vec<&str> = Vec::new();
  mono_families.extend(mono_candidates);
  mono_families.extend(GenericFamily::UiMonospace.fallback_families());
  mono_families.extend(["Noto Sans Mono", "DejaVu Sans Mono"]);

  let mut ui_font = None;
  let mut mono_font = None;

  if let Some(db) = system_db {
    ui_font = try_load_face(db, &ui_families);
    mono_font = try_load_face(db, &mono_families);
  }

  if ui_font.is_none() || mono_font.is_none() {
    ui_font = ui_font.or_else(|| try_load_face(bundled_db, &ui_families));
    mono_font = mono_font.or_else(|| try_load_face(bundled_db, &mono_families));
  }

  if let (Some(ui_font), Some(mono_font)) = (ui_font, mono_font) {
    apply_fonts_to_definitions(&mut definitions, &ui_font, &mono_font);
  }

  definitions
}

fn build_font_definitions() -> FontDefinitions {
  // The browser integration tests set `FASTR_USE_BUNDLED_FONTS=1` to avoid expensive system font
  // scans; respect that here so `apply_browser_theme` stays cheap/deterministic under tests/CI.
  let allow_system_fonts =
    !env_flag("FASTR_USE_BUNDLED_FONTS").unwrap_or(false) && !env_flag("CI").unwrap_or(false);

  if allow_system_fonts {
    let system_db = FontDatabase::shared_system();
    let bundled_db = FontDatabase::shared_bundled();
    build_font_definitions_from_dbs(Some(&system_db), &bundled_db)
  } else {
    let bundled_db = FontDatabase::shared_bundled();
    build_font_definitions_from_dbs(None, &bundled_db)
  }
}

pub fn apply_browser_theme(ctx: &egui::Context, theme: &BrowserTheme) {
  let ui_scale = DEFAULT_UI_SCALE;
  apply_browser_theme_with_ui_scale(ctx, theme, ui_scale);
}

pub fn apply_browser_theme_with_ui_scale(ctx: &egui::Context, theme: &BrowserTheme, ui_scale: f32) {
  let ui_scale = clamp_ui_scale(ui_scale);
  ctx.set_fonts(build_font_definitions());

  let mut style: Style = (*ctx.style()).clone();

  // Typography.
  let base = theme.typography.base_font_size * ui_scale;
  let mono = theme.typography.monospace_font_size * ui_scale;
  style.text_styles.insert(
    egui::TextStyle::Body,
    FontId::new(base, FontFamily::Proportional),
  );
  style.text_styles.insert(
    egui::TextStyle::Button,
    FontId::new(base, FontFamily::Proportional),
  );
  style.text_styles.insert(
    egui::TextStyle::Monospace,
    FontId::new(mono, FontFamily::Monospace),
  );
  style.text_styles.insert(
    egui::TextStyle::Small,
    FontId::new(base * 0.9, FontFamily::Proportional),
  );
  style.text_styles.insert(
    egui::TextStyle::Heading,
    FontId::new(base * 1.25, FontFamily::Proportional),
  );

  // Spacing / sizing.
  style.spacing.item_spacing = egui::vec2(theme.sizing.padding, theme.sizing.padding * 0.75);
  style.spacing.button_padding = egui::vec2(theme.sizing.padding, theme.sizing.padding * 0.65);
  style.spacing.window_margin = egui::Margin::same(theme.sizing.padding);
  style.spacing.menu_margin = egui::Margin::symmetric(theme.sizing.padding, theme.sizing.padding * 0.5);
  style.spacing.scroll_bar_width = 10.0;

  // Visuals.
  let mut visuals = match theme.mode {
    ThemeMode::Dark => egui::Visuals::dark(),
    _ => egui::Visuals::light(),
  };

  visuals.override_text_color = Some(theme.colors.text_primary);
  visuals.hyperlink_color = theme.colors.accent;
  visuals.faint_bg_color = theme.colors.surface;
  visuals.extreme_bg_color = theme.colors.bg;
  visuals.code_bg_color = theme.colors.raised;
  visuals.warn_fg_color = theme.colors.warn;
  visuals.error_fg_color = theme.colors.danger;

  visuals.panel_fill = theme.colors.bg;
  visuals.window_fill = theme.colors.raised;
  visuals.window_stroke = Stroke::new(theme.sizing.stroke_width, theme.colors.border);

  visuals.window_rounding = egui::Rounding::same(theme.sizing.corner_radius);
  visuals.menu_rounding = egui::Rounding::same(theme.sizing.corner_radius);

  // Popups: subtle depth.
  visuals.popup_shadow = Shadow {
    extrusion: 12.0,
    color: rgba_with_alpha(Color32::BLACK, if matches!(theme.mode, ThemeMode::Dark) { 90 } else { 40 }),
  };
  visuals.window_shadow = visuals.popup_shadow;

  // Selection + focus.
  let tuning = high_contrast::theme_tuning(theme.high_contrast);
  visuals.selection.bg_fill = rgba_with_alpha(theme.colors.accent, tuning.selection_bg_alpha);
  visuals.selection.stroke = Stroke::new(
    tuning.focus_stroke_width,
    rgba_with_alpha(theme.colors.accent, tuning.focus_stroke_alpha),
  );

  let rounding = egui::Rounding::same(theme.sizing.corner_radius);
  let stroke = Stroke::new(theme.sizing.stroke_width, theme.colors.border);
  let hovered_stroke = Stroke::new(
    theme.sizing.stroke_width,
    rgba_with_alpha(theme.colors.accent, tuning.hover_stroke_alpha),
  );
  let active_stroke = Stroke::new(theme.sizing.stroke_width, theme.colors.accent);

  visuals.widgets.noninteractive.rounding = rounding;
  visuals.widgets.inactive.rounding = rounding;
  visuals.widgets.hovered.rounding = rounding;
  visuals.widgets.active.rounding = rounding;
  visuals.widgets.open.rounding = rounding;

  visuals.widgets.noninteractive.bg_fill = theme.colors.bg;
  visuals.widgets.noninteractive.bg_stroke = stroke;
  visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, theme.colors.text_primary);

  visuals.widgets.inactive.bg_fill = theme.colors.surface;
  visuals.widgets.inactive.bg_stroke = stroke;
  visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, theme.colors.text_primary);

  visuals.widgets.hovered.bg_fill = theme.colors.raised;
  visuals.widgets.hovered.bg_stroke = hovered_stroke;
  visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, theme.colors.text_primary);

  visuals.widgets.active.bg_fill = theme.colors.raised;
  visuals.widgets.active.bg_stroke = active_stroke;
  visuals.widgets.active.fg_stroke = Stroke::new(1.0, theme.colors.text_primary);

  visuals.widgets.open.bg_fill = theme.colors.raised;
  visuals.widgets.open.bg_stroke = hovered_stroke;
  visuals.widgets.open.fg_stroke = Stroke::new(1.0, theme.colors.text_primary);

  style.visuals = visuals;
  ctx.set_style(style);
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::Mutex;

  static ENV_LOCK: Mutex<()> = Mutex::new(());

  #[test]
  fn parse_browser_theme_env() {
    assert_eq!("system".parse::<ThemeMode>().unwrap(), ThemeMode::System);
    assert_eq!("light".parse::<ThemeMode>().unwrap(), ThemeMode::Light);
    assert_eq!("dark".parse::<ThemeMode>().unwrap(), ThemeMode::Dark);
    assert_eq!(" DARK ".parse::<ThemeMode>().unwrap(), ThemeMode::Dark);
    assert!("auto".parse::<ThemeMode>().is_err());
  }

  #[test]
  fn resolve_theme_mode_prefers_override() {
    use winit::window::Theme as WinitTheme;

    assert_eq!(
      resolve_theme_mode_from_system_theme(Some(WinitTheme::Dark), None),
      ThemeMode::Dark
    );
    assert_eq!(
      resolve_theme_mode_from_system_theme(Some(WinitTheme::Dark), Some(ThemeMode::System)),
      ThemeMode::Dark
    );
    assert_eq!(
      resolve_theme_mode_from_system_theme(Some(WinitTheme::Dark), Some(ThemeMode::Light)),
      ThemeMode::Light
    );
    assert_eq!(
      resolve_theme_mode_from_system_theme(None, Some(ThemeMode::System)),
      ThemeMode::Light
    );
  }

  #[test]
  fn font_loading_falls_back_to_bundled_fonts() {
    // Simulate "no system fonts" (or system discovery disabled) and ensure we can still build
    // egui font definitions without panicking by using bundled fonts.
    let system_db = FontDatabase::empty();
    let bundled_db = FontDatabase::shared_bundled();
    let defs = build_font_definitions_from_dbs(Some(&system_db), &bundled_db);

    let proportional = defs
      .families
      .get(&egui::FontFamily::Proportional)
      .expect("expected proportional family to exist");
    let monospace = defs
      .families
      .get(&egui::FontFamily::Monospace)
      .expect("expected monospace family to exist");
    assert!(
      proportional.iter().any(|name| name == "fastr_ui"),
      "expected fallback UI font to be installed"
    );
    assert!(
      monospace.iter().any(|name| name == "fastr_mono"),
      "expected fallback monospace font to be installed"
    );
  }

  #[test]
  fn parse_hex_color_accepts_rgb_and_rgba() {
    assert_eq!(
      parse_hex_color("#ff0000"),
      Some(egui::Color32::from_rgb(0xFF, 0x00, 0x00))
    );
    assert_eq!(
      parse_hex_color("0f0"),
      Some(egui::Color32::from_rgb(0x00, 0xFF, 0x00))
    );
    assert_eq!(
      parse_hex_color("#11223344"),
      Some(egui::Color32::from_rgba_unmultiplied(0x11, 0x22, 0x33, 0x44))
    );
    assert_eq!(parse_hex_color("not-a-color"), None);
    assert_eq!(parse_hex_color("#12"), None);
  }

  #[test]
  fn clamp_ui_scale_defaults_and_clamps() {
    assert_eq!(clamp_ui_scale(f32::NAN), DEFAULT_UI_SCALE);
    assert_eq!(clamp_ui_scale(f32::INFINITY), DEFAULT_UI_SCALE);
    assert_eq!(clamp_ui_scale(f32::NEG_INFINITY), DEFAULT_UI_SCALE);

    assert_eq!(clamp_ui_scale(0.1), MIN_UI_SCALE);
    assert_eq!(clamp_ui_scale(10.0), MAX_UI_SCALE);
    assert_eq!(clamp_ui_scale(1.25), 1.25);
  }

  #[test]
  fn ui_scale_from_str_parses_and_clamps() {
    assert_eq!(ui_scale_from_str(""), None);
    assert_eq!(ui_scale_from_str("   "), None);
    assert_eq!(ui_scale_from_str("nope"), None);
    assert_eq!(ui_scale_from_str("NaN"), None);

    assert_eq!(ui_scale_from_str("1.0"), Some(1.0));
    assert_eq!(ui_scale_from_str(" 1.25 "), Some(1.25));
    assert_eq!(ui_scale_from_str("0.5"), Some(MIN_UI_SCALE));
    assert_eq!(ui_scale_from_str("2.5"), Some(MAX_UI_SCALE));
  }

  #[test]
  fn ui_scale_from_env_parses_and_clamps() {
    let _lock = ENV_LOCK.lock().unwrap();

    let original = std::env::var_os(ENV_UI_SCALE);

    std::env::remove_var(ENV_UI_SCALE);
    assert_eq!(ui_scale_from_env(), None);

    std::env::set_var(ENV_UI_SCALE, "1.5");
    assert_eq!(ui_scale_from_env(), Some(1.5));

    std::env::set_var(ENV_UI_SCALE, "1000");
    assert_eq!(ui_scale_from_env(), Some(MAX_UI_SCALE));

    std::env::set_var(ENV_UI_SCALE, "nope");
    assert_eq!(ui_scale_from_env(), None);

    match original {
      Some(v) => std::env::set_var(ENV_UI_SCALE, v),
      None => std::env::remove_var(ENV_UI_SCALE),
    }
  }
}
