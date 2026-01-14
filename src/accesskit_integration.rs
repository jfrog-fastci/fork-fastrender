#![cfg(feature = "browser_ui")]

use crate::api::PreparedDocument;
use crate::dom::{DomNode, DomNodeType, HTML_NAMESPACE};
use crate::geometry::{Point, Rect};
use crate::scroll::ScrollState;
use crate::tree::box_tree::BoxTree;
use crate::tree::fragment_tree::{FragmentNode, FragmentTree};
use crate::ui::messages::TabId;
use crate::ui::encode_page_node_id;
use accesskit::{Node, NodeBuilder, NodeClassSet, NodeId, Role, Tree, TreeUpdate};
use std::collections::HashMap;

/// Transforms FastRender page-coordinate bounds into AccessKit's root coordinate space.
///
/// ## Coordinate spaces
/// - FastRender layout/geometry data (`FragmentTree`) is expressed in **page coordinates** in CSS
///   pixels: the origin is the top-left of the document (unscrolled).
/// - Element scroll offsets (`scrollLeft`/`scrollTop`) are applied to the fragment tree before
///   extracting bounds (see [`PreparedDocument::fragment_tree_for_geometry`]).
/// - Viewport scroll (`scroll_state.viewport`) is *not* applied to the fragment tree (by design;
///   this matches the hit-testing convention). We therefore include `-scroll_state.viewport` in the
///   transform offset so node bounds describe **visible on-screen positions**.
///
/// The resulting AccessKit bounds are in a window-local coordinate space (logical pixels) suitable
/// for passing to `accesskit_winit` (which will further translate into global screen coordinates).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AccessKitBoundsTransform {
  /// Scale factor from CSS pixels to AccessKit logical pixels.
  ///
  /// For the windowed browser UI this is typically 1.0 because CSS px already correspond to winit
  /// logical pixels, but callers can supply a different value when embedding the renderer in a
  /// higher-DPI coordinate space.
  pub scale: f32,
  /// Translation (in CSS pixels) applied before scaling.
  pub offset: Point,
}

impl AccessKitBoundsTransform {
  /// Construct a transform for a document rendered at `document_offset` within the window.
  ///
  /// `document_offset` is the position of the document viewport's (0,0) within the window's local
  /// coordinate space.
  pub fn new(scroll_state: &ScrollState, document_offset: Point, scale: f32) -> Self {
    // Page → viewport-local: subtract the viewport scroll position.
    //
    // Viewport-local → window-local: add the document's offset within the window.
    let offset = Point::new(
      document_offset.x - scroll_state.viewport.x,
      document_offset.y - scroll_state.viewport.y,
    );
    Self { scale, offset }
  }

  /// Apply the transform to a FastRender rect (page coords, CSS px), producing an AccessKit rect.
  pub fn transform_rect(self, rect_page_css: Rect) -> Option<accesskit::Rect> {
    if rect_page_css == Rect::ZERO {
      return None;
    }

    // Translate first (still CSS px).
    let translated = Rect::from_xywh(
      rect_page_css.x() + self.offset.x,
      rect_page_css.y() + self.offset.y,
      rect_page_css.width(),
      rect_page_css.height(),
    );

    let scale = if self.scale.is_finite() { self.scale } else { 1.0 };
    let x0 = translated.min_x() * scale;
    let y0 = translated.min_y() * scale;
    let x1 = translated.max_x() * scale;
    let y1 = translated.max_y() * scale;

    if !(x0.is_finite() && y0.is_finite() && x1.is_finite() && y1.is_finite()) {
      return None;
    }

    // AccessKit rects are expressed as (min_x, min_y, max_x, max_y) in f64.
    Some(accesskit::Rect::new(
      x0 as f64,
      y0 as f64,
      x1 as f64,
      y1 as f64,
    ))
  }
}

