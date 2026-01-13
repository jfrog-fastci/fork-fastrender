//! FastRender → AccessKit tree builder.
//!
//! FastRender's internal accessibility tree (`crate::accessibility::AccessibilityNode`) uses a
//! `role="document"` root to represent the rendered document.
//!
//! Desktop accessibility APIs (via AccessKit) typically expect a platform-level root node that
//! represents the native window/application, with the document subtree attached beneath it. This
//! module adds that synthetic root so the returned `TreeUpdate` has a `Role::Window` root whose
//! direct child is the FastRender document node (`Role::Document`).

#![cfg(feature = "browser_ui")]

use crate::accessibility::AccessibilityNode;

use accesskit::{Node, NodeBuilder, NodeClassSet, NodeId, Rect, Role, Tree, TreeUpdate};
use std::num::NonZeroU128;

fn node_id_from_u128(raw: u128) -> NodeId {
  // AccessKit requires non-zero node IDs.
  NodeId(NonZeroU128::new(raw).expect("node id must be non-zero"))
}

fn normalize_optional_name(raw: Option<&str>) -> Option<String> {
  raw
    .map(str::trim)
    .filter(|s| !s.is_empty())
    .map(|s| s.to_string())
}

fn role_from_fastrender(role: &str) -> Role {
  match role {
    // Document root.
    "document" => Role::Document,
    // Common interactive/content roles.
    "button" => Role::Button,
    "checkbox" => Role::CheckBox,
    "radio" => Role::RadioButton,
    // AccessKit uses "TextField" for text input controls.
    "textbox" => Role::TextField,
    "searchbox" => Role::SearchBox,
    "link" => Role::Link,
    "img" | "image" => Role::Image,
    "heading" => Role::Heading,
    "list" => Role::List,
    "listitem" => Role::ListItem,
    "table" => Role::Table,
    "row" => Role::Row,
    "cell" => Role::Cell,
    "columnheader" => Role::ColumnHeader,
    "rowheader" => Role::RowHeader,
    "separator" => Role::GenericContainer,
    "progressbar" => Role::ProgressIndicator,
    "slider" => Role::Slider,
    // AccessKit 0.11 does not have a dedicated combobox role; treat comboboxes as text fields for
    // now so screen readers can still query their value.
    "combobox" => Role::TextField,
    "menu" => Role::Menu,
    "menuitem" => Role::MenuItem,
    "tab" => Role::Tab,
    "tabpanel" => Role::TabPanel,
    "banner" => Role::Banner,
    "navigation" => Role::Navigation,
    "main" => Role::Main,
    "contentinfo" => Role::ContentInfo,
    "form" => Role::Form,
    "region" => Role::Region,
    "alert" => Role::Alert,
    // Fallback: keep the tree shape, but mark as a generic container.
    _ => Role::GenericContainer,
  }
}

fn build_subtree_nodes(
  node: &AccessibilityNode,
  node_id: NodeId,
  default_bounds: Rect,
  classes: &mut NodeClassSet,
  next_id: &mut u128,
  out: &mut Vec<(NodeId, Node)>,
) {
  let mut child_ids: Vec<NodeId> = Vec::with_capacity(node.children.len());
  for child in &node.children {
    let id = node_id_from_u128(*next_id);
    *next_id = next_id.saturating_add(1);
    child_ids.push(id);
    build_subtree_nodes(child, id, default_bounds, classes, next_id, out);
  }

  let role = role_from_fastrender(&node.role);
  let mut builder = NodeBuilder::new(role);

  if let Some(name) = normalize_optional_name(node.name.as_deref()) {
    builder.set_name(name);
  }

  if let Some(role_description) = normalize_optional_name(node.role_description.as_deref()) {
    builder.set_role_description(role_description);
  }

  if let Some(desc) = normalize_optional_name(node.description.as_deref()) {
    builder.set_description(desc);
  }

  // For text inputs, preserve empty-string values (screen readers expect to query current value even
  // when empty). The JSON accessibility tree omits empty `value`s, so treat `None` as empty for
  // editable controls.
  if matches!(node.role.as_str(), "textbox" | "searchbox" | "combobox") {
    builder.set_value(node.value.clone().unwrap_or_default());
  } else if let Some(value) = normalize_optional_name(node.value.as_deref()) {
    builder.set_value(value);
  }

  // We currently do not have per-node bounds available in the exported `AccessibilityNode` tree.
  // Provide a conservative default so screen readers still have something reasonable to anchor to.
  builder.set_bounds(default_bounds);
  builder.set_children(child_ids);

  out.push((node_id, builder.build(classes)));
}

