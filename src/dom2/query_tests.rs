#![cfg(test)]

use super::{Document, NodeId, NodeKind};
use crate::dom::SVG_NAMESPACE;
use selectors::context::QuirksMode;

fn tag_name(doc: &Document, node: NodeId) -> Option<&str> {
  match &doc.node(node).kind {
    NodeKind::Element { tag_name, .. } => Some(tag_name.as_str()),
    NodeKind::Slot { .. } => Some("slot"),
    _ => None,
  }
}

fn find_node_by_id_attr(doc: &Document, id: &str) -> NodeId {
  doc
    .nodes()
    .iter()
    .enumerate()
    .find_map(|(idx, node)| {
      let (namespace, attrs) = match &node.kind {
        NodeKind::Element {
          namespace,
          attributes,
          ..
        }
        | NodeKind::Slot {
          namespace,
          attributes,
          ..
        } => (namespace.as_str(), attributes),
        _ => return None,
      };
      let is_html = doc.is_html_case_insensitive_namespace(namespace);
      attrs
        .iter()
        .any(|attr| attr.qualified_name_matches("id", is_html) && attr.value == id)
        .then_some(doc.node_id_from_index(idx).expect("index from enumerate"))
    })
    .unwrap_or_else(|| panic!("node with id={id:?} not found"))
}

fn detach(doc: &mut Document, node: NodeId) {
  let Some(parent) = doc.parent_node(node) else {
    doc.node_mut(node).parent = None;
    return;
  };

  {
    let parent_children = &mut doc.node_mut(parent).children;
    if let Some(pos) = parent_children.iter().position(|&c| c == node) {
      parent_children.remove(pos);
    }
  }

  doc.node_mut(node).parent = None;
}

fn append_child(doc: &mut Document, parent: NodeId, child: NodeId) {
  {
    let parent_children = &mut doc.node_mut(parent).children;
    parent_children.push(child);
  }
  doc.node_mut(child).parent = Some(parent);
}

fn insert_child_before(doc: &mut Document, parent: NodeId, child: NodeId, reference: NodeId) {
  {
    let parent_children = &mut doc.node_mut(parent).children;
    let pos = parent_children
      .iter()
      .position(|&c| c == reference)
      .expect("reference child not found");
    parent_children.insert(pos, child);
  }
  doc.node_mut(child).parent = Some(parent);
}

#[test]
fn sibling_traversal_updates_after_reorder() {
  let html = concat!(
    "<!DOCTYPE html>",
    "<html><body>",
    "<div id=parent><span id=a></span><span id=b></span><span id=c></span></div>",
    "</body></html>"
  );
  let root = crate::dom::parse_html(html).unwrap();
  let mut doc = Document::from_renderer_dom(&root);

  let parent = doc.get_element_by_id("parent").unwrap();
  let a = doc.get_element_by_id("a").unwrap();
  let b = doc.get_element_by_id("b").unwrap();
  let c = doc.get_element_by_id("c").unwrap();

  assert_eq!(doc.first_child(parent), Some(a));
  assert_eq!(doc.last_child(parent), Some(c));
  assert_eq!(doc.previous_sibling(a), None);
  assert_eq!(doc.next_sibling(a), Some(b));
  assert_eq!(doc.previous_sibling(b), Some(a));
  assert_eq!(doc.next_sibling(b), Some(c));
  assert_eq!(doc.previous_sibling(c), Some(b));
  assert_eq!(doc.next_sibling(c), None);

  {
    let parent_children = &mut doc.node_mut(parent).children;
    let pos = parent_children
      .iter()
      .position(|&id| id == b)
      .expect("b not found in parent children");
    parent_children.remove(pos);
    parent_children.push(b);
  }

  assert_eq!(doc.first_child(parent), Some(a));
  assert_eq!(doc.last_child(parent), Some(b));
  assert_eq!(doc.previous_sibling(a), None);
  assert_eq!(doc.next_sibling(a), Some(c));
  assert_eq!(doc.previous_sibling(c), Some(a));
  assert_eq!(doc.next_sibling(c), Some(b));
  assert_eq!(doc.previous_sibling(b), Some(c));
  assert_eq!(doc.next_sibling(b), None);
}

