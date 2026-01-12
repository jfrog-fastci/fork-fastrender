/// Environment variable used to override the browser UI theme.
pub const ENV_BROWSER_THEME: &str = "FASTR_BROWSER_THEME";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserTheme {
  /// Do not override; follow the UI toolkit's default/system theme.
  System,
  Light,
  Dark,
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

