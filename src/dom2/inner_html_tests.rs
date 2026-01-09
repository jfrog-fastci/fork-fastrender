use crate::dom::{parse_html, parse_html_with_options, DomParseOptions};
use crate::dom::HTML_NAMESPACE;

use super::{Document, DomError, NodeId, NodeKind};

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

fn find_descendant_by_id(doc: &Document, root: NodeId, id: &str) -> Option<NodeId> {
  let mut stack = vec![root];
  while let Some(node_id) = stack.pop() {
    let node = doc.node(node_id);
    if let NodeKind::Element { attributes, .. } | NodeKind::Slot { attributes, .. } = &node.kind {
      if attributes
        .iter()
        .any(|(name, value)| name.eq_ignore_ascii_case("id") && value == id)
      {
        return Some(node_id);
      }
    }
    for &child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
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
fn outer_html_setter_is_noop_when_node_is_detached() {
  let root = parse_html("<!doctype html><html><body><div id=root><span id=child>hi</span></div></body></html>")
    .unwrap();
  let mut doc = Document::from_renderer_dom(&root);
  let div = find_element_by_id(&doc, "root");
  let span = find_element_by_id(&doc, "child");

  // Detach `span` from the tree.
  doc.remove_child(div, span).unwrap();
  assert_eq!(doc.node(span).parent, None);

  let nodes_before = doc.nodes_len();
  doc
    .set_outer_html(span, "<p>should-not-appear</p>")
    .unwrap();
  assert_eq!(
    doc.nodes_len(),
    nodes_before,
    "outerHTML on detached nodes should not allocate/parse anything"
  );

  assert_eq!(
    doc.get_inner_html(div).unwrap(),
    "",
    "detached node should not affect its former parent"
  );
}

#[test]
fn outer_html_setter_throws_when_parent_is_document() {
  let root = parse_html("<!doctype html><html><body>hi</body></html>").unwrap();
  let mut doc = Document::from_renderer_dom(&root);
  let html = find_first_element_by_tag(&doc, "html");

  let err = doc
    .set_outer_html(html, "<html><body>nope</body></html>")
    .expect_err("expected outerHTML on document child to error");
  assert!(
    matches!(err, DomError::NoModificationAllowedError),
    "expected NoModificationAllowedError, got {err:?}",
  );
}

#[test]
fn outer_html_setter_parses_in_body_context_when_parent_is_document_fragment() {
  let root = parse_html("<!doctype html><html><body></body></html>").unwrap();
  let mut doc = Document::from_renderer_dom(&root);

  let frag = doc.create_document_fragment();
  let table = doc.create_element("table", HTML_NAMESPACE);
  doc.append_child(frag, table).unwrap();

  // Spec: if the parent is a DocumentFragment, outerHTML uses a synthetic `<body>` element for
  // fragment parsing context. In the HTML "in body" insertion mode, table-row tags like `<tr>` and
  // `<td>` are parse errors and are ignored, leaving only their text contents.
  //
  // (In contrast, parsing the same string in a `<table>` context would yield a `<tbody>`/`<tr>`
  // structure.)
  doc
    .set_outer_html(table, "<tr><td>x</td></tr>")
    .unwrap();

  let children = doc.node(frag).children.clone();
  assert_eq!(children.len(), 1);
  assert!(
    matches!(&doc.node(children[0]).kind, NodeKind::Text { content } if content == "x"),
    "expected <tr>/<td> tags to be ignored in body context, got {:#?}",
    doc.node(children[0]).kind,
  );
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

#[test]
fn inner_html_parses_noscript_as_text_when_scripting_enabled() {
  let root = parse_html("<!doctype html><html><body><div id=target></div></body></html>").unwrap();
  let mut doc = Document::from_renderer_dom(&root);
  let div = find_element_by_id(&doc, "target");

  doc
    .set_inner_html(div, "<noscript><p>fallback</p></noscript>")
    .unwrap();

  // With scripting enabled, the HTML parser treats <noscript> as a raw text element.
  // Our serializer currently escapes `<` inside text nodes.
  assert_eq!(
    doc.get_inner_html(div).unwrap(),
    "<noscript>&lt;p>fallback&lt;/p></noscript>"
  );

  let noscript_id = find_first_element_by_tag(&doc, "noscript");
  let children = doc.node(noscript_id).children.clone();
  assert_eq!(children.len(), 1);
  assert!(
    matches!(&doc.node(children[0]).kind, NodeKind::Text { content } if content.contains("<p>fallback</p>")),
    "expected noscript contents to be parsed as a single text node, got {:#?}",
    doc.node(children[0]).kind
  );
}

#[test]
fn inner_html_parses_noscript_children_when_scripting_disabled() {
  let options = DomParseOptions::with_scripting_enabled(false);
  let root = parse_html_with_options(
    "<!doctype html><html><body><div id=target></div></body></html>",
    options,
  )
  .unwrap();
  let mut doc = Document::from_renderer_dom(&root);
  let div = find_element_by_id(&doc, "target");

  doc
    .set_inner_html(div, "<noscript><p>fallback</p></noscript>")
    .unwrap();

  assert_eq!(
    doc.get_inner_html(div).unwrap(),
    "<noscript><p>fallback</p></noscript>"
  );

  let noscript_id = find_first_element_by_tag(&doc, "noscript");
  let children = doc.node(noscript_id).children.clone();
  assert!(
    children.iter().any(|&child| {
      matches!(
        &doc.node(child).kind,
        NodeKind::Element { tag_name, .. } if tag_name.eq_ignore_ascii_case("p")
      )
    }),
    "expected noscript contents to include a <p> element when scripting is disabled"
  );
}

#[test]
fn inner_html_marks_script_elements_as_already_started() {
  // In browsers, scripts created via `innerHTML` do not execute, even if later moved/reinserted.
  // The HTML spec models this using the per-element "already started" flag.
  let root = parse_html("<!doctype html><html><body><div id=target></div></body></html>").unwrap();
  let mut doc = Document::from_renderer_dom(&root);
  let div = find_element_by_id(&doc, "target");

  doc
    .set_inner_html(div, "<script id=s>console.log('no')</script>")
    .unwrap();

  let script = find_element_by_id(&doc, "s");
  assert!(
    doc.node(script).script_already_started,
    "scripts inserted via innerHTML should be marked already started"
  );
}

#[test]
fn inner_html_skips_shadow_root_children() {
  let html = r#"<div id=host><template shadowroot=open><span id=shadow>shadow</span></template><p id=light>light</p></div>"#;
  let root = parse_html(html).unwrap();
  let doc = Document::from_renderer_dom(&root);

  let host = find_element_by_id(&doc, "host");
  assert_eq!(doc.inner_html(host).unwrap(), r#"<p id="light">light</p>"#);
}

#[test]
fn set_inner_html_preserves_shadow_root() {
  let html = r#"<div id=host><template shadowroot=open><span id=shadow>shadow</span></template><p id=light>light</p></div>"#;
  let root = parse_html(html).unwrap();
  let mut doc = Document::from_renderer_dom(&root);

  let host = find_element_by_id(&doc, "host");
  let shadow_root = doc
    .node(host)
    .children
    .iter()
    .copied()
    .find(|&child| matches!(doc.node(child).kind, NodeKind::ShadowRoot { .. }))
    .expect("expected a ShadowRoot child under the host element");
  let shadow_span = find_descendant_by_id(&doc, shadow_root, "shadow")
    .expect("expected <span id=shadow> inside the shadow root");

  // ShadowRoot has no outerHTML in the web platform.
  assert_eq!(doc.outer_html(shadow_root), Err(super::DomError::InvalidNodeType));

  doc.set_inner_html(host, "<b>new</b>").unwrap();

  let host_children = doc.node(host).children.clone();
  assert_eq!(
    host_children.first().copied(),
    Some(shadow_root),
    "shadow root should remain first in host.children"
  );

  let b_child = host_children
    .iter()
    .copied()
    .find(|&child| match &doc.node(child).kind {
      NodeKind::Element { tag_name, .. } => tag_name.eq_ignore_ascii_case("b"),
      _ => false,
    })
    .expect("expected newly inserted <b> child");
  assert_eq!(doc.node(b_child).parent, Some(host));

  assert_eq!(doc.inner_html(host).unwrap(), "<b>new</b>");
  assert_eq!(
    doc.outer_html(shadow_span).unwrap(),
    r#"<span id="shadow">shadow</span>"#
  );
}