#[test]
fn sibling_traversal_updates_after_move_between_parents() {
  let html = concat!(
    "<!DOCTYPE html>",
    "<html><body>",
    "<div id=p1><span id=a></span><span id=b></span></div>",
    "<div id=p2><span id=c></span></div>",
    "</body></html>"
  );
  let root = crate::dom::parse_html(html).unwrap();
  let mut doc = Document::from_renderer_dom(&root);

  let p1 = doc.get_element_by_id("p1").unwrap();
  let p2 = doc.get_element_by_id("p2").unwrap();
  let a = doc.get_element_by_id("a").unwrap();
  let b = doc.get_element_by_id("b").unwrap();
  let c = doc.get_element_by_id("c").unwrap();

  assert_eq!(doc.parent_node(a), Some(p1));
  assert_eq!(doc.parent_node(b), Some(p1));
  assert_eq!(doc.parent_node(c), Some(p2));

  detach(&mut doc, b);
  insert_child_before(&mut doc, p2, b, c);

  assert_eq!(doc.parent_node(a), Some(p1));
  assert_eq!(doc.parent_node(b), Some(p2));
  assert_eq!(doc.parent_node(c), Some(p2));

  assert_eq!(doc.previous_sibling(a), None);
  assert_eq!(doc.next_sibling(a), None);

  assert_eq!(doc.previous_sibling(b), None);
  assert_eq!(doc.next_sibling(b), Some(c));
  assert_eq!(doc.previous_sibling(c), Some(b));
  assert_eq!(doc.next_sibling(c), None);
}

#[test]
fn is_connected_toggles_when_detaching_and_reattaching() {
  let html = concat!(
    "<!DOCTYPE html>",
    "<html><body>",
    "<div id=outer><span id=inner></span></div>",
    "</body></html>"
  );
  let root = crate::dom::parse_html(html).unwrap();
  let mut doc = Document::from_renderer_dom(&root);

  let outer = doc.get_element_by_id("outer").unwrap();
  let inner = doc.get_element_by_id("inner").unwrap();
  let body = doc.parent_node(outer).unwrap();

  assert!(doc.is_connected(doc.root()));
  assert!(doc.is_connected(outer));
  assert!(doc.is_connected(inner));

  detach(&mut doc, outer);
  assert!(!doc.is_connected(outer));
  assert!(!doc.is_connected(inner));
  assert_eq!(doc.get_element_by_id("outer"), None);
  assert_eq!(doc.get_element_by_id("inner"), None);

  append_child(&mut doc, body, outer);
  assert!(doc.is_connected(outer));
  assert!(doc.is_connected(inner));
  assert_eq!(doc.get_element_by_id("outer"), Some(outer));
  assert_eq!(doc.get_element_by_id("inner"), Some(inner));
}

#[test]
fn get_element_by_id_returns_first_in_tree_order_and_ignores_detached_nodes() {
  let html = concat!(
    "<!DOCTYPE html>",
    "<html><body>",
    "<div id=dup></div>",
    "<span id=dup></span>",
    "</body></html>"
  );
  let root = crate::dom::parse_html(html).unwrap();
  let mut doc = Document::from_renderer_dom(&root);

  assert_eq!(doc.get_element_by_id(""), None);

  let first = doc.get_element_by_id("dup").unwrap();
  assert_eq!(tag_name(&doc, first), Some("div"));

  detach(&mut doc, first);
  let second = doc.get_element_by_id("dup").unwrap();
  assert_eq!(tag_name(&doc, second), Some("span"));

  detach(&mut doc, second);
  assert_eq!(doc.get_element_by_id("dup"), None);
}

#[test]
fn document_element_returns_first_element_child_of_document() {
  let root = crate::dom::DomNode {
    node_type: crate::dom::DomNodeType::Document {
      quirks_mode: QuirksMode::NoQuirks,
      scripting_enabled: true,
      is_html_document: true,
    },
    children: vec![
      crate::dom::DomNode {
        node_type: crate::dom::DomNodeType::Text {
          content: "x".to_string(),
        },
        children: Vec::new(),
      },
      crate::dom::DomNode {
        node_type: crate::dom::DomNodeType::Element {
          tag_name: "div".to_string(),
          namespace: "".to_string(),
          attributes: vec![("id".to_string(), "root".to_string())],
        },
        children: Vec::new(),
      },
      crate::dom::DomNode {
        node_type: crate::dom::DomNodeType::Element {
          tag_name: "span".to_string(),
          namespace: "".to_string(),
          attributes: vec![("id".to_string(), "other".to_string())],
        },
        children: Vec::new(),
      },
    ],
  };
  let doc = Document::from_renderer_dom(&root);

  let doc_el = doc.document_element().unwrap();
  assert_eq!(tag_name(&doc, doc_el), Some("div"));
  assert_eq!(doc.get_element_by_id("root"), Some(doc_el));
}

