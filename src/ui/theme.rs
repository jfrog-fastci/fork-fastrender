#![cfg(feature = "browser_ui")]

use crate::text::font_db::{FontDatabase, FontStyle, FontWeight, GenericFamily, LoadedFont};
use crate::debug::runtime::runtime_toggles;
use crate::ui::high_contrast;
use crate::ui::theme_parsing;
use egui::{Color32, FontData, FontDefinitions, FontFamily, FontId, Stroke, Style};
use egui::epaint::Shadow;

pub const ENV_BROWSER_THEME: &str = theme_parsing::ENV_BROWSER_THEME;
pub const ENV_BROWSER_ACCENT: &str = theme_parsing::ENV_BROWSER_ACCENT;

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
  let toggles = runtime_toggles();
  let raw = toggles.get(ENV_UI_SCALE)?;
  ui_scale_from_str(raw)
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

pub fn accent_color_override_from_env() -> Option<Color32> {
  let raw = std::env::var(ENV_BROWSER_ACCENT).ok()?;
  if raw.trim().is_empty() {
    return None;
  }
  match theme_parsing::parse_hex_color(&raw) {
    Some(color) => Some(color.to_color32()),
    None => {
      eprintln!(
        "{ENV_BROWSER_ACCENT}: expected hex like #RGB, #RRGGBB, or #RRGGBBAA, got {raw:?}"
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

fn selection_bg_fill(theme: &BrowserTheme) -> Color32 {
  let tuning = high_contrast::theme_tuning(theme.high_contrast);
  rgba_with_alpha(theme.colors.accent, tuning.selection_bg_alpha)
}

fn selection_stroke(theme: &BrowserTheme) -> Stroke {
  let tuning = high_contrast::theme_tuning(theme.high_contrast);
  Stroke::new(
    tuning.focus_stroke_width,
    rgba_with_alpha(theme.colors.accent, tuning.focus_stroke_alpha),
  )
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
  // When running in deterministic/bundled mode (CI or `FASTR_USE_BUNDLED_FONTS=1`), avoid scanning
  // system fonts. System font discovery is expensive and makes UI rendering less predictable.
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
  let stroke_width = if theme.high_contrast {
    theme.sizing.stroke_width.max(2.0)
  } else {
    theme.sizing.stroke_width
  };

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
  visuals.window_stroke = Stroke::new(stroke_width, theme.colors.border);

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
  visuals.selection.bg_fill = selection_bg_fill(theme);
  visuals.selection.stroke = selection_stroke(theme);

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

/// Returns whether high contrast mode is enabled via `FASTR_BROWSER_HIGH_CONTRAST=1`.
///
/// Invalid values are treated as "off" (we don't want UI rendering to go fallible just because an
/// env var is misconfigured).
pub fn high_contrast_enabled() -> bool {
  match std::env::var(theme_parsing::ENV_BROWSER_HIGH_CONTRAST) {
    Ok(raw) => theme_parsing::parse_high_contrast_env(Some(&raw)).unwrap_or(false),
    Err(_) => false,
  }
}

fn high_contrast_visuals(base: &egui::Visuals) -> egui::Visuals {
  let dark_mode = base.dark_mode;
  let mut visuals = base.clone();

  // Base surfaces + text.
  let (bg, fg, button_bg, button_bg_hover, button_bg_active, border, selection_bg, selection_fg) =
    if dark_mode {
      (
        Color32::BLACK,
        Color32::WHITE,
        Color32::from_rgb(20, 20, 20),
        Color32::from_rgb(35, 35, 35),
        Color32::from_rgb(55, 55, 55),
        Color32::WHITE,
        Color32::from_rgb(255, 255, 0),
        Color32::BLACK,
      )
    } else {
      (
        Color32::WHITE,
        Color32::BLACK,
        Color32::from_rgb(245, 245, 245),
        Color32::from_rgb(235, 235, 235),
        Color32::from_rgb(220, 220, 220),
        Color32::BLACK,
        Color32::from_rgb(0, 92, 230),
        Color32::WHITE,
      )
    };

  visuals.override_text_color = Some(fg);
  visuals.panel_fill = bg;
  visuals.window_fill = bg;
  visuals.extreme_bg_color = bg;
  visuals.faint_bg_color = button_bg;
  visuals.hyperlink_color = selection_bg;

  // Stronger borders everywhere (including separators).
  visuals.window_stroke = Stroke::new(2.0, border);
  visuals.widgets.noninteractive.bg_stroke = Stroke::new(2.0, border);

  // Buttons / interactive widgets.
  visuals.widgets.inactive.bg_fill = button_bg;
  visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, fg);
  visuals.widgets.inactive.bg_stroke = Stroke::new(2.0, border);

  visuals.widgets.hovered.bg_fill = button_bg_hover;
  visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, fg);
  visuals.widgets.hovered.bg_stroke = Stroke::new(2.0, border);

  visuals.widgets.active.bg_fill = button_bg_active;
  visuals.widgets.active.fg_stroke = Stroke::new(1.0, fg);
  visuals.widgets.active.bg_stroke = Stroke::new(2.0, border);

  // Make selection highly visible (e.g. selected tab labels, text selection).
  visuals.selection.bg_fill = selection_bg;
  visuals.selection.stroke = Stroke::new(3.0, selection_fg);

  visuals
}

/// Apply high-contrast palette overrides to egui when `FASTR_BROWSER_HIGH_CONTRAST=1` is set.
///
/// This function is intentionally cheap and idempotent; the browser UI may call it every frame so
/// it does not need additional initialization plumbing.
pub fn apply_high_contrast_if_enabled(ctx: &egui::Context) {
  if !high_contrast_enabled() {
    return;
  }

  let mut style = (*ctx.style()).clone();
  style.visuals = high_contrast_visuals(&style.visuals);
  ctx.set_style(style);
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::ui::contrast;
  use crate::debug::runtime::{with_runtime_toggles, RuntimeToggles};
  use std::collections::HashMap;
  use std::sync::Arc;

  fn hex_rgba(color: Color32) -> String {
    format!(
      "#{:02X}{:02X}{:02X}{:02X}",
      color.r(),
      color.g(),
      color.b(),
      color.a()
    )
  }

  fn assert_min_contrast(
    theme_name: &str,
    fg_name: &str,
    fg: Color32,
    bg_name: &str,
    bg: Color32,
    min_ratio: f32,
  ) {
    let ratio = contrast::contrast_ratio(fg, bg);
    assert!(
      ratio >= min_ratio,
      "{theme_name}: expected contrast({fg_name} {} over {bg_name} {}) >= {min_ratio:.2}, got {ratio:.2}",
      hex_rgba(fg),
      hex_rgba(bg)
    );
  }

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
      theme_parsing::parse_hex_color("#ff0000").map(|c| c.to_color32()),
      Some(egui::Color32::from_rgb(0xFF, 0x00, 0x00))
    );
    assert_eq!(
      theme_parsing::parse_hex_color("0f0").map(|c| c.to_color32()),
      Some(egui::Color32::from_rgb(0x00, 0xFF, 0x00))
    );
    assert_eq!(
      theme_parsing::parse_hex_color("#11223344").map(|c| c.to_color32()),
      Some(egui::Color32::from_rgba_unmultiplied(0x11, 0x22, 0x33, 0x44))
    );
    assert_eq!(theme_parsing::parse_hex_color("not-a-color"), None);
    assert_eq!(theme_parsing::parse_hex_color("#12"), None);
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
    with_runtime_toggles(Arc::new(RuntimeToggles::from_map(HashMap::new())), || {
      assert_eq!(ui_scale_from_env(), None);
    });

    with_runtime_toggles(
      Arc::new(RuntimeToggles::from_map(HashMap::from([(
        ENV_UI_SCALE.to_string(),
        "1.5".to_string(),
      )]))),
      || {
        assert_eq!(ui_scale_from_env(), Some(1.5));
      },
    );

    with_runtime_toggles(
      Arc::new(RuntimeToggles::from_map(HashMap::from([(
        ENV_UI_SCALE.to_string(),
        "1000".to_string(),
      )]))),
      || {
        assert_eq!(ui_scale_from_env(), Some(MAX_UI_SCALE));
      },
    );

    with_runtime_toggles(
      Arc::new(RuntimeToggles::from_map(HashMap::from([(
        ENV_UI_SCALE.to_string(),
        "nope".to_string(),
      )]))),
      || {
        assert_eq!(ui_scale_from_env(), None);
      },
    );
  }

  #[test]
  fn selection_stroke_is_stronger_in_high_contrast() {
    let normal = BrowserTheme::light(None);
    let high = BrowserTheme::light_high_contrast(None);

    let normal_stroke = selection_stroke(&normal);
    let high_stroke = selection_stroke(&high);

    assert!(
      normal_stroke.width >= 2.0,
      "expected normal selection stroke (focus ring) to be thick enough for focus visibility (got {})",
      normal_stroke.width
    );
    assert!(
      high_stroke.width > normal_stroke.width,
      "expected high-contrast selection stroke width to exceed normal ({} > {})",
      high_stroke.width,
      normal_stroke.width
    );
    assert!(
      high_stroke.color.a() > normal_stroke.color.a(),
      "expected high-contrast selection stroke alpha to exceed normal ({} > {})",
      high_stroke.color.a(),
      normal_stroke.color.a()
    );
  }

  #[test]
  fn browser_theme_palette_meets_minimum_contrast() {
    // WCAG 2.1 AA thresholds:
    // - Normal body text: 4.5:1
    // - Non-text UI components / focus indicators: 3.0:1 (SC 1.4.11)
    const MIN_PRIMARY_TEXT: f32 = 4.5;
    const MIN_SECONDARY_TEXT: f32 = 3.0;
    const MIN_FOCUS_STROKE: f32 = 3.0;

    let cases: [(&str, BrowserTheme); 4] = [
      ("light", BrowserTheme::light(None)),
      ("dark", BrowserTheme::dark(None)),
      ("light_high_contrast", BrowserTheme::light_high_contrast(None)),
      ("dark_high_contrast", BrowserTheme::dark_high_contrast(None)),
    ];

    for (name, theme) in cases {
      let c = &theme.colors;
      let surfaces: [(&str, Color32); 3] =
        [("bg", c.bg), ("surface", c.surface), ("raised", c.raised)];

      for (surface_name, surface) in surfaces {
        assert_min_contrast(
          name,
          "text_primary",
          c.text_primary,
          surface_name,
          surface,
          MIN_PRIMARY_TEXT,
        );
        assert_min_contrast(
          name,
          "text_secondary",
          c.text_secondary,
          surface_name,
          surface,
          MIN_SECONDARY_TEXT,
        );
      }

      let focus_stroke = selection_stroke(&theme).color;
      for (surface_name, surface) in surfaces {
        assert_min_contrast(
          name,
          "focus/selection_stroke",
          focus_stroke,
          surface_name,
          surface,
          MIN_FOCUS_STROKE,
        );
      }
    }
  }
}
