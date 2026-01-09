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
use crate::{dom2, html};

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

/// Build a [`ScriptElementSpec`] from a `dom2` `<script>` element *at the moment it finishes
/// parsing*.
pub fn build_parser_inserted_script_element_spec_dom2(
  doc: &dom2::Document,
  script: dom2::NodeId,
  base_url_tracker: &html::base_url_tracker::BaseUrlTracker,
) -> ScriptElementSpec {
  let base_url = base_url_tracker.current_base_url();

  let async_attr = doc.has_attribute(script, "async");
  let defer_attr = doc.has_attribute(script, "defer");

  let src = doc
    .get_attribute(script, "src")
    .and_then(|raw_src| base_url_tracker.resolve_script_src(raw_src));

  let mut inline_text = String::new();
  for &child in &doc.node(script).children {
    if let dom2::NodeKind::Text { content } = &doc.node(child).kind {
      inline_text.push_str(content);
    }
  }

  let script_type = super::determine_script_type_dom2(doc, script);

  ScriptElementSpec {
    base_url,
    src,
    inline_text,
    async_attr,
    defer_attr,
    parser_inserted: true,
    script_type,
  }
}

#[cfg(test)]
mod tests {
  use super::{
    build_parser_inserted_script_element_spec, build_parser_inserted_script_element_spec_dom2,
  };
  use crate::css::loader::resolve_href_with_base;
  use crate::dom::{parse_html, DomNode, DomNodeType, HTML_NAMESPACE};
  use crate::html::base_url_tracker::BaseUrlTracker;
  use crate::html::document_base_url;
  use crate::js::determine_script_type;
  use selectors::context::QuirksMode;

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
    let spec =
      build_parser_inserted_script_element_spec(script_node, base_tracker.current_base_url());
    assert_eq!(spec.base_url.as_deref(), Some(document_url));
    assert_eq!(spec.src.as_deref(), Some("https://ex/a.js"));
    assert!(spec.parser_inserted);
  }

  #[test]
  fn src_without_base_url_is_preserved_as_relative() {
    let dom = parse_html(r#"<!doctype html><script src="a.js"></script>"#).unwrap();
    let script_node = first_html_script_node(&dom);
    let spec = build_parser_inserted_script_element_spec(script_node, None);
    assert_eq!(spec.base_url, None);
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

  fn build_dom2_script(
    attrs: &[(&str, &str)],
    text_children: &[&str],
  ) -> (crate::dom2::Document, crate::dom2::NodeId) {
    fn first_dom2_html_script_node(doc: &crate::dom2::Document) -> crate::dom2::NodeId {
      let mut stack = vec![doc.root()];
      while let Some(id) = stack.pop() {
        let node = doc.node(id);
        if let crate::dom2::NodeKind::Element {
          tag_name, namespace, ..
        } = &node.kind
        {
          if tag_name.eq_ignore_ascii_case("script")
            && (namespace.is_empty() || namespace == HTML_NAMESPACE)
          {
            return id;
          }
        }
        for &child in node.children.iter().rev() {
          stack.push(child);
        }
      }
      panic!("expected dom2 document to contain an HTML <script> element");
    }

    let script = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "script".to_string(),
        namespace: String::new(),
        attributes: attrs
          .iter()
          .map(|(k, v)| (k.to_string(), v.to_string()))
          .collect(),
      },
      children: text_children
        .iter()
        .map(|content| DomNode {
          node_type: DomNodeType::Text {
            content: content.to_string(),
          },
          children: Vec::new(),
        })
        .collect(),
    };

    let root = DomNode {
      node_type: DomNodeType::Document {
        quirks_mode: QuirksMode::NoQuirks,
      },
      children: vec![script],
    };

    let doc = crate::dom2::Document::from_renderer_dom(&root);
    let script_id = first_dom2_html_script_node(&doc);
    (doc, script_id)
  }

  fn build_renderer_dom_script(type_value: Option<&str>, language_value: Option<&str>) -> DomNode {
    let mut attributes: Vec<(String, String)> = Vec::new();
    if let Some(value) = type_value {
      attributes.push(("type".to_string(), value.to_string()));
    }
    if let Some(value) = language_value {
      attributes.push(("language".to_string(), value.to_string()));
    }

    DomNode {
      node_type: DomNodeType::Element {
        tag_name: "script".to_string(),
        namespace: String::new(),
        attributes,
      },
      children: Vec::new(),
    }
  }

  #[test]
  fn inline_text_concatenates_adjacent_text_nodes_dom2() {
    let (doc, script) = build_dom2_script(&[], &["console.", "log(1);"]);
    let base_tracker = BaseUrlTracker::new(None);

    let spec = build_parser_inserted_script_element_spec_dom2(&doc, script, &base_tracker);
    assert_eq!(spec.inline_text, "console.log(1);");
  }

  #[test]
  fn async_defer_parsing_is_presence_based_dom2() {
    let (doc, script) = build_dom2_script(&[("async", "false"), ("defer", "0")], &[]);
    let base_tracker = BaseUrlTracker::new(None);

    let spec = build_parser_inserted_script_element_spec_dom2(&doc, script, &base_tracker);
    assert!(spec.async_attr);
    assert!(spec.defer_attr);

    let (doc2, script2) = build_dom2_script(&[], &[]);
    let spec2 = build_parser_inserted_script_element_spec_dom2(&doc2, script2, &base_tracker);
    assert!(!spec2.async_attr);
    assert!(!spec2.defer_attr);
  }

  #[test]
  fn base_url_timing_without_document_url_preserves_relative_src_dom2() {
    let (doc, script) = build_dom2_script(&[("src", "a.js")], &[]);
    let base_tracker = BaseUrlTracker::new(None);

    let spec = build_parser_inserted_script_element_spec_dom2(&doc, script, &base_tracker);
    assert_eq!(spec.base_url, None);
    assert_eq!(spec.src, base_tracker.resolve_script_src("a.js"));
    assert_eq!(spec.src.as_deref(), Some("a.js"));
  }

  #[test]
  fn script_type_mapping_matches_determine_script_type_matrix_dom2() {
    let base_tracker = BaseUrlTracker::new(None);

    let cases: Vec<(Option<&str>, Option<&str>)> = vec![
      (None, None),
      (Some(""), None),
      (Some("  "), None),
      (None, Some("")),
      (None, Some("ecmascript")),
      (Some("module"), None),
      (Some("importmap"), None),
      (Some("text/javascript; charset=utf-8"), None),
      (Some("module; charset=utf-8"), None),
      (Some("module"), Some("javascript")),
      (None, Some("javascript1.5")),
    ];

    for (type_value, language_value) in cases {
      let mut attrs: Vec<(&str, &str)> = Vec::new();
      if let Some(v) = type_value {
        attrs.push(("type", v));
      }
      if let Some(v) = language_value {
        attrs.push(("language", v));
      }
      let (doc, script) = build_dom2_script(&attrs, &[]);
      let spec = build_parser_inserted_script_element_spec_dom2(&doc, script, &base_tracker);

      let renderer_dom = build_renderer_dom_script(type_value, language_value);
      let expected = determine_script_type(&renderer_dom);
      assert_eq!(
        spec.script_type, expected,
        "type={type_value:?} language={language_value:?}"
      );
    }
  }
}
