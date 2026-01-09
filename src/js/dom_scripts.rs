//! Best-effort `<script>` element extraction from a fully-parsed DOM tree.
//!
//! # WARNING: not for JavaScript execution
//!
//! This module walks the DOM *after* HTML parsing has completed. That is inherently incapable of
//! implementing the HTML script processing model correctly (parser pausing, `async`/`defer`
//! ordering, base URL timing, etc.). It is intended only for tooling that wants a best-effort list
//! of scripts in DOM order (e.g. diagnostics, bundling, crawlers).
//!
//! For spec-correct JavaScript execution, script specs must be constructed at parse time. Use the
//! streaming parse-time APIs in [`crate::js::streaming`].
//!
//! Note: even with best-effort improvements (like base URL tracking), this module still cannot be
//! spec-correct for executing scripts. It does not model parser pausing, script-inserted vs
//! parser-inserted differences, or dynamic base URL changes from earlier scripts.

use crate::dom::{DomNode, DomNodeType, HTML_NAMESPACE};
use crate::html::base_url_tracker::BaseUrlTracker;

use super::{determine_script_type, ScriptElementSpec};

/// Extract `<script>` elements from a parsed DOM tree in document order.
///
/// # WARNING: not for JavaScript execution
///
/// This is a post-parse DOM scan, so it cannot be used for spec-correct JavaScript execution. It
/// does not (and cannot) model parse-time behaviors like base URL timing, parser pausing, or
/// `async`/`defer` ordering.
///
/// Use [`crate::js::streaming`] to construct [`ScriptElementSpec`] at the moment a `<script>`
/// element finishes parsing.
///
/// Traversal is iterative (non-recursive) and skips inert subtrees:
///
/// - `<template>` contents (`DomNode::traversal_children`)
/// - Shadow roots (`DomNodeType::ShadowRoot`) (conservative initial behavior)
#[deprecated(
  note = "Post-parse DOM scan; cannot be spec-correct for JS execution (base URL timing, parser pausing, async/defer). Use crate::js::streaming instead."
)]
pub fn extract_script_elements(
  dom: &DomNode,
  document_url: Option<&str>,
) -> Vec<ScriptElementSpec> {
  let mut base_url_tracker = BaseUrlTracker::new(document_url);

  let mut out: Vec<ScriptElementSpec> = Vec::new();
  let mut stack: Vec<(&DomNode, bool, bool, bool)> = Vec::new();
  stack.push((dom, false, false, false));

  while let Some((node, in_head, in_foreign_namespace, in_template)) = stack.pop() {
    if let DomNodeType::Element {
      tag_name,
      namespace,
      attributes,
      ..
    } = &node.node_type
    {
      base_url_tracker.on_element_inserted(
        tag_name,
        namespace,
        attributes,
        in_head,
        in_foreign_namespace,
        in_template,
      );

      if tag_name.eq_ignore_ascii_case("script")
        && (namespace.is_empty() || namespace == HTML_NAMESPACE)
      {
        let base_url = base_url_tracker.current_base_url();
        let async_attr = node.get_attribute_ref("async").is_some();
        let defer_attr = node.get_attribute_ref("defer").is_some();

        let raw_src = node.get_attribute_ref("src");
        let src_attr_present = raw_src.is_some();
        let src = raw_src.and_then(|value| base_url_tracker.resolve_script_src(value));

        let mut inline_text = String::new();
        for child in &node.children {
          if let DomNodeType::Text { content } = &child.node_type {
            inline_text.push_str(content);
          }
        }

        out.push(ScriptElementSpec {
          base_url,
          src,
          src_attr_present,
          inline_text,
          async_attr,
          defer_attr,
          // Best-effort: treat DOM-parsed scripts as parser-inserted (matching the common case and
          // enabling scheduler tests). This is not reliable for dynamically inserted scripts.
          parser_inserted: true,
          node_id: None,
          script_type: determine_script_type(node),
        });
      }
    }

    if matches!(node.node_type, DomNodeType::ShadowRoot { .. }) {
      continue;
    }

    let is_head = node
      .tag_name()
      .is_some_and(|tag| tag.eq_ignore_ascii_case("head"))
      && matches!(node.namespace(), Some(ns) if ns.is_empty() || ns == HTML_NAMESPACE);
    let next_in_head = in_head || is_head;
    let next_in_template = in_template || node.is_template_element();
    let next_in_foreign_namespace = in_foreign_namespace
      || matches!(
        node.namespace(),
        Some(ns) if !(ns.is_empty() || ns == HTML_NAMESPACE)
      );

    // Push children in reverse so we traverse left-to-right in document order.
    for child in node.traversal_children().iter().rev() {
      stack.push((
        child,
        next_in_head,
        next_in_foreign_namespace,
        next_in_template,
      ));
    }
  }

  out
}

