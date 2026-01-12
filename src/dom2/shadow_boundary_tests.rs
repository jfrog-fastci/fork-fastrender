#![cfg(test)]

use super::{MutationObserverInit, MutationRecordType, NodeId, NodeKind};

fn node_id_attribute(kind: &NodeKind) -> Option<&str> {
  match kind {
    NodeKind::Element { attributes, .. } | NodeKind::Slot { attributes, .. } => attributes
      .iter()
      .find(|(k, _)| k.eq_ignore_ascii_case("id"))
      .map(|(_, v)| v.as_str()),
    _ => None,
  }
}

fn find_node_by_id(doc: &super::Document, id: &str) -> Option<NodeId> {
  doc
    .nodes()
    .iter()
    .enumerate()
    .find_map(|(idx, node)| (node_id_attribute(&node.kind) == Some(id)).then_some(NodeId(idx)))
}

#[test]
fn closest_stops_at_shadow_root_boundary() {
  let html =
    "<!doctype html><div id=host><template shadowroot=open><span id=inner></span></template></div>";
  let mut doc = crate::dom2::parse_html(html).unwrap();

  let inner = find_node_by_id(&doc, "inner").expect("inner element not found");
  assert_eq!(
    doc.closest(inner, "#host").unwrap(),
    None,
    "Element.closest() must not cross a shadow root boundary to the host"
  );
}

#[test]
fn mutation_observer_does_not_cross_shadow_root_boundary() {
  let html = "<!doctype html><div id=host><template shadowroot=open><div id=shadow_parent></div></template></div>";
  let mut doc = crate::dom2::parse_html(html).unwrap();

  let host = find_node_by_id(&doc, "host").expect("host element not found");
  let shadow_parent = find_node_by_id(&doc, "shadow_parent").expect("shadow_parent not found");
  let shadow_root = doc
    .parent_node(shadow_parent)
    .expect("shadow_parent should have a parent");
  assert!(
    matches!(doc.node(shadow_root).kind, NodeKind::ShadowRoot { .. }),
    "expected shadow_parent to be inside a shadow root"
  );

  doc
    .mutation_observer_observe(
      1,
      host,
      MutationObserverInit {
        child_list: true,
        subtree: true,
        ..Default::default()
      },
    )
    .unwrap();
  doc
    .mutation_observer_observe(
      2,
      shadow_root,
      MutationObserverInit {
        child_list: true,
        subtree: true,
        ..Default::default()
      },
    )
    .unwrap();

  let added = doc.create_element("span", "");
  doc.append_child(shadow_parent, added).unwrap();

  let deliveries = doc.mutation_observer_take_deliveries();
  assert!(
    deliveries.iter().all(|(id, _)| *id != 1),
    "observer on the host must not observe mutations inside its shadow tree"
  );
  let (_, records) = deliveries
    .into_iter()
    .find(|(id, _)| *id == 2)
    .expect("observer on the shadow root should observe mutations inside the shadow tree");
  assert_eq!(records.len(), 1);
  let record = &records[0];
  assert_eq!(record.type_, MutationRecordType::ChildList);
  assert_eq!(record.target, shadow_parent);
  assert_eq!(record.added_nodes, vec![added]);
}

