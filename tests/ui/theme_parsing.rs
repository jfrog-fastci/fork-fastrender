use fastrender::ui::theme_parsing::{parse_browser_theme_env, BrowserTheme};

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

