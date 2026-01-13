//! Seatbelt (macOS SBPL) profile string helpers.
//!
//! Seatbelt profiles embed paths inside double-quoted string literals, for example:
//!
//! ```text
//! (deny file-write* (subpath "/tmp"))
//! ```
//!
//! When we generate profiles dynamically (e.g. embedding a per-user temp directory), paths must be
//! escaped to avoid breaking the profile syntax or allowing injection.

/// Escape a path for use inside a Seatbelt profile double-quoted string literal.
///
/// The return value is the *content* to place between the quotes; callers are expected to add the
/// surrounding `"` themselves (e.g. `(subpath "{escaped}")`).
///
/// Seatbelt string literals use C-like escaping.
pub fn escape_seatbelt_string_literal(path: &str) -> String {
  let mut escaped = String::with_capacity(path.len());
  for ch in path.chars() {
    match ch {
      '\\' => escaped.push_str(r"\\"),
      '"' => escaped.push_str(r#"\""#),
      '\n' => escaped.push_str(r"\n"),
      '\r' => escaped.push_str(r"\r"),
      _ => escaped.push(ch),
    }
  }
  escaped
}

#[cfg(test)]
mod tests {
  use super::escape_seatbelt_string_literal;

  #[test]
  fn escape_preserves_spaces() {
    let path = "/Users/alice/My Documents/test.txt";
    assert_eq!(escape_seatbelt_string_literal(path), path);
  }

  #[test]
  fn escape_quotes() {
    let path = r#"/Users/alice/has"quote"/file.txt"#;
    assert_eq!(
      escape_seatbelt_string_literal(path),
      r#"/Users/alice/has\"quote\"/file.txt"#
    );
  }

  #[test]
  fn escape_backslashes() {
    let path = r#"/Users/alice/has\backslash\file.txt"#;
    assert_eq!(
      escape_seatbelt_string_literal(path),
      r#"/Users/alice/has\\backslash\\file.txt"#
    );
  }
}

