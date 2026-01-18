use crate::dom::parse_html;
use crate::dom::HTML_NAMESPACE;
use crate::dom::MATHML_NAMESPACE;
use selectors::context::QuirksMode;

use super::live_mutation::{LiveMutationEvent, LiveMutationTestRecorder};
use super::{
  Document, DomError, MutationObserverInit, MutationRecordType, NodeId, NodeKind, SlotAssignmentMode,
};

fn find_element_by_id(doc: &Document, id: &str) -> NodeId {
  doc
    .nodes()
    .iter()
    .enumerate()
    .find_map(|(idx, node)| match &node.kind {
      NodeKind::Element {
        namespace,
        attributes,
        ..
      }
      | NodeKind::Slot {
        namespace,
        attributes,
        ..
      } => {
        let is_html = doc.is_html_case_insensitive_namespace(namespace);
        attributes
          .iter()
          .any(|attr| attr.qualified_name_matches("id", is_html) && attr.value == id)
          .then_some(NodeId(idx))
      }
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
      NodeKind::Element { tag_name: t, .. } if t.eq_ignore_ascii_case(tag_name) => {
        Some(NodeId(idx))
      }
      _ => None,
    })
    .unwrap_or_else(|| panic!("missing <{tag_name}> element"))
}

fn find_descendant_by_id(doc: &Document, root: NodeId, id: &str) -> Option<NodeId> {
  let mut stack = vec![root];
  while let Some(node_id) = stack.pop() {
    let node = doc.node(node_id);
    if let NodeKind::Element {
      namespace,
      attributes,
      ..
    }
    | NodeKind::Slot {
      namespace,
      attributes,
      ..
    } = &node.kind
    {
      let is_html = doc.is_html_case_insensitive_namespace(namespace);
      if attributes
        .iter()
        .any(|attr| attr.qualified_name_matches("id", is_html) && attr.value == id)
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
fn inner_html_self_closing_slash_is_ignored_for_non_void_elements() {
  let root = parse_html("<!doctype html><html><body><div id=host></div></body></html>").unwrap();
  let mut doc = Document::from_renderer_dom(&root);
  let host = find_element_by_id(&doc, "host");

  doc
    .set_inner_html(host, r#"<div id="a"/><span id="b"></span>"#)
    .unwrap();

  let div = find_element_by_id(&doc, "a");
  let span = find_element_by_id(&doc, "b");

  assert_eq!(
    doc.node(host).children.as_slice(),
    &[div],
    "expected <div id=a/> to behave like <div> and remain open for the following <span>"
  );
  assert_eq!(
    doc.node(div).children.as_slice(),
    &[span],
    "expected <span id=b> to be inserted as a child of <div id=a>"
  );
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

  assert_eq!(
    doc.get_outer_html(div).unwrap(),
    "<div><span>hi</span></div>"
  );
}

#[test]
fn outer_html_does_not_escape_script_text() {
  let root = parse_html(
    "<!doctype html><html><body><div id=target><script>if (a < b) c(a & b);</script></div></body></html>",
  )
  .unwrap();
  let doc = Document::from_renderer_dom(&root);
  let div = find_element_by_id(&doc, "target");

  let html = doc.get_outer_html(div).unwrap();
  assert!(
    html.contains("a < b"),
    "expected raw '<' in serialized <script> text, got: {html}"
  );
  assert!(
    html.contains("a & b"),
    "expected raw '&' in serialized <script> text, got: {html}"
  );
  assert!(
    !html.contains("a &lt; b"),
    "unexpected escaping inside <script> text, got: {html}"
  );
  assert!(
    !html.contains("a &amp; b"),
    "unexpected escaping inside <script> text, got: {html}"
  );
}

#[test]
fn outer_html_does_not_escape_style_text() {
  let root = parse_html(
    r#"<!doctype html><html><body><div id=target><style>.a{content:"a < b & c"}</style></div></body></html>"#,
  )
  .unwrap();
  let doc = Document::from_renderer_dom(&root);
  let div = find_element_by_id(&doc, "target");

  let html = doc.get_outer_html(div).unwrap();
  assert!(
    html.contains("a < b & c"),
    "expected raw '<'/'&' in serialized <style> text, got: {html}"
  );
  assert!(
    !html.contains("&lt;"),
    "unexpected escaping inside <style> text, got: {html}"
  );
  assert!(
    !html.contains("&amp;"),
    "unexpected escaping inside <style> text, got: {html}"
  );
}

#[test]
fn inner_html_does_not_escape_script_or_style_text() {
  let root = parse_html(
    r#"<!doctype html><html><body>
      <script id=s>if (a < b) c(a & b);</script>
      <style id=st>.a{content:"a < b & c"}</style>
    </body></html>"#,
  )
  .unwrap();
  let doc = Document::from_renderer_dom(&root);

  let script = find_element_by_id(&doc, "s");
  let script_inner = doc.get_inner_html(script).unwrap();
  assert!(
    script_inner.contains("a < b"),
    "expected raw '<' in serialized <script> innerHTML, got: {script_inner}"
  );
  assert!(
    script_inner.contains("a & b"),
    "expected raw '&' in serialized <script> innerHTML, got: {script_inner}"
  );
  assert!(
    !script_inner.contains("&lt;"),
    "unexpected escaping inside <script> innerHTML, got: {script_inner}"
  );
  assert!(
    !script_inner.contains("&amp;"),
    "unexpected escaping inside <script> innerHTML, got: {script_inner}"
  );

  let style = find_element_by_id(&doc, "st");
  let style_inner = doc.get_inner_html(style).unwrap();
  assert!(
    style_inner.contains("a < b"),
    "expected raw '<' in serialized <style> innerHTML, got: {style_inner}"
  );
  assert!(
    style_inner.contains("a < b & c"),
    "expected raw '&' in serialized <style> innerHTML, got: {style_inner}"
  );
  assert!(
    !style_inner.contains("&lt;"),
    "unexpected escaping inside <style> innerHTML, got: {style_inner}"
  );
  assert!(
    !style_inner.contains("&amp;"),
    "unexpected escaping inside <style> innerHTML, got: {style_inner}"
  );
}

#[test]
fn outer_html_setter_replaces_node_in_parent_children() {
  let root = parse_html(
    "<!doctype html><html><body><div id=root><span id=child>hi</span></div></body></html>",
  )
  .unwrap();
  let mut doc = Document::from_renderer_dom(&root);
  let div = find_element_by_id(&doc, "root");
  let span = find_element_by_id(&doc, "child");

  doc.set_outer_html(span, "<p>one</p><p>two</p>").unwrap();

  assert_eq!(doc.get_inner_html(div).unwrap(), "<p>one</p><p>two</p>");
  assert_eq!(
    doc.node(span).parent,
    None,
    "replaced node must be detached"
  );
}

#[test]
fn outer_html_setter_no_parent_is_noop() {
  let root = parse_html(
    "<!doctype html><html><body><div id=root><span id=child>hi</span></div></body></html>",
  )
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
fn outer_html_setter_parent_document_throws_no_modification_allowed() {
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
fn outer_html_setter_parent_document_fragment_parses_with_body_context() {
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
  doc.set_outer_html(table, "<tr><td>x</td></tr>").unwrap();

  let children = doc.node(frag).children.clone();
  assert_eq!(children.len(), 1);
  assert!(
    matches!(&doc.node(children[0]).kind, NodeKind::Text { content } if content == "x"),
    "expected <tr>/<td> tags to be ignored in body context, got {:#?}",
    doc.node(children[0]).kind,
  );
}

#[test]
fn inner_html_setter_on_template_replaces_template_contents() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let template = doc.create_element("template", HTML_NAMESPACE);
  let old_child = doc.create_element("div", HTML_NAMESPACE);
  doc.append_child(template, old_child).unwrap();
  assert!(
    doc.node(template).inert_subtree,
    "template contents must remain inert"
  );

  doc
    .set_inner_html(template, "<span>new</span>")
    .expect("set_inner_html");

  assert_eq!(
    doc.node(old_child).parent,
    None,
    "old contents should detach"
  );
  let children = doc.children(template).unwrap();
  assert_eq!(children.len(), 1, "template contents should be replaced");
  assert!(
    matches!(
      &doc.node(children[0]).kind,
      NodeKind::Element { tag_name, .. } if tag_name.eq_ignore_ascii_case("span")
    ),
    "expected template contents to be replaced with a <span> element"
  );
  assert!(
    doc.node(template).inert_subtree,
    "template contents must remain inert"
  );
}

#[test]
fn inner_html_preserves_comments() {
  let root = parse_html("<!doctype html><html><body><div id=target></div></body></html>").unwrap();
  let mut doc = Document::from_renderer_dom(&root);
  let div = find_element_by_id(&doc, "target");

  doc
    .set_inner_html(div, "<!--ignored--><span>hi</span>")
    .unwrap();
  assert_eq!(
    doc.get_inner_html(div).unwrap(),
    "<!--ignored--><span>hi</span>"
  );
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
    3,
    "comment boundary should prevent adjacent text node merging"
  );
  assert!(
    matches!(&doc.node(children[0]).kind, NodeKind::Text { content } if content == "a"),
    "first child should be a text node containing 'a'"
  );
  assert!(
    matches!(&doc.node(children[1]).kind, NodeKind::Comment { content } if content == "comment"),
    "second child should be a comment node containing 'comment'"
  );
  assert!(
    matches!(&doc.node(children[2]).kind, NodeKind::Text { content } if content == "b"),
    "third child should be a text node containing 'b'"
  );
  assert_eq!(doc.inner_html(div).unwrap(), "a<!--comment-->b");
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
fn inner_html_noscript_parses_markup_when_scripting_disabled() {
  let mut doc = Document::new_with_scripting(QuirksMode::NoQuirks, false);

  let div = doc.create_element("div", HTML_NAMESPACE);
  doc.append_child(doc.root(), div).unwrap();

  doc
    .set_inner_html(div, "<noscript><p>hi</p></noscript>")
    .unwrap();

  assert_eq!(
    doc.get_inner_html(div).unwrap(),
    "<noscript><p>hi</p></noscript>"
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
fn inner_html_noscript_treats_markup_as_text_when_scripting_enabled() {
  let mut doc = Document::new_with_scripting(QuirksMode::NoQuirks, true);

  let div = doc.create_element("div", HTML_NAMESPACE);
  doc.append_child(doc.root(), div).unwrap();

  doc
    .set_inner_html(div, "<noscript><p>hi</p></noscript>")
    .unwrap();

  assert_eq!(
    doc.get_inner_html(div).unwrap(),
    "<noscript>&lt;p&gt;hi&lt;/p&gt;</noscript>"
  );

  let noscript_id = find_first_element_by_tag(&doc, "noscript");
  let children = doc.node(noscript_id).children.clone();
  assert_eq!(children.len(), 1);
  assert!(
    matches!(&doc.node(children[0]).kind, NodeKind::Text { content } if content.contains("<p>hi</p>")),
    "expected noscript contents to be parsed as a single text node, got {:#?}",
    doc.node(children[0]).kind
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
  assert!(
    !doc.node(script).script_force_async,
    "scripts created by innerHTML fragment parsing must have script_force_async=false"
  );
  assert!(
    !doc.node(script).script_parser_document,
    "scripts created by innerHTML fragment parsing must not be parser-inserted (script_parser_document=false)"
  );
}

#[test]
fn inner_html_sets_script_force_async_false() {
  let root = parse_html("<!doctype html><html><body><div id=target></div></body></html>").unwrap();
  let mut doc = Document::from_renderer_dom(&root);
  let div = find_element_by_id(&doc, "target");

  doc.set_inner_html(div, "<script id=s></script>").unwrap();

  let script = find_element_by_id(&doc, "s");
  assert!(
    !doc.node(script).script_force_async,
    "scripts created by fragment parsing should have force_async=false"
  );
  assert!(
    !doc.node(script).script_parser_document,
    "scripts created by fragment parsing must not be treated as parser-inserted"
  );
  assert!(
    doc.node(script).script_already_started,
    "scripts inserted via innerHTML should be marked already started"
  );
}

#[test]
fn outer_html_setter_marks_script_elements_as_already_started() {
  // HTML: scripts created via `outerHTML` parsing must not execute when inserted into the document.
  let root = parse_html(
    "<!doctype html><html><body><div id=root><span id=target>hi</span></div></body></html>",
  )
  .unwrap();
  let mut doc = Document::from_renderer_dom(&root);
  let target = find_element_by_id(&doc, "target");

  doc
    .set_outer_html(target, "<script id=s>console.log('no')</script>")
    .unwrap();

  let script = find_element_by_id(&doc, "s");
  assert!(
    doc.node(script).script_already_started,
    "scripts inserted via outerHTML should be marked already started"
  );
  assert!(
    !doc.node(script).script_force_async,
    "scripts created by outerHTML fragment parsing should have force_async=false"
  );
  assert!(
    !doc.node(script).script_parser_document,
    "scripts created by outerHTML fragment parsing must not be treated as parser-inserted"
  );
}

#[test]
fn insert_adjacent_html_marks_script_elements_as_already_started() {
  // HTML: scripts created via `insertAdjacentHTML` parsing must not execute when inserted into the
  // document.
  let root = parse_html("<!doctype html><html><body><div id=target></div></body></html>").unwrap();
  let mut doc = Document::from_renderer_dom(&root);
  let div = find_element_by_id(&doc, "target");

  doc
    .insert_adjacent_html(div, "beforeend", "<script id=s></script>")
    .unwrap();

  let script = find_element_by_id(&doc, "s");
  assert!(
    doc.node(script).script_already_started,
    "scripts inserted via insertAdjacentHTML should be marked already started"
  );
  assert!(
    !doc.node(script).script_force_async,
    "scripts created by insertAdjacentHTML fragment parsing should have force_async=false"
  );
  assert!(
    !doc.node(script).script_parser_document,
    "scripts created by insertAdjacentHTML fragment parsing must not be treated as parser-inserted"
  );
}

#[test]
fn insert_adjacent_html_preserves_comments() {
  let root = parse_html("<!doctype html><html><body><div id=target></div></body></html>").unwrap();
  let mut doc = Document::from_renderer_dom(&root);
  let div = find_element_by_id(&doc, "target");

  doc
    .insert_adjacent_html(div, "beforeend", "<!--x--><span>y</span>")
    .unwrap();
  assert_eq!(
    doc.inner_html(div).unwrap(),
    "<!--x--><span>y</span>",
    "expected insertAdjacentHTML to preserve comments"
  );
}

#[test]
fn create_element_sets_script_force_async_true() {
  // HTML: scripts created via `document.createElement('script')` have their "force async" flag set.
  // (Parser-inserted scripts clear the flag.)
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let script = doc.create_element("script", HTML_NAMESPACE);
  assert!(
    doc.node(script).script_force_async,
    "expected DOM-created scripts to have force_async=true"
  );
  assert!(
    !doc.node(script).script_parser_document,
    "DOM-created scripts must not be treated as parser-inserted"
  );
  assert!(
    !doc.node(script).script_already_started,
    "freshly created scripts must not be marked already started"
  );
}

#[test]
fn insert_adjacent_html_inserts_beforebegin_and_afterend() {
  let root = parse_html(
    "<!doctype html><html><body><div id=root><span id=target>hi</span></div></body></html>",
  )
  .unwrap();
  let mut doc = Document::from_renderer_dom(&root);
  let target = find_element_by_id(&doc, "target");
  let root_div = find_element_by_id(&doc, "root");

  doc
    .insert_adjacent_html(target, "beforebegin", "<p>one</p>")
    .unwrap();
  doc
    .insert_adjacent_html(target, "afterend", "<p>two</p>")
    .unwrap();

  assert_eq!(
    doc.inner_html(root_div).unwrap(),
    r#"<p>one</p><span id="target">hi</span><p>two</p>"#
  );
}

#[test]
fn insert_adjacent_html_inserts_afterbegin_and_beforeend() {
  let root = parse_html("<!doctype html><html><body><div id=target></div></body></html>").unwrap();
  let mut doc = Document::from_renderer_dom(&root);
  let div = find_element_by_id(&doc, "target");

  doc
    .insert_adjacent_html(div, "afterbegin", "<b>first</b>")
    .unwrap();
  doc
    .insert_adjacent_html(div, "beforeend", "<i>last</i>")
    .unwrap();

  assert_eq!(doc.inner_html(div).unwrap(), "<b>first</b><i>last</i>");
}

#[test]
fn insert_adjacent_html_errors_on_invalid_position() {
  let root = parse_html("<!doctype html><html><body><div id=target></div></body></html>").unwrap();
  let mut doc = Document::from_renderer_dom(&root);
  let div = find_element_by_id(&doc, "target");

  assert_eq!(
    doc.insert_adjacent_html(div, "nope", "<b>x</b>"),
    Err(super::DomError::SyntaxError)
  );
}

#[test]
fn insert_adjacent_html_errors_when_element_has_no_parent() {
  let mut doc = Document::new(selectors::context::QuirksMode::NoQuirks);
  let div = doc.create_element("div", HTML_NAMESPACE);
  assert_eq!(
    doc.insert_adjacent_html(div, "beforebegin", "<span>nope</span>"),
    Err(super::DomError::NoModificationAllowedError)
  );
}

#[test]
fn insert_adjacent_html_errors_when_parent_is_document() {
  let root = parse_html("<!doctype html><html><body></body></html>").unwrap();
  let mut doc = Document::from_renderer_dom(&root);
  let html = find_first_element_by_tag(&doc, "html");
  assert_eq!(
    doc.insert_adjacent_html(html, "beforebegin", "<p>x</p>"),
    Err(super::DomError::NoModificationAllowedError)
  );
}

#[test]
fn insert_adjacent_html_parses_in_body_context_when_parent_is_document_fragment() {
  let root = parse_html("<!doctype html><html><body></body></html>").unwrap();
  let mut doc = Document::from_renderer_dom(&root);

  let frag = doc.create_document_fragment();
  let div = doc.create_element("div", HTML_NAMESPACE);
  doc.append_child(frag, div).unwrap();

  doc
    .insert_adjacent_html(div, "beforebegin", "<span>before</span>")
    .unwrap();

  assert_eq!(
    super::serialization::serialize_children(&doc, frag),
    "<span>before</span><div></div>"
  );
}

#[test]
fn insert_adjacent_element_inserts_beforebegin_and_afterend() {
  let root = parse_html(
    "<!doctype html><html><body><div id=root><span id=target>hi</span></div></body></html>",
  )
  .unwrap();
  let mut doc = Document::from_renderer_dom(&root);
  let target = find_element_by_id(&doc, "target");
  let root_div = find_element_by_id(&doc, "root");

  let p1 = doc.create_element("p", HTML_NAMESPACE);
  let p1_text = doc.create_text("one");
  doc.append_child(p1, p1_text).unwrap();
  let p2 = doc.create_element("p", HTML_NAMESPACE);
  let p2_text = doc.create_text("two");
  doc.append_child(p2, p2_text).unwrap();

  assert_eq!(
    doc.insert_adjacent_element(target, "beforebegin", p1),
    Ok(Some(p1))
  );
  assert_eq!(
    doc.insert_adjacent_element(target, "afterend", p2),
    Ok(Some(p2))
  );

  assert_eq!(
    doc.inner_html(root_div).unwrap(),
    r#"<p>one</p><span id="target">hi</span><p>two</p>"#
  );
}

#[test]
fn insert_adjacent_element_inserts_afterbegin_and_beforeend() {
  let root = parse_html("<!doctype html><html><body><div id=target></div></body></html>").unwrap();
  let mut doc = Document::from_renderer_dom(&root);
  let div = find_element_by_id(&doc, "target");

  let b = doc.create_element("b", HTML_NAMESPACE);
  let b_text = doc.create_text("first");
  doc.append_child(b, b_text).unwrap();
  let i = doc.create_element("i", HTML_NAMESPACE);
  let i_text = doc.create_text("last");
  doc.append_child(i, i_text).unwrap();

  assert_eq!(
    doc.insert_adjacent_element(div, "afterbegin", b),
    Ok(Some(b))
  );
  assert_eq!(
    doc.insert_adjacent_element(div, "beforeend", i),
    Ok(Some(i))
  );

  assert_eq!(doc.inner_html(div).unwrap(), "<b>first</b><i>last</i>");
}

#[test]
fn insert_adjacent_element_errors_on_invalid_position() {
  let root = parse_html("<!doctype html><html><body><div id=target></div></body></html>").unwrap();
  let mut doc = Document::from_renderer_dom(&root);
  let div = find_element_by_id(&doc, "target");
  let b = doc.create_element("b", HTML_NAMESPACE);
  assert_eq!(
    doc.insert_adjacent_element(div, "nope", b),
    Err(super::DomError::SyntaxError)
  );
}

#[test]
fn insert_adjacent_element_returns_none_when_element_has_no_parent_for_beforebegin() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let div = doc.create_element("div", HTML_NAMESPACE);
  let span = doc.create_element("span", HTML_NAMESPACE);
  assert_eq!(
    doc.insert_adjacent_element(div, "beforebegin", span),
    Ok(None)
  );
}

#[test]
fn insert_adjacent_text_inserts_before_first_child_and_after_last_child() {
  let root = parse_html(
    "<!doctype html><html><body><div id=target><span id=mid>mid</span></div></body></html>",
  )
  .unwrap();
  let mut doc = Document::from_renderer_dom(&root);
  let div = find_element_by_id(&doc, "target");

  doc.insert_adjacent_text(div, "afterbegin", "a").unwrap();
  doc.insert_adjacent_text(div, "beforeend", "b").unwrap();

  assert_eq!(
    doc.inner_html(div).unwrap(),
    r#"a<span id="mid">mid</span>b"#
  );
}

#[test]
fn insert_adjacent_text_errors_on_invalid_position() {
  let root = parse_html("<!doctype html><html><body><div id=target></div></body></html>").unwrap();
  let mut doc = Document::from_renderer_dom(&root);
  let div = find_element_by_id(&doc, "target");
  assert_eq!(
    doc.insert_adjacent_text(div, "nope", "x"),
    Err(super::DomError::SyntaxError)
  );
}

#[test]
fn insert_adjacent_text_inserts_into_detached_element_for_afterbegin() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let div = doc.create_element("div", HTML_NAMESPACE);
  doc.insert_adjacent_text(div, "afterbegin", "x").unwrap();
  assert_eq!(doc.inner_html(div).unwrap(), "x");
}

#[test]
fn insert_adjacent_element_keeps_shadow_root_first_child_for_afterbegin() {
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

  let b = doc.create_element("b", HTML_NAMESPACE);
  let b_text = doc.create_text("new");
  doc.append_child(b, b_text).unwrap();

  assert_eq!(
    doc.insert_adjacent_element(host, "afterbegin", b),
    Ok(Some(b))
  );
  assert_eq!(
    doc.node(host).children.first().copied(),
    Some(shadow_root),
    "shadow root should remain first in host.children"
  );
  assert_eq!(
    doc.inner_html(host).unwrap(),
    r#"<b>new</b><p id="light">light</p>"#
  );
}

#[test]
fn insert_adjacent_text_keeps_shadow_root_first_child_for_afterbegin() {
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

  doc.insert_adjacent_text(host, "afterbegin", "x").unwrap();
  assert_eq!(
    doc.node(host).children.first().copied(),
    Some(shadow_root),
    "shadow root should remain first in host.children"
  );
  assert_eq!(doc.inner_html(host).unwrap(), r#"x<p id="light">light</p>"#);
}

#[test]
fn insert_adjacent_html_keeps_shadow_root_first_child_for_afterbegin() {
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

  doc
    .insert_adjacent_html(host, "afterbegin", "<b>new</b>")
    .unwrap();
  assert_eq!(
    doc.node(host).children.first().copied(),
    Some(shadow_root),
    "shadow root should remain first in host.children"
  );
  assert_eq!(
    doc.inner_html(host).unwrap(),
    r#"<b>new</b><p id="light">light</p>"#
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
  assert_eq!(
    doc.outer_html(shadow_root),
    Err(super::DomError::InvalidNodeTypeError)
  );

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

#[test]
fn set_inner_html_removals_and_insertions_use_structured_mutation_apis() {
  // This test ensures `innerHTML = ...` uses `remove_child` / `append_child` internally so:
  // - light-DOM removals trigger MutationObserver childList records, and
  // - ShadowRoot children under the host are preserved (not removed).

  let mut doc = Document::new(QuirksMode::NoQuirks);
  let recorder = LiveMutationTestRecorder::default();
  doc.set_live_mutation_hook(Some(Box::new(recorder.clone())));
  let root = doc.root();

  let host = doc.create_element("div", HTML_NAMESPACE);
  doc.append_child(root, host).unwrap();

  // Create a shadow root child under the host. There is no public `attachShadow()` API yet in
  // `dom2`, but tests can build the tree directly.
  use crate::dom::ShadowRootMode;
  let shadow_root = doc.push_node(
    NodeKind::ShadowRoot {
      mode: ShadowRootMode::Open,
      delegates_focus: false,
      slot_assignment: SlotAssignmentMode::Named,
      clonable: false,
      serializable: false,
      declarative: false,
    },
    Some(host),
    /* inert_subtree */ false,
  );

  let a = doc.create_element("p", HTML_NAMESPACE);
  doc.append_child(host, a).unwrap();
  let b = doc.create_element("p", HTML_NAMESPACE);
  doc.append_child(host, b).unwrap();

  doc
    .mutation_observer_observe(
      /* observer */ 1,
      host,
      MutationObserverInit {
        child_list: true,
        ..Default::default()
      },
    )
    .unwrap();

  let _ = recorder.take();
  doc
    .set_inner_html(host, r#"<span id="n1"></span><span id="n2"></span>"#)
    .unwrap();

  let n1 = doc
    .get_element_by_id("n1")
    .expect("expected <span id=n1> inserted via innerHTML");
  let n2 = doc
    .get_element_by_id("n2")
    .expect("expected <span id=n2> inserted via innerHTML");

  let live_events = recorder.take();
  let fragment_parent = live_events.iter().find_map(|event| match event {
    LiveMutationEvent::PreRemove { node, old_parent, .. } if *node == n1 => Some(*old_parent),
    _ => None,
  });
  let fragment_parent = fragment_parent.expect("expected innerHTML fragment insertion pre_remove hook");
  assert!(
    matches!(doc.node(fragment_parent).kind, NodeKind::DocumentFragment),
    "expected inserted nodes to be removed from a DocumentFragment before insertion"
  );

  assert_eq!(
    live_events,
    vec![
      LiveMutationEvent::PreRemove {
        node: a,
        old_parent: host,
        old_index: 1
      },
      LiveMutationEvent::PreRemove {
        node: b,
        old_parent: host,
        old_index: 1
      },
      LiveMutationEvent::PreRemove {
        node: n1,
        old_parent: fragment_parent,
        old_index: 0
      },
      LiveMutationEvent::PreRemove {
        node: n2,
        old_parent: fragment_parent,
        old_index: 1
      },
      LiveMutationEvent::PreInsert {
        parent: host,
        index: 1,
        count: 2
      }
    ],
    "unexpected live mutation hook sequence for innerHTML"
  );

  // Collect childList mutation records recorded on the host element.
  let records = doc.mutation_observer_take_records(1);
  assert!(
    !records.is_empty(),
    "expected MutationObserver records for innerHTML removals/insertions"
  );
  assert!(
    records
      .iter()
      .all(|r| r.type_ == MutationRecordType::ChildList && r.target == host),
    "expected only host childList records, got: {records:#?}"
  );

  let removed: Vec<NodeId> = records.iter().flat_map(|r| r.removed_nodes.iter().copied()).collect();
  assert_eq!(
    removed,
    vec![a, b],
    "expected each removed light child to be reported"
  );
  assert!(
    !removed.contains(&shadow_root),
    "shadow root child must not be removed by innerHTML"
  );

  let added: Vec<NodeId> = records.iter().flat_map(|r| r.added_nodes.iter().copied()).collect();
  let new_light_children: Vec<NodeId> = doc
    .node(host)
    .children
    .iter()
    .copied()
    .filter(|&child| {
      doc.node(child).parent == Some(host) && !matches!(doc.node(child).kind, NodeKind::ShadowRoot { .. })
    })
    .collect();
  assert_eq!(
    added,
    new_light_children,
    "expected inserted fragment children to be reported"
  );

  assert_eq!(
    doc.node(host).children.first().copied(),
    Some(shadow_root),
    "shadow root should remain the first child of the host element"
  );
}

#[test]
fn inner_html_mathml_annotation_xml_context_uses_encoding_attribute() {
  let root = parse_html(
    "<!doctype html><html><body><math>\
     <annotation-xml id=noenc></annotation-xml>\
     <annotation-xml id=enc encoding=\"text/html\"></annotation-xml>\
     </math></body></html>",
  )
  .unwrap();
  let mut doc = Document::from_renderer_dom(&root);

  let noenc = find_element_by_id(&doc, "noenc");
  let enc = find_element_by_id(&doc, "enc");

  for id in [noenc, enc] {
    match &doc.node(id).kind {
      NodeKind::Element {
        tag_name,
        namespace,
        ..
      } => {
        assert!(
          tag_name.eq_ignore_ascii_case("annotation-xml"),
          "expected annotation-xml element, got <{tag_name}>"
        );
        assert_eq!(
          namespace, MATHML_NAMESPACE,
          "annotation-xml should be in the MathML namespace"
        );
      }
      other => panic!("expected element node, got {other:?}"),
    }
  }

  let fragment = "<malignmark>hi</malignmark>";
  doc.set_inner_html(noenc, fragment).unwrap();
  doc.set_inner_html(enc, fragment).unwrap();

  fn assert_single_child_namespace(doc: &Document, parent: NodeId, expected_namespace: &str) {
    let children = doc.node(parent).children.clone();
    assert_eq!(children.len(), 1, "expected a single child node");

    let child = children[0];
    match &doc.node(child).kind {
      NodeKind::Element {
        tag_name,
        namespace,
        ..
      } => {
        assert!(
          tag_name.eq_ignore_ascii_case("malignmark"),
          "expected <malignmark> element, got <{tag_name}>"
        );
        assert_eq!(namespace, expected_namespace);
      }
      other => panic!("expected element child, got {other:?}"),
    }
  }

  // Without an `encoding` attribute, annotation-xml is not an HTML integration point and the
  // `<malignmark>` start tag is parsed into the MathML namespace (it is excluded from the MathML
  // text integration point fast path).
  assert_single_child_namespace(&doc, noenc, MATHML_NAMESPACE);

  // With `encoding=text/html`, annotation-xml becomes an HTML integration point and the same markup
  // is parsed into the HTML namespace (normalized to the empty string in dom2).
  assert_single_child_namespace(&doc, enc, "");
}
