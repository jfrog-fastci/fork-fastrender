use selectors::context::QuirksMode;

use super::{Document, NodeKind};

#[test]
fn inner_html_roundtrip_basic() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let div = doc.create_element("div", "");

  doc
    .set_inner_html(div, "<span>Hi</span>!")
    .expect("set_inner_html");

  let children = doc.children(div).unwrap();
  assert_eq!(children.len(), 2);

  let span = children[0];
  let bang = children[1];

  match &doc.node(span).kind {
    NodeKind::Element { tag_name, .. } => assert!(tag_name.eq_ignore_ascii_case("span")),
    other => panic!("expected <span> element, got {other:?}"),
  }
  match &doc.node(bang).kind {
    NodeKind::Text { content } => assert_eq!(content, "!"),
    other => panic!("expected text node, got {other:?}"),
  }

  let span_children = doc.children(span).unwrap();
  assert_eq!(span_children.len(), 1);
  match &doc.node(span_children[0]).kind {
    NodeKind::Text { content } => assert_eq!(content, "Hi"),
    other => panic!("expected text node, got {other:?}"),
  }

  assert_eq!(doc.inner_html(div).unwrap(), "<span>Hi</span>!");
  assert_eq!(doc.outer_html(div).unwrap(), "<div><span>Hi</span>!</div>");
}

#[test]
fn google_like_document_fragment_flow() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let div = doc.create_element("div", "");
  doc
    .set_inner_html(div, "<b>A</b><i>B</i>")
    .expect("set_inner_html");

  let div_children = doc.children(div).unwrap().to_vec();
  assert_eq!(div_children.len(), 2);
  let b = div_children[0];
  let i = div_children[1];

  let fragment = doc.create_document_fragment();

  // Move nodes into the fragment.
  while let Some(child) = doc.first_child(div) {
    doc.append_child(fragment, child).unwrap();
  }
  assert_eq!(doc.children(div).unwrap().len(), 0);

  // Replace placeholder with the fragment.
  let parent = doc.create_element("div", "");
  let placeholder = doc.create_element("p", "");
  doc.append_child(parent, placeholder).unwrap();
  doc.replace_child(parent, fragment, placeholder).unwrap();

  let parent_children = doc.children(parent).unwrap();
  assert_eq!(parent_children, &[b, i]);

  assert_eq!(
    doc.children(fragment).unwrap().len(),
    0,
    "fragment should be empty"
  );
}

#[test]
fn outer_html_includes_script_text_unescaped() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let div = doc.create_element("div", "");
  let script = doc.create_element("script", "");
  let text = doc.create_text("if (a < b) c();");

  doc.append_child(div, script).unwrap();
  doc.append_child(script, text).unwrap();

  assert_eq!(
    doc.outer_html(div).unwrap(),
    "<div><script>if (a < b) c();</script></div>"
  );
}

#[test]
fn inner_html_strips_authored_file_input_selection_state() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let div = doc.create_element("div", "");

  doc
    .set_inner_html(
      div,
      r#"<input type="file" data-fastr-files='["/etc/passwd"]' data-fastr-file-value="C:\\fakepath\\passwd" value="/etc/passwd">"#,
    )
    .expect("set_inner_html");

  let input = doc.children(div).unwrap()[0];
  assert_eq!(doc.get_attribute(input, "data-fastr-files").unwrap(), None);
  assert_eq!(doc.get_attribute(input, "data-fastr-file-value").unwrap(), None);
  assert_eq!(doc.get_attribute(input, "value").unwrap(), None);
}
