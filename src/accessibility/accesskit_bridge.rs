#![cfg(feature = "a11y_accesskit")]

use crate::accessibility::AccessibilityNode;
use crate::dom2::RendererDomMapping;
use rustc_hash::FxHashSet;

use accesskit::{Node, NodeBuilder, NodeClassSet, NodeId, Role, Tree, TreeUpdate};

use super::accesskit_ids::{accesskit_id_for_dom2, accesskit_id_for_renderer_preorder};

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

  fn normalize_optional_text(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
  }

  fn role_for_accessibility(node: &AccessibilityNode) -> Role {
    // The exported accessibility roles are currently stringly-typed. Map the common ones we need
    // for chrome/content integration and fall back to a generic container role.
    match node.role.as_str() {
      "button" => Role::Button,
      // AccessKit's `Document` role is useful for standalone trees; `RootWebArea` is the common web
      // root role. Use `RootWebArea` so assistive technologies treat it like a web document.
      "document" => Role::RootWebArea,
      "link" => Role::Link,
      "heading" => Role::Heading,
      "checkbox" => Role::CheckBox,
      "radio" => Role::RadioButton,
      "img" | "image" => Role::Image,
      "list" => Role::List,
      "listitem" => Role::ListItem,
      "paragraph" => Role::Paragraph,
      "statictext" => Role::StaticText,
      // Text inputs.
      "textbox" | "textbox-multiline" | "combobox" => Role::TextField,
      "searchbox" => Role::SearchBox,
      _ => Role::GenericContainer,
    }
  }

  fn fallback_id_for_preorder(preorder: usize) -> NodeId {
    // Preorder ids are 1-based. Encode them into a dedicated namespace (renderer-preorder) so they
    // can safely coexist with dom2-derived ids and with UI-level wrapper/page ids.
    accesskit_id_for_renderer_preorder(preorder)
  }

  fn accesskit_id_for_node(
    node: &AccessibilityNode,
    mapping: Option<&RendererDomMapping>,
  ) -> NodeId {
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

    if let Some(name) = normalize_optional_text(node.name.as_deref()) {
      builder.set_name(name);
    }

    if let Some(role_description) = normalize_optional_text(node.role_description.as_deref()) {
      builder.set_role_description(role_description);
    }

    if let Some(description) = normalize_optional_text(node.description.as_deref()) {
      builder.set_description(description);
    }

    // For editable text controls, preserve empty-string values (screen readers expect to query the
    // current value even when empty). The JSON accessibility tree omits empty `value`s, so treat
    // `None` as empty for editable controls.
    if matches!(
      node.role.as_str(),
      "textbox" | "textbox-multiline" | "searchbox" | "combobox"
    ) {
      builder.set_value(node.value.clone().unwrap_or_default());
    } else if let Some(value) = normalize_optional_text(node.value.as_deref()) {
      builder.set_value(value);
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

#[cfg(test)]
mod tests {
  use super::*;
  use accesskit::Role;

  fn first_node_by_role<'a>(update: &'a TreeUpdate, role: Role) -> &'a accesskit::Node {
    update
      .nodes
      .iter()
      .find_map(|(_id, node)| (node.role() == role).then_some(node))
      .expect("missing node")
  }

  #[test]
  fn accesskit_bridge_exposes_text_input_value() {
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

    let update = tree_update_from_accessibility_tree(&tree, None);
    let node = first_node_by_role(&update, Role::TextField);
    assert_eq!(node.value(), Some("abc"));
  }

  #[test]
  fn accesskit_bridge_exposes_aria_describedby_as_description() {
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

    let update = tree_update_from_accessibility_tree(&tree, None);
    let node = first_node_by_role(&update, Role::TextField);
    assert_eq!(node.description(), Some("Helpful hint"));
  }
}
