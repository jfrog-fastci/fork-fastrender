#![cfg(feature = "browser_ui")]

use crate::accessibility::AccessibilityNode;
use crate::ui::messages::PageAccessKitSubtree;
use crate::ui::TabId;
use std::num::NonZeroU128;

fn node_id_for_tab(tab_id: TabId, local_id: u64) -> accesskit::NodeId {
  // We keep page subtree node ids deterministic and namespaced per tab to avoid collisions with
  // egui's own AccessKit node ids.
  //
  // Layout:
  // - High 64 bits: `TabId` (non-zero in normal operation).
  // - Low 64 bits: stable, per-tree local id (1-based).
  //
  // This intentionally produces ids that are "large" (>= 2^64) so they are very unlikely to
  // collide with any UI/chrome node ids allocated by egui/accesskit_winit.
  let raw = ((tab_id.0 as u128) << 64) | (local_id as u128);
  accesskit::NodeId(NonZeroU128::new(raw).expect("node id must be non-zero")) // fastrender-allow-unwrap
}

fn role_from_fastr_role(role: &str) -> accesskit::Role {
  // `crate::accessibility` uses a stringy, browser-like role vocabulary. Map the subset we produce
  // today onto AccessKit roles. Unknown roles are treated as generic containers so the subtree is
  // still well-formed.
  match role {
    "document" => accesskit::Role::Document,
    "button" => accesskit::Role::Button,
    "link" => accesskit::Role::Link,
    "heading" => accesskit::Role::Heading,
    "textbox" => accesskit::Role::TextField,
    "textbox-multiline" => accesskit::Role::TextField,
    "checkbox" => accesskit::Role::CheckBox,
    "radio" => accesskit::Role::RadioButton,
    "image" => accesskit::Role::Image,
    "list" => accesskit::Role::List,
    "listitem" => accesskit::Role::ListItem,
    "paragraph" => accesskit::Role::Paragraph,
    "statictext" => accesskit::Role::StaticText,
    _ => accesskit::Role::GenericContainer,
  }
}

fn normalize_name(name: &str) -> Option<String> {
  let trimmed = name.trim();
  if trimmed.is_empty() {
    None
  } else {
    Some(trimmed.to_string())
  }
}

fn build_subtree_nodes(
  tab_id: TabId,
  node: &AccessibilityNode,
  next_local_id: &mut u64,
  classes: &mut accesskit::NodeClassSet,
  nodes_out: &mut Vec<(accesskit::NodeId, accesskit::Node)>,
  focus_out: &mut Option<accesskit::NodeId>,
) -> accesskit::NodeId {
  let local_id = *next_local_id;
  *next_local_id = next_local_id.saturating_add(1);
  let id = node_id_for_tab(tab_id, local_id);

  if node.states.focused {
    *focus_out = Some(id);
  }

  let mut children_ids = Vec::with_capacity(node.children.len());
  for child in &node.children {
    let child_id = build_subtree_nodes(tab_id, child, next_local_id, classes, nodes_out, focus_out);
    children_ids.push(child_id);
  }

  let role = role_from_fastr_role(&node.role);
  let mut builder = accesskit::NodeBuilder::new(role);
  if let Some(name) = node.name.as_deref().and_then(normalize_name) {
    builder.set_name(name);
  }
  if !children_ids.is_empty() {
    builder.set_children(children_ids);
  }

  // TODO: map more fields (value/checked/expanded/disabled, etc) as we wire up page actions.

  let built = builder.build(classes);
  nodes_out.push((id, built));
  id
}

/// Convert a renderer [`AccessibilityNode`] tree into an AccessKit subtree update suitable for
/// embedding into a windowed browser's overall accessibility tree.
pub fn accesskit_subtree_for_page(tab_id: TabId, root: &AccessibilityNode) -> PageAccessKitSubtree {
  let mut nodes: Vec<(accesskit::NodeId, accesskit::Node)> = Vec::new();
  let mut focus_id: Option<accesskit::NodeId> = None;
  let mut next_local_id: u64 = 1;
  let mut classes = accesskit::NodeClassSet::new();

  let root_id = build_subtree_nodes(
    tab_id,
    root,
    &mut next_local_id,
    &mut classes,
    &mut nodes,
    &mut focus_id,
  );

  PageAccessKitSubtree {
    root_id,
    nodes,
    focus_id,
  }
}
