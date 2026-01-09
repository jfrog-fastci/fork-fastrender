//! Spec-correct, parse-time script discovery helpers for `dom2`.
//!
//! Like [`crate::js::streaming`], these helpers are intended to be invoked at the moment a `<script>`
//! element finishes parsing, so they can observe the correct base URL and parser-inserted flags.

use crate::dom::HTML_NAMESPACE;
use crate::dom2::{Document, NodeId, NodeKind};
use crate::html::base_url_tracker::BaseUrlTracker;

use super::{determine_script_type_dom2, ScriptElementSpec, ScriptType};

/// Build a [`ScriptElementSpec`] from a `<script>` element in a `dom2` document *at the moment it
/// finishes parsing*.
///
/// This mirrors [`crate::js::dom_scripts::extract_script_elements`] (tooling-only DOM scan), but is
/// intended for parse-time construction so the base URL timing is correct.
///
/// For non-HTML-namespace elements (e.g. `<script>` inside `<svg>`), this returns an "ignored"
/// spec with `script_type=Unknown`.
pub fn build_parser_inserted_script_element_spec_dom2(
  doc: &Document,
  script: NodeId,
  base: &BaseUrlTracker,
) -> ScriptElementSpec {
  let base_url = base.current_base_url();

  let NodeKind::Element {
    tag_name,
    namespace,
    ..
  } = &doc.node(script).kind
  else {
    return ScriptElementSpec {
      base_url,
      src: None,
      src_attr_present: false,
      inline_text: String::new(),
      async_attr: false,
      defer_attr: false,
      parser_inserted: true,
      script_type: ScriptType::Unknown,
    };
  };

  // Only scripts in the HTML namespace participate in the HTML script processing model.
  if !tag_name.eq_ignore_ascii_case("script")
    || !(namespace.is_empty() || namespace == HTML_NAMESPACE)
  {
    return ScriptElementSpec {
      base_url,
      src: None,
      src_attr_present: false,
      inline_text: String::new(),
      async_attr: false,
      defer_attr: false,
      parser_inserted: true,
      script_type: ScriptType::Unknown,
    };
  }

  // HTML: "prepare a script" early-outs when the script element is not connected.
  //
  // `dom2` represents `<template>` contents by keeping the children in-tree while marking the
  // `<template>` element as `inert_subtree`. Scripts inside such subtrees must be ignored: they
  // must not be fetched or executed by the HTML script processing model.
  if !doc.is_connected_for_scripting(script) {
    return ScriptElementSpec {
      base_url,
      src: None,
      src_attr_present: false,
      inline_text: String::new(),
      async_attr: false,
      defer_attr: false,
      parser_inserted: true,
      script_type: ScriptType::Unknown,
    };
  }

  let async_attr = doc.has_attribute(script, "async").unwrap_or(false);
  let defer_attr = doc.has_attribute(script, "defer").unwrap_or(false);

  let raw_src = doc.get_attribute(script, "src").ok().flatten();
  let src_attr_present = raw_src.is_some();
  let src = raw_src.and_then(|raw_src| base.resolve_script_src(raw_src));

  let mut inline_text = String::new();
  for &child in &doc.node(script).children {
    if let NodeKind::Text { content } = &doc.node(child).kind {
      inline_text.push_str(content);
    }
  }

  ScriptElementSpec {
    base_url,
    src,
    src_attr_present,
    inline_text,
    async_attr,
    defer_attr,
    parser_inserted: true,
    script_type: determine_script_type_dom2(doc, script),
  }
}

#[cfg(test)]
mod tests {
  use super::build_parser_inserted_script_element_spec_dom2;
  use crate::dom::SVG_NAMESPACE;
  use crate::dom2::{Document as Dom2Document, NodeId, NodeKind};
  use crate::html::base_url_tracker::BaseUrlTracker;
  use crate::js::streaming::build_parser_inserted_script_element_spec;
  use crate::js::ScriptType;
  use selectors::context::QuirksMode;

