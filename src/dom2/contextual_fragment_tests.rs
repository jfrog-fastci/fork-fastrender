use crate::dom::HTML_NAMESPACE;
use selectors::context::QuirksMode;

use super::{Document, NodeId, NodeKind};

fn find_first_element_by_tag_in_subtree(doc: &Document, root: NodeId, tag: &str) -> Option<NodeId> {
  let mut stack = vec![root];
  while let Some(id) = stack.pop() {
    let node = doc.node(id);
    if let NodeKind::Element { tag_name, .. } = &node.kind {
      if tag_name.eq_ignore_ascii_case(tag) {
        return Some(id);
      }
    }
    for &child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

#[test]
fn create_contextual_fragment_resets_script_already_started() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let div = doc.create_element("div", HTML_NAMESPACE);
  doc.append_child(doc.root(), div).unwrap();

  let fragment = doc
    .create_contextual_fragment(div, r#"<script id=s>console.log("x")</script>"#)
    .unwrap();

  let script = find_first_element_by_tag_in_subtree(&doc, fragment, "script")
    .expect("expected a <script> element inside contextual fragment");
  assert!(
    !doc.node(script).script_already_started,
    "scripts created by createContextualFragment must not be marked already started"
  );
  assert!(
    !doc.node(script).script_force_async,
    "scripts created by fragment parsing should have force_async=false"
  );
  assert!(
    !doc.node(script).script_parser_document,
    "scripts created by createContextualFragment must not be treated as parser-inserted"
  );
  assert_eq!(
    super::serialization::serialize_children(&doc, fragment),
    r#"<script id="s">console.log("x")</script>"#
  );
}

#[test]
fn create_contextual_fragment_uses_parent_element_for_text_context() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let table = doc.create_element("table", HTML_NAMESPACE);
  doc.append_child(doc.root(), table).unwrap();
  let text = doc.create_text("context");
  doc.append_child(table, text).unwrap();

  // When the context node is Text, the spec uses its parent element as the fragment parsing context.
  // Parsing a <tr> in <table> context should yield a tbody/tr/td tree *without* synthesizing a
  // surrounding <table> wrapper.
  let fragment = doc
    .create_contextual_fragment(text, "<tr><td>x</td></tr>")
    .unwrap();
  let out = super::serialization::serialize_children(&doc, fragment);
  assert!(
    out.contains("<tbody>") && out.contains("<td>x</td>"),
    "expected table-context parsing to create tbody/td, got: {out}"
  );
  assert!(
    !out.contains("<table"),
    "unexpected <table> wrapper when parsing in <table> context, got: {out}"
  );
}

#[test]
fn create_contextual_fragment_falls_back_to_body_when_context_is_document() {
  let mut doc = Document::new(QuirksMode::NoQuirks);

  // When context is not an Element (or Text/Comment with a parent element), the spec parses in a
  // synthetic <body> element. In the HTML "in body" insertion mode, table-row tags like `<tr>` and
  // `<td>` are parse errors and are ignored, leaving only their text contents.
  let fragment = doc
    .create_contextual_fragment(doc.root(), "<tr><td>x</td></tr>")
    .unwrap();
  let out = super::serialization::serialize_children(&doc, fragment);
  assert_eq!(out, "x");
}

#[test]
fn create_contextual_fragment_falls_back_to_body_when_context_is_html_element() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let html = doc.create_element("html", HTML_NAMESPACE);
  doc.append_child(doc.root(), html).unwrap();

  let fragment = doc
    .create_contextual_fragment(html, "<tr><td>x</td></tr>")
    .unwrap();
  let out = super::serialization::serialize_children(&doc, fragment);
  assert_eq!(out, "x");
}

#[test]
fn create_contextual_fragment_uses_parent_element_for_comment_context() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let div = doc.create_element("div", HTML_NAMESPACE);
  doc.append_child(doc.root(), div).unwrap();
  let comment = doc.create_comment("ctx");
  doc.append_child(div, comment).unwrap();

  let fragment = doc.create_contextual_fragment(comment, "<span>hi</span>").unwrap();
  assert_eq!(
    super::serialization::serialize_children(&doc, fragment),
    "<span>hi</span>"
  );
}
