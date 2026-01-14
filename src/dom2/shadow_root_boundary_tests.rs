use super::{Document, NodeId, NodeKind};

fn find_node_by_id_attribute(doc: &Document, id: &str) -> Option<NodeId> {
  if id.is_empty() {
    return None;
  }
  doc.nodes().iter().enumerate().find_map(|(idx, node)| {
    let (namespace, attributes) = match &node.kind {
      NodeKind::Element {
        namespace,
        attributes,
        ..
      }
      | NodeKind::Slot {
        namespace,
        attributes,
        ..
      } => (namespace, attributes),
      _ => return None,
    };

    let is_html = doc.is_html_case_insensitive_namespace(namespace);
    attributes
      .iter()
      .any(|attr| attr.qualified_name_matches("id", is_html) && attr.value == id)
      .then_some(NodeId::from_index(idx))
  })
}

fn find_shadow_root_for_host(doc: &Document, host: NodeId) -> Option<NodeId> {
  let host_node = doc.nodes().get(host.index())?;
  host_node.children.iter().copied().find(|&child| {
    let Some(child_node) = doc.nodes().get(child.index()) else {
      return false;
    };
    child_node.parent == Some(host) && matches!(child_node.kind, NodeKind::ShadowRoot { .. })
  })
}

#[test]
fn closest_does_not_cross_shadow_root_to_host() {
  let html = concat!(
    "<!doctype html>",
    "<html><body>",
    "<div id=host>",
    "<template shadowroot=open>",
    "<div id=inner><span id=leaf></span></div>",
    "</template>",
    "</div>",
    "</body></html>",
  );
  let root = crate::dom::parse_html(html).unwrap();
  let mut doc = Document::from_renderer_dom(&root);

  let leaf = find_node_by_id_attribute(&doc, "leaf").expect("expected #leaf inside shadow root");
  let inner = find_node_by_id_attribute(&doc, "inner").expect("expected #inner inside shadow root");

  assert_eq!(doc.closest(leaf, "#host").unwrap(), None);
  assert_eq!(doc.closest(leaf, "#inner").unwrap(), Some(inner));
}

#[test]
fn get_element_by_id_from_shadow_root_finds_shadow_ids_but_document_does_not() {
  let html = concat!(
    "<!doctype html>",
    "<html><body>",
    "<div id=host>",
    "<template shadowroot=open>",
    "<div id=inner><span id=leaf></span></div>",
    "</template>",
    "</div>",
    "</body></html>",
  );
  let root = crate::dom::parse_html(html).unwrap();
  let doc = Document::from_renderer_dom(&root);

  let host = doc.get_element_by_id("host").expect("expected #host in light DOM");
  let shadow_root =
    find_shadow_root_for_host(&doc, host).expect("expected promoted ShadowRoot child of host");
  let leaf = find_node_by_id_attribute(&doc, "leaf").expect("expected #leaf inside shadow root");

  assert_eq!(doc.get_element_by_id("leaf"), None);
  assert_eq!(doc.get_element_by_id_from(shadow_root, "leaf"), Some(leaf));
}

#[test]
fn get_element_by_id_from_shadow_root_does_not_pierce_nested_shadow_roots() {
  let html = concat!(
    "<!doctype html>",
    "<html><body>",
    "<div id=host>",
    "<template shadowroot=open>",
    "<span id=leaf></span>",
    "<div id=nested_host>",
    "<template shadowroot=open><span id=nested></span></template>",
    "</div>",
    "</template>",
    "</div>",
    "</body></html>",
  );
  let root = crate::dom::parse_html(html).unwrap();
  let doc = Document::from_renderer_dom(&root);

  let host = doc.get_element_by_id("host").expect("expected #host in light DOM");
  let outer_shadow_root =
    find_shadow_root_for_host(&doc, host).expect("expected promoted ShadowRoot child of host");

  let nested_host =
    find_node_by_id_attribute(&doc, "nested_host").expect("expected nested shadow host element");
  let nested_shadow_root =
    find_shadow_root_for_host(&doc, nested_host).expect("expected nested shadow root");
  let nested = find_node_by_id_attribute(&doc, "nested").expect("expected nested shadow element");

  assert_eq!(doc.get_element_by_id_from(outer_shadow_root, "nested"), None);
  assert_eq!(doc.get_element_by_id_from(nested_shadow_root, "nested"), Some(nested));
}
