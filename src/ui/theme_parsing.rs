/// Environment variable used to override the browser UI theme.
pub const ENV_BROWSER_THEME: &str = "FASTR_BROWSER_THEME";

/// Environment variable used to override the browser UI accent color.
pub const ENV_BROWSER_ACCENT: &str = "FASTR_BROWSER_ACCENT";

/// Environment variable used to enable a high-contrast UI palette.
pub const ENV_BROWSER_HIGH_CONTRAST: &str = "FASTR_BROWSER_HIGH_CONTRAST";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeEnvError {
  pub message: String,
}

impl ThemeEnvError {
  fn new(message: impl Into<String>) -> Self {
    Self {
      message: message.into(),
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserTheme {
  /// Do not override; follow the UI toolkit's default/system theme.
  System,
  Light,
  Dark,
}

/// RGBA color used by the browser chrome theme (accent, etc).
///
/// This struct is intentionally lightweight and does **not** depend on egui types so it can be
/// used by session persistence and env parsing without the `browser_ui` feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RgbaColor {
  pub r: u8,
  pub g: u8,
  pub b: u8,
  pub a: u8,
}

impl RgbaColor {
  pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
    Self { r, g, b, a }
  }

  pub fn to_hex_string(self) -> String {
    format_hex_color(self)
  }

  #[cfg(feature = "browser_ui")]
  pub fn to_color32(self) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(self.r, self.g, self.b, self.a)
  }
}

#[cfg(feature = "browser_ui")]
impl From<egui::Color32> for RgbaColor {
  fn from(value: egui::Color32) -> Self {
    let [r, g, b, a] = value.to_array();
    Self { r, g, b, a }
  }
}

impl Default for BrowserTheme {
  fn default() -> Self {
    Self::System
  }
}

impl BrowserTheme {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::System => "system",
      Self::Light => "light",
      Self::Dark => "dark",
    }
  }
}

impl serde::Serialize for BrowserTheme {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: serde::Serializer,
  {
    serializer.serialize_str(self.as_str())
  }
}

impl<'de> serde::Deserialize<'de> for BrowserTheme {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: serde::Deserializer<'de>,
  {
    let raw = <String as serde::Deserialize<'de>>::deserialize(deserializer)?;
    // Be permissive so hand-edited session files don't hard-fail on unknown values. Unknown/empty
    // strings fall back to the default `System` theme.
    Ok(parse_browser_theme(&raw).unwrap_or(BrowserTheme::System))
  }
}

/// Parse a user-provided theme string.
///
/// This is intentionally forgiving:
/// - ASCII case-insensitive
/// - trims whitespace
/// - returns `None` for unknown values (caller should fall back to defaults)
pub fn parse_browser_theme(raw: &str) -> Option<BrowserTheme> {
  let v = raw.trim().to_ascii_lowercase();
  if v.is_empty() {
    return None;
  }
  match v.as_str() {
    "system" | "auto" | "default" => Some(BrowserTheme::System),
    "light" => Some(BrowserTheme::Light),
    "dark" => Some(BrowserTheme::Dark),
    _ => None,
  }
}

/// Parse the `FASTR_BROWSER_THEME` environment variable value.
pub fn parse_browser_theme_env(raw: Option<&str>) -> Option<BrowserTheme> {
  raw.and_then(parse_browser_theme)
}

/// Parse a hex color string (`#RGB`, `#RRGGBB`, or `#RRGGBBAA`).
///
/// This is shared between:
/// - env var parsing (`FASTR_BROWSER_ACCENT`)
/// - persisted appearance settings in the session file
///
/// Parsing is intentionally forgiving:
/// - leading `#` is optional
/// - trims whitespace
/// - ASCII hex digits only
pub fn parse_hex_color(raw: &str) -> Option<RgbaColor> {
  let value = raw.trim();
  if value.is_empty() {
    return None;
  }
  let value = value.strip_prefix('#').unwrap_or(value);

  fn nibble(c: char) -> Option<u8> {
    Some(u8::try_from(c.to_digit(16)?).ok()?)
  }
  fn byte2(s: &str) -> Option<u8> {
    u8::from_str_radix(s, 16).ok()
  }

  match value.len() {
    3 => {
      let mut chars = value.chars();
      let r = nibble(chars.next()?)?;
      let g = nibble(chars.next()?)?;
      let b = nibble(chars.next()?)?;
      // Duplicate each nibble, e.g. `f` → `ff`.
      Some(RgbaColor::new(r * 17, g * 17, b * 17, 0xFF))
    }
    6 => {
      let r = byte2(value.get(0..2)?)?;
      let g = byte2(value.get(2..4)?)?;
      let b = byte2(value.get(4..6)?)?;
      Some(RgbaColor::new(r, g, b, 0xFF))
    }
    8 => {
      let r = byte2(value.get(0..2)?)?;
      let g = byte2(value.get(2..4)?)?;
      let b = byte2(value.get(4..6)?)?;
      let a = byte2(value.get(6..8)?)?;
      Some(RgbaColor::new(r, g, b, a))
    }
    _ => None,
  }
}

