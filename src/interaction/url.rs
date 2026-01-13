use url::Url;

fn trim_ascii_whitespace(value: &str) -> &str {
  // HTML URL-ish attributes strip leading/trailing ASCII whitespace (TAB/LF/FF/CR/SPACE) but do not
  // treat all Unicode whitespace as ignorable. Use an explicit trim to avoid incorrectly dropping
  // characters like NBSP (U+00A0).
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
}

/// Resolve an HTML `href`-style string against a base URL.
///
/// This is shared across interaction paths (pointer clicks, form submission, hover status).
///
/// - Empty/whitespace-only hrefs resolve to the base URL (`Url::join(\"\")` semantics).
/// - Rejects `javascript:` URLs (both raw and after resolution).
/// - Uses `url::Url::join` for relative/absolute resolution when `base_url` parses.
pub fn resolve_url(base_url: &str, href: &str) -> Option<String> {
  let href = trim_ascii_whitespace(href);
  if href
    .as_bytes()
    .get(.."javascript:".len())
    .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"javascript:"))
  {
    return None;
  }

  if let Ok(base) = Url::parse(base_url) {
    if let Ok(joined) = base.join(href) {
      if joined.scheme().eq_ignore_ascii_case("javascript") {
        return None;
      }
      return Some(joined.to_string());
    }
    // Empty hrefs are valid same-document navigations; if `Url::join` fails (e.g. for
    // cannot-be-a-base URLs), still treat the base as the resolved URL.
    if href.is_empty() && !base.scheme().eq_ignore_ascii_case("javascript") {
      let mut base = base;
      base.set_fragment(None);
      return Some(base.to_string());
    }
  }

  if href.is_empty() {
    return None;
  }

  // Fallback: if base URL parsing fails, accept absolute hrefs.
  let absolute = Url::parse(href).ok()?;
  (!absolute.scheme().eq_ignore_ascii_case("javascript")).then(|| absolute.to_string())
}

#[cfg(test)]
mod tests {
  use super::resolve_url;

  #[test]
  fn fragment_only_href_is_resolved_against_base() {
    assert_eq!(
      resolve_url("https://example.com/page.html", "#target").as_deref(),
      Some("https://example.com/page.html#target")
    );
  }

  #[test]
  fn percent_encoded_fragment_is_preserved() {
    assert_eq!(
      resolve_url("https://example.com/page.html", "#caf%C3%A9").as_deref(),
      Some("https://example.com/page.html#caf%C3%A9")
    );
  }

  #[test]
  fn unicode_fragment_is_percent_encoded() {
    assert_eq!(
      resolve_url("https://example.com/page.html", "#café").as_deref(),
      Some("https://example.com/page.html#caf%C3%A9")
    );
  }

  #[test]
  fn relative_paths_are_resolved_and_encoded() {
    assert_eq!(
      resolve_url("https://example.com/dir/page.html", "a b.html").as_deref(),
      Some("https://example.com/dir/a%20b.html")
    );
  }

  #[test]
  fn empty_href_resolves_to_base_url() {
    assert_eq!(
      resolve_url("https://example.com/dir/page.html", "").as_deref(),
      Some("https://example.com/dir/page.html")
    );
    assert_eq!(
      resolve_url("https://example.com/dir/page.html", "   ").as_deref(),
      Some("https://example.com/dir/page.html")
    );
  }
}
