use super::Document;
use selectors::context::QuirksMode;

#[test]
fn query_selector_finds_matches_within_detached_scope_subtree() {
  let mut doc = Document::new(QuirksMode::NoQuirks);

  let detached_root = doc.create_element("div", "");
  let span1 = doc.create_element("span", "");
  let inner_div = doc.create_element("div", "");
  let span2 = doc.create_element("span", "");

  doc.append_child(detached_root, span1).unwrap();
  doc.append_child(inner_div, span2).unwrap();
  doc.append_child(detached_root, inner_div).unwrap();

  assert_eq!(
    doc.query_selector("span", Some(detached_root)).unwrap(),
    Some(span1)
  );
  assert_eq!(
    doc.query_selector_all("span", Some(detached_root)).unwrap(),
    vec![span1, span2]
  );
}

#[test]
fn matches_selector_works_on_detached_elements() {
  let mut doc = Document::new(QuirksMode::NoQuirks);

  let detached_root = doc.create_element("div", "");
  let inner_div = doc.create_element("div", "");
  let span = doc.create_element("span", "");

  doc.append_child(detached_root, inner_div).unwrap();
  doc.append_child(inner_div, span).unwrap();

  assert!(doc.matches_selector(span, "span").unwrap());
  assert!(doc.matches_selector(span, "div span").unwrap());
  assert!(doc.matches_selector(span, "div > div > span").unwrap());
}

#[test]
fn query_selector_from_template_scope_does_not_traverse_inert_contents() {
  let mut doc = Document::new(QuirksMode::NoQuirks);

  let template = doc.create_element("template", "");
  let inert_div = doc.create_element("div", "");
  doc.append_child(template, inert_div).unwrap();

  assert_eq!(
    doc.query_selector("div", Some(template)).unwrap(),
    None,
    "template contents should be inert for selector traversal"
  );
  assert_eq!(
    doc.query_selector_all("div", Some(template)).unwrap(),
    Vec::new(),
    "template contents should be inert for selector traversal"
  );
}

