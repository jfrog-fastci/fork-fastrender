#![cfg(feature = "browser_ui")]

use crate::accessibility::AccessibilityNode;
use crate::accessibility::accesskit_mapping::accesskit_role_for_fastr_role;
use crate::ui::messages::PageAccessKitSubtree;
use crate::ui::encode_page_node_id;
use crate::ui::TabId;

fn role_from_fastr_role(role: &str) -> accesskit::Role {
  // Map the renderer-exported role string vocabulary onto AccessKit roles.
  //
  // `crate::accessibility` primarily emits ARIA role tokens (validated via
  // `FASTRENDER_VALID_ARIA_ROLE_TOKENS`), plus `generic` as a fallback. Older call sites and tests
  // also use a small set of legacy role strings; keep those working explicitly.
  match role {
    "textbox-multiline" => accesskit::Role::TextField,
    "image" => accesskit::Role::Image,
    "statictext" => accesskit::Role::StaticText,
    other => accesskit_role_for_fastr_role(other),
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
  tree_generation: u32,
  node: &AccessibilityNode,
  classes: &mut accesskit::NodeClassSet,
  nodes_out: &mut Vec<(accesskit::NodeId, accesskit::Node)>,
  focus_out: &mut Option<accesskit::NodeId>,
) -> accesskit::NodeId {
  let id = encode_page_node_id(tab_id, tree_generation, node.dom_node_id);

  if node.states.focused {
    *focus_out = Some(id);
  }

  let mut children_ids = Vec::with_capacity(node.children.len());
  for child in &node.children {
    let child_id = build_subtree_nodes(
      tab_id,
      tree_generation,
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
  if let Some(role_description) = node.role_description.as_deref().and_then(normalize_name) {
    builder.set_role_description(role_description);
  }
  if let Some(description) = node.description.as_deref().and_then(normalize_name) {
    builder.set_description(description);
  }

  // For text inputs, preserve empty-string values (screen readers expect to query the current value
  // even when empty). The JSON accessibility tree omits empty `value`s, so treat `None` as empty for
  // editable controls.
  if matches!(
    node.role.as_str(),
    "textbox" | "textbox-multiline" | "searchbox" | "combobox"
  ) {
    builder.set_value(node.value.clone().unwrap_or_default());
  } else if let Some(value) = node.value.as_deref().and_then(normalize_name) {
    builder.set_value(value);
  }
  if !children_ids.is_empty() {
    builder.set_children(children_ids);
  }

  let built = builder.build(classes);
  nodes_out.push((id, built));
  id
}

/// Convert a renderer [`AccessibilityNode`] tree into an AccessKit subtree update suitable for
/// embedding into a windowed browser's overall accessibility tree.
pub fn accesskit_subtree_for_page(
  tab_id: TabId,
  tree_generation: u32,
  root: &AccessibilityNode,
) -> PageAccessKitSubtree {
  let mut nodes: Vec<(accesskit::NodeId, accesskit::Node)> = Vec::new();
  let mut focus_id: Option<accesskit::NodeId> = None;
  let mut classes = accesskit::NodeClassSet::new();

  let root_id = build_subtree_nodes(
    tab_id,
    tree_generation,
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

#[cfg(test)]
mod tests {
  use super::*;

  use accesskit::Role;

  fn first_node_by_role<'a>(
    subtree: &'a PageAccessKitSubtree,
    role: Role,
  ) -> &'a accesskit::Node {
    subtree
      .nodes
      .iter()
      .find_map(|(_id, node)| (node.role() == role).then_some(node))
      .expect("missing node")
  }

  #[test]
  fn accesskit_subtree_exposes_text_input_value() {
    let mut renderer = crate::FastRender::new().expect("renderer");
    let html = r##"
      <html>
        <body>
          <input value="abc" />
        </body>
      </html>
    "##;
    let dom = renderer.parse_html(html).expect("parse");
    let tree = renderer
      .accessibility_tree(&dom, 800, 600)
      .expect("accessibility tree");

    let subtree = accesskit_subtree_for_page(TabId(1), 1, &tree);
    let node = first_node_by_role(&subtree, Role::TextField);
    assert_eq!(node.value(), Some("abc"));
  }

  #[test]
  fn accesskit_subtree_exposes_aria_describedby_as_description() {
    let mut renderer = crate::FastRender::new().expect("renderer");
    let html = r##"
      <html>
        <body>
          <div id="d">Helpful hint</div>
          <input aria-describedby="d" />
        </body>
      </html>
    "##;
    let dom = renderer.parse_html(html).expect("parse");
    let tree = renderer
      .accessibility_tree(&dom, 800, 600)
      .expect("accessibility tree");

    let subtree = accesskit_subtree_for_page(TabId(1), 1, &tree);
    let node = first_node_by_role(&subtree, Role::TextField);
    assert_eq!(node.description(), Some("Helpful hint"));
  }
}
