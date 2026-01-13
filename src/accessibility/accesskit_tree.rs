//! FastRender → AccessKit tree builder.
//!
//! FastRender's internal accessibility tree (`crate::accessibility::AccessibilityNode`) uses a
//! `role="document"` root to represent the rendered document.
//!
//! Desktop accessibility APIs (via AccessKit) typically expect a platform-level root node that
//! represents the native window/application, with the document subtree attached beneath it. This
//! module adds that synthetic root so the returned `TreeUpdate` has a `Role::Window` root whose
//! direct child is the FastRender document node (`Role::Document`).
//!
//! # NodeId encoding
//!
//! AccessKit `NodeId`s must not collide when multiple independently generated trees are merged
//! (e.g. browser chrome + page content). This module uses FastRender's marker+namespace encoding
//! (`accessibility::accesskit_ids`) so wrapper nodes and DOM-backed nodes live in disjoint spaces.

#![cfg(feature = "browser_ui")]

use crate::accessibility::AccessibilityNode;
use crate::Transform2D;
use crate::accessibility::accesskit_mapping::accesskit_role_for_fastr_role;

use accesskit::{Node, NodeBuilder, NodeClassSet, NodeId, Rect, Role, Tree, TreeUpdate};

use super::accesskit_ids::{
  accesskit_id_for_chrome_dom_preorder, accesskit_id_for_chrome_wrapper,
  accesskit_id_for_page_dom_preorder, accesskit_id_for_renderer_preorder, ChromeWrapperNode,
};

fn normalize_optional_name(raw: Option<&str>) -> Option<String> {
  raw
    .map(str::trim)
    .filter(|s| !s.is_empty())
    .map(|s| s.to_string())
}

fn build_subtree_nodes_with_ids(
  node: &AccessibilityNode,
  id_for_node: &impl Fn(&AccessibilityNode) -> NodeId,
  default_bounds: Rect,
  classes: &mut NodeClassSet,
  out: &mut Vec<(NodeId, Node)>,
) -> Option<NodeId> {
  let node_id = id_for_node(node);
  let mut child_ids: Vec<NodeId> = Vec::with_capacity(node.children.len());
  let mut focus = node.states.focused.then_some(node_id);

  for child in &node.children {
    child_ids.push(id_for_node(child));
    let child_focus = build_subtree_nodes_with_ids(child, id_for_node, default_bounds, classes, out);
    if focus.is_none() {
      focus = child_focus;
    }
  }

  let role = accesskit_role_for_fastr_role(&node.role);
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
  if matches!(
    node.role.as_str(),
    "textbox" | "textbox-multiline" | "searchbox" | "combobox"
  ) {
    builder.set_value(node.value.clone().unwrap_or_default());
  } else if let Some(value) = normalize_optional_name(node.value.as_deref()) {
    builder.set_value(value);
  }

  // We currently do not have per-node bounds available in the exported `AccessibilityNode` tree.
  // Provide a conservative default so screen readers still have something reasonable to anchor to.
  builder.set_bounds(default_bounds);
  if !child_ids.is_empty() {
    builder.set_children(child_ids);
  }

  out.push((node_id, builder.build(classes)));
  focus
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
  // Use a stable wrapper id for the platform/window root.
  let window_id = accesskit_id_for_chrome_wrapper(ChromeWrapperNode::Window);

  let mut nodes: Vec<(NodeId, Node)> = Vec::new();
  let mut classes = NodeClassSet::new();

  // Build the document subtree (including the document root). Use preorder-derived ids in a
  // dedicated namespace so they never collide with wrapper ids.
  let id_for_node = |node: &AccessibilityNode| accesskit_id_for_renderer_preorder(node.dom_node_id);
  let document_id = id_for_node(document);
  let focus = build_subtree_nodes_with_ids(
    document,
    &id_for_node,
    window_bounds,
    &mut classes,
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
    focus,
  }
}

