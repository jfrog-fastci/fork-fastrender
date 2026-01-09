use crate::css::loader::{resolve_href, resolve_href_with_base};
use crate::dom::HTML_NAMESPACE;
use url::Url;

// HTML defines "ASCII whitespace" as: U+0009 TAB, U+000A LF, U+000C FF, U+000D CR, U+0020 SPACE.
// Avoid `str::trim()` here because it removes additional Unicode whitespace like NBSP (U+00A0),
// which should be preserved and percent-encoded by URL parsing.
fn trim_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
}

/// Resolve a `<script src>` value at parse time, given the base URL as it exists at that moment.
///
/// When `base` is `None` (e.g. the document URL is unknown and no `<base href>` has been parsed
/// yet), relative URLs are preserved (after ASCII whitespace trimming + scheme filtering) so the
/// caller can defer resolution until a document URL becomes available.
pub(crate) fn resolve_script_src_at_parse_time(base: Option<&str>, raw_src: &str) -> Option<String> {
  match base {
    Some(base) => resolve_href_with_base(Some(base), raw_src),
    None => {
      let trimmed = trim_ascii_whitespace(raw_src);
      if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
      }

      fn starts_with_ignore_ascii_case(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.len() >= needle.len() && haystack[..needle.len()].eq_ignore_ascii_case(needle)
      }

      let bytes = trimmed.as_bytes();
      if starts_with_ignore_ascii_case(bytes, b"javascript:")
        || starts_with_ignore_ascii_case(bytes, b"vbscript:")
        || starts_with_ignore_ascii_case(bytes, b"mailto:")
      {
        return None;
      }

      // `resolve_href` rejects `javascript:`/`vbscript:`/`mailto:` URLs and returns `None` for
      // relative URLs without a base. For the base-less case, keep relative URLs as-is so callers
      // can defer resolution until a document URL becomes available.
      resolve_href("", trimmed).or_else(|| Some(trimmed.to_string()))
    }
  }
}

/// Parse-time tracker for the document base URL.
///
/// The HTML `<base href>` element affects URL resolution only *after* it has been parsed and
/// inserted, so script `src` values must resolve against the current base URL at the time each
/// `<script>` element is encountered. This tracker supports that streaming usage.
#[derive(Debug, Clone)]
pub struct BaseUrlTracker {
  document_url: Option<String>,
  current_base_url: Option<String>,
  /// Whether we've encountered the first `<base href>` in the document `<head>` with a non-empty,
  /// non-fragment-only href after ASCII-whitespace trimming.
  ///
  /// After this becomes `true`, subsequent `<base>` elements must not affect the document base
  /// URL.
  base_href_frozen: bool,
}

impl BaseUrlTracker {
  pub fn new(document_url: Option<&str>) -> Self {
    let document_url = document_url.map(|url| url.to_string());
    Self {
      current_base_url: document_url.clone(),
      document_url,
      base_href_frozen: false,
    }
  }

  pub fn current_base_url(&self) -> Option<String> {
    self.current_base_url.clone()
  }

  pub fn on_element_inserted(
    &mut self,
    tag_name: &str,
    namespace: &str,
    attrs: &[(String, String)],
    in_head: bool,
    in_foreign_namespace: bool,
    in_template: bool,
  ) {
    if self.base_href_frozen {
      return;
    }
    if !in_head || in_template || in_foreign_namespace {
      return;
    }
    if !tag_name.eq_ignore_ascii_case("base") {
      return;
    }
    if !(namespace.is_empty() || namespace == HTML_NAMESPACE) {
      return;
    }

    let Some(href_raw) = attrs
      .iter()
      .find_map(|(name, value)| name.eq_ignore_ascii_case("href").then_some(value.as_str()))
    else {
      return;
    };

    let href = trim_ascii_whitespace(href_raw);
    if href.is_empty() || href.starts_with('#') {
      return;
    }

    // This is the first base candidate per our rules; ignore any later ones even if resolution
    // fails.
    self.base_href_frozen = true;

    let resolved = if let Some(document_url) = self.document_url.as_deref() {
      resolve_href(document_url, href)
    } else {
      Url::parse(href).ok().map(|u| u.to_string())
    };
    if let Some(resolved) = resolved {
      self.current_base_url = Some(resolved);
    }
  }

  /// Resolve a `<script src>` value against the current base URL.
  pub fn resolve_script_src(&self, raw_src: &str) -> Option<String> {
    resolve_script_src_at_parse_time(self.current_base_url.as_deref(), raw_src)
  }
}

#[cfg(test)]
mod tests {
  use super::BaseUrlTracker;
  use crate::dom::HTML_NAMESPACE;

