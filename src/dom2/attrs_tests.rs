use crate::dom::SVG_NAMESPACE;
use selectors::context::QuirksMode;

use super::{Document, DomError, NodeId, NodeKind};

fn make_element(doc: &mut Document, namespace: &str) -> NodeId {
  doc.push_node(
    NodeKind::Element {
      tag_name: "div".to_string(),
      namespace: namespace.to_string(),
      attributes: Vec::new(),
    },
    Some(doc.root()),
    /* inert_subtree */ false,
  )
}

fn make_text(doc: &mut Document, content: &str) -> NodeId {
  doc.push_node(
    NodeKind::Text {
      content: content.to_string(),
    },
    Some(doc.root()),
    /* inert_subtree */ false,
  )
}

#[test]
fn html_attribute_names_are_ascii_case_insensitive() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let el = make_element(&mut doc, /* namespace */ "");

  assert_eq!(doc.set_attribute(el, "id", "a").unwrap(), true);
  assert_eq!(doc.set_attribute(el, "CLASS", "b").unwrap(), true);

  assert_eq!(doc.get_attribute(el, "ID"), Some("a"));
  assert_eq!(doc.get_attribute(el, "class"), Some("b"));
  assert!(doc.has_attribute(el, "Id"));
  assert!(doc.has_attribute(el, "ClAsS"));

  assert_eq!(doc.id(el), Some("a"));
  assert_eq!(doc.class_name(el), Some("b"));

  // Setting the same value is a no-op.
  assert_eq!(doc.set_attribute(el, "ID", "a").unwrap(), false);

  // Updating a value should not change attribute insertion order.
  assert_eq!(doc.set_attribute(el, "ID", "c").unwrap(), true);
  let attrs = match &doc.node(el).kind {
    NodeKind::Element { attributes, .. } => attributes,
    _ => panic!("expected element"),
  };
  assert_eq!(attrs.len(), 2);
  assert!(attrs[0].0.eq_ignore_ascii_case("id"));
  assert!(attrs[1].0.eq_ignore_ascii_case("class"));
}

#[test]
fn non_html_attribute_names_are_case_sensitive() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let el = make_element(&mut doc, SVG_NAMESPACE);

  assert_eq!(doc.set_attribute(el, "viewBox", "A").unwrap(), true);
  assert_eq!(doc.get_attribute(el, "viewBox"), Some("A"));
  assert_eq!(doc.get_attribute(el, "viewbox"), None);

  // Different casing is treated as a distinct attribute.
  assert_eq!(doc.set_attribute(el, "viewbox", "B").unwrap(), true);
  assert_eq!(doc.get_attribute(el, "viewBox"), Some("A"));
  assert_eq!(doc.get_attribute(el, "viewbox"), Some("B"));

  let attrs = match &doc.node(el).kind {
    NodeKind::Element { attributes, .. } => attributes,
    _ => panic!("expected element"),
  };
  assert_eq!(attrs.len(), 2);
}

#[test]
fn remove_attribute_changed_flag_behavior() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let el = make_element(&mut doc, /* namespace */ "");

  assert_eq!(doc.set_attribute(el, "id", "a").unwrap(), true);
  assert_eq!(doc.remove_attribute(el, "ID").unwrap(), true);
  assert_eq!(doc.remove_attribute(el, "id").unwrap(), false);
}

#[test]
fn set_bool_attribute_matches_existing_interaction_helpers() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let el = make_element(&mut doc, /* namespace */ "");

  assert_eq!(doc.set_bool_attribute(el, "disabled", true).unwrap(), true);
  assert_eq!(doc.set_bool_attribute(el, "DISABLED", true).unwrap(), false);
  assert!(doc.has_attribute(el, "disabled"));

  assert_eq!(doc.set_bool_attribute(el, "disabled", false).unwrap(), true);
  assert_eq!(doc.set_bool_attribute(el, "disabled", false).unwrap(), false);
  assert!(!doc.has_attribute(el, "disabled"));
}

#[test]
fn set_attribute_errors_on_non_element_nodes() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let text = make_text(&mut doc, "x");

  assert_eq!(
    doc.set_attribute(text, "id", "a"),
    Err(DomError::InvalidNodeType)
  );
  assert_eq!(
    doc.remove_attribute(text, "id"),
    Err(DomError::InvalidNodeType)
  );
}

#[test]
fn text_data_editing_works_and_errors_on_non_text_nodes() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let text = make_text(&mut doc, "hello");
  let el = make_element(&mut doc, /* namespace */ "");

  assert_eq!(doc.text_data(text).unwrap(), "hello");
  assert_eq!(doc.set_text_data(text, "hello").unwrap(), false);
  assert_eq!(doc.set_text_data(text, "world").unwrap(), true);
  assert_eq!(doc.text_data(text).unwrap(), "world");

  assert_eq!(doc.text_data(el), Err(DomError::InvalidNodeType));
  assert_eq!(doc.set_text_data(el, "x"), Err(DomError::InvalidNodeType));
}

