#![cfg(feature = "a11y_accesskit")]

use crate::accessibility::AccessibilityNode;
use crate::dom2::RendererDomMapping;
use rustc_hash::FxHashSet;

use accesskit::{Node, NodeBuilder, NodeClassSet, NodeId, Role, Tree, TreeUpdate};

use super::accesskit_ids::accesskit_id_for_dom2;

/// Build an AccessKit [`TreeUpdate`] from a FastRender [`AccessibilityNode`] tree.
///
/// When a [`RendererDomMapping`] is provided, node ids are derived from stable `dom2::NodeId`
/// values instead of renderer preorder indices. This keeps AccessKit ids stable across DOM
/// insertions/removals that would otherwise renumber the renderer snapshot.
///
/// ## Duplicate id handling
///
/// [`RendererDomMapping::node_id_for_preorder`] is not necessarily 1:1: some renderer preorder ids
/// can map to the same `dom2::NodeId` (currently: `<wbr>` synthesizes a ZWSP text node that maps to
/// the owning `<wbr>` element).
///
/// To keep the AccessKit `NodeId` mapping 1:1 and reliably routable back into the DOM:
/// - We only use the dom2-derived id when the preorder id matches
///   [`RendererDomMapping::preorder_for_node_id`] (i.e. it's the "real" node).
/// - Any additional/synthetic preorder ids that map to the same `dom2::NodeId` fall back to a
///   preorder-derived id in a separate namespace.
///
/// As a defensive fallback, if we still encounter a duplicate AccessKit id during construction, we
/// drop the later node and splice its children into the parent.
pub fn tree_update_from_accessibility_tree(
  root: &AccessibilityNode,
  mapping: Option<&RendererDomMapping>,
) -> TreeUpdate {
  let mut nodes: Vec<(NodeId, Node)> = Vec::new();
  let mut used_ids: FxHashSet<NodeId> = FxHashSet::default();
  let mut classes = NodeClassSet::default();

  fn role_for_accessibility(node: &AccessibilityNode) -> Role {
    // The exported accessibility roles are currently stringly-typed. Map the common ones we need
    // for chrome/content integration and fall back to a generic container role.
    match node.role.as_str() {
      "button" => Role::Button,
      // AccessKit does not have a dedicated "document" role; `RootWebArea` is the standard web root.
      "document" => Role::RootWebArea,
      _ => Role::GenericContainer,
    }
  }

  fn fallback_id_for_preorder(preorder: usize) -> NodeId {
    // Preorder ids are 1-based. Encode them into a separate namespace to avoid colliding with the
    // dom2-derived ids produced by `accesskit_id_for_dom2`.
    const MARKER: u128 = 0xFA;
    const NAMESPACE_PREORDER: u128 = 0x02;
    let payload = (preorder as u128).saturating_add(1);
    let raw = (MARKER << 120) | (NAMESPACE_PREORDER << 112) | (payload & ((1u128 << 112) - 1));
    NodeId(std::num::NonZeroU128::new(raw).expect("preorder-derived AccessKit NodeId must be non-zero")) // fastrender-allow-unwrap
  }

  fn accesskit_id_for_node(node: &AccessibilityNode, mapping: Option<&RendererDomMapping>) -> NodeId {
    if let Some(mapping) = mapping {
      if let Some(dom2_id) = mapping.node_id_for_preorder(node.node_id) {
        // Avoid duplicate AccessKit ids when multiple renderer preorder ids map to the same dom2
        // node (synthetic snapshot nodes). Only the "real" preorder id gets the dom2-derived id.
        if mapping.preorder_for_node_id(dom2_id) == Some(node.node_id) {
          return accesskit_id_for_dom2(dom2_id);
        }
        return fallback_id_for_preorder(node.node_id);
      }
    }
    fallback_id_for_preorder(node.node_id)
  }

  fn build_node(
    node: &AccessibilityNode,
    mapping: Option<&RendererDomMapping>,
    nodes: &mut Vec<(NodeId, Node)>,
    used_ids: &mut FxHashSet<NodeId>,
    classes: &mut NodeClassSet,
  ) -> Vec<NodeId> {
    let id = accesskit_id_for_node(node, mapping);

    let mut child_ids: Vec<NodeId> = Vec::new();
    for child in node.children.iter() {
      child_ids.extend(build_node(child, mapping, nodes, used_ids, classes));
    }

    if !used_ids.insert(id) {
      // Duplicate id: drop this node and splice its children.
      return child_ids;
    }

    let mut builder = NodeBuilder::new(role_for_accessibility(node));
    builder.set_children(child_ids);
    if let Some(name) = node.name.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
      builder.set_name(name.to_string());
    }
    nodes.push((id, builder.build(classes)));
    vec![id]
  }

  let root_ids = build_node(root, mapping, &mut nodes, &mut used_ids, &mut classes);
  let root_id = root_ids
    .first()
    .copied()
    .expect("accessibility tree must produce at least one AccessKit node"); // fastrender-allow-unwrap

  debug_assert!(
    root_ids.len() == 1,
    "accessibility tree root collapsed into multiple nodes; root id selection may be ambiguous"
  );

  TreeUpdate {
    nodes,
    tree: Some(Tree::new(root_id)),
    focus: None,
  }
}
