//! Parsing for `<meta name="referrer">` directives.
//!
//! Real pages can control the default referrer policy for subresource requests via
//! `<meta name="referrer" content="...">`. This module implements a small subset of the
//! [Referrer Policy](https://www.w3.org/TR/referrer-policy/) surface area sufficient for
//! propagating the policy into our fetch layer.
//!
//! Notes:
//! - We currently extract the final policy after parsing the full document head. Because FastRender
//!   fetches subresources after DOM parse (during style/layout), this is a reasonable approximation
//!   of browser behavior without needing to model incremental parser state.
//! - When multiple policies are provided (comma/whitespace separated), the last recognized token
//!   wins (matches `Referrer-Policy` header parsing semantics).

use crate::dom::{DomNode, DomNodeType, HTML_NAMESPACE};
use crate::error::{Error, RenderStage, Result};
use crate::render_control::check_active_periodic;
use crate::resource::ReferrerPolicy;
use memchr::memchr;
use std::ops::ControlFlow;

const REFERRER_POLICY_DEADLINE_STRIDE: usize = 1024;
const MAX_REFERRER_POLICY_SCAN_BYTES: usize = 256 * 1024;
const MAX_ATTRIBUTES_PER_TAG: usize = 128;

fn scan_html_prefix(html: &str, max_bytes: usize) -> &str {
  if html.len() <= max_bytes {
    return html;
  }
  let mut end = max_bytes.min(html.len());
  while end > 0 && !html.is_char_boundary(end) {
    end -= 1;
  }
  &html[..end]
}

fn for_each_attribute<'a>(
  tag: &'a str,
  mut visit: impl FnMut(&'a str, &'a str) -> ControlFlow<()>,
) {
  let bytes = tag.as_bytes();
  let mut i = 0usize;
  let mut attrs_seen = 0usize;

  // Skip opening `<` + tag name.
  if bytes.get(i) == Some(&b'<') {
    i += 1;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
      i += 1;
    }
    while i < bytes.len() && bytes[i] != b'>' && !bytes[i].is_ascii_whitespace() {
      i += 1;
    }
  }

  while i < bytes.len() {
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
      i += 1;
    }
    if i >= bytes.len() || bytes[i] == b'>' {
      break;
    }
    // Ignore self-closing markers.
    if bytes[i] == b'/' {
      i += 1;
      continue;
    }

    let name_start = i;
    while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'=' && bytes[i] != b'>'
    {
      i += 1;
    }
    let name_end = i;
    if name_end == name_start {
      i = i.saturating_add(1);
      continue;
    }
    let name = &tag[name_start..name_end];

    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
      i += 1;
    }

    let mut value = "";
    if i < bytes.len() && bytes[i] == b'=' {
      i += 1;
      while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
      }

      if i + 1 < bytes.len() && bytes[i] == b'\\' && (bytes[i + 1] == b'"' || bytes[i + 1] == b'\'')
      {
        let quote = bytes[i + 1];
        i += 2;
        let start = i;
        while i < bytes.len() && bytes[i] != quote {
          i += 1;
        }
        value = &tag[start..i];
        if i < bytes.len() {
          i += 1;
        }
      } else if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
        let quote = bytes[i];
        i += 1;
        let start = i;
        while i < bytes.len() && bytes[i] != quote {
          i += 1;
        }
        value = &tag[start..i];
        if i < bytes.len() {
          i += 1;
        }
      } else {
        let start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'>' {
          i += 1;
        }
        value = &tag[start..i];
      }
    }

    attrs_seen += 1;
    if let ControlFlow::Break(()) = visit(name, value) {
      break;
    }
    if attrs_seen >= MAX_ATTRIBUTES_PER_TAG {
      break;
    }
  }
}

/// Parse a referrer policy token list (as used by `<meta name="referrer" content="...">` and the
/// `Referrer-Policy` HTTP header).
///
/// Returns `None` when no recognized policy tokens are present.
pub fn parse_referrer_policy_value(value: &str) -> Option<ReferrerPolicy> {
  ReferrerPolicy::parse_value_list(value)
}

