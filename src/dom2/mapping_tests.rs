#![cfg(test)]

use crate::dom::{DomNode, DomNodeType};
use selectors::context::QuirksMode;
use std::cmp::Ordering;

use super::{Document, NodeId, NodeKind, SlotAssignmentMode};

fn find_node_id_by_attr_id_anywhere(doc: &Document, id: &str) -> Option<NodeId> {
  for index in 0..doc.nodes_len() {
    let node_id = NodeId::from_index(index);
    let node = doc.node(node_id);
    let attributes = match &node.kind {
      NodeKind::Element { attributes, .. } | NodeKind::Slot { attributes, .. } => attributes,
      _ => continue,
    };
    if attributes
      .iter()
      .any(|(name, value)| name.eq_ignore_ascii_case("id") && value == id)
    {
      return Some(node_id);
    }
  }
  None
}

fn find_first_node_matching(doc: &Document, predicate: impl Fn(&NodeKind) -> bool) -> Option<NodeId> {
  for index in 0..doc.nodes_len() {
    let node_id = NodeId::from_index(index);
    if predicate(&doc.node(node_id).kind) {
      return Some(node_id);
    }
  }
  None
}

fn find_node_by_id<'a>(node: &'a DomNode, id: &str) -> Option<&'a DomNode> {
  if node.get_attribute_ref("id") == Some(id) {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_node_by_id(child, id) {
      return Some(found);
    }
  }
  None
}

fn node_kind_from_dom_node_type(node_type: &DomNodeType) -> NodeKind {
  match node_type {
    DomNodeType::Document { quirks_mode, .. } => NodeKind::Document {
      quirks_mode: *quirks_mode,
    },
    DomNodeType::ShadowRoot {
      mode,
      delegates_focus,
    } => NodeKind::ShadowRoot {
      mode: *mode,
      delegates_focus: *delegates_focus,
      slot_assignment: SlotAssignmentMode::Named,
      clonable: false,
      serializable: false,
      declarative: false,
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
      prefix: None,
      attributes: attributes.clone(),
    },
    DomNodeType::Text { content } => NodeKind::Text {
      content: content.clone(),
    },
  }
}

