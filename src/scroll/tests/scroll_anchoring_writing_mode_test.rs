use crate::geometry::{Point, Rect, Size};
use crate::scroll::{apply_scroll_anchoring_between_fragment_trees, ScrollState};
use crate::style::types::WritingMode;
use crate::style::ComputedStyle;
use crate::tree::fragment_tree::{FragmentNode, FragmentTree};
use std::sync::Arc;

#[test]
fn scroll_anchoring_adjusts_along_block_axis_in_vertical_writing_mode() {
  // Ensure scroll anchoring adjusts the physical scroll axis that corresponds to the anchor's
  // movement even under vertical writing modes.
  let writing_mode = WritingMode::VerticalRl;

  let mut root_style = ComputedStyle::default();
  root_style.writing_mode = writing_mode;
  let root_style = Arc::new(root_style);

  // The anchor is the only visible fragment with a box id.
  let anchor_old_bounds = Rect::from_xywh(20.0, 0.0, 10.0, 10.0);
  let anchor_new_bounds = Rect::from_xywh(0.0, 0.0, 10.0, 10.0);

  let anchor_old = FragmentNode::new_block_with_id(anchor_old_bounds, 1, vec![]);
  let root_old = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    vec![anchor_old],
    root_style.clone(),
  );
  let prev_tree = FragmentTree::with_viewport(root_old, Size::new(100.0, 100.0));

  let anchor_new = FragmentNode::new_block_with_id(anchor_new_bounds, 1, vec![]);
  let root_new = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    vec![anchor_new],
    root_style,
  );
  let next_tree = FragmentTree::with_viewport(root_new, Size::new(100.0, 100.0));

  // Non-zero scroll offset in the block axis (horizontal for vertical writing modes).
  let scroll_state = ScrollState::with_viewport(Point::new(20.0, 0.0));

  let adjusted = apply_scroll_anchoring_between_fragment_trees(&prev_tree, &next_tree, &scroll_state);

  // The scroll adjustment is the movement of the anchor fragment's origin in physical coordinates.
  let expected_x = scroll_state.viewport.x + (anchor_new_bounds.x() - anchor_old_bounds.x());

  assert!(
    (adjusted.viewport.x - expected_x).abs() < 1e-3,
    "expected scroll anchoring to update viewport.x from {} to {}; got {}",
    scroll_state.viewport.x,
    expected_x,
    adjusted.viewport.x
  );
  assert!(
    adjusted.viewport.y.abs() < 1e-3,
    "scroll anchoring should not touch the physical Y axis in vertical writing modes; got y={}",
    adjusted.viewport.y
  );
}
