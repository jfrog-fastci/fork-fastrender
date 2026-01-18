use crate::geometry::{Point, Rect, Size};
use crate::scroll::{apply_scroll_anchoring_between_fragment_trees, ScrollState};
use crate::style::types::WritingMode;
use crate::style::ComputedStyle;
use crate::tree::fragment_tree::{FragmentNode, FragmentTree};
use std::sync::Arc;

#[test]
fn scroll_anchoring_adjusts_along_block_axis_in_vertical_writing_mode() {
  // In `writing-mode: vertical-rl`, the block axis is horizontal. Scroll anchoring should still
  // adjust the physical X scroll offset when layout shifts along that axis.
  let writing_mode = WritingMode::VerticalRl;

  let mut root_style = ComputedStyle::default();
  root_style.writing_mode = writing_mode;
  let root_style = Arc::new(root_style);

  // The anchor is the only visible fragment with a box id and remains in view under the initial
  // scroll offset.
  let anchor_old_bounds = Rect::from_xywh(20.0, 0.0, 10.0, 10.0);
  let anchor_new_bounds = Rect::from_xywh(35.0, 0.0, 10.0, 10.0);

  let anchor_old = FragmentNode::new_block_with_id(anchor_old_bounds, 1, vec![]);
  let root_old = FragmentNode::new_block_styled(
    // Ensure the scroll bounds allow non-zero horizontal scrolling so the test exercises
    // scroll anchoring adjustment rather than being clamped to 0.
    Rect::from_xywh(0.0, 0.0, 200.0, 100.0),
    vec![anchor_old],
    root_style.clone(),
  );
  let viewport = Size::new(100.0, 100.0);
  let prev_tree = FragmentTree::with_viewport(root_old, viewport);

  let anchor_new = FragmentNode::new_block_with_id(anchor_new_bounds, 1, vec![]);
  let root_new = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 200.0, 100.0),
    vec![anchor_new],
    root_style,
  );
  let next_tree = FragmentTree::with_viewport(root_new, viewport);

  // Non-zero scroll offset in the block axis (horizontal for `vertical-rl`).
  let scroll_state = ScrollState::with_viewport(Point::new(20.0, 0.0));

  let adjusted = apply_scroll_anchoring_between_fragment_trees(&prev_tree, &next_tree, &scroll_state);

  // The scroll adjustment tracks the movement of the selected anchor origin in physical
  // coordinates.
  // In `vertical-rl`, the block axis progresses from right-to-left, so anchor movement in physical
  // +X coordinates decreases the logical scroll offset.
  let expected_x = scroll_state.viewport.x - (anchor_new_bounds.x() - anchor_old_bounds.x());

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
