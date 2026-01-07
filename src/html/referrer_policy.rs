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

use crate::dom::{DomNode, DomNodeType};
use crate::error::{Error, RenderStage, Result};
use crate::render_control::check_active_periodic;
use crate::resource::ReferrerPolicy;

const REFERRER_POLICY_DEADLINE_STRIDE: usize = 1024;

/// Parse a referrer policy token list (as used by `<meta name="referrer" content="...">` and the
/// `Referrer-Policy` HTTP header).
///
/// Returns `None` when no recognized policy tokens are present.
pub fn parse_referrer_policy_value(value: &str) -> Option<ReferrerPolicy> {
  let mut policy = None;
  for raw_token in value.split(|c: char| c == ',' || c.is_whitespace()) {
    let token = raw_token.trim();
    if token.is_empty() {
      continue;
    }

    policy = Some(match token {
      t if t.eq_ignore_ascii_case("no-referrer") => ReferrerPolicy::NoReferrer,
      t if t.eq_ignore_ascii_case("no-referrer-when-downgrade") => {
        ReferrerPolicy::NoReferrerWhenDowngrade
      }
      t if t.eq_ignore_ascii_case("origin") => ReferrerPolicy::Origin,
      t if t.eq_ignore_ascii_case("origin-when-cross-origin") => ReferrerPolicy::OriginWhenCrossOrigin,
      t if t.eq_ignore_ascii_case("same-origin") => ReferrerPolicy::SameOrigin,
      t if t.eq_ignore_ascii_case("strict-origin") => ReferrerPolicy::StrictOrigin,
      t if t.eq_ignore_ascii_case("strict-origin-when-cross-origin") => {
        ReferrerPolicy::StrictOriginWhenCrossOrigin
      }
      t if t.eq_ignore_ascii_case("unsafe-url") => ReferrerPolicy::UnsafeUrl,
      _ => continue,
    });
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

pub(crate) fn extract_referrer_policy_with_deadline(dom: &DomNode) -> Result<Option<ReferrerPolicy>> {
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
      check_active_periodic(counter, REFERRER_POLICY_DEADLINE_STRIDE, RenderStage::DomParse)
        .map_err(Error::Render)?;
    }

    if let DomNodeType::ShadowRoot { .. } = node.node_type {
      continue;
    }
    if node.is_template_element() {
      continue;
    }

    if let Some(tag) = node.tag_name() {
      if tag.eq_ignore_ascii_case("head") {
        head = Some(node);
        break;
      }
    }

    for child in node.traversal_children().iter().rev() {
      stack.push(child);
    }
  }

  let Some(head) = head else {
    return Ok(None);
  };

  let mut stack = vec![head];
  let mut policy: Option<ReferrerPolicy> = None;

  while let Some(node) = stack.pop() {
    if let Some(counter) = deadline_counter.as_deref_mut() {
      check_active_periodic(counter, REFERRER_POLICY_DEADLINE_STRIDE, RenderStage::DomParse)
        .map_err(Error::Render)?;
    }

    let tag_name = node.tag_name();
    if let Some(tag) = tag_name {
      if tag.eq_ignore_ascii_case("meta") {
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
    }

    let skip_children =
      matches!(node.node_type, DomNodeType::ShadowRoot { .. }) || node.is_template_element();
    if skip_children {
      continue;
    }

    for child in node.traversal_children().iter().rev() {
      stack.push(child);
    }
  }

  Ok(policy)
}

#[cfg(test)]
mod tests {
  use super::*;

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
}

