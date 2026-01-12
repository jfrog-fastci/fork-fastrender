#![cfg(test)]

use crate::dom::HTML_NAMESPACE;
use selectors::context::QuirksMode;

use super::Document;

#[test]
fn xml_query_selector_is_case_sensitive_for_tag_and_attribute_names() {
  let mut doc = Document::new_xml();
  let foo = doc.create_element("Foo", "");
  doc.set_attribute(foo, "id", "A").unwrap();
  doc.append_child(doc.root(), foo).unwrap();

  assert_eq!(doc.query_selector("Foo", None).unwrap(), Some(foo));
  assert_eq!(doc.query_selector("foo", None).unwrap(), None);

  assert_eq!(doc.query_selector(r#"[id="A"]"#, None).unwrap(), Some(foo));
  assert_eq!(doc.query_selector(r#"[ID="A"]"#, None).unwrap(), None);
}

#[test]
fn xml_xhtml_elements_are_case_sensitive_even_in_html_namespace() {
  let mut doc = Document::new_xml();
  let foo = doc.create_element("Foo", HTML_NAMESPACE);
  doc.set_attribute(foo, "id", "A").unwrap();
  doc.append_child(doc.root(), foo).unwrap();

  assert_eq!(doc.query_selector("Foo", None).unwrap(), Some(foo));
  assert_eq!(doc.query_selector("foo", None).unwrap(), None);

  assert_eq!(doc.query_selector(r#"[id="A"]"#, None).unwrap(), Some(foo));
  assert_eq!(doc.query_selector(r#"[ID="A"]"#, None).unwrap(), None);
}

#[test]
fn html_document_query_selector_remains_case_insensitive() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let div = doc.create_element("DIV", "");
  doc.append_child(doc.root(), div).unwrap();

  assert_eq!(doc.query_selector("div", None).unwrap(), Some(div));
  assert_eq!(doc.query_selector("DIV", None).unwrap(), Some(div));
}

#[test]
fn xml_detached_scope_and_matches_are_case_sensitive() {
  let mut doc = Document::new_xml();
  let scope = doc.create_element("Scope", "");
  let child = doc.create_element("Foo", "");
  doc.set_attribute(child, "id", "A").unwrap();
  doc.append_child(scope, child).unwrap();

  // The scope subtree is detached (not appended to `doc.root()`), so querySelector(All) must use a
  // subtree snapshot and still respect XML case-sensitivity.
  assert_eq!(doc.query_selector("Foo", Some(scope)).unwrap(), Some(child));
  assert_eq!(doc.query_selector("foo", Some(scope)).unwrap(), None);

  assert!(doc.matches_selector(child, "Foo").unwrap());
  assert!(!doc.matches_selector(child, "foo").unwrap());

  assert!(doc.matches_selector(child, r#"[id="A"]"#).unwrap());
  assert!(!doc.matches_selector(child, r#"[ID="A"]"#).unwrap());
}
