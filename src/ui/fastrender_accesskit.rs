#![cfg(feature = "browser_ui")]

use crate::accessibility::AccessibilityNode;
use crate::dom::DomNode;
use crate::interaction::InteractionState;
use crate::ui::encode_page_node_id;
use crate::{FastRender, Result};
use accesskit::{NodeBuilder, NodeId, Role, Tree, TreeUpdate};

fn accesskit_role_from_fastrender(role: &str) -> Role {
  match role {
    "document" => Role::Document,
    "textbox" => Role::TextField,
    "searchbox" => Role::SearchBox,
    // AccessKit 0.11 does not have a dedicated combobox role; treat editable comboboxes as text
    // inputs until we add a richer mapping.
    "combobox" => Role::TextField,
    "button" => Role::Button,
    "link" => Role::Link,
    "checkbox" => Role::CheckBox,
    "radio" => Role::RadioButton,
    // Fallback for roles we haven't mapped yet.
    _ => Role::GenericContainer,
  }
}

fn build_accesskit_node_recursive(
  tab_id: crate::ui::messages::TabId,
  tree_generation: u32,
  node: &AccessibilityNode,
  interaction_state: Option<&InteractionState>,
  classes: &mut accesskit::NodeClassSet,
  out: &mut Vec<(NodeId, accesskit::Node)>,
) -> NodeId {
  let id = encode_page_node_id(tab_id, tree_generation, node.dom_node_id);

  let mut child_ids = Vec::with_capacity(node.children.len());
  for child in &node.children {
    child_ids.push(build_accesskit_node_recursive(
      tab_id,
      tree_generation,
      child,
      interaction_state,
      classes,
      out,
    ));
  }

  let role = accesskit_role_from_fastrender(&node.role);
  let mut builder = NodeBuilder::new(role);

  if let Some(name) = node.name.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
    builder.set_name(name.to_string());
  }
  if let Some(role_desc) = node
    .role_description
    .as_deref()
    .map(str::trim)
    .filter(|s| !s.is_empty())
  {
    builder.set_role_description(role_desc.to_string());
  }
  if let Some(desc) = node
    .description
    .as_deref()
    .map(str::trim)
    .filter(|s| !s.is_empty())
  {
    builder.set_description(desc.to_string());
  }

  // For text inputs, we want to preserve empty-string values (screen readers expect to query the
  // current value even when empty). The JSON accessibility tree omits empty `value`s, so treat that
  // as an empty string for editable controls.
  if matches!(node.role.as_str(), "textbox" | "searchbox" | "combobox") {
    builder.set_value(node.value.clone().unwrap_or_default());
  } else if let Some(value) = node.value.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
    builder.set_value(value.to_string());
  }

  builder.set_children(child_ids);

  // Expose caret/selection state for focused text controls.
  if matches!(node.role.as_str(), "textbox" | "searchbox" | "combobox") {
    if let Some(state) = interaction_state {
      if let Some(edit) = state.text_edit_for(node.dom_node_id) {
        let text_len = node
          .value
          .as_deref()
          .unwrap_or("")
          .chars()
          .count();
        let caret = edit.caret.min(text_len);
        let (sel_start, sel_end) = edit.selection.unwrap_or((caret, caret));
        let sel_start = sel_start.min(text_len);
        let sel_end = sel_end.min(text_len);

        let anchor = accesskit::TextPosition {
          node: id,
          character_index: sel_start,
        };
        let focus = accesskit::TextPosition {
          node: id,
          character_index: sel_end,
        };
        builder.set_text_selection(accesskit::TextSelection { anchor, focus });
      }
    }
  }

  out.push((id, builder.build(classes)));
  id
}

/// Build an AccessKit tree update for a DOM document.
///
/// This bridges FastRender's accessibility tree (roles/names/states) into an AccessKit `TreeUpdate`
/// suitable for windowed UI integration.
pub fn build_accesskit_tree_update_for_dom(
  tab_id: crate::ui::messages::TabId,
  tree_generation: u32,
  renderer: &mut FastRender,
  dom: &DomNode,
  width: u32,
  height: u32,
  interaction_state: Option<&InteractionState>,
) -> Result<TreeUpdate> {
  let tree = renderer.accessibility_tree_with_interaction_state(dom, width, height, interaction_state)?;
  Ok(accesskit_tree_update_from_accessibility_tree(
    tab_id,
    tree_generation,
    &tree,
    interaction_state,
  ))
}

