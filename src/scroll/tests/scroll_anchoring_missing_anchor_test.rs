use crate::geometry::{Point, Rect, Size};
use crate::scroll::anchoring::apply_scroll_anchoring;
use crate::scroll::{capture_scroll_anchors, ScrollState};
use crate::style::types::{Overflow, OverflowAnchor};
use crate::tree::fragment_tree::{FragmentContent, FragmentNode, FragmentTree};
use crate::ComputedStyle;
use std::collections::HashMap;
use std::sync::Arc;

fn scroller_style() -> Arc<ComputedStyle> {
  let mut style = ComputedStyle::default();
  style.overflow_y = Overflow::Scroll;
  Arc::new(style)
}

fn node_with_id(bounds: Rect, id: usize, style: Arc<ComputedStyle>, children: Vec<FragmentNode>) -> FragmentNode {
  FragmentNode::new_with_style(bounds, FragmentContent::Block { box_id: Some(id) }, children, style)
}

#[test]
fn scroll_anchoring_does_not_adjust_when_anchor_is_removed() {
  // Old layout: A is the anchor at y=100 and we're scrolled so A is at the top of the scrollport.
  let a_style = Arc::new(ComputedStyle::default());
  let b_style = Arc::new(ComputedStyle::default());
  let a_old = node_with_id(Rect::from_xywh(0.0, 100.0, 100.0, 50.0), 2, a_style.clone(), vec![]);
  let b_old = node_with_id(Rect::from_xywh(0.0, 200.0, 100.0, 50.0), 3, b_style.clone(), vec![]);

  let scroller_old = node_with_id(
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    1,
    scroller_style(),
    vec![a_old, b_old],
  );
  let root_old = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 0.0, 0.0), vec![scroller_old]);
  let tree_old = FragmentTree::with_viewport(root_old, Size::new(100.0, 100.0));

  let scroll = ScrollState::from_parts(Point::ZERO, HashMap::from([(1usize, Point::new(0.0, 100.0))]));
  let snapshot = capture_scroll_anchors(&tree_old, &scroll);
  assert_eq!(
    snapshot.elements.get(&1).map(|a| a.box_id),
    Some(2),
    "expected old layout to select A as the anchor"
  );

  // New layout: A is gone and B has shifted up (simulating content removal above).
  // Anchoring must *not* adjust the scroll offset when the previous anchor disappears.
  let b_new = node_with_id(Rect::from_xywh(0.0, 150.0, 100.0, 50.0), 3, b_style, vec![]);
  let scroller_new = node_with_id(
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    1,
    scroller_style(),
    vec![b_new],
  );
  let root_new = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 0.0, 0.0), vec![scroller_new]);
  let tree_new = FragmentTree::with_viewport(root_new, Size::new(100.0, 100.0));

  let (adjusted, _next_snapshot) = apply_scroll_anchoring(&snapshot, &tree_new, &scroll);
  assert_eq!(
    adjusted.elements.get(&1).copied(),
    Some(Point::new(0.0, 100.0)),
    "expected no element scroll adjustment when the anchor is missing"
  );
}

#[test]
fn scroll_anchoring_does_not_adjust_when_anchor_becomes_overflow_anchor_none() {
  let a_style = Arc::new(ComputedStyle::default());
  let b_style = Arc::new(ComputedStyle::default());
  let a_old = node_with_id(Rect::from_xywh(0.0, 100.0, 100.0, 50.0), 2, a_style.clone(), vec![]);
  let b_old = node_with_id(Rect::from_xywh(0.0, 200.0, 100.0, 50.0), 3, b_style.clone(), vec![]);
  let scroller_old = node_with_id(
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    1,
    scroller_style(),
    vec![a_old, b_old],
  );
  let root_old = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 0.0, 0.0), vec![scroller_old]);
  let tree_old = FragmentTree::with_viewport(root_old, Size::new(100.0, 100.0));

  let scroll = ScrollState::from_parts(Point::ZERO, HashMap::from([(1usize, Point::new(0.0, 100.0))]));
  let snapshot = capture_scroll_anchors(&tree_old, &scroll);
  assert_eq!(
    snapshot.elements.get(&1).map(|a| a.box_id),
    Some(2),
    "expected old layout to select A as the anchor"
  );

  // New layout: A still exists but opts out of scroll anchoring via `overflow-anchor:none`.
  // This should behave like the anchor disappearing: no adjustment for the current relayout.
  let mut a_new_style = (*a_style).clone();
  a_new_style.overflow_anchor = OverflowAnchor::None;
  let a_new_style = Arc::new(a_new_style);
  let a_new = node_with_id(Rect::from_xywh(0.0, 200.0, 100.0, 50.0), 2, a_new_style, vec![]);
  let b_new = node_with_id(Rect::from_xywh(0.0, 150.0, 100.0, 50.0), 3, b_style, vec![]);

  let scroller_new = node_with_id(
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    1,
    scroller_style(),
    vec![a_new, b_new],
  );
  let root_new = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 0.0, 0.0), vec![scroller_new]);
  let tree_new = FragmentTree::with_viewport(root_new, Size::new(100.0, 100.0));

  let (adjusted, _next_snapshot) = apply_scroll_anchoring(&snapshot, &tree_new, &scroll);
  assert_eq!(
    adjusted.elements.get(&1).copied(),
    Some(Point::new(0.0, 100.0)),
    "expected no element scroll adjustment when the previous anchor becomes overflow-anchor:none"
  );
}
