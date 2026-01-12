//! Collect Unicode codepoints used by a styled tree.
//!
//! This is primarily used to decide which `@font-face` subsets (via `unicode-range`) should be
//! loaded. A DOM-only scan misses generated content such as Font Awesome icons (e.g. `::before {
//! content: "\f04c" }`), which can cause icon fonts to be skipped entirely.

use crate::dom::{DomNode, DomNodeType};
use crate::error::{Error, RenderStage, Result};
use crate::render_control::check_active_periodic;
use crate::style::cascade::StyledNode;
use crate::style::content::{ContentItem, ContentValue};
use rustc_hash::FxHashSet;

const USED_CODEPOINTS_DEADLINE_STRIDE: usize = 1024;

fn insert_text_codepoints(seen: &mut FxHashSet<u32>, text: &str) {
  if text.is_ascii() {
    for &b in text.as_bytes() {
      seen.insert(b as u32);
    }
  } else {
    for ch in text.chars() {
      seen.insert(ch as u32);
    }
  }
}

fn insert_ascii_digits(seen: &mut FxHashSet<u32>) {
  for cp in b'0'..=b'9' {
    seen.insert(cp as u32);
  }
}

fn collect_content_item(
  seen: &mut FxHashSet<u32>,
  node: &DomNode,
  quotes: &[(String, String)],
  item: &ContentItem,
) {
  match item {
    ContentItem::String(s) => insert_text_codepoints(seen, s),
    ContentItem::Attr { name, fallback, .. } => {
      if let Some(value) = node.get_attribute_ref(name) {
        insert_text_codepoints(seen, value);
      } else if let Some(fallback) = fallback {
        insert_text_codepoints(seen, fallback);
      }
    }
    ContentItem::OpenQuote
    | ContentItem::CloseQuote
    | ContentItem::NoOpenQuote
    | ContentItem::NoCloseQuote => {
      // Quote characters can be injected without appearing in the DOM text.
      for (open, close) in quotes {
        insert_text_codepoints(seen, open);
        insert_text_codepoints(seen, close);
      }
    }
    ContentItem::Counter { .. } | ContentItem::Counters { .. } => {
      // Counter values can introduce digits that are not present in the document text.
      insert_ascii_digits(seen);
    }
    _ => {}
  }
}

fn collect_content_value(
  seen: &mut FxHashSet<u32>,
  node: &DomNode,
  styles: &crate::style::ComputedStyle,
) {
  let ContentValue::Items(items) = &styles.content_value else {
    return;
  };
  let quotes = styles.quotes.as_ref();
  for item in items {
    collect_content_item(seen, node, quotes, item);
  }
}

/// Collect the set of Unicode codepoints present in DOM text nodes **and** generated content.
///
/// This extends [`crate::dom::collect_text_codepoints`] by also collecting text that will be
/// generated via the `content` property on pseudo-elements.
pub fn collect_used_codepoints(root: &StyledNode) -> Result<Vec<u32>> {
  let mut stack = vec![root];
  let mut seen: FxHashSet<u32> = FxHashSet::default();
  seen.reserve(256);
  let mut deadline_counter = 0usize;

  while let Some(current) = stack.pop() {
    check_active_periodic(
      &mut deadline_counter,
      USED_CODEPOINTS_DEADLINE_STRIDE,
      RenderStage::Cascade,
    )
    .map_err(Error::Render)?;

    if let DomNodeType::Text { content } = &current.node.node_type {
      insert_text_codepoints(&mut seen, content);
    }

    // Include generated content from pseudo-elements so icon fonts and similar content are counted.
    collect_content_value(&mut seen, &current.node, current.styles.as_ref());
    if let Some(styles) = current.before_styles.as_deref() {
      collect_content_value(&mut seen, &current.node, styles);
    }
    if let Some(styles) = current.after_styles.as_deref() {
      collect_content_value(&mut seen, &current.node, styles);
    }
    if let Some(styles) = current.marker_styles.as_deref() {
      collect_content_value(&mut seen, &current.node, styles);
    }
    if let Some(styles) = current.placeholder_styles.as_deref() {
      collect_content_value(&mut seen, &current.node, styles);
    }
    if let Some(styles) = current.file_selector_button_styles.as_deref() {
      collect_content_value(&mut seen, &current.node, styles);
    }
    if let Some(styles) = current.footnote_call_styles.as_deref() {
      collect_content_value(&mut seen, &current.node, styles);
    }
    if let Some(styles) = current.footnote_marker_styles.as_deref() {
      collect_content_value(&mut seen, &current.node, styles);
    }

    for child in current.children.iter().rev() {
      stack.push(child);
    }
  }

  let mut codepoints: Vec<u32> = seen.into_iter().collect();
  codepoints.sort_unstable();
  Ok(codepoints)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn includes_generated_content_from_pseudo_elements() {
    let dom = crate::dom::parse_html("<html><body><i class=\"fa\"></i></body></html>").unwrap();
    let sheet = crate::css::parser::parse_stylesheet(".fa::before{content:\"\\f04c\";}").unwrap();
    let styled = crate::style::cascade::apply_styles(&dom, &sheet);

    let codepoints = collect_used_codepoints(&styled).unwrap();
    assert!(
      codepoints.contains(&0xF04C),
      "expected generated content U+F04C to be included in used codepoints"
    );
  }

  #[test]
  fn includes_generated_content_from_custom_property_var() {
    let dom = crate::dom::parse_html("<html><body><i class=\"fa\"></i></body></html>").unwrap();
    let sheet =
      crate::css::parser::parse_stylesheet(r#".fa{--fa:"\f04c";}.fa::before{content:var(--fa);}"#)
        .unwrap();
    let styled = crate::style::cascade::apply_styles(&dom, &sheet);

    let codepoints = collect_used_codepoints(&styled).unwrap();
    assert!(
      codepoints.contains(&0xF04C),
      "expected var()-substituted generated content U+F04C to be included in used codepoints"
    );
  }
}