/// Extract the effective referrer policy from an HTML string.
///
/// This is a best-effort, bounded scan used by CLI tools that need to follow client-side redirects
/// without fully parsing the DOM. It intentionally:
/// - Only considers `<meta name="referrer" content="...">` directives found before `<body>` or
///   `</head>`.
/// - Ignores inert `<template>` subtrees.
/// - Skips raw-text elements like `<script>`/`<style>` so quoted markup does not trigger false
///   positives.
///
/// When multiple valid directives are present, the last one wins.
pub fn extract_referrer_policy_from_html(html: &str) -> Option<ReferrerPolicy> {
  let html = scan_html_prefix(html, MAX_REFERRER_POLICY_SCAN_BYTES);
  let bytes = html.as_bytes();
  let mut template_depth: usize = 0;
  let mut policy: Option<ReferrerPolicy> = None;
  let mut i: usize = 0;

  while let Some(rel) = memchr(b'<', &bytes[i..]) {
    let tag_start = i + rel;

    if bytes
      .get(tag_start..tag_start + 4)
      .is_some_and(|head| head == b"<!--")
    {
      let end = super::find_bytes(bytes, tag_start + 4, b"-->")
        .map(|pos| pos + 3)
        .unwrap_or(bytes.len());
      i = end;
      continue;
    }

    if bytes
      .get(tag_start..tag_start + 9)
      .is_some_and(|head| head.eq_ignore_ascii_case(b"<![cdata["))
    {
      let end = super::find_bytes(bytes, tag_start + 9, b"]]>")
        .map(|pos| pos + 3)
        .unwrap_or(bytes.len());
      i = end;
      continue;
    }

    // Markup declarations / processing instructions (`<!doctype ...>`, `<?xml ...?>`, etc.)
    if bytes
      .get(tag_start + 1)
      .is_some_and(|b| *b == b'!' || *b == b'?')
    {
      let Some(end) = super::find_tag_end(bytes, tag_start) else {
        break;
      };
      i = end;
      continue;
    }

    let Some(tag_end) = super::find_tag_end(bytes, tag_start) else {
      break;
    };

    let Some((is_end, name_start, name_end)) =
      super::parse_tag_name_range(bytes, tag_start, tag_end)
    else {
      i = tag_start + 1;
      continue;
    };
    let name = &bytes[name_start..name_end];

    let raw_text_tag: Option<&'static [u8]> = if !is_end && name.eq_ignore_ascii_case(b"script") {
      Some(b"script")
    } else if !is_end && name.eq_ignore_ascii_case(b"style") {
      Some(b"style")
    } else if !is_end && name.eq_ignore_ascii_case(b"textarea") {
      Some(b"textarea")
    } else if !is_end && name.eq_ignore_ascii_case(b"title") {
      Some(b"title")
    } else if !is_end && name.eq_ignore_ascii_case(b"xmp") {
      Some(b"xmp")
    } else {
      None
    };

    if !is_end && name.eq_ignore_ascii_case(b"plaintext") {
      // `<plaintext>` consumes the remainder of the document as text; stop scanning to avoid
      // treating anything following it as markup.
      break;
    }

    if name.eq_ignore_ascii_case(b"template") {
      if is_end {
        if template_depth > 0 {
          template_depth -= 1;
        }
      } else {
        template_depth += 1;
      }
    }

    // Respect the HTML specification requirement that `<meta name=referrer>` appears in the head.
    // Treat everything before `<body>` as head content so documents that omit an explicit `<head>`
    // tag still work (the DOM parser would synthesize a head element).
    if template_depth == 0 && !is_end && name.eq_ignore_ascii_case(b"body") {
      break;
    }
    if template_depth == 0 && is_end && name.eq_ignore_ascii_case(b"head") {
      break;
    }

    if template_depth == 0 && !is_end && name.eq_ignore_ascii_case(b"meta") {
      let tag = &html[tag_start..tag_end];
      let mut meta_name: Option<&str> = None;
      let mut content: Option<&str> = None;

      for_each_attribute(tag, |attr, value| {
        if attr.eq_ignore_ascii_case("name") {
          meta_name = Some(value);
        } else if attr.eq_ignore_ascii_case("content") {
          content = Some(value);
        }
        // Keep scanning: `name` and `content` can appear in any order and might both be present.
        ControlFlow::Continue(())
      });

      if meta_name
        .map(|v| v.eq_ignore_ascii_case("referrer"))
        .unwrap_or(false)
      {
        if let Some(content) = content {
          if let Some(parsed) = parse_referrer_policy_value(content) {
            policy = Some(parsed);
          }
        }
      }
    }

    if let Some(tag) = raw_text_tag {
      i = super::find_raw_text_element_end(bytes, tag_end, tag);
      continue;
    }

    i = tag_end;
  }

  policy
}

