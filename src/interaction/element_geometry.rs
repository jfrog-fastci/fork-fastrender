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

#[cfg(test)]
mod tests {
  use super::element_geometry_for_styled_node_id;
  use crate::geometry::Rect;
  use crate::style::display::FormattingContextType;
  use crate::style::types::BorderStyle;
  use crate::style::values::Length;
  use crate::style::ComputedStyle;
  use crate::tree::box_tree::{BoxNode, BoxTree};
  use crate::tree::fragment_tree::{FragmentNode, FragmentTree};
  use std::sync::Arc;

  #[test]
  fn element_geometry_for_styled_node_id_unions_non_pseudo_boxes() {
    let styled_node_id = 10;

    let mut principal_style = ComputedStyle::default();
    principal_style.border_left_style = BorderStyle::Solid;
    principal_style.border_top_style = BorderStyle::Solid;
    principal_style.border_right_style = BorderStyle::Solid;
    principal_style.border_bottom_style = BorderStyle::Solid;
    principal_style.border_left_width = Length::px(1.0);
    principal_style.border_top_width = Length::px(2.0);
    principal_style.border_right_width = Length::px(3.0);
    principal_style.border_bottom_width = Length::px(4.0);
    principal_style.padding_left = Length::px(5.0);
    principal_style.padding_top = Length::px(6.0);
    principal_style.padding_right = Length::px(7.0);
    principal_style.padding_bottom = Length::px(8.0);
    let principal_style = Arc::new(principal_style);

    // Use a distinct style for the second box so we can assert the principal style is the first box.
    let secondary_style = Arc::new(ComputedStyle::default());

    let mut principal_box = BoxNode::new_block(
      Arc::clone(&principal_style),
      FormattingContextType::Block,
      vec![],
    );
    principal_box.styled_node_id = Some(styled_node_id);
    let mut secondary_box = BoxNode::new_block(
      Arc::clone(&secondary_style),
      FormattingContextType::Block,
      vec![],
    );
    secondary_box.styled_node_id = Some(styled_node_id);

    let root = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      FormattingContextType::Block,
      vec![principal_box, secondary_box],
    );
    let box_tree = BoxTree::new(root);
    let principal_box_id = box_tree.root.children[0].id;
    let secondary_box_id = box_tree.root.children[1].id;

    let fragment_a = FragmentNode::new_block_with_id(
      Rect::from_xywh(5.0, 6.0, 30.0, 40.0),
      principal_box_id,
      vec![],
    );
    let parent_a = FragmentNode::new_block(
      Rect::from_xywh(10.0, 20.0, 100.0, 100.0),
      vec![fragment_a],
    );
    let fragment_b = FragmentNode::new_block_with_id(
      Rect::from_xywh(100.0, 0.0, 20.0, 20.0),
      principal_box_id,
      vec![],
    );
    let fragment_c = FragmentNode::new_block_with_id(
      Rect::from_xywh(10.0, 10.0, 50.0, 50.0),
      secondary_box_id,
      vec![],
    );
    let parent_c = FragmentNode::new_block(
      Rect::from_xywh(0.0, 100.0, 100.0, 100.0),
      vec![fragment_c],
    );

    let fragment_tree = FragmentTree::new(FragmentNode::new_block(
      Rect::from_xywh(0.0, 0.0, 500.0, 500.0),
      vec![parent_a, fragment_b, parent_c],
    ));

    let (geometry, style) =
      element_geometry_for_styled_node_id(&box_tree, &fragment_tree, styled_node_id)
        .expect("expected element geometry");

    assert!(
      Arc::ptr_eq(&style, &principal_style),
      "expected principal style to come from the first non-pseudo box in pre-order traversal"
    );

    let expected_border_box = Rect::from_xywh(10.0, 0.0, 110.0, 160.0);
    assert_eq!(geometry.border_box, expected_border_box);

    assert_eq!(geometry.padding_box, Rect::from_xywh(11.0, 2.0, 106.0, 154.0));
    assert_eq!(geometry.content_box, Rect::from_xywh(16.0, 8.0, 94.0, 140.0));
  }
}
