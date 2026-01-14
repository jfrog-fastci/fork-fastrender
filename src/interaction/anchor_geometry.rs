use crate::geometry::{Point, Rect};
use crate::scroll::ScrollState;
use crate::tree::box_tree::BoxTree;
use crate::tree::fragment_tree::FragmentTree;

use super::fragment_geometry::absolute_bounds_by_styled_node_id;

/// Compute the viewport-local CSS pixel bounds of a DOM element (styled node).
///
/// This is primarily used to position UI overlays (select dropdowns, pickers, media controls, etc)
/// relative to an element. The returned rect is in **viewport-local** coordinates, so it can be
/// used directly by UI code: the document scroll offset is subtracted from the element's absolute
/// page-space bounds.
///
/// Notes:
/// - If the element produced multiple fragments (e.g. split across lines/pages), the returned
///   bounds is the union of **all** fragments produced for the `styled_node_id`.
pub fn styled_node_anchor_css(
  box_tree: &BoxTree,
  fragment_tree: &FragmentTree,
  scroll_state: &ScrollState,
  styled_node_id: usize,
) -> Option<Rect> {
  let bounds = absolute_bounds_by_styled_node_id(box_tree, fragment_tree)
    .get(&styled_node_id)
    .copied()?;

  Some(bounds.translate(Point::new(
    -scroll_state.viewport.x,
    -scroll_state.viewport.y,
  )))
}

#[cfg(test)]
mod tests {
  use super::styled_node_anchor_css;
  use crate::geometry::{Point, Rect};
  use crate::scroll::ScrollState;
  use crate::style::display::FormattingContextType;
  use crate::style::ComputedStyle;
  use crate::tree::box_tree::{BoxNode, BoxTree};
  use crate::tree::fragment_tree::{FragmentNode, FragmentTree};
  use std::sync::Arc;

  #[test]
  fn styled_node_anchor_css_subtracts_viewport_scroll_offset() {
    let styled_node_id = 2;

    let mut child = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      FormattingContextType::Block,
      vec![],
    );
    child.styled_node_id = Some(styled_node_id);

    let mut root = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      FormattingContextType::Block,
      vec![child],
    );
    root.styled_node_id = Some(1);

    let box_tree = BoxTree::new(root);
    let child_box_id = box_tree.root.children[0].id;

    let child_fragment = FragmentNode::new_block_with_id(
      Rect::from_xywh(5.0, 6.0, 7.0, 8.0),
      child_box_id,
      vec![],
    );
    let fragment_tree = FragmentTree::new(FragmentNode::new_block(
      Rect::from_xywh(100.0, 200.0, 300.0, 400.0),
      vec![child_fragment],
    ));

    let scroll_state = ScrollState::with_viewport(Point::new(10.0, 20.0));
    let anchor = styled_node_anchor_css(&box_tree, &fragment_tree, &scroll_state, styled_node_id)
      .expect("expected anchor rect");

    // Absolute page-space bounds:
    // - root origin (100,200) + child origin (5,6) = (105,206)
    // Subtract viewport scroll (10,20) to get viewport-local bounds.
    assert_eq!(anchor, Rect::from_xywh(95.0, 186.0, 7.0, 8.0));
  }

  #[test]
  fn styled_node_anchor_css_unions_multiple_boxes_for_styled_node_id() {
    let styled_node_id = 2;

    let mut box_a = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      FormattingContextType::Block,
      vec![],
    );
    box_a.styled_node_id = Some(styled_node_id);

    let mut box_b = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      FormattingContextType::Block,
      vec![],
    );
    box_b.styled_node_id = Some(styled_node_id);

    let mut root = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      FormattingContextType::Block,
      vec![box_a, box_b],
    );
    root.styled_node_id = Some(1);
    let box_tree = BoxTree::new(root);
    let box_a_id = box_tree.root.children[0].id;
    let box_b_id = box_tree.root.children[1].id;

    let fragment_a = FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      box_a_id,
      vec![],
    );
    let fragment_b = FragmentNode::new_block_with_id(
      Rect::from_xywh(20.0, 5.0, 10.0, 10.0),
      box_b_id,
      vec![],
    );
    let fragment_tree = FragmentTree::new(FragmentNode::new_block(
      Rect::from_xywh(10.0, 20.0, 100.0, 100.0),
      vec![fragment_a, fragment_b],
    ));

    let scroll_state = ScrollState::with_viewport(Point::ZERO);
    let anchor = styled_node_anchor_css(&box_tree, &fragment_tree, &scroll_state, styled_node_id)
      .expect("expected anchor rect");

    // Fragment A absolute bounds: (10,20)-(20,30)
    // Fragment B absolute bounds: (30,25)-(40,35)
    // Union => (10,20)-(40,35)
    assert_eq!(anchor, Rect::from_xywh(10.0, 20.0, 30.0, 15.0));
  }
}

