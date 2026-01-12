use crate::geometry::Rect;
use crate::style::ComputedStyle;
use crate::tree::box_tree::{BoxNode, BoxTree};
use crate::tree::fragment_tree::FragmentTree;
use std::sync::Arc;

use super::fragment_geometry::{
  absolute_bounds_for_box_id, content_rect_for_border_rect, padding_rect_for_border_rect,
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ElementBoxGeometry {
  pub border_box: Rect,
  pub padding_box: Rect,
  pub content_box: Rect,
}

pub fn element_geometry_for_styled_node_id(
  box_tree: &BoxTree,
  fragment_tree: &FragmentTree,
  styled_node_id: usize,
) -> Option<(ElementBoxGeometry, Arc<ComputedStyle>)> {
  let mut box_ids: Vec<usize> = Vec::new();
  let mut principal_style: Option<Arc<ComputedStyle>> = None;

  // Match the pre-order traversal order used by `box_tree::assign_implicit_anchor_box_ids`.
  let mut stack: Vec<&BoxNode> = vec![&box_tree.root];
  while let Some(node) = stack.pop() {
    if node.generated_pseudo.is_none() && node.styled_node_id == Some(styled_node_id) {
      box_ids.push(node.id);
      if principal_style.is_none() {
        principal_style = Some(Arc::clone(&node.style));
      }
    }

    if let Some(body) = node.footnote_body.as_deref() {
      stack.push(body);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }

  let principal_style = principal_style?;

  let mut border_box: Option<Rect> = None;
  for box_id in box_ids {
    let Some(bounds) = absolute_bounds_for_box_id(fragment_tree, box_id) else {
      continue;
    };
    border_box = Some(match border_box {
      Some(existing) => existing.union(bounds),
      None => bounds,
    });
  }

  let border_box = border_box?;
  let viewport_size = fragment_tree.viewport_size();
  let padding_box = padding_rect_for_border_rect(border_box, principal_style.as_ref(), viewport_size);
  let content_box = content_rect_for_border_rect(border_box, principal_style.as_ref(), viewport_size);

  Some((
    ElementBoxGeometry {
      border_box,
      padding_box,
      content_box,
    },
    principal_style,
  ))
}

