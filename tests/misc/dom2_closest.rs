use fastrender::dom::parse_html;
use fastrender::dom2::{Document, NodeId, NodeKind};

fn find_node_by_id_attr(doc: &Document, id: &str) -> NodeId {
  doc
    .nodes()
    .iter()
    .enumerate()
    .find_map(|(idx, node)| {
      let attrs = match &node.kind {
        NodeKind::Element { attributes, .. } | NodeKind::Slot { attributes, .. } => attributes,
        _ => return None,
      };
      attrs
        .iter()
        .any(|(name, value)| name.eq_ignore_ascii_case("id") && value == id)
        .then_some(doc.node_id_from_index(idx).expect("index from enumerate"))
    })
    .unwrap_or_else(|| panic!("node with id={id:?} not found"))
}

#[test]
fn closest_returns_self_or_nearest_ancestor() {
  let html = concat!(
    "<!DOCTYPE html>",
    "<html><body>",
    "<div id=outer class=hit><span id=inner></span></div>",
    "</body></html>"
  );
  let root = parse_html(html).unwrap();
  let mut doc = Document::from_renderer_dom(&root);

  let outer = doc.get_element_by_id("outer").unwrap();
  let inner = doc.get_element_by_id("inner").unwrap();

  assert_eq!(doc.closest(inner, "span").unwrap(), Some(inner));
  assert_eq!(doc.closest(inner, "div.hit").unwrap(), Some(outer));
  assert_eq!(doc.closest(inner, "p").unwrap(), None);

  assert!(
    doc.closest(inner, "div..bad").is_err(),
    "invalid selector should return a DOM exception"
  );
}

#[test]
fn closest_works_for_detached_subtrees() {
  let html = concat!(
    "<!DOCTYPE html>",
    "<html><body>",
    "<div id=scope><span id=target class=hit></span></div>",
    "</body></html>"
  );
  let root = parse_html(html).unwrap();
  let mut doc = Document::from_renderer_dom(&root);

  let scope = doc.get_element_by_id("scope").unwrap();
  let target = doc.get_element_by_id("target").unwrap();
  let parent = doc.parent_node(scope).expect("scope has parent");

  assert!(doc.remove_child(parent, scope).unwrap());

  assert_eq!(doc.get_element_by_id("target"), None);
  assert_eq!(doc.closest(target, "div").unwrap(), Some(scope));
  assert_eq!(doc.closest(target, "body").unwrap(), None);
}

#[test]
fn closest_does_not_cross_inert_template_boundaries() {
  let html = concat!(
    "<!DOCTYPE html>",
    "<html><body>",
    "<template><div id=outer><span id=inner></span></div></template>",
    "</body></html>"
  );
  let root = parse_html(html).unwrap();
  let mut doc = Document::from_renderer_dom(&root);

  let outer = find_node_by_id_attr(&doc, "outer");
  let inner = find_node_by_id_attr(&doc, "inner");
  assert!(doc.is_descendant_of_inert_template(inner));

  // `Document.getElementById` must not return elements inside inert `<template>` contents.
  assert_eq!(doc.get_element_by_id("outer"), None);
  assert_eq!(doc.get_element_by_id("inner"), None);

  assert_eq!(doc.closest(inner, "div#outer").unwrap(), Some(outer));
  assert_eq!(doc.closest(inner, "body").unwrap(), None);
  assert_eq!(doc.closest(inner, "template").unwrap(), None);
  assert_eq!(doc.closest(outer, "template").unwrap(), None);
}

#[test]
fn get_element_by_id_does_not_traverse_shadow_roots() {
  let html = concat!(
    "<!DOCTYPE html>",
    "<html><body>",
    "<div id=host>",
    "<template shadowroot=open><div id=shadow></div></template>",
    "</div>",
    "<div id=light></div>",
    "</body></html>"
  );
  let root = parse_html(html).unwrap();
  let doc = Document::from_renderer_dom(&root);

  assert!(
    doc.get_element_by_id("host").is_some(),
    "expected #host to be discoverable in the light DOM"
  );
  assert!(
    doc.get_element_by_id("light").is_some(),
    "expected #light to be discoverable in the light DOM"
  );

  // `Document.getElementById` does not cross shadow DOM boundaries.
  assert_eq!(doc.get_element_by_id("shadow"), None);

  // The element still exists in the underlying tree; it's just not reachable by `getElementById`.
  let shadow = find_node_by_id_attr(&doc, "shadow");
  assert!(
    doc.ancestors(shadow).any(|ancestor| matches!(doc.node(ancestor).kind, NodeKind::ShadowRoot { .. })),
    "expected #shadow to be inside a shadow root"
  );
}
