use crate::dom::parse_html;

use super::{Document, NodeId, NodeKind};

fn attr_value<'a>(doc: &'a Document, node: NodeId, name: &str) -> Option<&'a str> {
  let node = doc.node(node);
  let attrs = match &node.kind {
    NodeKind::Element { attributes, .. } | NodeKind::Slot { attributes, .. } => attributes,
    _ => return None,
  };
  attrs
    .iter()
    .find(|(n, _)| n.eq_ignore_ascii_case(name))
    .map(|(_, v)| v.as_str())
}

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
    .unwrap_or_else(|| panic!("element with id={id:?} not found"))
}

fn find_inert_descendant_with_class(doc: &Document, class: &str) -> NodeId {
  doc
    .nodes()
    .iter()
    .enumerate()
    .find_map(|(idx, node)| match &node.kind {
      NodeKind::Element { attributes, .. } | NodeKind::Slot { attributes, .. } => {
        let has_class = attributes.iter().any(|(name, value)| {
          name.eq_ignore_ascii_case("class") && value.split_ascii_whitespace().any(|c| c == class)
        });
        let id = NodeId(idx);
        (has_class && doc.is_descendant_of_inert_template(id)).then_some(id)
      }
      _ => None,
    })
    .unwrap_or_else(|| panic!("inert descendant with class={class:?} not found"))
}

#[test]
fn query_selector_skips_inert_template_contents() {
  let root = parse_html(
    r#"<!doctype html>
    <html>
      <body>
        <template><div class=x></div></template>
        <div class=x id=out></div>
      </body>
    </html>"#,
  )
  .unwrap();
  let mut doc = Document::from_renderer_dom(&root);

  let inert_div = find_inert_descendant_with_class(&doc, "x");
  assert!(
    doc.is_descendant_of_inert_template(inert_div),
    "expected test fixture to contain an inert template descendant"
  );

  let result = doc
    .query_selector(".x", None)
    .unwrap()
    .expect("expected a match for .x");
  assert_eq!(
    attr_value(&doc, result, "id"),
    Some("out"),
    "query_selector should skip inert template descendants"
  );
}

#[test]
fn query_selector_scope_limits_to_subtree_and_detached_scope_returns_none() {
  let root = parse_html(
    r#"<!doctype html>
    <html>
      <body>
        <div id=scope>
          <div class=x id=in></div>
        </div>
        <div class=x id=out></div>
      </body>
    </html>"#,
  )
  .unwrap();
  let mut doc = Document::from_renderer_dom(&root);

  let scope = find_element_by_id(&doc, "scope");

  let result = doc
    .query_selector(".x", Some(scope))
    .unwrap()
    .expect("expected a scoped match");
  assert_eq!(attr_value(&doc, result, "id"), Some("in"));

  let all = doc.query_selector_all(".x", Some(scope)).unwrap();
  assert_eq!(all.len(), 1);
  assert_eq!(attr_value(&doc, all[0], "id"), Some("in"));

  // Detached scopes should be treated as non-existent.
  let detached = doc.push_node(
    NodeKind::Element {
      tag_name: "div".to_string(),
      namespace: "".to_string(),
      attributes: Vec::new(),
    },
    None,
    /* inert_subtree */ false,
  );
  assert_eq!(doc.query_selector(".x", Some(detached)).unwrap(), None);
  assert!(doc.query_selector_all(".x", Some(detached)).unwrap().is_empty());
}

#[test]
fn matches_selector_returns_false_for_non_elements_and_inert_template_descendants() {
  let root = parse_html(
    r#"<!doctype html>
    <html>
      <body>
        <template><div class=x></div></template>
        <div class=x id=out></div>
        text
      </body>
    </html>"#,
  )
  .unwrap();
  let mut doc = Document::from_renderer_dom(&root);

  assert!(
    !doc.matches_selector(doc.root(), ".x").unwrap(),
    "document node should never match selectors"
  );

  let text_node = doc
    .nodes()
    .iter()
    .enumerate()
    .find_map(|(idx, node)| matches!(node.kind, NodeKind::Text { .. }).then_some(NodeId(idx)))
    .expect("expected a text node");
  assert!(
    !doc.matches_selector(text_node, "div").unwrap(),
    "text nodes should never match selectors"
  );

  let inert_div = find_inert_descendant_with_class(&doc, "x");
  assert!(doc.is_descendant_of_inert_template(inert_div));
  assert!(
    !doc.matches_selector(inert_div, ".x").unwrap(),
    "inert template descendants should not match selectors"
  );

  let out_div = find_element_by_id(&doc, "out");
  assert!(
    doc.matches_selector(out_div, ".x").unwrap(),
    "sanity: non-inert elements should still match selectors"
  );
}

