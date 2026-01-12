use crate::dom::HTML_NAMESPACE;
use crate::dom::{DomNode, DomNodeType};
use selectors::context::QuirksMode;

use super::{Document, NodeKind};

#[test]
fn new_xml_defaults_to_no_quirks_and_scripting_disabled() {
  let doc = Document::new_xml();
  assert!(!doc.is_html_document());

  let snapshot = doc.to_renderer_dom();
  match &snapshot.node_type {
    DomNodeType::Document {
      quirks_mode,
      scripting_enabled,
    } => {
      assert_eq!(*quirks_mode, QuirksMode::NoQuirks);
      assert!(!*scripting_enabled);
    }
    other => panic!("expected document root, got {other:?}"),
  }
}

#[test]
fn xml_create_element_slot_is_plain_element() {
  let mut doc = Document::new_xml();
  let slot = doc.create_element("slot", "");
  assert!(
    matches!(&doc.node(slot).kind, NodeKind::Element { tag_name, .. } if tag_name == "slot"),
    "expected XML Document.createElement('slot') to create a plain Element node"
  );
}

#[test]
fn xml_create_element_template_does_not_mark_inert_subtree() {
  let mut doc = Document::new_xml();
  let template = doc.create_element("template", "");
  assert!(
    !doc.node(template).inert_subtree,
    "XML <template> must not trigger HTML template inertness"
  );
}

#[test]
fn xml_create_element_script_does_not_set_force_async() {
  let mut doc = Document::new_xml();
  let script = doc.create_element("script", "");
  assert!(
    !doc.node(script).script_force_async,
    "XML <script> must not get HTML script internal slot defaults"
  );
}

#[test]
fn xml_attributes_are_case_sensitive() {
  let mut doc = Document::new_xml();
  let el = doc.create_element("div", "");

  doc.set_attribute(el, "ID", "x").unwrap();
  assert_eq!(doc.get_attribute(el, "id").unwrap(), None);
  assert_eq!(doc.get_attribute(el, "ID").unwrap(), Some("x"));

  // In XML documents, different-cased names are distinct attributes.
  doc.set_attribute(el, "id", "y").unwrap();
  assert_eq!(doc.get_attribute(el, "id").unwrap(), Some("y"));
  assert_eq!(doc.get_attribute(el, "ID").unwrap(), Some("x"));
}

#[test]
fn xml_head_and_body_are_none_even_with_xhtml_root() {
  let mut doc = Document::new_xml();

  let html = doc.create_element("html", HTML_NAMESPACE);
  doc.append_child(doc.root(), html).unwrap();

  let head = doc.create_element("head", HTML_NAMESPACE);
  doc.append_child(html, head).unwrap();
  let body = doc.create_element("body", HTML_NAMESPACE);
  doc.append_child(html, body).unwrap();

  assert_eq!(doc.head(), None);
  assert_eq!(doc.body(), None);
}

#[test]
fn xml_to_renderer_dom_does_not_inject_wbr_zwsp() {
  let mut doc = Document::new_xml();
  let root_el = doc.create_element("root", "");
  doc.append_child(doc.root(), root_el).unwrap();
  let wbr = doc.create_element("wbr", "");
  doc.append_child(root_el, wbr).unwrap();

  let snapshot = doc.to_renderer_dom();

  fn find_wbr<'a>(node: &'a DomNode) -> Option<&'a DomNode> {
    if let DomNodeType::Element { tag_name, .. } = &node.node_type {
      if tag_name == "wbr" {
        return Some(node);
      }
    }
    node.children.iter().find_map(find_wbr)
  }

  let wbr_node = find_wbr(&snapshot).expect("expected <wbr> element in snapshot");
  assert!(
    wbr_node.children.is_empty(),
    "XML documents must not inject a synthetic ZWSP text node for <wbr>"
  );
}
