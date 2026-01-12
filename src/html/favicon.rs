//! Document favicon discovery.
//!
//! This is a best-effort helper used by the browser UI worker to find the current page's favicon
//! after navigation commit.

use crate::css::loader::resolve_href;
use crate::dom::{DomNode, DomNodeType, HTML_NAMESPACE};

// HTML defines "ASCII whitespace" as: TAB/LF/FF/CR/SPACE.
fn trim_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
}

fn rel_contains_icon(rel: &str) -> bool {
  // Match `rel="icon"`, `rel="shortcut icon"`, and any other rel value containing "icon"
  // (e.g. `apple-touch-icon`, `mask-icon`).
  rel
    .as_bytes()
    .windows(4)
    .any(|w| w.eq_ignore_ascii_case(b"icon"))
}

fn find_head(root: &DomNode) -> Option<&DomNode> {
  let mut stack: Vec<&DomNode> = vec![root];
  while let Some(node) = stack.pop() {
    if matches!(node.node_type, DomNodeType::ShadowRoot { .. }) {
      continue;
    }
    if node.is_template_element() {
      continue;
    }
    if node
      .tag_name()
      .is_some_and(|tag| tag.eq_ignore_ascii_case("head"))
      && matches!(node.namespace(), Some(ns) if ns.is_empty() || ns == HTML_NAMESPACE)
    {
      return Some(node);
    }

    for child in node.traversal_children().iter().rev() {
      stack.push(child);
    }
  }
  None
}

fn find_favicon_href(root: &DomNode) -> Option<&str> {
  // Track whether we've entered a foreign namespace (e.g. SVG) so we don't accidentally treat
  // non-HTML `<link>` elements as candidates.
  let mut stack: Vec<(&DomNode, bool)> = vec![(root, false)];
  while let Some((node, in_foreign_namespace)) = stack.pop() {
    if matches!(node.node_type, DomNodeType::ShadowRoot { .. }) {
      continue;
    }
    if node.is_template_element() {
      continue;
    }

    let next_in_foreign_namespace = in_foreign_namespace
      || matches!(
        node.namespace(),
        Some(ns) if !(ns.is_empty() || ns == HTML_NAMESPACE)
      );

    if !in_foreign_namespace
      && node
        .tag_name()
        .is_some_and(|tag| tag.eq_ignore_ascii_case("link"))
      && matches!(node.namespace(), Some(ns) if ns.is_empty() || ns == HTML_NAMESPACE)
    {
      let rel = node.get_attribute_ref("rel").unwrap_or("");
      if rel_contains_icon(rel) {
        if let Some(href) = node.get_attribute_ref("href") {
          let href = trim_ascii_whitespace(href);
          if !href.is_empty() && !href.starts_with('#') {
            return Some(href);
          }
        }
      }
    }

    for child in node.traversal_children().iter().rev() {
      stack.push((child, next_in_foreign_namespace));
    }
  }
  None
}

/// Discover the favicon URL for a document.
///
/// - Searches for `<link rel="icon" href="...">` (and related `rel` values containing `icon`).
/// - Prefers a link in the document `<head>`, falling back to the whole document.
/// - Resolves the `href` against `base_url` (typically the navigation base URL).
pub fn find_document_favicon_url(dom: &DomNode, base_url: &str) -> Option<String> {
  let base_url = trim_ascii_whitespace(base_url);
  if base_url.is_empty() {
    return None;
  }

  if let Some(head) = find_head(dom) {
    if let Some(href) = find_favicon_href(head) {
      if let Some(resolved) = resolve_href(base_url, href) {
        return Some(resolved);
      }
    }
  }

  let href = find_favicon_href(dom)?;
  resolve_href(base_url, href)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::dom::parse_html;

  #[test]
  fn finds_icon_link_in_head() {
    let dom =
      parse_html(r#"<html><head><link rel="icon" href="icon.png"></head><body></body></html>"#)
        .unwrap();
    assert_eq!(
      find_document_favicon_url(&dom, "https://example.com/dir/page.html"),
      Some("https://example.com/dir/icon.png".to_string())
    );
  }

  #[test]
  fn finds_shortcut_icon() {
    let dom =
      parse_html(r#"<html><head><link rel="shortcut icon" href="/favicon.ico"></head></html>"#)
        .unwrap();
    assert_eq!(
      find_document_favicon_url(&dom, "https://example.com/dir/page.html"),
      Some("https://example.com/favicon.ico".to_string())
    );
  }

  #[test]
  fn finds_mask_icon_via_substring_match() {
    let dom =
      parse_html(r#"<html><head><link rel="mask-icon" href="mask.svg"></head></html>"#).unwrap();
    assert_eq!(
      find_document_favicon_url(&dom, "https://example.com/dir/page.html"),
      Some("https://example.com/dir/mask.svg".to_string())
    );
  }
}