pub fn parse_browser_accent_env(raw: Option<&str>) -> Option<RgbaColor> {
  raw.and_then(parse_hex_color)
}

pub fn format_hex_color(color: RgbaColor) -> String {
  if color.a == 0xFF {
    format!("#{:02x}{:02x}{:02x}", color.r, color.g, color.b)
  } else {
    format!(
      "#{:02x}{:02x}{:02x}{:02x}",
      color.r, color.g, color.b, color.a
    )
  }
}

fn parse_env_bool(key: &str, raw: &str) -> Result<bool, ThemeEnvError> {
  let v = raw.trim().to_ascii_lowercase();
  if v.is_empty() {
    return Ok(false);
  }

  if matches!(v.as_str(), "1" | "true" | "yes" | "on") {
    return Ok(true);
  }
  if matches!(v.as_str(), "0" | "false" | "no" | "off") {
    return Ok(false);
  }

  Err(ThemeEnvError::new(format!(
    "{key}: invalid value {raw:?}; expected 0|1|true|false"
  )))
}

/// Parse the `FASTR_BROWSER_HIGH_CONTRAST` environment variable value.
///
/// `None` (var unset) is treated as "off". Invalid values return an error so callers/tests can
/// surface misconfiguration.
pub fn parse_high_contrast_env(raw: Option<&str>) -> Result<bool, ThemeEnvError> {
  let Some(raw) = raw else {
    return Ok(false);
  };
  parse_env_bool(ENV_BROWSER_HIGH_CONTRAST, raw)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn theme_env_parsing_accepts_known_values() {
    assert_eq!(parse_browser_theme_env(None), None);
    assert_eq!(parse_browser_theme_env(Some("")), None);
    assert_eq!(parse_browser_theme_env(Some("   ")), None);

    assert_eq!(parse_browser_theme_env(Some("system")), Some(BrowserTheme::System));
    assert_eq!(parse_browser_theme_env(Some("auto")), Some(BrowserTheme::System));
    assert_eq!(parse_browser_theme_env(Some("default")), Some(BrowserTheme::System));

    assert_eq!(parse_browser_theme_env(Some("light")), Some(BrowserTheme::Light));
    assert_eq!(parse_browser_theme_env(Some("dark")), Some(BrowserTheme::Dark));

    // Case-insensitive + whitespace-tolerant.
    assert_eq!(parse_browser_theme_env(Some("  DARK ")), Some(BrowserTheme::Dark));
  }

  #[test]
  fn theme_env_parsing_ignores_invalid_values() {
    assert_eq!(parse_browser_theme_env(Some("wat")), None);
    assert_eq!(parse_browser_theme_env(Some("dark-mode")), None);
    assert_eq!(parse_browser_theme_env(Some("0")), None);
    assert_eq!(parse_browser_theme_env(Some("1")), None);
  }

  #[test]
  fn parse_high_contrast_env_values() {
    assert_eq!(parse_high_contrast_env(None), Ok(false));
    assert_eq!(parse_high_contrast_env(Some("")), Ok(false));
    assert_eq!(parse_high_contrast_env(Some("0")), Ok(false));
    assert_eq!(parse_high_contrast_env(Some("1")), Ok(true));
    assert_eq!(parse_high_contrast_env(Some("true")), Ok(true));
    assert_eq!(parse_high_contrast_env(Some("yes")), Ok(true));
    assert_eq!(parse_high_contrast_env(Some("on")), Ok(true));
    assert_eq!(parse_high_contrast_env(Some("false")), Ok(false));
    assert_eq!(parse_high_contrast_env(Some("no")), Ok(false));
    assert_eq!(parse_high_contrast_env(Some("off")), Ok(false));
    assert!(parse_high_contrast_env(Some("maybe")).is_err());
  }

  #[test]
  fn parse_hex_color_accepts_rgb_and_rgba() {
    assert_eq!(
      parse_hex_color("#ff0000"),
      Some(RgbaColor::new(0xFF, 0x00, 0x00, 0xFF))
    );
    assert_eq!(parse_hex_color("0f0"), Some(RgbaColor::new(0x00, 0xFF, 0x00, 0xFF)));
    assert_eq!(
      parse_hex_color("#11223344"),
      Some(RgbaColor::new(0x11, 0x22, 0x33, 0x44))
    );
    assert_eq!(parse_hex_color("not-a-color"), None);
    assert_eq!(parse_hex_color("#12"), None);
    assert_eq!(parse_hex_color(""), None);
  }

  #[test]
  fn format_hex_color_omits_alpha_when_opaque() {
    assert_eq!(format_hex_color(RgbaColor::new(0, 0, 0, 0xFF)), "#000000");
    assert_eq!(
      format_hex_color(RgbaColor::new(0x11, 0x22, 0x33, 0x44)),
      "#11223344"
    );
  }
}