/// Build a composited AccessKit [`TreeUpdate`] for a browser window.
///
/// The returned tree has a synthetic `Role::Window` root node whose children are:
/// - a chrome region wrapper (`Role::Group`)
/// - a content region wrapper (`Role::WebView`)
///
/// The chrome/content subtrees are assigned distinct FastRender AccessKit namespaces (see
/// [`accessibility::accesskit_ids`]) so their node IDs can never collide when merged.
///
/// `*_bounds_transform` are reserved for the future when FastRender exports per-node bounds; they
/// will be used to map subtree-local bounds into window coordinates.
#[allow(clippy::needless_pass_by_value)]
pub fn build_window_tree_update(
  chrome_a11y_root: &AccessibilityNode,
  chrome_bounds_transform: Transform2D,
  content_a11y_root: &AccessibilityNode,
  content_bounds_transform: Transform2D,
  content_tab_id: u64,
  content_document_generation: u32,
  window_title: Option<&str>,
  window_bounds: Rect,
) -> TreeUpdate {
  let _ = chrome_bounds_transform;
  let _ = content_bounds_transform;

  // Keep the top-level wrapper IDs stable so the AccessKit adapter sees a consistent tree.
  let window_id = accesskit_id_for_chrome_wrapper(ChromeWrapperNode::Window);
  let chrome_region_id = accesskit_id_for_chrome_wrapper(ChromeWrapperNode::Chrome);
  let content_region_id = accesskit_id_for_chrome_wrapper(ChromeWrapperNode::Page);

  let mut nodes: Vec<(NodeId, Node)> = Vec::new();
  let mut classes = NodeClassSet::new();

  let chrome_id_for_node =
    |node: &AccessibilityNode| accesskit_id_for_chrome_dom_preorder(node.dom_node_id);
  let chrome_root_id = chrome_id_for_node(chrome_a11y_root);
  let chrome_focus = build_subtree_nodes_with_ids(
    chrome_a11y_root,
    &chrome_id_for_node,
    window_bounds,
    &mut classes,
    &mut nodes,
  );

  let content_id_for_node = |node: &AccessibilityNode| {
    accesskit_id_for_page_dom_preorder(
      content_tab_id,
      content_document_generation,
      node.dom_node_id,
    )
  };
  let content_root_id = content_id_for_node(content_a11y_root);
  let content_focus = build_subtree_nodes_with_ids(
    content_a11y_root,
    &content_id_for_node,
    window_bounds,
    &mut classes,
    &mut nodes,
  );

  // Prefer chrome focus if both subtrees report focus (chrome is "active" whenever the user is
  // interacting with browser UI controls).
  let focus = chrome_focus.or(content_focus);

  // Chrome wrapper.
  let mut chrome_builder = NodeBuilder::new(Role::Group);
  chrome_builder.set_bounds(window_bounds);
  chrome_builder.set_children(vec![chrome_root_id]);
  nodes.push((chrome_region_id, chrome_builder.build(&mut classes)));

  // Content wrapper (WebView). Preserve the document title from the subtree root when available.
  let mut content_builder = NodeBuilder::new(Role::WebView);
  if let Some(name) = normalize_optional_name(content_a11y_root.name.as_deref()) {
    content_builder.set_name(name);
  }
  content_builder.set_bounds(window_bounds);
  content_builder.set_children(vec![content_root_id]);
  nodes.push((content_region_id, content_builder.build(&mut classes)));

  // Window root.
  let mut window_builder = NodeBuilder::new(Role::Window);
  let window_name =
    normalize_optional_name(window_title).unwrap_or_else(|| "FastRender".to_string());
  window_builder.set_name(window_name);
  window_builder.set_bounds(window_bounds);
  window_builder.set_children(vec![chrome_region_id, content_region_id]);
  nodes.push((window_id, window_builder.build(&mut classes)));

  TreeUpdate {
    nodes,
    tree: Some(Tree::new(window_id)),
    focus,
  }
}

#[cfg(all(test, feature = "browser_ui"))]
mod tests {
  use super::*;
  use crate::{FastRender, FontConfig, RenderOptions};
  use std::collections::HashSet;

