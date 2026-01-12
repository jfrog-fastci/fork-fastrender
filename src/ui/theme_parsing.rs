/// Environment variable used to override the browser UI theme.
pub const ENV_BROWSER_THEME: &str = "FASTR_BROWSER_THEME";

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
}
