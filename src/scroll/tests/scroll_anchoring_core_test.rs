use crate::geometry::{Point, Rect, Size};
use crate::scroll::{apply_scroll_anchoring, capture_scroll_anchors, ScrollState};
use crate::style::types::{Overflow, OverflowAnchor};
use crate::tree::fragment_tree::{FragmentContent, FragmentNode, FragmentTree};
use crate::ComputedStyle;
use std::collections::HashMap;
use std::sync::Arc;

fn adjust(old_tree: &FragmentTree, new_tree: &FragmentTree, state: &ScrollState) -> ScrollState {
  let snapshot = capture_scroll_anchors(old_tree, state);
  apply_scroll_anchoring(&snapshot, new_tree, state).0
}

fn viewport_tree(spacer_height: f32, anchor_top: f32, root_style: Option<Arc<ComputedStyle>>) -> FragmentTree {
  let spacer = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, spacer_height), vec![]);
  let anchor = FragmentNode::new_block_with_id(
    Rect::from_xywh(0.0, anchor_top, 100.0, 20.0),
    2,
    vec![],
  );
  let root_bounds_height = 300.0;

  let root = if let Some(style) = root_style {
    FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, root_bounds_height),
      vec![spacer, anchor],
      style,
    )
  } else {
    FragmentNode::new_block(
      Rect::from_xywh(0.0, 0.0, 100.0, root_bounds_height),
      vec![spacer, anchor],
    )
  };

  FragmentTree::with_viewport(root, Size::new(100.0, 100.0))
}

#[test]
fn viewport_scroll_anchoring_adjusts_scroll_offset_when_anchor_moves() {
  let old_tree = viewport_tree(50.0, 50.0, None);
  let new_tree = viewport_tree(70.0, 70.0, None);

  // Scroll beyond the spacer so it is fully clipped and the anchor is chosen.
  let state = ScrollState::from_parts(Point::new(0.0, 60.0), HashMap::new());
  let adjusted = adjust(&old_tree, &new_tree, &state);

  assert_eq!(adjusted.viewport, Point::new(0.0, 80.0));
}

#[test]
fn overflow_anchor_none_on_scroller_disables_adjustment() {
  let mut style = ComputedStyle::default();
  style.overflow_anchor = OverflowAnchor::None;
  let style = Arc::new(style);

  let old_tree = viewport_tree(50.0, 50.0, Some(style.clone()));
  let new_tree = viewport_tree(70.0, 70.0, Some(style));

  let state = ScrollState::from_parts(Point::new(0.0, 60.0), HashMap::new());
  let adjusted = adjust(&old_tree, &new_tree, &state);

  assert_eq!(adjusted.viewport, state.viewport);
}

#[test]
fn element_scroll_container_adjusts_element_scroll_offset() {
  let mut scroller_style = ComputedStyle::default();
  scroller_style.overflow_y = Overflow::Scroll;
  let scroller_style = Arc::new(scroller_style);

  let spacer_old = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 50.0), vec![]);
  let anchor_old = FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 50.0, 100.0, 20.0), 3, vec![]);
  let filler_old = FragmentNode::new_block(Rect::from_xywh(0.0, 70.0, 100.0, 300.0), vec![]);

  let scroller_old = FragmentNode::new_with_style(
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    FragmentContent::Block { box_id: Some(1) },
    vec![spacer_old, anchor_old, filler_old],
    scroller_style.clone(),
  );
  let root_old = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 100.0), vec![scroller_old]);
  let old_tree = FragmentTree::with_viewport(root_old, Size::new(100.0, 100.0));

  let spacer_new = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 70.0), vec![]);
  let anchor_new = FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 70.0, 100.0, 20.0), 3, vec![]);
  let filler_new = FragmentNode::new_block(Rect::from_xywh(0.0, 90.0, 100.0, 300.0), vec![]);
  let scroller_new = FragmentNode::new_with_style(
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    FragmentContent::Block { box_id: Some(1) },
    vec![spacer_new, anchor_new, filler_new],
    scroller_style,
  );
  let root_new = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 100.0), vec![scroller_new]);
  let new_tree = FragmentTree::with_viewport(root_new, Size::new(100.0, 100.0));

  let state = ScrollState::from_parts(Point::ZERO, HashMap::from([(1usize, Point::new(0.0, 60.0))]));
  let adjusted = adjust(&old_tree, &new_tree, &state);

  assert_eq!(adjusted.elements.get(&1).copied(), Some(Point::new(0.0, 80.0)));
}

#[test]
fn adjusted_scroll_offsets_are_clamped_to_bounds() {
  // Viewport height 100, root content height 150 => max scroll y = 50.
  let spacer_old = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 30.0), vec![]);
  let anchor_old = FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 30.0, 100.0, 20.0), 2, vec![]);
  let root_old = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 150.0), vec![spacer_old, anchor_old]);
  let old_tree = FragmentTree::with_viewport(root_old, Size::new(100.0, 100.0));

  let spacer_new = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 50.0), vec![]);
  let anchor_new = FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 50.0, 100.0, 20.0), 2, vec![]);
  let root_new = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 150.0), vec![spacer_new, anchor_new]);
  let new_tree = FragmentTree::with_viewport(root_new, Size::new(100.0, 100.0));

  let state = ScrollState::from_parts(Point::new(0.0, 40.0), HashMap::new());
  let adjusted = adjust(&old_tree, &new_tree, &state);
  assert_eq!(adjusted.viewport, Point::new(0.0, 50.0));
}

#[test]
fn scroll_offset_zero_suppresses_adjustment() {
  let old_tree = viewport_tree(50.0, 50.0, None);
  let new_tree = viewport_tree(70.0, 70.0, None);

  let state = ScrollState::from_parts(Point::ZERO, HashMap::new());
  let adjusted = adjust(&old_tree, &new_tree, &state);

  assert_eq!(adjusted.viewport, Point::ZERO);
}