fn collect_box_bounds(tree: &FragmentTree) -> HashMap<usize, Rect> {
  struct Frame<'a> {
    node: &'a FragmentNode,
    parent_offset: Point,
  }

  let mut out: HashMap<usize, Rect> = HashMap::new();
  let mut stack: Vec<Frame<'_>> = Vec::new();

  for root in tree.additional_fragments.iter().rev() {
    stack.push(Frame {
      node: root,
      parent_offset: Point::ZERO,
    });
  }
  stack.push(Frame {
    node: &tree.root,
    parent_offset: Point::ZERO,
  });

  while let Some(frame) = stack.pop() {
    let abs = frame.node.bounds.translate(frame.parent_offset);
    if let Some(box_id) = frame.node.box_id() {
      out
        .entry(box_id)
        .and_modify(|existing| *existing = existing.union(abs))
        .or_insert(abs);
    }

    let child_parent_offset = abs.origin;
    for child in frame.node.children.iter().rev() {
      stack.push(Frame {
        node: child,
        parent_offset: child_parent_offset,
      });
    }
  }

  out
}

fn collect_bounds_by_styled_node_id(
  box_tree: &BoxTree,
  box_bounds: &HashMap<usize, Rect>,
) -> HashMap<usize, Rect> {
  let mut out: HashMap<usize, Rect> = HashMap::new();

  let mut stack: Vec<&crate::tree::box_tree::BoxNode> = vec![&box_tree.root];
  while let Some(node) = stack.pop() {
    if node.generated_pseudo.is_none() {
      if let Some(styled_id) = node.styled_node_id {
        if let Some(bounds) = box_bounds.get(&node.id).copied() {
          out
            .entry(styled_id)
            .and_modify(|existing| *existing = existing.union(bounds))
            .or_insert(bounds);
        }
      }
    }

    if let Some(body) = node.footnote_body.as_deref() {
      stack.push(body);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }

  out
}

fn normalized_text_content(node: &DomNode) -> String {
  let mut raw = String::new();
  let mut stack: Vec<&DomNode> = vec![node];
  while let Some(node) = stack.pop() {
    if let DomNodeType::Text { content } = &node.node_type {
      raw.push_str(content);
      raw.push(' ');
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }

  raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn role_for_dom_node(node: &DomNode) -> Role {
  match &node.node_type {
    DomNodeType::Document { .. } => Role::Document,
    DomNodeType::ShadowRoot { .. } => Role::Document,
    DomNodeType::Slot { .. } => Role::GenericContainer,
    DomNodeType::Element { tag_name, namespace, .. } => {
      let is_html = namespace.is_empty() || namespace == HTML_NAMESPACE;
      if is_html && tag_name.eq_ignore_ascii_case("button") {
        Role::Button
      } else if is_html && tag_name.eq_ignore_ascii_case("a") {
        Role::Link
      } else {
        Role::GenericContainer
      }
    }
    DomNodeType::Text { .. } => Role::StaticText,
  }
}

fn include_dom_node_in_accesskit_tree(node: &DomNode) -> bool {
  matches!(
    node.node_type,
    DomNodeType::Document { .. }
      | DomNodeType::ShadowRoot { .. }
      | DomNodeType::Slot { .. }
      | DomNodeType::Element { .. }
  )
}

fn accessible_name_for_dom_node(node: &DomNode) -> Option<String> {
  // Prefer `aria-label` when provided.
  if let Some(label) = node
    .get_attribute_ref("aria-label")
    .map(str::trim)
    .filter(|s| !s.is_empty())
  {
    return Some(label.to_string());
  }

  // For interactive elements (button/link), fall back to text content.
  match &node.node_type {
    DomNodeType::Element { tag_name, namespace, .. } => {
      let is_html = namespace.is_empty() || namespace == HTML_NAMESPACE;
      if is_html
        && (tag_name.eq_ignore_ascii_case("button") || tag_name.eq_ignore_ascii_case("a"))
      {
        let text = normalized_text_content(node);
        return (!text.is_empty()).then_some(text);
      }
    }
    _ => {}
  }

  None
}

/// Build an AccessKit `TreeUpdate` for a prepared document at a given scroll state.
///
/// This is intended as the bridge layer between FastRender layout artifacts and `accesskit_winit`.
///
/// `document_offset` positions the document viewport within the containing window (e.g. for a
/// split chrome/content layout). For a single full-window document, pass `Point::ZERO`.
pub fn build_accesskit_tree_update_for_document(
  tab_id: TabId,
  tree_generation: u32,
  prepared: &PreparedDocument,
  scroll_state: &ScrollState,
  document_offset: Point,
  scale: f32,
) -> TreeUpdate {
  // Layout produces fragment trees in an unscrolled coordinate space. Clone + apply element scroll
  // offsets (and sticky positioning) so extracted bounds match what the user sees.
  //
  // Note: viewport scroll is not applied here; `AccessKitBoundsTransform` accounts for it.
  let tree = prepared.fragment_tree_for_geometry(scroll_state);

  // Bounds map keyed by `styled_node_id` (which matches `DomNode` preorder ids).
  //
  // These bounds are in *page coordinates* (CSS px).
  let box_bounds = collect_box_bounds(&tree);
  let bounds_by_styled_node_id = collect_bounds_by_styled_node_id(prepared.box_tree(), &box_bounds);

  let node_ids = crate::dom::enumerate_dom_ids(prepared.dom());
  let transform = AccessKitBoundsTransform::new(scroll_state, document_offset, scale);

  #[derive(Clone, Copy)]
  struct Frame<'a> {
    node: &'a DomNode,
    next_child: usize,
    node_id: NodeId,
    dom_node_id: usize,
  }

  // Keep an explicit stack to avoid recursion (deep DOMs should not overflow).
  let mut stack: Vec<Frame<'_>> = Vec::new();
  let root_preorder_id = *node_ids
    .get(&(prepared.dom() as *const DomNode))
    .expect("root DOM node should have a preorder id");
  let root_node_id = encode_page_node_id(tab_id, tree_generation, root_preorder_id);
  stack.push(Frame {
    node: prepared.dom(),
    next_child: 0,
    node_id: root_node_id,
    dom_node_id: root_preorder_id,
  });

  // Built AccessKit nodes.
  let mut nodes: Vec<(NodeId, Node)> = Vec::new();
  let mut node_classes = NodeClassSet::default();
  // Children list for each frame (parallel stack).
  let mut children_stack: Vec<Vec<NodeId>> = Vec::new();
  children_stack.push(Vec::new());

  while let Some(frame) = stack.last_mut() {
    if frame.next_child < frame.node.children.len() {
      let child = &frame.node.children[frame.next_child];
      frame.next_child += 1;

      if !include_dom_node_in_accesskit_tree(child) {
        continue;
      }

      let preorder_id = *node_ids
        .get(&(child as *const DomNode))
        .expect("DOM traversal should have assigned a preorder id");
      let child_node_id = encode_page_node_id(tab_id, tree_generation, preorder_id);

      stack.push(Frame {
        node: child,
        next_child: 0,
        node_id: child_node_id,
        dom_node_id: preorder_id,
      });
      children_stack.push(Vec::new());
      continue;
    }

    let finished = stack.pop().expect("frame exists");
    let children = children_stack.pop().expect("children stack should align");

    let role = role_for_dom_node(finished.node);
    let mut builder = NodeBuilder::new(role);

    if let Some(name) = accessible_name_for_dom_node(finished.node) {
      builder.set_name(name);
    }

    if let Some(bounds_page) = bounds_by_styled_node_id.get(&finished.dom_node_id) {
      if let Some(bounds) = transform.transform_rect(*bounds_page) {
        builder.set_bounds(bounds);
      }
    }

    if !children.is_empty() {
      builder.set_children(children.clone());
    }

    let node = builder.build(&mut node_classes);
    nodes.push((finished.node_id, node));

    if let Some(parent_children) = children_stack.last_mut() {
      parent_children.push(finished.node_id);
    }
  }

  TreeUpdate {
    nodes,
    tree: Some(Tree::new(root_node_id)),
    focus: None,
  }
}

#[cfg(all(test, feature = "browser_ui"))]
mod tests {
  use super::build_accesskit_tree_update_for_document;
  use crate::dom::{enumerate_dom_ids, DomNode};
  use crate::geometry::Point;
  use crate::scroll::ScrollState;
  use crate::text::font_db::FontConfig;
  use crate::{FastRender, RenderOptions};
  use accesskit::NodeId;

  fn node_id_for_dom_id(root: &DomNode, id_attr: &str) -> usize {
    let ids = enumerate_dom_ids(root);
    let mut stack: Vec<&DomNode> = vec![root];
    while let Some(node) = stack.pop() {
      if node.get_attribute_ref("id").is_some_and(|id| id == id_attr) {
        return *ids
          .get(&(node as *const DomNode))
          .unwrap_or_else(|| panic!("node id missing for element with id={id_attr:?}"));
      }
      for child in node.children.iter().rev() {
        stack.push(child);
      }
    }
    panic!("no element with id attribute {id_attr:?}");
  }

  fn node_bounds_y0(update: &accesskit::TreeUpdate, node_id: NodeId) -> f64 {
    let node = update
      .nodes
      .iter()
      .find_map(|(id, node)| (*id == node_id).then_some(node))
      .unwrap_or_else(|| panic!("node {node_id:?} missing from TreeUpdate"));

    node
      .bounds()
      .map(|rect| rect.y0)
      .unwrap_or_else(|| panic!("node {node_id:?} had no bounds"))
  }

  #[test]
  fn accesskit_bounds_account_for_viewport_scroll() {
    let renderer = FastRender::builder()
      .font_sources(FontConfig::bundled_only())
      .build()
      .expect("build deterministic renderer");

    let html = r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            button { margin: 0; }
          </style>
        </head>
        <body>
          <div style="height: 1800px"></div>
          <button id="target">Scroll target</button>
        </body>
      </html>"#;

    let mut renderer = renderer;
    let options = RenderOptions::new()
      .with_viewport(320, 240)
      .with_device_pixel_ratio(1.0);
    let prepared = renderer
      .prepare_html(html, options)
      .expect("prepare html");

    let target_dom_id = node_id_for_dom_id(prepared.dom(), "target");
    let tab_id = crate::ui::messages::TabId(1);
    let tree_generation = 1;
    let target_node_id = crate::ui::encode_page_node_id(tab_id, tree_generation, target_dom_id);

    let update_unscrolled = build_accesskit_tree_update_for_document(
      tab_id,
      tree_generation,
      &prepared,
      &ScrollState::with_viewport(Point::ZERO),
      Point::ZERO,
      1.0,
    );
    let y0_unscrolled = node_bounds_y0(&update_unscrolled, target_node_id);

    // Keep the scroll offset well within the expected scroll range so future clamping behaviour
    // does not make this test flaky.
    let scroll_y = 1000.0;
    let update_scrolled = build_accesskit_tree_update_for_document(
      tab_id,
      tree_generation,
      &prepared,
      &ScrollState::with_viewport(Point::new(0.0, scroll_y)),
      Point::ZERO,
      1.0,
    );
    let y0_scrolled = node_bounds_y0(&update_scrolled, target_node_id);

    // When the viewport scrolls down, the element should move up in viewport/window coordinates by
    // the same delta.
    let delta = y0_unscrolled - y0_scrolled;
    assert!(
      (delta - scroll_y as f64).abs() < 0.01,
      "expected bounds to shift by scroll_y={scroll_y} (unscrolled y0={y0_unscrolled}, scrolled y0={y0_scrolled})"
    );
  }
}
