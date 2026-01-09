use selectors::context::QuirksMode;

use super::{Document, DomError, NodeId, NodeKind};

fn make_element(doc: &mut Document) -> NodeId {
  doc.push_node(
    NodeKind::Element {
      tag_name: "div".to_string(),
      namespace: "".to_string(),
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
fn class_list_tokenization_splits_on_dom_ascii_whitespace() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let el = make_element(&mut doc);

  // Mix of DOM ASCII whitespace characters (TAB/LF/FF/CR/SPACE) and duplicates.
  doc
    .set_attribute(el, "class", "  a\tb\nc\rd\u{000C}e  a  ")
    .unwrap();

  assert_eq!(
    doc.class_list_tokens(el).unwrap(),
    vec!["a", "b", "c", "d", "e"]
  );
}

#[test]
fn class_list_tokenization_does_not_split_on_vertical_tab() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let el = make_element(&mut doc);

  // U+000B VERTICAL TAB is not part of DOM "ASCII whitespace".
  doc
    .set_attribute(el, "class", "a\u{000B}b c")
    .unwrap();

  assert_eq!(
    doc.class_list_tokens(el).unwrap(),
    vec!["a\u{000B}b", "c"]
  );
}

#[test]
fn add_remove_toggle_update_attribute_with_normalized_serialization() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let el = make_element(&mut doc);

  // Start from a messy attribute value so we can observe normalization on write.
  doc.set_attribute(el, "CLASS", "  a\tb  ").unwrap();

  assert_eq!(doc.class_list_add(el, &["c"]).unwrap(), true);
  assert_eq!(doc.get_attribute(el, "class").unwrap(), Some("a b c"));

  // Idempotent.
  assert_eq!(doc.class_list_add(el, &["b"]).unwrap(), false);
  assert_eq!(doc.get_attribute(el, "class").unwrap(), Some("a b c"));

  assert_eq!(doc.class_list_remove(el, &["b"]).unwrap(), true);
  assert_eq!(doc.get_attribute(el, "class").unwrap(), Some("a c"));

  // Idempotent.
  assert_eq!(doc.class_list_remove(el, &["b"]).unwrap(), false);
  assert_eq!(doc.get_attribute(el, "class").unwrap(), Some("a c"));

  // Toggle removes when present.
  assert_eq!(doc.class_list_toggle(el, "c", None).unwrap(), false);
  assert_eq!(doc.get_attribute(el, "class").unwrap(), Some("a"));

  // Toggle adds when absent.
  assert_eq!(doc.class_list_toggle(el, "c", None).unwrap(), true);
  assert_eq!(doc.get_attribute(el, "class").unwrap(), Some("a c"));

  // force=true keeps it present (no change).
  assert_eq!(doc.class_list_toggle(el, "c", Some(true)).unwrap(), true);
  assert_eq!(doc.get_attribute(el, "class").unwrap(), Some("a c"));

  // force=false keeps it absent (no change).
  assert_eq!(doc.class_list_toggle(el, "d", Some(false)).unwrap(), false);
  assert_eq!(doc.get_attribute(el, "class").unwrap(), Some("a c"));
}

#[test]
fn removing_last_class_removes_attribute() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let el = make_element(&mut doc);

  doc.set_attribute(el, "class", "a").unwrap();
  assert_eq!(doc.class_list_remove(el, &["a"]).unwrap(), true);
  assert_eq!(doc.get_attribute(el, "class").unwrap(), None);
}

#[test]
fn invalid_tokens_throw_syntax_error_and_do_not_mutate() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let el = make_element(&mut doc);

  doc.set_attribute(el, "class", "a").unwrap();

  assert_eq!(doc.class_list_contains(el, ""), Err(DomError::SyntaxError));
  assert_eq!(doc.class_list_contains(el, "a b"), Err(DomError::SyntaxError));
  assert_eq!(
    doc.class_list_toggle(el, "a\tb", None),
    Err(DomError::SyntaxError)
  );

  // Batch validation: any invalid token aborts the whole operation.
  assert_eq!(
    doc.class_list_add(el, &["b", "c d"]),
    Err(DomError::SyntaxError)
  );
  assert_eq!(doc.get_attribute(el, "class").unwrap(), Some("a"));
}

#[test]
fn class_list_errors_on_non_elements() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let text = make_text(&mut doc, "x");

  assert_eq!(
    doc.class_list_tokens(text),
    Err(DomError::InvalidNodeType)
  );
  assert_eq!(
    doc.class_list_add(text, &["a"]),
    Err(DomError::InvalidNodeType)
  );
}