  fn find_node<'a>(update: &'a TreeUpdate, id: NodeId) -> &'a Node {
    update
      .nodes
      .iter()
      .find_map(|(node_id, node)| (*node_id == id).then_some(node))
      .expect("node must exist in TreeUpdate")
  }

  fn node_name(node: &Node) -> &str {
    node.name().unwrap_or("").trim()
  }

  fn contains_name(update: &TreeUpdate, expected: &str) -> bool {
    update
      .nodes
      .iter()
      .any(|(_id, node)| node_name(node) == expected)
  }

  fn clear_focus(node: &mut AccessibilityNode) {
    node.states.focused = false;
    for child in &mut node.children {
      clear_focus(child);
    }
  }

  fn set_focus_by_id(node: &mut AccessibilityNode, id: &str) -> bool {
    if node.id.as_deref() == Some(id) {
      node.states.focused = true;
      return true;
    }
    for child in &mut node.children {
      if set_focus_by_id(child, id) {
        return true;
      }
    }
    false
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
      dom_node_id: 1,
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

  #[test]
  fn composited_window_tree_contains_both_subtrees_and_unique_ids() {
    let mut renderer = FastRender::builder()
      .font_sources(FontConfig::bundled_only())
      .build()
      .expect("build renderer");

    let options = RenderOptions::new().with_viewport(200, 120);
    let window_bounds = Rect {
      x0: 0.0,
      y0: 0.0,
      x1: 200.0,
      y1: 120.0,
    };

    let chrome_html = r#"<!doctype html>
      <html>
        <body>
          <button id="chrome-btn">Chrome</button>
        </body>
      </html>"#;

    let content_html = r#"<!doctype html>
      <html>
        <body>
          <button id="content-btn">Content</button>
        </body>
      </html>"#;

    let chrome = renderer
      .accessibility_tree_html(chrome_html, options.clone())
      .expect("chrome a11y tree");
    let content = renderer
      .accessibility_tree_html(content_html, options)
      .expect("content a11y tree");

    let update = build_window_tree_update(
      &chrome,
      Transform2D::IDENTITY,
      &content,
      Transform2D::translate(0.0, 40.0),
      1,
      1,
      Some("Window title"),
      window_bounds,
    );

    assert!(
      update.tree.is_some(),
      "expected TreeUpdate.tree to be Some(..)"
    );
    assert!(contains_name(&update, "Chrome"));
    assert!(contains_name(&update, "Content"));

    let mut ids: HashSet<u128> = HashSet::new();
    for (id, _node) in &update.nodes {
      assert!(
        ids.insert(id.0.get()),
        "duplicate NodeId detected: {}",
        id.0.get()
      );
    }
  }

  #[test]
  fn composited_window_tree_focus_can_come_from_chrome_or_content() {
    let mut renderer = FastRender::builder()
      .font_sources(FontConfig::bundled_only())
      .build()
      .expect("build renderer");

    let options = RenderOptions::new().with_viewport(200, 120);
    let window_bounds = Rect {
      x0: 0.0,
      y0: 0.0,
      x1: 200.0,
      y1: 120.0,
    };

    let chrome_html = r#"<!doctype html>
      <html>
        <body>
          <button id="chrome-btn">Chrome</button>
        </body>
      </html>"#;
    let content_html = r#"<!doctype html>
      <html>
        <body>
          <button id="content-btn">Content</button>
        </body>
      </html>"#;

    let chrome = renderer
      .accessibility_tree_html(chrome_html, options.clone())
      .expect("chrome a11y tree");
    let content = renderer
      .accessibility_tree_html(content_html, options)
      .expect("content a11y tree");

    // Chrome focus.
    let mut chrome_focus = chrome.clone();
    let mut content_focus = content.clone();
    clear_focus(&mut chrome_focus);
    clear_focus(&mut content_focus);
    assert!(set_focus_by_id(&mut chrome_focus, "chrome-btn"));

    let update = build_window_tree_update(
      &chrome_focus,
      Transform2D::IDENTITY,
      &content_focus,
      Transform2D::IDENTITY,
      1,
      1,
      None,
      window_bounds,
    );
    let focus = update
      .focus
      .expect("expected focus for chrome-focused tree");
    let node = find_node(&update, focus);
    assert_eq!(node_name(node), "Chrome");

    // Content focus.
    let mut chrome_focus = chrome.clone();
    let mut content_focus = content.clone();
    clear_focus(&mut chrome_focus);
    clear_focus(&mut content_focus);
    assert!(set_focus_by_id(&mut content_focus, "content-btn"));

    let update = build_window_tree_update(
      &chrome_focus,
      Transform2D::IDENTITY,
      &content_focus,
      Transform2D::IDENTITY,
      1,
      1,
      None,
      window_bounds,
    );
    let focus = update
      .focus
      .expect("expected focus for content-focused tree");
    let node = find_node(&update, focus);
    assert_eq!(node_name(node), "Content");
  }
}