#[test]
fn renderer_dom_mapping_public_api_matches_snapshot_mapping() {
  // Include nodes that exist in dom2 but are dropped from renderer snapshots (doctype + comments),
  // plus synthetic renderer nodes (`<wbr>` ZWSP child), inert `<template>` contents, and declarative
  // shadow roots.
  let html = concat!(
    "<!DOCTYPE html>",
    "<!-- before -->",
    "<html><head><!-- head --></head><body>",
    "<!-- body -->",
    "<template id=t><div id=in-template>tmpl</div></template>",
    "<div id=host>",
    "<template shadowroot=open>",
    "<div id=in-shadow><span>shadow</span></div>",
    "</template>",
    "<p>light</p>",
    "</div>",
    "<wbr id=w>",
    "</body></html>",
  );

  let mut doc = super::parse_html(html).unwrap();

  // Create a detached node (reachable via `NodeId` but not attached under the document root).
  let detached = doc.push_node(
    NodeKind::Element {
      tag_name: "div".to_string(),
      namespace: "".to_string(),
      prefix: None,
      attributes: Vec::new(),
    },
    None,
    /* inert_subtree */ false,
  );

  let mapping_only = doc.renderer_dom_mapping();
  let snapshot = doc.to_renderer_dom_with_mapping();
  let renderer_ids = crate::dom::enumerate_dom_ids(&snapshot.dom);

  // The mapping-only API should exactly match the mapping returned alongside a full renderer
  // snapshot for every renderer preorder id.
  for preorder_id in 1..=renderer_ids.len() {
    assert_eq!(
      mapping_only.node_id_for_preorder(preorder_id),
      snapshot.mapping.node_id_for_preorder(preorder_id),
      "preorder mapping mismatch at renderer id {preorder_id}"
    );
  }

  // Also verify the reverse mapping matches for a representative subset of `NodeId`s, including
  // nodes that are dropped from the renderer snapshot and detached nodes.
  let doctype = find_first_node_matching(&doc, |k| matches!(k, NodeKind::Doctype { .. }))
    .expect("expected a doctype node in dom2 document");
  let comment = find_first_node_matching(&doc, |k| matches!(k, NodeKind::Comment { .. }))
    .expect("expected a comment node in dom2 document");
  let shadow_root = find_first_node_matching(&doc, |k| matches!(k, NodeKind::ShadowRoot { .. }))
    .expect("expected a shadow root node in dom2 document");

  let wbr = doc.get_element_by_id("w").expect("missing `<wbr id=w>`");
  let template = doc.get_element_by_id("t").expect("missing `<template id=t>`");
  let in_template =
    find_node_id_by_attr_id_anywhere(&doc, "in-template").expect("missing `#in-template`");
  let in_shadow =
    find_node_id_by_attr_id_anywhere(&doc, "in-shadow").expect("missing `#in-shadow`");

  let sample_node_ids = [
    doc.root(),
    doctype,
    comment,
    template,
    in_template,
    shadow_root,
    in_shadow,
    wbr,
    detached,
  ];

  for node_id in sample_node_ids {
    assert_eq!(
      mapping_only.preorder_for_node_id(node_id),
      snapshot.mapping.preorder_for_node_id(node_id),
      "reverse mapping mismatch for node {node_id:?}"
    );
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
      prefix: None,
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
      scripting_enabled: true,
      is_html_document: true,
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
      scripting_enabled: true,
      is_html_document: true,
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

#[test]
fn renderer_dom_mapping_ignores_stale_child_entries_when_parent_pointer_mismatches() {
  let root = crate::dom::parse_html(
    r#"<!doctype html>
    <html>
      <body>
        <div id=host><div id=in></div></div>
        <div id=out></div>
      </body>
    </html>"#,
  )
  .unwrap();
  let mut doc = Document::from_renderer_dom(&root);

  let in_div = doc.get_element_by_id("in").expect("missing in div");
  doc.node_mut(in_div).parent = None;

  let snapshot = doc.to_renderer_dom_with_mapping();
  assert_eq!(
    snapshot.mapping.preorder_for_node_id(in_div),
    None,
    "detached nodes should not be assigned renderer preorder ids"
  );

  // Ensure the snapshot DOM tree itself no longer contains the detached node.
  let mut stack: Vec<&DomNode> = vec![&snapshot.dom];
  while let Some(node) = stack.pop() {
    if node.get_attribute_ref("id") == Some("in") {
      panic!("detached node unexpectedly present in renderer snapshot DOM");
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
}

#[test]
fn renderer_dom_mapping_round_trips_with_form_control_attribute_overlays() {
  let html = concat!(
    "<!doctype html>",
    "<html><body>",
    "<input id=i type=checkbox value=foo checked>",
    "<textarea id=t>hello</textarea>",
    "<select multiple><option id=o selected>One</option></select>",
    "</body></html>",
  );
  let mut doc = crate::dom2::parse_html(html).unwrap();
  let input = doc.get_element_by_id("i").expect("input element");
  let textarea = doc.get_element_by_id("t").expect("textarea element");
  let option = doc.get_element_by_id("o").expect("option element");

  // Mutate internal form state so snapshots must overlay state back into attributes.
  doc.set_input_value(input, "bar").unwrap();
  doc.set_input_checked(input, false).unwrap();
  doc.set_textarea_value(textarea, "dirty").unwrap();
  doc.set_option_selected(option, false).unwrap();

  let snapshot = doc.to_renderer_dom_with_mapping();
  let renderer_ids = crate::dom::enumerate_dom_ids(&snapshot.dom);

  // Every renderer preorder id should map back to a `dom2` `NodeId` and round-trip.
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

    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }

  // Spot-check mapping for nodes whose attributes were overlaid.
  for (id_attr, expected) in [("i", input), ("t", textarea), ("o", option)] {
    let node = find_node_by_id(&snapshot.dom, id_attr).expect("missing node in snapshot");
    let preorder_id = *renderer_ids
      .get(&(node as *const DomNode))
      .expect("missing preorder id for node");
    assert_eq!(
      snapshot.mapping.node_id_for_preorder(preorder_id),
      Some(expected),
      "mapping mismatch for node with id={id_attr}"
    );
  }
}

#[test]
fn renderer_dom_mapping_cmp_node_ids_matches_enumerate_dom_ids_order() {
  let root = crate::dom::parse_html(
    r#"<!doctype html>
    <html>
      <body>
        <div id=a>
          <span id=b></span>
          <p id=c></p>
        </div>
        <div id=d></div>
      </body>
    </html>"#,
  )
  .unwrap();
  let doc = Document::from_renderer_dom(&root);
  let snapshot = doc.to_renderer_dom_with_mapping();
  let renderer_ids = crate::dom::enumerate_dom_ids(&snapshot.dom);

  // Build a stable expected-order mapping by inspecting the renderer snapshot DOM with
  // `enumerate_dom_ids`.
  let mut id_to_preorder: std::collections::HashMap<String, usize> =
    std::collections::HashMap::new();
  let mut stack: Vec<&DomNode> = vec![&snapshot.dom];
  while let Some(node) = stack.pop() {
    if let Some(id) = node.get_attribute_ref("id") {
      let preorder_id = *renderer_ids
        .get(&(node as *const DomNode))
        .expect("missing renderer preorder id");
      id_to_preorder.insert(id.to_string(), preorder_id);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }

  let a = doc.get_element_by_id("a").expect("missing #a");
  let b = doc.get_element_by_id("b").expect("missing #b");
  let c = doc.get_element_by_id("c").expect("missing #c");
  let d = doc.get_element_by_id("d").expect("missing #d");

  for (id, node_id) in [("a", a), ("b", b), ("c", c), ("d", d)] {
    assert_eq!(
      snapshot.mapping.preorder_for_node_id(node_id),
      id_to_preorder.get(id).copied(),
      "preorder mismatch for #{id}"
    );
  }

  let expected_ab = id_to_preorder["a"].cmp(&id_to_preorder["b"]);
  let expected_ac = id_to_preorder["a"].cmp(&id_to_preorder["c"]);
  let expected_ad = id_to_preorder["a"].cmp(&id_to_preorder["d"]);
  let expected_bd = id_to_preorder["b"].cmp(&id_to_preorder["d"]);

  assert_eq!(snapshot.mapping.cmp_node_ids(a, b), Some(expected_ab));
  assert_eq!(snapshot.mapping.cmp_node_ids(a, c), Some(expected_ac));
  assert_eq!(snapshot.mapping.cmp_node_ids(a, d), Some(expected_ad));
  assert_eq!(snapshot.mapping.cmp_node_ids(b, d), Some(expected_bd));

  // Ensure comparison is anti-symmetric (including equality).
  assert_eq!(
    snapshot.mapping.cmp_node_ids(b, a),
    Some(expected_ab.reverse())
  );
}

#[test]
fn renderer_dom_mapping_cmp_node_ids_returns_none_for_detached_nodes() {
  let root = crate::dom::parse_html("<!doctype html><html><body><div id=a></div></body></html>")
    .unwrap();
  let mut doc = Document::from_renderer_dom(&root);
  let attached = doc.get_element_by_id("a").expect("missing #a");

  let detached = doc.create_element("div", "");

  let snapshot = doc.to_renderer_dom_with_mapping();
  assert_eq!(snapshot.mapping.cmp_node_ids(attached, detached), None);
  assert_eq!(snapshot.mapping.cmp_node_ids(detached, attached), None);
  assert_eq!(snapshot.mapping.cmp_node_ids(detached, detached), None);
}

#[test]
fn renderer_dom_mapping_cmp_node_ids_reflects_sibling_insertion_order_changes() {
  let root = crate::dom::parse_html(
    "<!doctype html><html><body><div id=a></div><div id=b></div></body></html>",
  )
  .unwrap();
  let mut doc = Document::from_renderer_dom(&root);
  let a = doc.get_element_by_id("a").expect("missing #a");
  let b = doc.get_element_by_id("b").expect("missing #b");
  let body = doc.body().expect("missing <body>");

  let snapshot_before = doc.to_renderer_dom_with_mapping();
  assert_eq!(
    snapshot_before.mapping.cmp_node_ids(a, b),
    Some(Ordering::Less),
    "expected initial DOM order to be a < b"
  );

  // Create a new node after `b` in stable NodeId index space, then insert it between `a` and `b`.
  // `cmp_node_ids` must reflect DOM order, not NodeId allocation order.
  let c = doc.create_element("div", "");
  doc.set_attribute(c, "id", "c").unwrap();
  assert!(
    c.index() > b.index(),
    "expected newly created node to have a larger NodeId index than existing siblings"
  );
  doc.insert_before(body, c, Some(b)).unwrap();

  let snapshot_after = doc.to_renderer_dom_with_mapping();
  assert_eq!(
    snapshot_after.mapping.cmp_node_ids(a, c),
    Some(Ordering::Less),
    "expected DOM order to be a < c after insertion"
  );
  assert_eq!(
    snapshot_after.mapping.cmp_node_ids(c, b),
    Some(Ordering::Less),
    "expected DOM order to be c < b after insertion"
  );
  assert_eq!(
    snapshot_after.mapping.cmp_node_ids(a, b),
    Some(Ordering::Less),
    "expected DOM order to remain a < b after insertion"
  );
}
