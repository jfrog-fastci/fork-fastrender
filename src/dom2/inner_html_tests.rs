use crate::dom::parse_html;

use super::{Document, NodeId, NodeKind};

fn find_element_by_id(doc: &Document, id: &str) -> NodeId {
  doc
    .nodes()
    .iter()
    .enumerate()
    .find_map(|(idx, node)| match &node.kind {
      NodeKind::Element { attributes, .. } | NodeKind::Slot { attributes, .. } => attributes
        .iter()
        .any(|(name, value)| name.eq_ignore_ascii_case("id") && value == id)
        .then_some(NodeId(idx)),
      _ => None,
    })
    .unwrap_or_else(|| panic!("missing element with id={id}"))
}

fn find_first_element_by_tag(doc: &Document, tag_name: &str) -> NodeId {
  doc
    .nodes()
    .iter()
    .enumerate()
    .find_map(|(idx, node)| match &node.kind {
      NodeKind::Element { tag_name: t, .. } if t.eq_ignore_ascii_case(tag_name) => Some(NodeId(idx)),
      _ => None,
    })
    .unwrap_or_else(|| panic!("missing <{tag_name}> element"))
}

#[test]
fn inner_html_round_trip_basic() {
  let root = parse_html("<!doctype html><html><body><div id=target></div></body></html>").unwrap();
  let mut doc = Document::from_renderer_dom(&root);
  let div = find_element_by_id(&doc, "target");

  doc.set_inner_html(div, "<span>hi</span>").unwrap();
  assert_eq!(doc.get_inner_html(div).unwrap(), "<span>hi</span>");
}

#[test]
fn inner_html_escapes_text() {
  let root = parse_html("<!doctype html><html><body><div id=target></div></body></html>").unwrap();
  let mut doc = Document::from_renderer_dom(&root);
  let div = find_element_by_id(&doc, "target");

  doc.set_inner_html(div, "a & b").unwrap();
  assert_eq!(doc.get_inner_html(div).unwrap(), "a &amp; b");
}

#[test]
fn outer_html_getter_serializes_element() {
  let root =
    parse_html("<!doctype html><html><body><div><span>hi</span></div></body></html>").unwrap();
  let doc = Document::from_renderer_dom(&root);
  let div = find_first_element_by_tag(&doc, "div");

  assert_eq!(doc.get_outer_html(div).unwrap(), "<div><span>hi</span></div>");
}

#[test]
fn outer_html_setter_replaces_node_in_parent_children() {
  let root = parse_html("<!doctype html><html><body><div id=root><span id=child>hi</span></div></body></html>")
    .unwrap();
  let mut doc = Document::from_renderer_dom(&root);
  let div = find_element_by_id(&doc, "root");
  let span = find_element_by_id(&doc, "child");

  doc
    .set_outer_html(span, "<p>one</p><p>two</p>")
    .unwrap();

  assert_eq!(doc.get_inner_html(div).unwrap(), "<p>one</p><p>two</p>");
  assert_eq!(doc.node(span).parent, None, "replaced node must be detached");
}

#[test]
fn inner_html_ignores_comments_for_now() {
  let root = parse_html("<!doctype html><html><body><div id=target></div></body></html>").unwrap();
  let mut doc = Document::from_renderer_dom(&root);
  let div = find_element_by_id(&doc, "target");

  doc
    .set_inner_html(div, "<!--ignored--><span>hi</span>")
    .unwrap();
  assert_eq!(doc.get_inner_html(div).unwrap(), "<span>hi</span>");
}

#[test]
fn inner_html_comment_prevents_text_merge_across_boundary() {
  let root = parse_html("<!doctype html><html><body><div id=target></div></body></html>").unwrap();
  let mut doc = Document::from_renderer_dom(&root);
  let div = find_element_by_id(&doc, "target");

  doc.set_inner_html(div, "a<!--comment-->b").unwrap();

  let children = doc.node(div).children.clone();
  assert_eq!(
    children.len(),
    2,
    "comment boundary should prevent adjacent text node merging"
  );
  assert!(
    matches!(&doc.node(children[0]).kind, NodeKind::Text { content } if content == "a"),
    "first child should be a text node containing 'a'"
  );
  assert!(
    matches!(&doc.node(children[1]).kind, NodeKind::Text { content } if content == "b"),
    "second child should be a text node containing 'b'"
  );
}

#[test]
fn inner_html_preserves_template_contents_and_marks_inert() {
  let root = parse_html("<!doctype html><html><body><div id=target></div></body></html>").unwrap();
  let mut doc = Document::from_renderer_dom(&root);
  let div = find_element_by_id(&doc, "target");

  doc
    .set_inner_html(div, "<template><span>in</span></template>")
    .unwrap();
  assert_eq!(
    doc.get_inner_html(div).unwrap(),
    "<template><span>in</span></template>"
  );

  let template_id = find_first_element_by_tag(&doc, "template");
  assert!(
    doc.node(template_id).inert_subtree,
    "template contents must be treated as inert"
  );
  let first_child = doc.node(template_id).children.first().copied();
  let Some(first_child) = first_child else {
    panic!("template should have a child node for its contents");
  };
  assert!(
    matches!(&doc.node(first_child).kind, NodeKind::Element { tag_name, .. } if tag_name.eq_ignore_ascii_case("span")),
    "template contents should include the <span> element"
  );
}