/// Extracts the effective document referrer policy from `<meta name="referrer">`.
///
/// Only meta tags inside the document `<head>` are considered. When multiple valid directives are
/// present, the last one wins.
pub fn extract_referrer_policy(dom: &DomNode) -> Option<ReferrerPolicy> {
  extract_referrer_policy_impl(dom, None).ok().flatten()
}

pub(crate) fn extract_referrer_policy_with_deadline(
  dom: &DomNode,
) -> Result<Option<ReferrerPolicy>> {
  let mut counter = 0usize;
  extract_referrer_policy_impl(dom, Some(&mut counter))
}

fn extract_referrer_policy_impl(
  dom: &DomNode,
  mut deadline_counter: Option<&mut usize>,
) -> Result<Option<ReferrerPolicy>> {
  let mut stack = vec![dom];
  let mut head: Option<&DomNode> = None;

  while let Some(node) = stack.pop() {
    if let Some(counter) = deadline_counter.as_deref_mut() {
      check_active_periodic(
        counter,
        REFERRER_POLICY_DEADLINE_STRIDE,
        RenderStage::DomParse,
      )
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
  let mut policy: Option<ReferrerPolicy> = None;

  while let Some((node, in_foreign_namespace)) = stack.pop() {
    if let Some(counter) = deadline_counter.as_deref_mut() {
      check_active_periodic(
        counter,
        REFERRER_POLICY_DEADLINE_STRIDE,
        RenderStage::DomParse,
      )
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
        .map(|n| n.eq_ignore_ascii_case("referrer"))
        .unwrap_or(false)
      {
        if let Some(content) = content_attr {
          if let Some(parsed) = parse_referrer_policy_value(content) {
            policy = Some(parsed);
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

  Ok(policy)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::dom::{DomNode, DomNodeType, SVG_NAMESPACE};
  use selectors::context::QuirksMode;

  #[test]
  fn parses_single_referrer_policy_token() {
    assert_eq!(
      parse_referrer_policy_value("no-referrer"),
      Some(ReferrerPolicy::NoReferrer)
    );
    assert_eq!(
      parse_referrer_policy_value("strict-origin-when-cross-origin"),
      Some(ReferrerPolicy::StrictOriginWhenCrossOrigin)
    );
  }

  #[test]
  fn parses_referrer_policy_token_list_last_wins() {
    assert_eq!(
      parse_referrer_policy_value("unknown, origin, no-referrer"),
      Some(ReferrerPolicy::NoReferrer)
    );
  }

  #[test]
  fn extracts_referrer_policy_from_head_meta() {
    let dom = crate::dom::parse_html(
      r#"<!doctype html><html><head>
        <meta name="referrer" content="no-referrer">
        <meta name="referrer" content="origin">
      </head><body></body></html>"#,
    )
    .unwrap();
    assert_eq!(extract_referrer_policy(&dom), Some(ReferrerPolicy::Origin));
  }

  #[test]
  fn ignores_referrer_policy_meta_outside_head() {
    let dom = crate::dom::parse_html(
      r#"<!doctype html><html><head></head><body>
        <meta name="referrer" content="no-referrer">
      </body></html>"#,
    )
    .unwrap();
    assert_eq!(extract_referrer_policy(&dom), None);
  }

  #[test]
  fn extracts_referrer_policy_from_html_prefix() {
    let html = r#"<!doctype html><html><head>
      <meta name="referrer" content="no-referrer">
      <meta name="referrer" content="origin">
    </head><body></body></html>"#;
    assert_eq!(
      extract_referrer_policy_from_html(html),
      Some(ReferrerPolicy::Origin)
    );
  }

  #[test]
  fn ignores_referrer_policy_from_html_after_body() {
    let html = r#"<!doctype html><html><head></head><body>
      <meta name="referrer" content="no-referrer">
    </body></html>"#;
    assert_eq!(extract_referrer_policy_from_html(html), None);
  }

  #[test]
  fn extract_referrer_policy_ignores_foreign_namespace_meta() {
    let dom = DomNode {
      node_type: DomNodeType::Document {
        quirks_mode: QuirksMode::NoQuirks,
      },
      children: vec![DomNode {
        node_type: DomNodeType::Element {
          tag_name: "head".to_string(),
          namespace: crate::dom::HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![
          DomNode {
            node_type: DomNodeType::Element {
              tag_name: "svg".to_string(),
              namespace: SVG_NAMESPACE.to_string(),
              attributes: vec![],
            },
            children: vec![DomNode {
              node_type: DomNodeType::Element {
                tag_name: "meta".to_string(),
                namespace: SVG_NAMESPACE.to_string(),
                attributes: vec![
                  ("name".to_string(), "referrer".to_string()),
                  ("content".to_string(), "no-referrer".to_string()),
                ],
              },
              children: vec![],
            }],
          },
          DomNode {
            node_type: DomNodeType::Element {
              tag_name: "meta".to_string(),
              namespace: crate::dom::HTML_NAMESPACE.to_string(),
              attributes: vec![
                ("name".to_string(), "referrer".to_string()),
                ("content".to_string(), "origin".to_string()),
              ],
            },
            children: vec![],
          },
        ],
      }],
    };

    assert_eq!(extract_referrer_policy(&dom), Some(ReferrerPolicy::Origin));
  }

  #[test]
  fn extract_referrer_policy_ignores_foreign_namespace_meta_when_foreign_meta_is_last() {
    let dom = DomNode {
      node_type: DomNodeType::Document {
        quirks_mode: QuirksMode::NoQuirks,
      },
      children: vec![DomNode {
        node_type: DomNodeType::Element {
          tag_name: "head".to_string(),
          namespace: crate::dom::HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![
          DomNode {
            node_type: DomNodeType::Element {
              tag_name: "meta".to_string(),
              namespace: crate::dom::HTML_NAMESPACE.to_string(),
              attributes: vec![
                ("name".to_string(), "referrer".to_string()),
                ("content".to_string(), "origin".to_string()),
              ],
            },
            children: vec![],
          },
          DomNode {
            node_type: DomNodeType::Element {
              tag_name: "svg".to_string(),
              namespace: SVG_NAMESPACE.to_string(),
              attributes: vec![],
            },
            children: vec![DomNode {
              node_type: DomNodeType::Element {
                tag_name: "meta".to_string(),
                namespace: SVG_NAMESPACE.to_string(),
                attributes: vec![
                  ("name".to_string(), "referrer".to_string()),
                  ("content".to_string(), "no-referrer".to_string()),
                ],
              },
              children: vec![],
            }],
          },
        ],
      }],
    };

    assert_eq!(extract_referrer_policy(&dom), Some(ReferrerPolicy::Origin));
  }

  #[test]
  fn extract_referrer_policy_ignores_template_contents() {
    let dom = crate::dom::parse_html(
      r#"<!doctype html><html><head>
        <template><meta name="referrer" content="no-referrer"></template>
        <meta name="referrer" content="origin">
      </head></html>"#,
    )
    .unwrap();

    assert_eq!(extract_referrer_policy(&dom), Some(ReferrerPolicy::Origin));
  }

  #[test]
  fn extract_referrer_policy_ignores_shadow_root_contents() {
    let dom = crate::dom::parse_html(
      r#"<!doctype html><html><head></head><body>
        <div id="host">
          <template shadowroot="open"><meta name="referrer" content="no-referrer"></template>
        </div>
      </body></html>"#,
    )
    .unwrap();

    assert_eq!(extract_referrer_policy(&dom), None);
  }
}
