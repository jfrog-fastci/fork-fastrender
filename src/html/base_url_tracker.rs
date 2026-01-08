use crate::css::loader::{resolve_href, resolve_href_with_base};
use crate::dom::HTML_NAMESPACE;
use url::Url;

// HTML defines "ASCII whitespace" as: U+0009 TAB, U+000A LF, U+000C FF, U+000D CR, U+0020 SPACE.
// Avoid `str::trim()` here because it removes additional Unicode whitespace like NBSP (U+00A0),
// which should be preserved and percent-encoded by URL parsing.
fn trim_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
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
    resolve_href_with_base(self.current_base_url.as_deref(), raw_src)
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

    assert_eq!(tracker.current_base_url().as_deref(), Some("https://ex/base/"));
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
}