/// Build an AccessKit [`TreeUpdate`] for a FastRender document.
///
/// The returned tree has a synthetic `Role::Window` root node that contains the FastRender document
/// node (`Role::Document`) as its direct child.
///
/// This synthetic root is also the intended attachment point for future composition (e.g. browser
/// chrome UI accessibility tree + document accessibility tree).
pub fn build_accesskit_tree_update(
  document: &AccessibilityNode,
  window_title: Option<&str>,
  window_bounds: Rect,
) -> TreeUpdate {
  // Reserve stable top-level IDs so the caller can add additional sibling subtrees later
  // (e.g. chrome + content).
  let window_id = node_id_from_u128(1);
  let document_id = node_id_from_u128(2);

  let mut nodes: Vec<(NodeId, Node)> = Vec::new();
  let mut classes = NodeClassSet::new();

  // Build the document subtree (including the document root).
  let mut next_id = 3u128;
  build_subtree_nodes(
    document,
    document_id,
    window_bounds,
    &mut classes,
    &mut next_id,
    &mut nodes,
  );

  // Build the synthetic window root.
  let mut window_builder = NodeBuilder::new(Role::Window);
  let window_name =
    normalize_optional_name(window_title).unwrap_or_else(|| "FastRender".to_string());
  window_builder.set_name(window_name);
  window_builder.set_bounds(window_bounds);
  window_builder.set_children(vec![document_id]);
  nodes.push((window_id, window_builder.build(&mut classes)));

  TreeUpdate {
    nodes,
    tree: Some(Tree::new(window_id)),
    focus: None,
  }
}

#[cfg(all(test, feature = "browser_ui"))]
mod tests {
  use super::*;

  fn find_node<'a>(update: &'a TreeUpdate, id: NodeId) -> &'a Node {
    update
      .nodes
      .iter()
      .find_map(|(node_id, node)| (*node_id == id).then_some(node))
      .expect("node must exist in TreeUpdate")
  }

  #[test]
  fn accesskit_root_is_window_or_application_and_contains_document_child() {
    let doc = AccessibilityNode {
      node_id: 1,
      role: "document".to_string(),
      role_description: None,
      name: Some("Document title".to_string()),
      description: None,
      value: None,
      level: None,
      html_tag: Some("document".to_string()),
      id: None,
      relations: None,
      states: crate::accessibility::AccessibilityState::default(),
      children: Vec::new(),
      #[cfg(any(debug_assertions, feature = "a11y_debug"))]
      debug: None,
    };

    let bounds = Rect {
      x0: 0.0,
      y0: 0.0,
      x1: 800.0,
      y1: 600.0,
    };

    let update = build_accesskit_tree_update(&doc, Some("Window title"), bounds);

    let tree = update.tree.as_ref().expect("tree must be present");
    let root_id = tree.root;
    let root_node = find_node(&update, root_id);
    assert!(
      matches!(root_node.role(), Role::Window | Role::Application),
      "expected AccessKit root role Window/Application, got {:?}",
      root_node.role()
    );

    let root_children = root_node.children();
    assert_eq!(
      root_children.len(),
      1,
      "synthetic window root should have exactly one child (the document)"
    );
    let document_id = root_children[0];

    let document_node = find_node(&update, document_id);
    assert_eq!(document_node.role(), Role::Document);
  }

  #[test]
  fn accesskit_node_exposes_text_input_value() {
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

    let update = build_accesskit_tree_update(
      &tree,
      Some("Window title"),
      Rect {
        x0: 0.0,
        y0: 0.0,
        x1: 800.0,
        y1: 600.0,
      },
    );

    let text_fields: Vec<&Node> = update
      .nodes
      .iter()
      .filter_map(|(_id, node)| (node.role() == Role::TextField).then_some(node))
      .collect();

    assert_eq!(
      text_fields.len(),
      1,
      "expected exactly one text field node, got {}",
      text_fields.len()
    );
    assert_eq!(text_fields[0].value(), Some("abc"));
  }

  #[test]
  fn accesskit_node_exposes_aria_describedby_as_description() {
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

    let update = build_accesskit_tree_update(
      &tree,
      Some("Window title"),
      Rect {
        x0: 0.0,
        y0: 0.0,
        x1: 800.0,
        y1: 600.0,
      },
    );

    let text_fields: Vec<&Node> = update
      .nodes
      .iter()
      .filter_map(|(_id, node)| (node.role() == Role::TextField).then_some(node))
      .collect();

    assert_eq!(
      text_fields.len(),
      1,
      "expected exactly one text field node, got {}",
      text_fields.len()
    );
    assert_eq!(text_fields[0].description(), Some("Helpful hint"));
  }
}
