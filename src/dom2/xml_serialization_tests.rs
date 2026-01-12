#![cfg(test)]

use selectors::context::QuirksMode;

use super::Document;

#[test]
fn injects_xhtml_namespace_on_root_element() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let div = doc.create_element("div", /* namespace */ "");
  doc.append_child(doc.root(), div).unwrap();

  assert_eq!(
    doc.xml_serialize(doc.root()).unwrap(),
    "<div xmlns=\"http://www.w3.org/1999/xhtml\"/>"
  );
}

#[test]
fn uses_empty_element_syntax_for_empty_elements() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let el = doc.create_element("x", /* namespace */ "");
  doc.append_child(doc.root(), el).unwrap();

  assert_eq!(
    doc.xml_serialize(el).unwrap(),
    "<x xmlns=\"http://www.w3.org/1999/xhtml\"/>"
  );
}

#[test]
fn document_fragment_sibling_root_elements_each_get_xmlns() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let frag = doc.create_document_fragment();

  let a = doc.create_element("a", /* namespace */ "");
  let b = doc.create_element("b", /* namespace */ "");
  doc.append_child(frag, a).unwrap();
  doc.append_child(frag, b).unwrap();

  assert_eq!(
    doc.xml_serialize(frag).unwrap(),
    "<a xmlns=\"http://www.w3.org/1999/xhtml\"/><b xmlns=\"http://www.w3.org/1999/xhtml\"/>"
  );
}

#[test]
fn escapes_text_and_attribute_values() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let el = doc.create_element("div", /* namespace */ "");
  doc
    .set_attribute(el, "data", "a&b<\"")
    .expect("set attribute");
  let text = doc.create_text("1 & 2 < 3");
  doc.append_child(el, text).unwrap();

  assert_eq!(
    doc.xml_serialize(el).unwrap(),
    "<div xmlns=\"http://www.w3.org/1999/xhtml\" data=\"a&amp;b&lt;&quot;\">1 &amp; 2 &lt; 3</div>"
  );
}
