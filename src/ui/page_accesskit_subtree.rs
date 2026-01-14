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
    "searchbox" => accesskit::Role::SearchBox,
    // AccessKit 0.11 does not have a dedicated combobox role; treat as a text field so screen
    // readers can still query/set the current value.
    "combobox" => accesskit::Role::TextField,
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
