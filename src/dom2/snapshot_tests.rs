use crate::debug::snapshot::snapshot_dom;
use crate::dom::{DomNode, DomNodeType};
use selectors::context::QuirksMode;

use super::{Document, NodeId, NodeKind};

fn node_kind_from_dom_node_type(node_type: &DomNodeType) -> NodeKind {
  match node_type {
    DomNodeType::Document { quirks_mode } => NodeKind::Document {
      quirks_mode: *quirks_mode,
    },
    DomNodeType::ShadowRoot {
      mode,
      delegates_focus,
    } => NodeKind::ShadowRoot {
      mode: *mode,
      delegates_focus: *delegates_focus,
    },
    DomNodeType::Slot {
      namespace,
      attributes,
      assigned,
    } => NodeKind::Slot {
      namespace: namespace.clone(),
      attributes: attributes.clone(),
      assigned: *assigned,
    },
    DomNodeType::Element {
      tag_name,
      namespace,
      attributes,
    } => NodeKind::Element {
      tag_name: tag_name.clone(),
      namespace: namespace.clone(),
      attributes: attributes.clone(),
    },
    DomNodeType::Text { content } => NodeKind::Text {
      content: content.clone(),
    },
  }
}

#[test]
fn snapshot_with_mapping_uses_0_for_detached_nodes() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();

  doc.push_node(
    NodeKind::Element {
      tag_name: "div".to_string(),
      namespace: "".to_string(),
      attributes: Vec::new(),
    },
    Some(root),
    /* inert_subtree */ false,
  );
  let detached = doc.push_node(
    NodeKind::Text {
      content: "detached".to_string(),
    },
    None,
    /* inert_subtree */ false,
  );

  let snapshot = doc.to_renderer_dom_with_mapping();
  assert_eq!(
    snapshot.nodeid_to_preorder[detached.index()],
    0,
    "detached nodes should not be represented in the snapshot mapping"
  );
  assert!(
    !snapshot
      .preorder_to_nodeid
      .iter()
      .any(|&v| v == Some(detached)),
    "detached nodes should not appear in preorder_to_nodeid"
  );
}

#[test]
fn snapshot_with_mapping_matches_renderer_preorder_ids() {
  let html = concat!(
    "<!DOCTYPE html>",
    "<html><head><title>x</title></head>",
    "<body>",
    "<div id=host>",
    "<template shadowroot=open>",
    "<slot name=s></slot><span>shadow</span>",
    "</template>",
    "<p>light</p>",
    "</div>",
    "<template><span>in</span></template>",
    "</body></html>"
  );
  let root = crate::dom::parse_html(html).unwrap();
  let doc = Document::from_renderer_dom(&root);

  let snapshot = doc.to_renderer_dom_with_mapping();

  // Ensure the renderer snapshot structure is unchanged.
  assert_eq!(snapshot_dom(&root), snapshot_dom(&snapshot.dom));

  // Basic invariants.
  assert_eq!(snapshot.preorder_to_nodeid[0], None);
  assert_eq!(snapshot.preorder_to_nodeid[1], Some(doc.root()));

  // Mapping length should be node_count + 1 (synthetic 0 slot).
  let renderer_ids = crate::dom::enumerate_dom_ids(&snapshot.dom);
  let renderer_node_count = renderer_ids.len();
  assert_eq!(snapshot.preorder_to_nodeid.len(), renderer_node_count + 1);
  assert_eq!(snapshot.nodeid_to_preorder.len(), doc.nodes_len());

  // Walk snapshot DOM and verify mapping points back to the corresponding dom2 node. We resolve the
  // renderer id for each `DomNode` via `enumerate_dom_ids` to ensure the mapping stays aligned with
  // the renderer's authoritative pre-order ids.
  let mut stack: Vec<&DomNode> = vec![&snapshot.dom];
  while let Some(node) = stack.pop() {
    let preorder_id = *renderer_ids
      .get(&(node as *const DomNode))
      .expect("missing renderer preorder id");

    let mapped = snapshot.preorder_to_nodeid[preorder_id]
      .expect("missing dom2 node mapping for renderer preorder id");
    assert_eq!(
      snapshot.nodeid_to_preorder[mapped.index()],
      preorder_id,
      "reverse mapping mismatch for dom2 node {mapped:?}"
    );

    let expected_kind = node_kind_from_dom_node_type(&node.node_type);
    assert_eq!(
      doc.node(mapped).kind, expected_kind,
      "node kind mismatch at renderer preorder id {preorder_id}"
    );

    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }

  // Verify that the reverse map (when present) is consistent with the forward map.
  for (idx, &preorder) in snapshot.nodeid_to_preorder.iter().enumerate() {
    if preorder == 0 {
      continue;
    }
    assert_eq!(
      snapshot.preorder_to_nodeid[preorder],
      Some(NodeId(idx)),
      "reverse/forward mapping disagreement for dom2 node index {idx}"
    );
  }
}

#[test]
fn snapshot_with_mapping_handles_deep_trees_without_recursion_overflow() {
  // A depth that would almost certainly overflow recursive snapshotting/traversals on typical test
  // stacks.
  const DEPTH: usize = 50_000;

  let mut node = DomNode {
    node_type: DomNodeType::Text {
      content: "leaf".to_string(),
    },
    children: Vec::new(),
  };
  for _ in 0..DEPTH {
    node = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: "".to_string(),
        attributes: Vec::new(),
      },
      children: vec![node],
    };
  }

  let root = DomNode {
    node_type: DomNodeType::Document {
      quirks_mode: selectors::context::QuirksMode::NoQuirks,
    },
    children: vec![node],
  };
  let doc = Document::from_renderer_dom(&root);

  let snapshot = doc.to_renderer_dom_with_mapping();

  // Document root + DEPTH elements + leaf text, plus synthetic 0 mapping slot.
  assert_eq!(snapshot.preorder_to_nodeid.len(), doc.nodes_len() + 1);
  assert_eq!(snapshot.preorder_to_nodeid[0], None);
  assert_eq!(snapshot.preorder_to_nodeid[1], Some(doc.root()));

  // Sanity check a few positions, including the deepest leaf.
  let last_preorder = snapshot.preorder_to_nodeid.len() - 1;
  let leaf_id = snapshot.preorder_to_nodeid[last_preorder]
    .expect("missing mapping for last preorder id");
  assert_eq!(
    doc.node(leaf_id).kind,
    NodeKind::Text {
      content: "leaf".to_string()
    }
  );
  assert_eq!(snapshot.nodeid_to_preorder[leaf_id.index()], last_preorder);
}
