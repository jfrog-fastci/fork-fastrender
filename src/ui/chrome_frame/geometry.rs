use crate::geometry::{Point, Rect};
use crate::dom::DomNode;
use crate::scroll::ScrollState;
use crate::tree::box_tree::BoxNode;
use crate::PreparedDocument;

fn preorder_id_for_element_id(dom: &DomNode, element_id: &str) -> Option<usize> {
  // Match `crate::dom::enumerate_dom_ids` preorder traversal and `DomIndex` semantics:
  // - IDs are assigned to every node in pre-order (including `<template>` contents)
  // - `id` attribute lookup ignores inert `<template>` subtrees (matching `getElementById`)
  //
  // We intentionally traverse `children` (not `traversal_children`) so template contents still count
  // towards stable preorder ids.
  let mut next_id: usize = 0;
  let mut stack: Vec<(&DomNode, bool)> = vec![(dom, false)];
  while let Some((node, in_template_contents)) = stack.pop() {
    next_id += 1;

    if !in_template_contents {
      if let Some(id) = node.get_attribute_ref("id") {
        if id == element_id {
          return Some(next_id);
        }
      }
    }

    let child_in_template_contents = in_template_contents || node.is_template_element();
    for child in node.children.iter().rev() {
      stack.push((child, child_in_template_contents));
    }
  }

  None
}

pub fn element_border_rect_by_id(prepared: &PreparedDocument, element_id: &str) -> Option<Rect> {
  let scroll_state = prepared.default_scroll_state();
  element_border_rect_by_id_with_scroll_state(prepared, element_id, &scroll_state)
}

pub fn element_border_rect_by_id_with_scroll_state(
  prepared: &PreparedDocument,
  element_id: &str,
  scroll_state: &ScrollState,
) -> Option<Rect> {
  let node_id = preorder_id_for_element_id(prepared.dom(), element_id)?;

  // Convert the fragment tree into paint-time geometry coordinates (element scroll offsets +
  // sticky adjustments). The returned tree is still in page coordinates; convert it to
  // viewport-local space at the end by subtracting the viewport scroll offset.
  let geometry_tree = prepared.fragment_tree_for_geometry(scroll_state);

  let mut out: Option<Rect> = None;
  // Match `BoxTree::assign_implicit_anchor_box_ids` traversal order.
  let mut stack: Vec<&BoxNode> = vec![&prepared.box_tree().root];
  while let Some(node) = stack.pop() {
    if node.generated_pseudo.is_none() && node.styled_node_id == Some(node_id) {
      if let Some(bounds) = crate::interaction::fragment_geometry::absolute_bounds_for_box_id(
        &geometry_tree,
        node.id,
      ) {
        out = Some(match out {
          Some(existing) => existing.union(bounds),
          None => bounds,
        });
      }
    }

    if let Some(body) = node.footnote_body.as_deref() {
      stack.push(body);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }

  let rect = out?;
  let viewport_scroll = scroll_state.viewport;
  Some(rect.translate(Point::new(-viewport_scroll.x, -viewport_scroll.y)))
}

pub fn element_border_rect_by_id_with_viewport_scroll(
  prepared: &PreparedDocument,
  element_id: &str,
  viewport_scroll: Point,
) -> Option<Rect> {
  let scroll_state = ScrollState::with_viewport(viewport_scroll);
  element_border_rect_by_id_with_scroll_state(prepared, element_id, &scroll_state)
}
