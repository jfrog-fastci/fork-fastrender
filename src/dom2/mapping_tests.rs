use crate::dom::{DomNode, DomNodeType};
use selectors::context::QuirksMode;

use super::{Document, NodeKind};

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
fn renderer_dom_mapping_aligns_with_enumerate_dom_ids_including_templates_and_shadow_roots() {
  let html = concat!(
    "<!DOCTYPE html>",
    "<html><body>",
    "<template><div id=in-template><span></span></div></template>",
    "<div id=after-template></div>",
    "<div id=host>",
    "<template shadowroot=open>",
    "<slot name=s></slot><div><span>shadow</span></div>",
    "</template>",
    "<p>light</p>",
    "</div>",
    "</body></html>",
  );
  let root = crate::dom::parse_html(html).unwrap();
  let mut doc = Document::from_renderer_dom(&root);

  // Create a detached node (reachable via `NodeId` but not attached under the document root).
  let detached = doc.push_node(
    NodeKind::Element {
      tag_name: "div".to_string(),
      namespace: "".to_string(),
      attributes: Vec::new(),
    },
    None,
    /* inert_subtree */ false,
  );

  let snapshot = doc.to_renderer_dom_with_mapping();
  let renderer_ids = crate::dom::enumerate_dom_ids(&snapshot.dom);

  // Walk the snapshot DOM and verify that every renderer preorder id round-trips through the
  // mapping and points back to a `dom2` node whose kind matches the snapshot node.
  let mut stack: Vec<&DomNode> = vec![&snapshot.dom];
  while let Some(node) = stack.pop() {
    let preorder_id = *renderer_ids
      .get(&(node as *const DomNode))
      .expect("missing renderer preorder id");

    let dom2_id = snapshot
      .mapping
      .node_id_for_preorder(preorder_id)
      .expect("missing dom2 node mapping for renderer preorder id");

    assert_eq!(
      snapshot.mapping.preorder_for_node_id(dom2_id),
      Some(preorder_id),
      "reverse mapping mismatch for dom2 node {dom2_id:?}"
    );

    assert_eq!(
      doc.node(dom2_id).kind,
      node_kind_from_dom_node_type(&node.node_type),
      "node kind mismatch at renderer preorder id {preorder_id}"
    );

    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }

  // Detached nodes must not have renderer preorder ids.
  assert_eq!(
    snapshot.mapping.preorder_for_node_id(detached),
    None,
    "detached nodes should not be assigned renderer preorder ids"
  );
  for id in 1..=renderer_ids.len() {
    assert_ne!(
      snapshot.mapping.node_id_for_preorder(id),
      Some(detached),
      "detached nodes must not appear in renderer preorder mapping"
    );
  }
}

#[test]
fn renderer_dom_mapping_handles_deep_trees_without_recursion_overflow() {
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
      quirks_mode: QuirksMode::NoQuirks,
    },
    children: vec![node],
  };
  let doc = Document::from_renderer_dom(&root);
  let snapshot = doc.to_renderer_dom_with_mapping();

  let renderer_ids = crate::dom::enumerate_dom_ids(&snapshot.dom);
  assert_eq!(
    snapshot.mapping.node_id_for_preorder(0),
    None,
    "renderer preorder ids must be 1-based"
  );
  assert_eq!(
    renderer_ids.len(),
    doc.nodes_len(),
    "snapshot node count should match dom2 connected node count"
  );

  // Verify the last node maps to the leaf text node.
  let last_preorder = renderer_ids.len();
  let leaf_id = snapshot
    .mapping
    .node_id_for_preorder(last_preorder)
    .expect("missing mapping for last renderer preorder id");
  assert_eq!(
    snapshot.mapping.preorder_for_node_id(leaf_id),
    Some(last_preorder),
    "reverse mapping mismatch for leaf node"
  );
  assert_eq!(
    doc.node(leaf_id).kind,
    NodeKind::Text {
      content: "leaf".to_string()
    }
  );
}

#[test]
fn renderer_dom_mapping_models_wbr_synthetic_zwsp_nodes() {
  // The renderer synthesizes a trailing ZWSP text node for HTML `<wbr>` elements so line breaking
  // can treat it as a break opportunity. The dom2 snapshot/mapping must account for these synthetic
  // nodes while still mapping them back to the real `<wbr>` `NodeId`.
  let root = DomNode {
    node_type: DomNodeType::Document {
      quirks_mode: QuirksMode::NoQuirks,
    },
    children: vec![DomNode {
      node_type: DomNodeType::Element {
        tag_name: "wbr".to_string(),
        namespace: "".to_string(),
        attributes: vec![("id".to_string(), "w".to_string())],
      },
      children: Vec::new(),
    }],
  };
  let doc = Document::from_renderer_dom(&root);
  let snapshot = doc.to_renderer_dom_with_mapping();

  let renderer_ids = crate::dom::enumerate_dom_ids(&snapshot.dom);
  assert_eq!(
    renderer_ids.len(),
    doc.nodes_len() + 1,
    "expected the renderer snapshot to include one synthetic node for the `<wbr>` ZWSP child"
  );

  let wbr_id = doc.get_element_by_id("w").expect("missing `<wbr>` element");
  let wbr_preorder = snapshot
    .mapping
    .preorder_for_node_id(wbr_id)
    .expect("missing preorder id for `<wbr>` element");

  // In this minimal tree, the synthetic ZWSP should be the immediate next preorder id after the
  // `<wbr>` element.
  let zwsp_preorder = wbr_preorder + 1;
  assert_eq!(
    snapshot.mapping.node_id_for_preorder(zwsp_preorder),
    Some(wbr_id),
    "synthetic ZWSP node should map back to its parent `<wbr>` element"
  );
  assert_eq!(
    snapshot.mapping.preorder_for_node_id(wbr_id),
    Some(wbr_preorder),
    "reverse mapping for `<wbr>` should remain the element's own preorder id"
  );
}
