//! Spec-correct, parse-time script discovery helpers for `dom2`.
//!
//! This mirrors [`super::streaming`] but reads from a live [`crate::dom2::Document`], which is the
//! DOM representation used by the JavaScript integration workstream.

use crate::dom2::{Document, NodeId, NodeKind};
use crate::html::base_url_tracker::BaseUrlTracker;

use super::ScriptElementSpec;

/// Build a [`ScriptElementSpec`] from a `<script>` element in a `dom2` document.
///
/// Callers must pass the document base URL that applies at this parse position (typically from
/// [`crate::html::base_url_tracker::BaseUrlTracker::current_base_url`]). This is a required input
/// because the correct base can change over the course of parsing.
pub fn build_parser_inserted_script_element_spec_dom2(
  doc: &Document,
  script_node: NodeId,
  base_url_at_this_point: Option<String>,
) -> ScriptElementSpec {
  let NodeKind::Element { tag_name, .. } = &doc.node(script_node).kind else {
    return ScriptElementSpec {
      base_url: base_url_at_this_point,
      src: None,
      inline_text: String::new(),
      async_attr: false,
      defer_attr: false,
      parser_inserted: true,
      script_type: super::ScriptType::Unknown,
    };
  };

  let async_attr = doc.has_attribute(script_node, "async");
  let defer_attr = doc.has_attribute(script_node, "defer");

  // Use BaseUrlTracker's `src` resolution semantics so relative URLs are preserved when no base is
  // known yet (matching streaming script preparation).
  let resolver = BaseUrlTracker::new(base_url_at_this_point.as_deref());
  let src = doc
    .get_attribute(script_node, "src")
    .and_then(|raw| resolver.resolve_script_src(raw));

  let mut inline_text = String::new();
  for &child in &doc.node(script_node).children {
    if let NodeKind::Text { content } = &doc.node(child).kind {
      inline_text.push_str(content);
    }
  }

  let script_type = super::determine_script_type_from_attrs(
    tag_name,
    doc.get_attribute(script_node, "type"),
    doc.get_attribute(script_node, "language"),
  );

  ScriptElementSpec {
    base_url: base_url_at_this_point,
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
  use super::build_parser_inserted_script_element_spec_dom2;
  use crate::dom2::{Document as Dom2Document, NodeId, NodeKind};
  use crate::js::streaming::build_parser_inserted_script_element_spec;

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
    let spec_dom2 = build_parser_inserted_script_element_spec_dom2(&dom2, script_id, base_url);

    assert_eq!(spec_dom2, spec_dom);
  }
}

