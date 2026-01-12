/// Environment variable used to override the browser UI theme.
pub const ENV_BROWSER_THEME: &str = "FASTR_BROWSER_THEME";

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

#[cfg(test)]
mod tests {
  use super::{parse_browser_theme_env, BrowserTheme};

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
}
