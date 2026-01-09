//! Spec-correct, parse-time script discovery helpers.
//!
//! The HTML script processing model is defined in terms of *parsing events* (parser pausing,
//! `async`/`defer` behavior, and the base URL as it exists at that moment). Any API that scans the
//! DOM after parsing cannot be spec-correct for execution.
//!
//! This module provides small building blocks intended to be used by a streaming HTML
//! parse+execute pipeline.

use crate::dom::{DomNode, DomNodeType};
use crate::html::base_url_tracker::resolve_script_src_at_parse_time;

use super::{determine_script_type, ScriptElementSpec};

/// Build a [`ScriptElementSpec`] from a `<script>` element *at the moment it finishes parsing*.
///
/// Callers must pass the base URL that applies at this parse position (typically from
/// [`crate::html::base_url_tracker::BaseUrlTracker::current_base_url`]). This is a required input
/// because the correct base can change over the course of parsing.
pub fn build_parser_inserted_script_element_spec(
  script: &DomNode,
  base_url_at_this_point: Option<String>,
) -> ScriptElementSpec {
  let base_url_ref = base_url_at_this_point.as_deref();
  let async_attr = script.get_attribute_ref("async").is_some();
  let defer_attr = script.get_attribute_ref("defer").is_some();

  let src = script
    .get_attribute_ref("src")
    .and_then(|value| resolve_script_src_at_parse_time(base_url_ref, value));

  let mut inline_text = String::new();
  for child in &script.children {
    if let DomNodeType::Text { content } = &child.node_type {
      inline_text.push_str(content);
    }
  }

  ScriptElementSpec {
    base_url: base_url_at_this_point,
    src,
    inline_text,
    async_attr,
    defer_attr,
    parser_inserted: true,
    script_type: determine_script_type(script),
  }
}

#[cfg(test)]
mod tests {
  use super::build_parser_inserted_script_element_spec;
  use crate::css::loader::resolve_href_with_base;
  use crate::dom::{parse_html, DomNode, HTML_NAMESPACE};
  use crate::html::base_url_tracker::BaseUrlTracker;
  use crate::html::document_base_url;

  fn first_html_script_node<'a>(root: &'a DomNode) -> &'a DomNode {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
      if node
        .tag_name()
        .is_some_and(|tag| tag.eq_ignore_ascii_case("script"))
        && matches!(node.namespace(), Some(ns) if ns.is_empty() || ns == HTML_NAMESPACE)
      {
        return node;
      }
      for child in node.traversal_children().iter().rev() {
        stack.push(child);
      }
    }
    panic!("expected HTML to contain a <script> element");
  }

  #[test]
  fn base_url_timing_regression_script_before_base_href() {
    let html = r#"<!doctype html>
      <html>
        <head>
          <script src="a.js"></script>
          <base href="https://ex/base/">
        </head>
      </html>"#;
    let document_url = "https://ex/doc.html";
    let dom = parse_html(html).unwrap();

    // Naive "whole-document base URL" resolution applies `<base href>` to *all* scripts. That
    // corresponds to `dom_scripts::extract_script_elements`' old behavior and is not spec-correct
    // for parser-inserted script preparation time.
    let final_base_url = document_base_url(&dom, Some(document_url));
    assert_eq!(final_base_url.as_deref(), Some("https://ex/base/"));
    let naive_src = resolve_href_with_base(final_base_url.as_deref(), "a.js");
    assert_eq!(naive_src.as_deref(), Some("https://ex/base/a.js"));

    // Parse-time: before the `<base>` tag is encountered, the document base URL is still the
    // document URL.
    let base_tracker = BaseUrlTracker::new(Some(document_url));
    let script_node = first_html_script_node(&dom);
    let spec = build_parser_inserted_script_element_spec(script_node, base_tracker.current_base_url());
    assert_eq!(spec.base_url.as_deref(), Some(document_url));
    assert_eq!(spec.src.as_deref(), Some("https://ex/a.js"));
    assert!(spec.parser_inserted);
  }

  #[test]
  fn src_without_base_url_is_preserved_as_relative() {
    let dom = parse_html(r#"<!doctype html><script src="a.js"></script>"#).unwrap();
    let script_node = first_html_script_node(&dom);
    let spec = build_parser_inserted_script_element_spec(script_node, None);
    assert_eq!(spec.src.as_deref(), Some("a.js"));
  }

  #[test]
  fn src_without_base_url_rejects_dangerous_schemes() {
    for src in [
      "javascript:alert(1)",
      "vbscript:msgbox(1)",
      "mailto:test@example.com",
      " \t\r\nJaVaScRiPt:alert(1)\n",
    ] {
      let html = format!(r#"<!doctype html><script src="{src}"></script>"#);
      let dom = parse_html(&html).unwrap();
      let script_node = first_html_script_node(&dom);
      let spec = build_parser_inserted_script_element_spec(script_node, None);
      assert!(spec.src.is_none(), "expected src to be rejected: {src}");
    }
  }
}