/// Convert a FastRender accessibility tree into an AccessKit tree update.
pub fn accesskit_tree_update_from_accessibility_tree(
  tab_id: crate::ui::messages::TabId,
  tree_generation: u32,
  tree: &AccessibilityNode,
  interaction_state: Option<&InteractionState>,
) -> TreeUpdate {
  let mut classes = accesskit::NodeClassSet::default();
  let mut nodes = Vec::new();
  let root_id = build_accesskit_node_recursive(
    tab_id,
    tree_generation,
    tree,
    interaction_state,
    &mut classes,
    &mut nodes,
  );

  let focus = interaction_state
    .and_then(|state| state.focused)
    .map(|id| encode_page_node_id(tab_id, tree_generation, id));

  TreeUpdate {
    nodes,
    tree: Some(Tree::new(root_id)),
    focus,
  }
}

#[cfg(all(test, feature = "browser_ui"))]
mod tests {
  use super::*;
  use crate::dom::{enumerate_dom_ids, DomNodeType};
  use crate::interaction::InteractionEngine;

  fn first_dom_node_id_by_tag(dom: &DomNode, tag_name: &str) -> usize {
    let ids = enumerate_dom_ids(dom);
    let mut stack: Vec<&DomNode> = vec![dom];
    while let Some(node) = stack.pop() {
      if matches!(node.node_type, DomNodeType::Element { .. } | DomNodeType::Slot { .. }) {
        if node
          .tag_name()
          .is_some_and(|tag| tag.eq_ignore_ascii_case(tag_name))
        {
          return *ids
            .get(&(node as *const DomNode))
            .unwrap_or_else(|| panic!("missing node id for <{tag_name}>"));
        }
      }
      for child in node.children.iter().rev() {
        stack.push(child);
      }
    }
    panic!("missing element <{tag_name}>");
  }

  fn node_from_update(update: &TreeUpdate, id: NodeId) -> &accesskit::Node {
    update
      .nodes
      .iter()
      .find_map(|(node_id, node)| (*node_id == id).then_some(node))
      .unwrap_or_else(|| panic!("missing accesskit node {id:?}"))
  }

  #[test]
  fn accesskit_textbox_exposes_caret_and_selection() {
    let mut renderer = FastRender::new().expect("renderer");
    let html = r##"
      <html>
        <body>
          <input value="abc" />
        </body>
      </html>
    "##;
    let mut dom = renderer.parse_html(html).expect("parse");

    let input_id = first_dom_node_id_by_tag(&dom, "input");

    let mut engine = InteractionEngine::new();
    let _ = engine.focus_node_id(&mut dom, Some(input_id), true);
    engine.set_text_selection_range(input_id, 1, 3);

    let update = build_accesskit_tree_update_for_dom(
      crate::ui::messages::TabId(1),
      1,
      &mut renderer,
      &dom,
      800,
      600,
      Some(engine.interaction_state()),
    )
    .expect("accesskit tree");

    let node_id = encode_page_node_id(crate::ui::messages::TabId(1), 1, input_id);
    let node = node_from_update(&update, node_id);

    assert_eq!(node.role(), Role::TextField);
    assert_eq!(node.value(), Some("abc"));
    assert_eq!(update.focus, Some(node_id));

    let sel = node.text_selection().expect("text selection");
    assert_eq!(sel.anchor.node, node_id);
    assert_eq!(sel.focus.node, node_id);
    assert_eq!(sel.anchor.character_index, 1);
    assert_eq!(sel.focus.character_index, 3);
  }

  #[test]
  fn accesskit_textbox_exposes_collapsed_selection_as_caret() {
    let mut renderer = FastRender::new().expect("renderer");
    let html = r##"
      <html>
        <body>
          <input value="abc" />
        </body>
      </html>
    "##;
    let mut dom = renderer.parse_html(html).expect("parse");

    let input_id = first_dom_node_id_by_tag(&dom, "input");

    let mut engine = InteractionEngine::new();
    let _ = engine.focus_node_id(&mut dom, Some(input_id), true);
    engine.set_text_selection_caret(input_id, 2);

    let update = build_accesskit_tree_update_for_dom(
      crate::ui::messages::TabId(1),
      1,
      &mut renderer,
      &dom,
      800,
      600,
      Some(engine.interaction_state()),
    )
    .expect("accesskit tree");

    let node_id = encode_page_node_id(crate::ui::messages::TabId(1), 1, input_id);
    let node = node_from_update(&update, node_id);

    let sel = node.text_selection().expect("text selection");
    assert_eq!(sel.anchor.character_index, 2);
    assert_eq!(sel.focus.character_index, 2);
  }
}
