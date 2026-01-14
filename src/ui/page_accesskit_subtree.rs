#![cfg(feature = "browser_ui")]

use crate::accessibility::AccessibilityNode;
use crate::ui::messages::PageAccessKitSubtree;
use crate::ui::encode_page_node_id;
use crate::ui::TabId;

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
  document_generation: u32,
  node: &AccessibilityNode,
  classes: &mut accesskit::NodeClassSet,
  nodes_out: &mut Vec<(accesskit::NodeId, accesskit::Node)>,
  focus_out: &mut Option<accesskit::NodeId>,
) -> accesskit::NodeId {
  let id = encode_page_node_id(tab_id, document_generation, node.dom_node_id);

  if node.states.focused {
    *focus_out = Some(id);
  }

  let mut children_ids = Vec::with_capacity(node.children.len());
  for child in &node.children {
    let child_id = build_subtree_nodes(
      tab_id,
      document_generation,
      child,
      classes,
      nodes_out,
      focus_out,
    );
    children_ids.push(child_id);
  }

  let role = role_from_fastr_role(&node.role);
  let mut builder = accesskit::NodeBuilder::new(role);
  if role == accesskit::Role::Document {
    builder.add_action(accesskit::Action::Focus);
  }
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
pub fn accesskit_subtree_for_page(
  tab_id: TabId,
  document_generation: u32,
  root: &AccessibilityNode,
) -> PageAccessKitSubtree {
  let mut nodes: Vec<(accesskit::NodeId, accesskit::Node)> = Vec::new();
  let mut focus_id: Option<accesskit::NodeId> = None;
  let mut classes = accesskit::NodeClassSet::new();

  let root_id = build_subtree_nodes(
    tab_id,
    document_generation,
    root,
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
