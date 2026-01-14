use crate::geometry::{Point, Rect, Size};
use crate::scroll::{
  apply_scroll_anchoring, apply_scroll_anchoring_between_trees, capture_scroll_anchors, ScrollState,
};
use crate::style::types::Overflow;
use crate::tree::fragment_tree::{FragmentContent, FragmentNode, FragmentTree};
use crate::ComputedStyle;
use std::collections::HashMap;
use std::sync::Arc;

fn assert_scroll_state_finite(state: &ScrollState) {
  assert!(
    state.viewport.x.is_finite() && state.viewport.y.is_finite(),
    "viewport scroll must be finite, got {:?}",
    state.viewport
  );
  assert!(
    state.viewport_delta.x.is_finite() && state.viewport_delta.y.is_finite(),
    "viewport_delta must be finite, got {:?}",
    state.viewport_delta
  );
  for (&id, &offset) in &state.elements {
    assert!(
      offset.x.is_finite() && offset.y.is_finite(),
      "element scroll offset for {id} must be finite, got {:?}",
      offset
    );
  }
  for (&id, &delta) in &state.elements_delta {
    assert!(
      delta.x.is_finite() && delta.y.is_finite(),
      "element scroll delta for {id} must be finite, got {:?}",
      delta
    );
  }
}

#[test]
fn scroll_anchoring_suppresses_non_finite_viewport_anchor_geometry() {
  let prev_anchor = FragmentNode::new_block_with_id(
    Rect::from_xywh(0.0, 50.0, 100.0, 10.0),
    1,
    vec![],
  );
  let prev_root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 200.0), vec![prev_anchor]);
  let prev_tree = FragmentTree::with_viewport(prev_root, Size::new(100.0, 100.0));

  let next_anchor = FragmentNode::new_block_with_id(
    Rect::from_xywh(0.0, f32::NAN, 100.0, 10.0),
    1,
    vec![],
  );
  let next_root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 200.0), vec![next_anchor]);
  let next_tree = FragmentTree::with_viewport(next_root, Size::new(100.0, 100.0));

  let state = ScrollState::with_viewport(Point::new(0.0, 50.0));
  let snapshot = capture_scroll_anchors(&prev_tree, &state);
  let (anchored, _next_snapshot) = apply_scroll_anchoring(&snapshot, &next_tree, &state);
  assert_scroll_state_finite(&anchored);
}

#[test]
fn scroll_anchoring_suppresses_non_finite_element_anchor_geometry() {
  let mut scroller_style = ComputedStyle::default();
  scroller_style.overflow_y = Overflow::Scroll;
  let scroller_style = Arc::new(scroller_style);

  // Content starts at y=50, but the scroller is scrolled by 50px so the anchor point at the top of
  // the scrollport hits this child.
  let prev_child = FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 50.0, 100.0, 50.0), 3, vec![]);
  let prev_scroller = FragmentNode::new_with_style(
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    FragmentContent::Block { box_id: Some(2) },
    vec![prev_child],
    scroller_style.clone(),
  );
  let prev_root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 100.0), vec![prev_scroller]);
  let prev_tree = FragmentTree::with_viewport(prev_root, Size::new(100.0, 100.0));

  // Next layout produces a non-finite child position; anchoring must be suppressed without
  // propagating NaN into the scroll offsets.
  let next_child =
    FragmentNode::new_block_with_id(Rect::from_xywh(0.0, f32::NAN, 100.0, 50.0), 3, vec![]);
  let next_scroller = FragmentNode::new_with_style(
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    FragmentContent::Block { box_id: Some(2) },
    vec![next_child],
    scroller_style,
  );
  let next_root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 100.0), vec![next_scroller]);
  let next_tree = FragmentTree::with_viewport(next_root, Size::new(100.0, 100.0));

  let state =
    ScrollState::from_parts(Point::ZERO, HashMap::from([(2usize, Point::new(0.0, 50.0))]));
  let snapshot = capture_scroll_anchors(&prev_tree, &state);
  let (anchored, _next_snapshot) = apply_scroll_anchoring(&snapshot, &next_tree, &state);
  assert_scroll_state_finite(&anchored);
}

#[test]
fn scroll_anchoring_between_trees_suppresses_non_finite_viewport_anchor_geometry() {
  let prev_anchor = FragmentNode::new_block_with_id(
    Rect::from_xywh(0.0, 50.0, 100.0, 10.0),
    1,
    vec![],
  );
  let prev_root =
    FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 200.0), vec![prev_anchor]);
  let prev_tree = FragmentTree::with_viewport(prev_root, Size::new(100.0, 100.0));

  let next_anchor = FragmentNode::new_block_with_id(
    Rect::from_xywh(0.0, f32::NAN, 100.0, 10.0),
    1,
    vec![],
  );
  let next_root =
    FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 200.0), vec![next_anchor]);
  let next_tree = FragmentTree::with_viewport(next_root, Size::new(100.0, 100.0));

  let state = ScrollState::with_viewport(Point::new(0.0, 50.0));
  let anchored = apply_scroll_anchoring_between_trees(
    &prev_tree,
    &next_tree,
    &state,
    next_tree.viewport_size(),
    None,
  );
  assert_scroll_state_finite(&anchored);
}

#[test]
fn scroll_anchoring_between_trees_suppresses_non_finite_element_anchor_geometry() {
  let mut scroller_style = ComputedStyle::default();
  scroller_style.overflow_y = Overflow::Scroll;
  let scroller_style = Arc::new(scroller_style);

  let prev_child =
    FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 50.0, 100.0, 50.0), 3, vec![]);
  let prev_scroller = FragmentNode::new_with_style(
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    FragmentContent::Block { box_id: Some(2) },
    vec![prev_child],
    scroller_style.clone(),
  );
  let prev_root =
    FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 100.0), vec![prev_scroller]);
  let prev_tree = FragmentTree::with_viewport(prev_root, Size::new(100.0, 100.0));

  let next_child =
    FragmentNode::new_block_with_id(Rect::from_xywh(0.0, f32::NAN, 100.0, 50.0), 3, vec![]);
  let next_scroller = FragmentNode::new_with_style(
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    FragmentContent::Block { box_id: Some(2) },
    vec![next_child],
    scroller_style,
  );
  let next_root =
    FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 100.0), vec![next_scroller]);
  let next_tree = FragmentTree::with_viewport(next_root, Size::new(100.0, 100.0));

  let state =
    ScrollState::from_parts(Point::ZERO, HashMap::from([(2usize, Point::new(0.0, 50.0))]));
  let anchored = apply_scroll_anchoring_between_trees(
    &prev_tree,
    &next_tree,
    &state,
    next_tree.viewport_size(),
    None,
  );
  assert_scroll_state_finite(&anchored);
}