#[cfg(test)]
mod tests {
  #![allow(deprecated)]

  use super::extract_script_elements;
  use crate::dom::parse_html;

  #[test]
  fn extracts_inline_script_text() {
    let dom = parse_html("<!doctype html><script>console.log(1)</script>").unwrap();
    let scripts = extract_script_elements(&dom, None);
    assert_eq!(scripts.len(), 1);
    assert_eq!(scripts[0].inline_text, "console.log(1)");
    assert!(scripts[0].src.is_none());
  }

  #[test]
  fn resolves_relative_src_against_base_href() {
    let dom = parse_html(
      r#"<!doctype html>
      <html>
        <head>
          <base href="https://example.com/base/">
        </head>
        <body>
          <script src="app.js"></script>
        </body>
      </html>"#,
    )
    .unwrap();

    let scripts = extract_script_elements(&dom, None);
    assert_eq!(scripts.len(), 1);
    assert_eq!(
      scripts[0].src.as_deref(),
      Some("https://example.com/base/app.js")
    );
    assert_eq!(
      scripts[0].base_url.as_deref(),
      Some("https://example.com/base/")
    );
  }

  #[test]
  fn resolves_relative_src_against_document_url() {
    let dom = parse_html(r#"<!doctype html><script src="app.js"></script>"#).unwrap();
    let scripts = extract_script_elements(&dom, Some("https://example.com/dir/page.html"));
    assert_eq!(scripts.len(), 1);
    assert_eq!(
      scripts[0].src.as_deref(),
      Some("https://example.com/dir/app.js")
    );
    assert_eq!(
      scripts[0].base_url.as_deref(),
      Some("https://example.com/dir/page.html")
    );
  }

  #[test]
  fn script_before_base_uses_document_url_for_resolution() {
    // The `<base>` element affects URL resolution only after it has been parsed and inserted.
    // This document has a `<script>` that appears before the `<base href>` in tree order.
    let dom = parse_html(
      r#"<!doctype html>
      <html>
        <script src="a.js"></script>
        <head><base href="https://ex/base/"></head>
      </html>"#,
    )
    .unwrap();
    let scripts = extract_script_elements(&dom, Some("https://example.com/dir/page.html"));
    assert_eq!(scripts.len(), 1);
    assert_eq!(
      scripts[0].src.as_deref(),
      Some("https://example.com/dir/a.js")
    );
    assert_eq!(
      scripts[0].base_url.as_deref(),
      Some("https://example.com/dir/page.html")
    );
  }

  #[test]
  fn trims_src_using_ascii_whitespace() {
    let dom = parse_html(
      r#"<!doctype html>
      <html>
        <head>
          <base href="https://example.com/base/">
        </head>
        <body>
          <script src="  app.js  "></script>
        </body>
      </html>"#,
    )
    .unwrap();
    let scripts = extract_script_elements(&dom, None);
    assert_eq!(scripts.len(), 1);
    assert_eq!(
      scripts[0].src.as_deref(),
      Some("https://example.com/base/app.js")
    );
  }

  #[test]
  fn src_does_not_trim_non_ascii_whitespace() {
    let nbsp = "\u{00A0}";
    let html = format!(
      "<!doctype html><html><head><base href=\"https://example.com/base/\"></head>\
       <body><script src=\"app.js{nbsp}\"></script></body></html>"
    );
    let dom = parse_html(&html).unwrap();
    let scripts = extract_script_elements(&dom, None);
    assert_eq!(scripts.len(), 1);
    assert_eq!(
      scripts[0].src.as_deref(),
      Some("https://example.com/base/app.js%C2%A0")
    );
  }

  #[test]
  fn skips_template_and_shadow_root_scripts_and_preserves_order() {
    let dom = parse_html(
      r#"<!doctype html>
      <html>
        <body>
          <script>a</script>
          <template><script>ignored-template</script></template>
          <div id="host">
            <template shadowroot="open"><script>ignored-shadow</script></template>
            <script>b</script>
          </div>
          <script>c</script>
        </body>
      </html>"#,
    )
    .unwrap();

    let scripts = extract_script_elements(&dom, None);
    let texts: Vec<&str> = scripts.iter().map(|s| s.inline_text.as_str()).collect();
    assert_eq!(texts, vec!["a", "b", "c"]);
  }
}