#[test]
fn get_element_by_id_returns_none_for_empty_id() {
  let root = crate::dom::parse_html(r#"<!doctype html><div id=a></div>"#).unwrap();
  let doc = Document::from_renderer_dom(&root);
  assert_eq!(doc.get_element_by_id(""), None);
}

#[test]
fn query_selector_works_for_detached_subtrees() {
  let html = concat!(
    "<!DOCTYPE html>",
    "<html><body>",
    "<div id=scope><span id=target class=hit></span></div>",
    "</body></html>"
  );
  let root = crate::dom::parse_html(html).unwrap();
  let mut doc = Document::from_renderer_dom(&root);

  let scope = doc.get_element_by_id("scope").unwrap();
  let target = doc.get_element_by_id("target").unwrap();

  // Detach the scope subtree from the document.
  detach(&mut doc, scope);
  assert_eq!(doc.get_element_by_id("target"), None);

  // Document-wide queries should not see detached nodes.
  assert_eq!(doc.query_selector(".hit", None).unwrap(), None);

  // But querying within the detached subtree should still work.
  assert_eq!(
    doc.query_selector(".hit", Some(scope)).unwrap(),
    Some(target)
  );
  assert_eq!(
    doc.query_selector_all(".hit", Some(scope)).unwrap(),
    vec![target]
  );
}

#[test]
fn matches_selector_works_for_detached_elements() {
  let html = concat!(
    "<!DOCTYPE html>",
    "<html><body>",
    "<div id=scope><span id=target class=hit></span></div>",
    "</body></html>"
  );
  let root = crate::dom::parse_html(html).unwrap();
  let mut doc = Document::from_renderer_dom(&root);

  let scope = doc.get_element_by_id("scope").unwrap();
  let target = doc.get_element_by_id("target").unwrap();

  detach(&mut doc, scope);

  assert!(doc.matches_selector(target, ".hit").unwrap());
  assert!(doc.matches_selector(target, "div span.hit").unwrap());
}

#[test]
fn get_element_by_id_ignores_inert_template_contents() {
  let html = concat!(
    "<!DOCTYPE html>",
    "<html><body>",
    "<template><div id=inert></div><div id=dup></div></template>",
    "<span id=dup></span>",
    "</body></html>"
  );
  let root = crate::dom::parse_html(html).unwrap();
  let doc = Document::from_renderer_dom(&root);

  assert_eq!(doc.get_element_by_id("inert"), None);

  let dup = doc.get_element_by_id("dup").unwrap();
  assert_eq!(tag_name(&doc, dup), Some("span"));
}

#[test]
fn get_element_by_id_returns_none_when_only_in_inert_template_contents() {
  let html = concat!(
    "<!DOCTYPE html>",
    "<html><body>",
    "<template><div id=only></div></template>",
    "</body></html>"
  );
  let root = crate::dom::parse_html(html).unwrap();
  let doc = Document::from_renderer_dom(&root);

  assert_eq!(doc.get_element_by_id("only"), None);
}

#[test]
fn get_element_by_id_matches_attribute_name_case_insensitively_only_in_html_namespace() {
  let root = crate::dom::DomNode {
    node_type: crate::dom::DomNodeType::Document {
      quirks_mode: QuirksMode::NoQuirks,
      scripting_enabled: true,
      is_html_document: true,
    },
    children: vec![
      // In HTML, attribute names are ASCII case-insensitive.
      crate::dom::DomNode {
        node_type: crate::dom::DomNodeType::Element {
          tag_name: "div".to_string(),
          namespace: "".to_string(),
          attributes: vec![("ID".to_string(), "a".to_string())],
        },
        children: Vec::new(),
      },
      // In non-HTML namespaces, attribute name matching is case-sensitive.
      crate::dom::DomNode {
        node_type: crate::dom::DomNodeType::Element {
          tag_name: "svg".to_string(),
          namespace: SVG_NAMESPACE.to_string(),
          attributes: vec![("ID".to_string(), "b".to_string())],
        },
        children: Vec::new(),
      },
    ],
  };

  let doc = Document::from_renderer_dom(&root);
  let html = doc.get_element_by_id("a").unwrap();
  assert_eq!(tag_name(&doc, html), Some("div"));
  assert_eq!(doc.get_element_by_id("b"), None);
}

#[test]
fn selector_apis_work_for_inert_template_subtrees() {
  let html = concat!(
    "<!DOCTYPE html>",
    "<html><body>",
    "<template><div id=scope><span id=target class=hit></span></div></template>",
    "<span id=outside class=hit></span>",
    "</body></html>"
  );
  let root = crate::dom::parse_html(html).unwrap();
  let mut doc = Document::from_renderer_dom(&root);

  // Inert template contents are still present in the `dom2` node list, but must not be reachable
  // via document-wide queries.
  assert_eq!(doc.get_element_by_id("scope"), None);
  assert_eq!(doc.get_element_by_id("target"), None);

  let outside = doc.get_element_by_id("outside").unwrap();
  assert_eq!(doc.query_selector(".hit", None).unwrap(), Some(outside));

  // Locate template contents by scanning the node list.
  let mut scope: Option<NodeId> = None;
  let mut target: Option<NodeId> = None;
  for (idx, node) in doc.nodes().iter().enumerate() {
    let (namespace, attrs) = match &node.kind {
      NodeKind::Element {
        namespace,
        attributes,
        ..
      }
      | NodeKind::Slot {
        namespace,
        attributes,
        ..
      } => (namespace.as_str(), attributes),
      _ => continue,
    };
    let is_html = doc.is_html_case_insensitive_namespace(namespace);
    if attrs
      .iter()
      .any(|attr| attr.qualified_name_matches("id", is_html) && attr.value == "scope")
    {
      scope = Some(NodeId(idx));
    }
    if attrs
      .iter()
      .any(|attr| attr.qualified_name_matches("id", is_html) && attr.value == "target")
    {
      target = Some(NodeId(idx));
    }
  }
  let scope = scope.expect("inert scope node not found");
  let target = target.expect("inert target node not found");

  assert_eq!(
    doc.query_selector(".hit", Some(scope)).unwrap(),
    Some(target)
  );
  assert!(doc.matches_selector(target, "div span.hit").unwrap());
}

#[test]
fn get_element_by_id_ignores_shadow_root_subtrees() {
  let html = concat!(
    "<!doctype html>",
    "<html><body>",
    "<div id=host>",
    "<template shadowroot=open><span id=shadow></span></template>",
    "<span id=light></span>",
    "</div>",
    "</body></html>"
  );
  let root = crate::dom::parse_html(html).unwrap();
  let doc = Document::from_renderer_dom(&root);

  assert_eq!(doc.get_element_by_id("shadow"), None);
  let light = doc.get_element_by_id("light").unwrap();
  assert_eq!(tag_name(&doc, light), Some("span"));
}

#[test]
fn shadow_root_get_element_by_id_finds_nodes_in_tree_scope() {
  let html = concat!(
    "<!doctype html>",
    "<html><body>",
    "<div id=host>",
    "<template shadowroot=open><span id=shadow></span></template>",
    "<span id=light></span>",
    "</div>",
    "</body></html>"
  );
  let root = crate::dom::parse_html(html).unwrap();
  let mut doc = Document::from_renderer_dom(&root);

  assert_eq!(doc.get_element_by_id("shadow"), None);

  let shadow_root = doc
    .nodes()
    .iter()
    .enumerate()
    .find_map(|(idx, node)| matches!(&node.kind, NodeKind::ShadowRoot { .. }).then_some(NodeId(idx)))
    .expect("shadow root not found");

  let shadow_el = doc
    .query_selector("#shadow", Some(shadow_root))
    .unwrap()
    .expect("expected shadow element within scoped query");

  assert_eq!(doc.get_element_by_id_from(shadow_root, "shadow"), Some(shadow_el));
}

#[test]
fn closest_does_not_cross_shadow_root_boundary_to_host() {
  let html = concat!(
    "<!doctype html>",
    "<html><body>",
    "<div id=host>",
    "<template shadowroot=open><span id=shadow></span></template>",
    "</div>",
    "</body></html>"
  );
  let root = crate::dom::parse_html(html).unwrap();
  let mut doc = Document::from_renderer_dom(&root);

  let shadow_root = doc
    .nodes()
    .iter()
    .enumerate()
    .find_map(|(idx, node)| matches!(&node.kind, NodeKind::ShadowRoot { .. }).then_some(NodeId(idx)))
    .expect("shadow root not found");

  let shadow_el = doc
    .query_selector("#shadow", Some(shadow_root))
    .unwrap()
    .expect("expected shadow element within scoped query");

  assert_eq!(doc.closest(shadow_el, "#host").unwrap(), None);
  assert_eq!(doc.closest(shadow_el, "#shadow").unwrap(), Some(shadow_el));
}

#[test]
fn shadow_root_get_element_by_id_does_not_pierce_into_nested_shadow_roots() {
  let html = concat!(
    "<!doctype html>",
    "<div id=host>",
    "<template shadowroot=open>",
    "<div id=inner_host>",
    "<template shadowroot=open><span id=nested></span></template>",
    "</div>",
    "<span id=shadow></span>",
    "</template>",
    "</div>",
  );
  let root = crate::dom::parse_html(html).unwrap();
  let mut doc = Document::from_renderer_dom(&root);

  let host = doc.get_element_by_id("host").expect("host element missing");
  let inner_host = find_node_by_id_attr(&doc, "inner_host");
  let nested_span = find_node_by_id_attr(&doc, "nested");

  let mut outer_shadow_root: Option<NodeId> = None;
  let mut inner_shadow_root: Option<NodeId> = None;
  for (idx, node) in doc.nodes().iter().enumerate() {
    if !matches!(&node.kind, NodeKind::ShadowRoot { .. }) {
      continue;
    }
    let shadow_root_id = doc
      .node_id_from_index(idx)
      .expect("shadow root index from enumerate");
    match doc.parent_node(shadow_root_id) {
      Some(parent) if parent == host => outer_shadow_root = Some(shadow_root_id),
      Some(parent) if parent == inner_host => inner_shadow_root = Some(shadow_root_id),
      _ => {}
    }
  }
  let outer_shadow_root = outer_shadow_root.expect("outer shadow root missing");
  let inner_shadow_root = inner_shadow_root.expect("inner shadow root missing");

  // Document-scoped queries must not see into shadow DOM.
  assert_eq!(doc.get_element_by_id("nested"), None);

  // ShadowRoot.getElementById sees its own tree scope but must not pierce into nested shadow roots.
  assert_eq!(doc.get_element_by_id_from(outer_shadow_root, "nested"), None);
  assert_eq!(
    doc.get_element_by_id_from(inner_shadow_root, "nested"),
    Some(nested_span)
  );
}

#[test]
fn query_selector_skips_inert_templates_and_shadow_roots_by_default_but_can_scope_into_shadow_root() {
  let html = concat!(
    "<!doctype html>",
    "<html><body>",
    "<template><span id=inert></span></template>",
    "<div id=host>",
    "<template shadowroot=open><span id=shadow></span></template>",
    "<span id=light></span>",
    "</div>",
    "</body></html>"
  );
  let root = crate::dom::parse_html(html).unwrap();
  let mut doc = Document::from_renderer_dom(&root);

  assert_eq!(doc.query_selector("#inert", None).unwrap(), None);
  assert_eq!(doc.query_selector("#shadow", None).unwrap(), None);
  let light = doc.query_selector("#light", None).unwrap().unwrap();
  assert_eq!(tag_name(&doc, light), Some("span"));

  let shadow_root = doc
    .nodes()
    .iter()
    .enumerate()
    .find_map(|(idx, node)| {
      matches!(&node.kind, NodeKind::ShadowRoot { .. }).then_some(NodeId(idx))
    })
    .expect("shadow root not found");

  let shadow_el = doc
    .query_selector("#shadow", Some(shadow_root))
    .unwrap()
    .unwrap();
  assert_eq!(tag_name(&doc, shadow_el), Some("span"));
}

#[test]
fn closest_returns_self_or_nearest_ancestor() {
  let html = concat!(
    "<!DOCTYPE html>",
    "<html><body>",
    "<div id=outer class=hit><span id=inner></span></div>",
    "</body></html>"
  );
  let root = crate::dom::parse_html(html).unwrap();
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
  let root = crate::dom::parse_html(html).unwrap();
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
  let root = crate::dom::parse_html(html).unwrap();
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
  let root = crate::dom::parse_html(html).unwrap();
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
    doc
      .ancestors(shadow)
      .any(|ancestor| matches!(doc.node(ancestor).kind, NodeKind::ShadowRoot { .. })),
    "expected #shadow to be inside a shadow root"
  );
}
