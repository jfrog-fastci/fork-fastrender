#![cfg(test)]

use selectors::context::QuirksMode;

use super::{Document, NULL_NAMESPACE};

#[test]
fn xml_namespace_prefix_type_selector_matches() {
  let mut doc = Document::new(QuirksMode::NoQuirks);

  let root = doc.create_element("root", NULL_NAMESPACE);
  doc.append_child(doc.root(), root).unwrap();
  doc.set_attribute(root, "xmlns:x", "urn:x").unwrap();

  let child = doc.create_element_ns("child", "urn:x", Some("x"));
  doc.append_child(root, child).unwrap();

  assert_eq!(doc.query_selector("x|child", None).unwrap(), Some(child));
}

#[test]
fn xml_namespace_default_namespace_affects_type_selectors() {
  let mut doc = Document::new(QuirksMode::NoQuirks);

  let root = doc.create_element("root", "urn:d");
  doc.append_child(doc.root(), root).unwrap();
  doc.set_attribute(root, "xmlns", "urn:d").unwrap();

  let child = doc.create_element("child", "urn:d");
  doc.append_child(root, child).unwrap();

  assert_eq!(doc.query_selector("child", None).unwrap(), Some(child));
  assert_eq!(doc.query_selector("|child", None).unwrap(), None);
}

#[test]
fn xml_namespace_prefixed_attribute_selector_matches() {
  let mut doc = Document::new(QuirksMode::NoQuirks);

  let root = doc.create_element("root", NULL_NAMESPACE);
  doc.append_child(doc.root(), root).unwrap();
  doc.set_attribute(root, "xmlns:x", "urn:x").unwrap();

  let n = doc.create_element("n", NULL_NAMESPACE);
  doc.set_attribute(n, "x:href", "v").unwrap();
  doc.append_child(root, n).unwrap();

  assert_eq!(
    doc.query_selector("[x|href='v']", None).unwrap(),
    Some(n)
  );
}