  #[test]
  fn dom2_builder_resolves_src_and_concats_inline_text() {
    let mut doc = Dom2Document::new(QuirksMode::NoQuirks);
    let script = doc.create_element("script", "");
    doc
      .set_attribute(script, "src", "  app.js  ")
      .expect("set_attribute");
    doc
      .set_bool_attribute(script, "async", true)
      .expect("set_bool_attribute");

    let text_a = doc.create_text("console.");
    let text_b = doc.create_text("log(1);");
    let child_el = doc.create_element("span", "");
    let child_el_text = doc.create_text("ignored");
    doc
      .append_child(child_el, child_el_text)
      .expect("append_child");

    doc.append_child(script, text_a).expect("append_child");
    doc.append_child(script, child_el).expect("append_child");
    doc.append_child(script, text_b).expect("append_child");
    doc.append_child(doc.root(), script).expect("append_child");

    let mut base = BaseUrlTracker::new(Some("https://example.com/dir/page.html"));
    base.on_element_inserted(
      "base",
      crate::dom::HTML_NAMESPACE,
      &[("href".to_string(), "https://example.com/base/".to_string())],
      /* in_head */ true,
      /* in_foreign_namespace */ false,
      /* in_template */ false,
    );

    let spec = build_parser_inserted_script_element_spec_dom2(&doc, script, &base);
    assert_eq!(spec.base_url.as_deref(), Some("https://example.com/base/"));
    assert_eq!(
      spec.src.as_deref(),
      Some("https://example.com/base/app.js"),
      "expected ASCII whitespace trimming for src"
    );
    assert_eq!(spec.inline_text, "console.log(1);");
    assert!(spec.async_attr);
    assert!(!spec.defer_attr);
    assert!(spec.parser_inserted);
    assert_eq!(spec.script_type, ScriptType::Classic);
  }

  #[test]
  fn dom2_builder_ignores_non_html_namespace_scripts() {
    let mut doc = Dom2Document::new(QuirksMode::NoQuirks);
    let script = doc.create_element("script", SVG_NAMESPACE);
    doc
      .set_attribute(script, "src", "app.js")
      .expect("set_attribute");
    doc.append_child(doc.root(), script).expect("append_child");

    let base = BaseUrlTracker::new(Some("https://example.com/dir/page.html"));
    let spec = build_parser_inserted_script_element_spec_dom2(&doc, script, &base);
    assert_eq!(spec.script_type, ScriptType::Unknown);
    assert!(spec.src.is_none());
    assert_eq!(spec.inline_text, "");
  }

  #[test]
  fn dom2_builder_ignores_scripts_inside_inert_template_subtrees() {
    let mut doc = Dom2Document::new(QuirksMode::NoQuirks);
    let template = doc.create_element("template", "");
    doc.node_mut(template).inert_subtree = true;

    let script = doc.create_element("script", "");
    doc
      .set_attribute(script, "src", "inert.js")
      .expect("set_attribute");
    let text = doc.create_text("console.log('inert');");
    doc.append_child(script, text).expect("append_child");
    doc.append_child(template, script).expect("append_child");
    doc.append_child(doc.root(), template).expect("append_child");

    let base = BaseUrlTracker::new(Some("https://example.com/dir/page.html"));
    let spec = build_parser_inserted_script_element_spec_dom2(&doc, script, &base);

    assert_eq!(
      spec.base_url.as_deref(),
      Some("https://example.com/dir/page.html")
    );
    assert_eq!(spec.script_type, ScriptType::Unknown);
    assert!(
      spec.src.is_none(),
      "template scripts must not fetch external resources"
    );
    assert_eq!(spec.inline_text, "");
  }

  fn find_first_script_dom2(doc: &Dom2Document) -> NodeId {
    let mut stack = vec![doc.root()];
    while let Some(id) = stack.pop() {
      if let NodeKind::Element { tag_name, .. } = &doc.node(id).kind {
        if tag_name.eq_ignore_ascii_case("script") {
          return id;
        }
      }
      for &child in doc.node(id).children.iter().rev() {
        stack.push(child);
      }
    }
    panic!("expected dom2 document to contain a <script> element");
  }

  fn find_first_script_domnode<'a>(root: &'a crate::dom::DomNode) -> &'a crate::dom::DomNode {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
      if node
        .tag_name()
        .is_some_and(|tag| tag.eq_ignore_ascii_case("script"))
      {
        return node;
      }
      for child in node.traversal_children().iter().rev() {
        stack.push(child);
      }
    }
    panic!("expected DomNode document to contain a <script> element");
  }

  #[test]
  fn dom2_script_spec_matches_renderer_domnode_when_base_is_known() {
    let html = r#"<!doctype html>
      <script async defer type="module" src="a.js">console.log(1)</script>"#;
    let base_url = Some("https://example.com/base/".to_string());

    let dom = crate::dom::parse_html(html).unwrap();
    let script_dom = find_first_script_domnode(&dom);
    let spec_dom = build_parser_inserted_script_element_spec(script_dom, base_url.clone());

    let dom2 = Dom2Document::from_renderer_dom(&dom);
    let script_id = find_first_script_dom2(&dom2);
    let base_tracker = BaseUrlTracker::new(base_url.as_deref());
    let spec_dom2 = build_parser_inserted_script_element_spec_dom2(&dom2, script_id, &base_tracker);

    assert_eq!(spec_dom2, spec_dom);
  }
}