  #[test]
  fn script_before_base_uses_document_url() {
    let mut tracker = BaseUrlTracker::new(Some("https://example.com/dir/page.html"));

    let resolved = tracker.resolve_script_src("a.js");
    assert_eq!(resolved.as_deref(), Some("https://example.com/dir/a.js"));

    tracker.on_element_inserted(
      "base",
      HTML_NAMESPACE,
      &[("href".to_string(), "https://ex/base/".to_string())],
      true,
      false,
      false,
    );

    assert_eq!(
      tracker.current_base_url().as_deref(),
      Some("https://ex/base/")
    );
  }

  #[test]
  fn script_before_base_without_document_url_remains_relative() {
    let mut tracker = BaseUrlTracker::new(None);

    let resolved = tracker.resolve_script_src("a.js");
    assert_eq!(resolved.as_deref(), Some("a.js"));

    tracker.on_element_inserted(
      "base",
      HTML_NAMESPACE,
      &[("href".to_string(), "https://ex/base/".to_string())],
      true,
      false,
      false,
    );

    // Base applies after it is parsed, so the earlier relative script resolution stays relative.
    assert_eq!(
      tracker.current_base_url().as_deref(),
      Some("https://ex/base/")
    );
  }

  #[test]
  fn script_after_base_uses_base_href() {
    let mut tracker = BaseUrlTracker::new(Some("https://example.com/dir/page.html"));

    tracker.on_element_inserted(
      "base",
      HTML_NAMESPACE,
      &[("href".to_string(), "https://ex/base/".to_string())],
      true,
      false,
      false,
    );

    let resolved = tracker.resolve_script_src("a.js");
    assert_eq!(resolved.as_deref(), Some("https://ex/base/a.js"));
  }

  #[test]
  fn base_href_trims_ascii_whitespace_but_not_nbsp() {
    let nbsp = "\u{00A0}";
    let href = format!(" \t\r\nhttps://example.com/base/{nbsp} \n");

    let mut tracker = BaseUrlTracker::new(None);
    tracker.on_element_inserted(
      "base",
      HTML_NAMESPACE,
      &[("href".to_string(), href)],
      true,
      false,
      false,
    );

    assert_eq!(
      tracker.current_base_url().as_deref(),
      Some("https://example.com/base/%C2%A0")
    );
  }

  #[test]
  fn fragment_only_base_is_ignored() {
    let mut tracker = BaseUrlTracker::new(Some("https://example.com/dir/page.html"));
    tracker.on_element_inserted(
      "base",
      HTML_NAMESPACE,
      &[("href".to_string(), "#x".to_string())],
      true,
      false,
      false,
    );

    assert_eq!(
      tracker.current_base_url().as_deref(),
      Some("https://example.com/dir/page.html")
    );
    assert_eq!(
      tracker.resolve_script_src("a.js").as_deref(),
      Some("https://example.com/dir/a.js")
    );
  }

  #[test]
  fn base_in_foreign_namespace_is_ignored_and_does_not_freeze_base() {
    let mut tracker = BaseUrlTracker::new(Some("https://example.com/dir/page.html"));

    tracker.on_element_inserted(
      "base",
      HTML_NAMESPACE,
      &[("href".to_string(), "https://bad.example/".to_string())],
      true,
      /* in_foreign_namespace */ true,
      false,
    );
    assert_eq!(
      tracker.current_base_url().as_deref(),
      Some("https://example.com/dir/page.html")
    );

    tracker.on_element_inserted(
      "base",
      HTML_NAMESPACE,
      &[("href".to_string(), "https://good.example/".to_string())],
      true,
      false,
      false,
    );
    assert_eq!(
      tracker.current_base_url().as_deref(),
      Some("https://good.example/")
    );
  }

  #[test]
  fn base_in_template_is_ignored_and_does_not_freeze_base() {
    let mut tracker = BaseUrlTracker::new(Some("https://example.com/dir/page.html"));

    tracker.on_element_inserted(
      "base",
      HTML_NAMESPACE,
      &[("href".to_string(), "https://bad.example/".to_string())],
      true,
      false,
      /* in_template */ true,
    );
    assert_eq!(
      tracker.current_base_url().as_deref(),
      Some("https://example.com/dir/page.html")
    );

    tracker.on_element_inserted(
      "base",
      HTML_NAMESPACE,
      &[("href".to_string(), "https://good.example/".to_string())],
      true,
      false,
      false,
    );
    assert_eq!(
      tracker.current_base_url().as_deref(),
      Some("https://good.example/")
    );
  }
}
