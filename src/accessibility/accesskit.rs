#![cfg(feature = "browser_ui")]

use super::AccessibilityNode;
use super::accesskit_mapping::accesskit_role_for_fastr_role;
pub use accesskit::{Node, NodeBuilder, NodeClassSet, NodeId, Role, TextDirection, Tree, TreeUpdate};
use std::collections::HashMap;
use std::num::NonZeroU128;

/// Result of converting a FastRender [`AccessibilityNode`] tree into an AccessKit [`TreeUpdate`].
///
/// The `id_map` resolves HTML `id` attribute values (as exported by FastRender) to the corresponding
/// AccessKit [`NodeId`]s, which is useful for resolving relationship properties.
#[derive(Debug)]
pub struct AccessKitTreeResult {
  pub update: TreeUpdate,
  pub id_map: HashMap<String, NodeId>,
}

#[derive(Debug)]
struct AssignedNode<'a> {
  id: NodeId,
  node: &'a AccessibilityNode,
  children: Vec<AssignedNode<'a>>,
}

fn new_node_id(raw: u128) -> NodeId {
  // AccessKit uses a non-zero identifier type; use a monotonic counter so ids are stable for a
  // given traversal order.
  NodeId(NonZeroU128::new(raw).expect("node id counter must never be zero")) // fastrender-allow-unwrap
}

fn assign_ids<'a>(
  node: &'a AccessibilityNode,
  next_id: &mut u128,
  id_map: &mut HashMap<String, NodeId>,
) -> AssignedNode<'a> {
  let id = new_node_id(*next_id);
  *next_id = next_id.saturating_add(1);

  if let Some(html_id) = node.id.as_ref().filter(|s| !s.is_empty()) {
    // HTML ids are expected to be unique; keep the first occurrence if the document is invalid.
    id_map.entry(html_id.clone()).or_insert(id);
  }

  let children = node
    .children
    .iter()
    .map(|child| assign_ids(child, next_id, id_map))
    .collect();

  AssignedNode { id, node, children }
}

fn role_from_fastrender(role: &str) -> Role {
  accesskit_role_for_fastr_role(role)
}

fn build_accesskit_nodes(
  assigned: &AssignedNode<'_>,
  id_map: &HashMap<String, NodeId>,
  classes: &mut NodeClassSet,
  out: &mut Vec<(NodeId, Node)>,
) {
  let mut builder = NodeBuilder::new(role_from_fastrender(&assigned.node.role));

  if let Some(name) = assigned.node.name.as_ref() {
    if !name.is_empty() {
      builder.set_name(name.clone());
    }
  }

  if let Some(description) = assigned.node.description.as_ref() {
    if !description.is_empty() {
      builder.set_description(description.clone());
    }
  }

  if let Some(value) = assigned.node.value.as_ref() {
    if !value.is_empty() {
      builder.set_value(value.clone());
    }
  }

  if let Some(role_description) = assigned.node.role_description.as_ref() {
    if !role_description.is_empty() {
      builder.set_role_description(role_description.clone());
    }
  }

  // FastRender exports relationship targets as HTML ids. Resolve those ids to AccessKit node ids
  // during tree construction.
  if let Some(relations) = assigned.node.relations.as_ref() {
    let labelled_by: Vec<NodeId> = relations
      .labelled_by
      .iter()
      .filter_map(|id| id_map.get(id).copied())
      .collect();
    if !labelled_by.is_empty() {
      builder.set_labelled_by(labelled_by);
    }

    let described_by: Vec<NodeId> = relations
      .described_by
      .iter()
      .filter_map(|id| id_map.get(id).copied())
      .collect();
    if !described_by.is_empty() {
      builder.set_described_by(described_by);
    }

    if let Some(id) = relations
      .active_descendant
      .as_ref()
      .and_then(|id| id_map.get(id).copied())
    {
      builder.set_active_descendant(id);
    }

    if let Some(id) = relations
      .details
      .as_ref()
      .and_then(|id| id_map.get(id).copied())
    {
      builder.set_details(vec![id]);
    }

    if let Some(id) = relations
      .error_message
      .as_ref()
      .and_then(|id| id_map.get(id).copied())
    {
      builder.set_error_message(id);
    }

    let controls: Vec<NodeId> = relations
      .controls
      .iter()
      .filter_map(|id| id_map.get(id).copied())
      .collect();
    if !controls.is_empty() {
      builder.set_controls(controls);
    }

    // FastRender already applies `aria-owns` reparenting when building the accessibility tree.
    // AccessKit does not expose an "owns" property in 0.11, so there is no additional relationship
    // mapping to perform here.
  }

  let child_ids: Vec<NodeId> = assigned.children.iter().map(|child| child.id).collect();
  if !child_ids.is_empty() {
    builder.set_children(child_ids);
  }

  out.push((assigned.id, builder.build(classes)));

  for child in &assigned.children {
    build_accesskit_nodes(child, id_map, classes, out);
  }
}

/// Convert FastRender's exported accessibility tree into an AccessKit tree update.
pub fn build_accesskit_tree_update(root: &AccessibilityNode) -> AccessKitTreeResult {
  let mut id_map: HashMap<String, NodeId> = HashMap::new();
  let mut next_id: u128 = 1;
  let assigned_root = assign_ids(root, &mut next_id, &mut id_map);

  let mut nodes: Vec<(NodeId, Node)> = Vec::new();
  let mut classes = NodeClassSet::new();
  build_accesskit_nodes(&assigned_root, &id_map, &mut classes, &mut nodes);

  AccessKitTreeResult {
    update: TreeUpdate {
      nodes,
      tree: Some(Tree::new(assigned_root.id)),
      focus: None,
    },
    id_map,
  }
}

#[cfg(all(test, feature = "browser_ui"))]
mod tests {
  use super::*;
  use crate::api::{FastRender, RenderOptions};

  fn node_for_id<'a>(update: &'a TreeUpdate, id: NodeId) -> &'a Node {
    update
      .nodes
      .iter()
      .find_map(|(node_id, node)| (*node_id == id).then_some(node))
      .expect("missing node in update")
  }

  #[test]
  fn html_label_for_maps_to_accesskit_labelled_by_relation() {
    let html = r#"
      <html><body>
        <label id="l" for="i">Name</label>
        <input id="i" />
      </body></html>
    "#;

    let mut renderer = FastRender::new().expect("renderer");
    let options = RenderOptions::new().with_viewport(800, 600);
    let tree = renderer
      .accessibility_tree_html(html, options)
      .expect("accessibility tree");

    let result = build_accesskit_tree_update(&tree);

    let input_id = *result.id_map.get("i").expect("input node id");
    let label_id = *result.id_map.get("l").expect("label node id");
    let input_node = node_for_id(&result.update, input_id);

    let labelled_by = input_node.labelled_by();
    assert_eq!(labelled_by, &[label_id]);
  }
}
