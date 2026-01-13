//! Theme variable generation for renderer-driven browser chrome.
//!
//! The chrome frame HTML generator injects a small `<style>` block that defines CSS variables so the
//! chrome stylesheet can follow the user's selected theme + accent color without hardcoding values.

use crate::ui::appearance::AppearanceSettings;
use crate::ui::theme_parsing::{parse_hex_color, BrowserTheme, RgbaColor};

const DEFAULT_ACCENT_LIGHT: RgbaColor = RgbaColor::new(0x3B, 0x82, 0xF6, 0xFF); // blue-500
const DEFAULT_ACCENT_DARK: RgbaColor = RgbaColor::new(0x60, 0xA5, 0xFA, 0xFF); // blue-400

fn sanitize_accent_override(raw: Option<&str>) -> Option<RgbaColor> {
  raw
    .and_then(parse_hex_color)
    // The chrome accent is used as a solid color. Ignore the optional alpha channel.
    .map(|c| RgbaColor::new(c.r, c.g, c.b, 0xFF))
}

fn theme_color_scheme(theme: BrowserTheme) -> &'static str {
  match theme {
    BrowserTheme::Light => "light",
    BrowserTheme::Dark => "dark",
    BrowserTheme::System => "light dark",
  }
}

/// Generate CSS that defines `--chrome-*` theme variables on `:root`.
pub(crate) fn chrome_theme_css(settings: &AppearanceSettings) -> String {
  let accent_override = sanitize_accent_override(settings.accent_color.as_deref());

  let (accent_light, accent_dark) = match accent_override {
    Some(accent) => (accent, accent),
    None => (DEFAULT_ACCENT_LIGHT, DEFAULT_ACCENT_DARK),
  };

  let (base_accent, media_accent) = match settings.theme {
    BrowserTheme::Light => (accent_light, None),
    BrowserTheme::Dark => (accent_dark, None),
    BrowserTheme::System => {
      if accent_light == accent_dark {
        (accent_light, None)
      } else {
        (accent_light, Some(accent_dark))
      }
    }
  };

  // Match the alpha constants used by existing internal about-page theming helpers.
  let (bg_alpha, border_alpha, focus_alpha) = if settings.high_contrast {
    (0.28, 0.78, 1.0)
  } else {
    (0.18, 0.55, 0.65)
  };

  let mut css = String::with_capacity(256);
  css.push_str(":root {\n");
  css.push_str(&format!(
    "  color-scheme: {};\n",
    theme_color_scheme(settings.theme)
  ));
  css.push_str(&format!("  --chrome-theme: {};\n", settings.theme.as_str()));
  css.push_str(&format!(
    "  --chrome-accent: rgb({}, {}, {});\n",
    base_accent.r, base_accent.g, base_accent.b
  ));
  css.push_str(&format!(
    "  --chrome-accent-bg: rgba({}, {}, {}, {bg_alpha});\n",
    base_accent.r, base_accent.g, base_accent.b
  ));
  css.push_str(&format!(
    "  --chrome-accent-border: rgba({}, {}, {}, {border_alpha});\n",
    base_accent.r, base_accent.g, base_accent.b
  ));
  css.push_str(&format!(
    "  --chrome-focus-ring: rgba({}, {}, {}, {focus_alpha});\n",
    base_accent.r, base_accent.g, base_accent.b
  ));
  css.push_str("}\n");

  if let Some(accent) = media_accent {
    css.push_str("@media (prefers-color-scheme: dark) {\n");
    css.push_str("  :root {\n");
    css.push_str(&format!(
      "    --chrome-accent: rgb({}, {}, {});\n",
      accent.r, accent.g, accent.b
    ));
    css.push_str(&format!(
      "    --chrome-accent-bg: rgba({}, {}, {}, {bg_alpha});\n",
      accent.r, accent.g, accent.b
    ));
    css.push_str(&format!(
      "    --chrome-accent-border: rgba({}, {}, {}, {border_alpha});\n",
      accent.r, accent.g, accent.b
    ));
    css.push_str(&format!(
      "    --chrome-focus-ring: rgba({}, {}, {}, {focus_alpha});\n",
      accent.r, accent.g, accent.b
    ));
    css.push_str("  }\n");
    css.push_str("}\n");
  }

  css
}

