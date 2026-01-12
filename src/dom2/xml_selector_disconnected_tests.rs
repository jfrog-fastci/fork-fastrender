#![cfg(test)]

use super::Document;

#[test]
fn xml_selectors_are_case_sensitive_in_detached_subtrees() {
  let mut doc = Document::new_xml();

  let foo = doc.create_element("Foo", "");
  let bar = doc.create_element("Bar", "");
  doc.append_child(foo, bar).unwrap();
  doc.set_attribute(foo, "ID", "x").unwrap();

  assert!(doc.matches_selector(foo, "Foo").unwrap());
  assert!(!doc.matches_selector(foo, "foo").unwrap());

  assert!(doc.matches_selector(bar, "Foo Bar").unwrap());
  assert!(!doc.matches_selector(bar, "foo Bar").unwrap());

  assert!(doc.matches_selector(foo, r#"[ID="x"]"#).unwrap());
  assert!(!doc.matches_selector(foo, r#"[id="x"]"#).unwrap());

  assert_eq!(doc.query_selector("Foo", Some(foo)).unwrap(), Some(foo));
  assert_eq!(doc.query_selector("foo", Some(foo)).unwrap(), None);
  assert_eq!(doc.query_selector(r#"[ID="x"]"#, Some(foo)).unwrap(), Some(foo));
  assert_eq!(doc.query_selector(r#"[id="x"]"#, Some(foo)).unwrap(), None);
}

#[test]
fn xml_selectors_are_case_sensitive_in_document_fragment_scopes() {
  let mut doc = Document::new_xml();
  let frag = doc.create_document_fragment();

  let foo = doc.create_element("Foo", "");
  doc.set_attribute(foo, "ID", "x").unwrap();
  doc.append_child(frag, foo).unwrap();

  assert_eq!(doc.query_selector("Foo", Some(frag)).unwrap(), Some(foo));
  assert_eq!(doc.query_selector("foo", Some(frag)).unwrap(), None);

  assert_eq!(doc.query_selector(r#"[ID="x"]"#, Some(frag)).unwrap(), Some(foo));
  assert_eq!(doc.query_selector(r#"[id="x"]"#, Some(frag)).unwrap(), None);

  assert_eq!(
    doc.query_selector(":scope > Foo", Some(frag)).unwrap(),
    Some(foo)
  );
  assert_eq!(doc.query_selector(":scope > foo", Some(frag)).unwrap(), None);
}

