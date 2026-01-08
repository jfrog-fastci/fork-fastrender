use fastrender::style::content::CounterStyle;
use fastrender::Display;
use fastrender::Position;

#[test]
fn css_keywords_are_ascii_case_insensitive_only() {
  // U+212A KELVIN SIGN (K) lowercases to ASCII "k" under Unicode case folding.
  // CSS keywords are defined to be ASCII case-insensitive, so we must *not* treat K as "k".

  assert!(Display::parse("block").is_ok());
  assert!(Position::parse("sticky").is_ok());
  assert_eq!(CounterStyle::parse("lower-greek"), Some(CounterStyle::LowerGreek));

  assert!(Display::parse("bloc\u{212A}").is_err());
  assert!(Position::parse("stic\u{212A}y").is_err());
  assert_eq!(CounterStyle::parse("lower-gree\u{212A}"), None);
}

