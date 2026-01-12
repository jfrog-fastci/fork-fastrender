use crate::dom::HTML_NAMESPACE;
use selectors::context::QuirksMode;

use super::{Document, NodeId, NodeKind};

fn find_first_html_script(doc: &Document) -> NodeId {
  doc
    .nodes()
    .iter()
    .enumerate()
    .find_map(|(idx, node)| match &node.kind {
      NodeKind::Element {
        tag_name,
        namespace,
        ..
      } if tag_name.eq_ignore_ascii_case("script")
        && doc.is_html_case_insensitive_namespace(namespace) =>
      {
        Some(NodeId::from_index(idx))
      }
      _ => None,
    })
    .expect("expected an HTML <script> element")
}

#[test]
fn dom_create_element_initializes_script_internal_slots() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let script = doc.create_element("script", HTML_NAMESPACE);

  let node = doc.node(script);
  assert!(node.script_force_async);
  assert!(!node.script_parser_document);
  assert!(!node.script_already_started);
}

#[test]
fn full_document_parse_marks_scripts_as_parser_inserted() {
  let html = "<!doctype html><html><head><script id=s></script></head></html>";

  let renderer_root = crate::dom::parse_html(html).unwrap();
  let doc_from_renderer = Document::from_renderer_dom(&renderer_root);
  let script_from_renderer = find_first_html_script(&doc_from_renderer);
  let node = doc_from_renderer.node(script_from_renderer);
  assert!(!node.script_force_async);
  assert!(node.script_parser_document);
  assert!(!node.script_already_started);

  let doc_parsed = crate::dom2::parse_html(html).unwrap();
  let script_parsed = find_first_html_script(&doc_parsed);
  let node = doc_parsed.node(script_parsed);
  assert!(!node.script_force_async);
  assert!(node.script_parser_document);
  assert!(!node.script_already_started);
}

#[test]
fn async_attribute_added_clears_force_async_flag() {
  let mut doc = Document::new(QuirksMode::NoQuirks);

  let script = doc.create_element("script", HTML_NAMESPACE);
  assert!(doc.node(script).script_force_async);
  doc.set_attribute(script, "async", "").unwrap();
  assert!(!doc.node(script).script_force_async);

  // The force-async flag is sticky; removing the attribute must not reset it.
  doc.remove_attribute(script, "async").unwrap();
  assert!(!doc.node(script).script_force_async);

  let script2 = doc.create_element("script", HTML_NAMESPACE);
  assert!(doc.node(script2).script_force_async);
  doc.set_bool_attribute(script2, "async", true).unwrap();
  assert!(!doc.node(script2).script_force_async);
}

#[test]
fn async_attribute_added_does_not_touch_non_script_elements() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let div = doc.create_element("div", HTML_NAMESPACE);
  doc.node_mut(div).script_force_async = true;
  doc.set_attribute(div, "async", "").unwrap();
  assert!(doc.node(div).script_force_async);
}

#[test]
fn clone_copies_script_already_started_but_not_other_script_slots() {
  let html = "<!doctype html><html><head><script id=s></script></head></html>";
  let mut doc = crate::dom2::parse_html(html).unwrap();
  let script = find_first_html_script(&doc);

  assert!(
    !doc.node(script).script_force_async && doc.node(script).script_parser_document,
    "expected parser-inserted script defaults"
  );

  doc.set_script_already_started(script, true).unwrap();
  let cloned = doc.clone_node(script, false).unwrap();

  let cloned_node = doc.node(cloned);
  assert!(cloned_node.script_already_started);
  assert!(cloned_node.script_force_async);
  assert!(!cloned_node.script_parser_document);
}

#[test]
fn clone_preserves_async_attribute_and_keeps_force_async_cleared() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let script = doc.create_element("script", HTML_NAMESPACE);
  doc.set_attribute(script, "async", "").unwrap();
  assert!(doc.has_attribute(script, "async").unwrap());
  assert!(!doc.node(script).script_force_async);

  // Ensure clone doesn't just copy this field from the source node.
  doc.node_mut(script).script_force_async = true;

  let cloned = doc.clone_node(script, false).unwrap();
  assert!(doc.has_attribute(cloned, "async").unwrap());

  let cloned_node = doc.node(cloned);
  assert!(!cloned_node.script_force_async);
  assert!(!cloned_node.script_parser_document);
  assert!(!cloned_node.script_already_started);

  assert!(doc.remove_attribute(cloned, "async").unwrap());
  assert!(!doc.has_attribute(cloned, "async").unwrap());
  assert!(!doc.node(cloned).script_force_async);
}
