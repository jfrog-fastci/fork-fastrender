//! Parsing for `<meta name="color-scheme">` directives.
//!
//! When enabled by the embedding pipeline, `<meta name="color-scheme" content="...">` is treated
//! as a user-agent `:root { color-scheme: ... }` baseline. This helps `light-dark()` and UA system
//! colors resolve consistently with modern browsers.

use crate::dom::{DomNode, DomNodeType, HTML_NAMESPACE};
use crate::error::{Error, RenderStage, Result};
use crate::render_control::check_active_periodic;
use crate::style::types::ColorSchemePreference;

const COLOR_SCHEME_DEADLINE_STRIDE: usize = 1024;

// HTML defines "ASCII whitespace" as: U+0009 TAB, U+000A LF, U+000C FF, U+000D CR, U+0020 SPACE.
fn is_ascii_whitespace_html_css(ch: char) -> bool {
  matches!(ch, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' ')
}

fn trim_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(is_ascii_whitespace_html_css)
}

fn split_ascii_whitespace(value: &str) -> impl Iterator<Item = &str> {
  value
    .split(is_ascii_whitespace_html_css)
    .filter(|part| !part.is_empty())
}

/// Parse the `content` attribute of a `<meta name="color-scheme">` tag.
///
/// Returns `None` when the value is empty or invalid.
pub fn parse_meta_color_scheme_content(content: &str) -> Option<ColorSchemePreference> {
  let trimmed = trim_ascii_whitespace(content);
  if trimmed.is_empty() {
    return None;
  }

  // The `color-scheme` grammar does not allow commas. Authors sometimes use comma-separated lists
  // out of habit; treat those as invalid to match CSS parsing behavior.
  if trimmed.contains(',') {
    return None;
  }

  let tokens: Vec<String> = split_ascii_whitespace(trimmed).map(|t| t.to_string()).collect();
  crate::style::properties::parse_color_scheme_tokens(&tokens)
}

/// Extracts the first valid `<meta name="color-scheme">` directive within the document head.
///
/// Unknown and malformed directives are skipped until a valid one is found.
pub fn extract_color_scheme(dom: &DomNode) -> Option<ColorSchemePreference> {
  extract_color_scheme_impl(dom, None).ok().flatten()
}

pub(crate) fn extract_color_scheme_with_deadline(dom: &DomNode) -> Result<Option<ColorSchemePreference>> {
  let mut deadline_counter = 0usize;
  extract_color_scheme_impl(dom, Some(&mut deadline_counter))
}

fn extract_color_scheme_impl(
  dom: &DomNode,
  mut deadline_counter: Option<&mut usize>,
) -> Result<Option<ColorSchemePreference>> {
  let mut stack = vec![dom];
  let mut head: Option<&DomNode> = None;

  while let Some(node) = stack.pop() {
    if let Some(counter) = deadline_counter.as_deref_mut() {
      check_active_periodic(counter, COLOR_SCHEME_DEADLINE_STRIDE, RenderStage::DomParse)
        .map_err(Error::Render)?;
    }

    if let DomNodeType::ShadowRoot { .. } = node.node_type {
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
      head = Some(node);
      break;
    }

    for child in node.traversal_children().iter().rev() {
      stack.push(child);
    }
  }

  let Some(head) = head else {
    return Ok(None);
  };

  let mut stack: Vec<(&DomNode, bool)> = vec![(head, false)];
  while let Some((node, in_foreign_namespace)) = stack.pop() {
    if let Some(counter) = deadline_counter.as_deref_mut() {
      check_active_periodic(counter, COLOR_SCHEME_DEADLINE_STRIDE, RenderStage::DomParse)
        .map_err(Error::Render)?;
    }

    let next_in_foreign_namespace = in_foreign_namespace
      || matches!(
        node.namespace(),
        Some(ns) if !(ns.is_empty() || ns == HTML_NAMESPACE)
      );

    if !in_foreign_namespace
      && node
        .tag_name()
        .is_some_and(|tag| tag.eq_ignore_ascii_case("meta"))
      && matches!(node.namespace(), Some(ns) if ns.is_empty() || ns == HTML_NAMESPACE)
    {
      let name_attr = node.get_attribute_ref("name");
      let content_attr = node.get_attribute_ref("content");

      if name_attr
        .map(|n| n.eq_ignore_ascii_case("color-scheme"))
        .unwrap_or(false)
      {
        if let Some(content) = content_attr {
          if let Some(parsed) = parse_meta_color_scheme_content(content) {
            return Ok(Some(parsed));
          }
        }
      }
    }

    let skip_children = matches!(node.node_type, DomNodeType::ShadowRoot { .. })
      || node.is_template_element()
      || next_in_foreign_namespace;
    if skip_children {
      continue;
    }

    for child in node.traversal_children().iter().rev() {
      stack.push((child, next_in_foreign_namespace));
    }
  }

  Ok(None)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::style::types::ColorSchemeEntry;

  #[test]
  fn parse_meta_color_scheme_content_is_case_insensitive() {
    let parsed = parse_meta_color_scheme_content("LiGhT DaRk").unwrap();
    assert_eq!(
      parsed,
      ColorSchemePreference::Supported {
        schemes: vec![ColorSchemeEntry::Light, ColorSchemeEntry::Dark],
        only: false,
      }
    );
  }

  #[test]
  fn extracts_first_valid_meta_color_scheme() {
    let html = "<head><meta name=color-scheme content='normal dark'><meta name=color-scheme content='light dark'></head>";
    let dom = crate::dom::parse_html(html).unwrap();
    let parsed = extract_color_scheme(&dom).unwrap();
    assert_eq!(
      parsed,
      ColorSchemePreference::Supported {
        schemes: vec![ColorSchemeEntry::Light, ColorSchemeEntry::Dark],
        only: false,
      }
    );
  }

  #[test]
  fn first_valid_meta_wins_even_if_later_meta_exists() {
    let html = "<head><meta name='Color-Scheme' content='dark'><meta name=color-scheme content='light'></head>";
    let dom = crate::dom::parse_html(html).unwrap();
    let parsed = extract_color_scheme(&dom).unwrap();
    assert_eq!(
      parsed,
      ColorSchemePreference::Supported {
        schemes: vec![ColorSchemeEntry::Dark],
        only: false,
      }
    );
  }

  #[test]
  fn ignores_body_meta_color_scheme() {
    let html = "<html><head></head><body><meta name=color-scheme content='light dark'></body></html>";
    let dom = crate::dom::parse_html(html).unwrap();
    assert!(extract_color_scheme(&dom).is_none());
  }
}

