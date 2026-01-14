use crate::geometry::{Point, Rect, Size};
use crate::scroll::{capture_scroll_anchors, ScrollState};
use crate::style::position::Position;
use crate::style::types::Overflow;
use crate::tree::fragment_tree::{FragmentContent, FragmentNode, FragmentTree};
use crate::ComputedStyle;
use std::collections::HashMap;
use std::sync::Arc;

#[test]
fn scroll_anchoring_excludes_fixed_position_fragments() {
  let mut scroller_style = ComputedStyle::default();
  scroller_style.overflow_y = Overflow::Scroll;
  let scroller_style = Arc::new(scroller_style);

  let mut fixed_style = ComputedStyle::default();
  fixed_style.position = Position::Fixed;
  let fixed_style = Arc::new(fixed_style);

  let normal_style = Arc::new(ComputedStyle::default());

  let fixed_rect = Rect::from_xywh(0.0, 0.0, 100.0, 10.0);
  let normal_rect = Rect::from_xywh(0.0, 20.0, 100.0, 10.0);

  let fixed = FragmentNode::new_with_style(
    fixed_rect,
    FragmentContent::Block { box_id: Some(1) },
    vec![],
    Arc::clone(&fixed_style),
  );
  let normal = FragmentNode::new_with_style(
    normal_rect,
    FragmentContent::Block { box_id: Some(2) },
    vec![],
    Arc::clone(&normal_style),
  );

  let scroller = FragmentNode::new_with_style(
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    FragmentContent::Block { box_id: Some(10) },
    vec![fixed, normal],
    Arc::clone(&scroller_style),
  );

  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 0.0, 0.0), vec![scroller]);
  let tree = FragmentTree::with_viewport(root, Size::new(100.0, 100.0));
  let scroll = ScrollState::from_parts(Point::ZERO, HashMap::from([(10usize, Point::ZERO)]));

  // Ensure the fixed fragment is visible and would otherwise be eligible.
  let scrollport = Rect::from_xywh(0.0, 0.0, 100.0, 100.0);
  assert!(
    fixed_rect.intersects(scrollport),
    "test requires fixed fragment to be visible inside the scrollport"
  );
  assert!(
    normal_rect.intersects(scrollport),
    "test requires normal fragment to be visible inside the scrollport"
  );

  let snapshot = capture_scroll_anchors(&tree, &scroll);
  let anchor = snapshot
    .elements
    .get(&10)
    .map(|anchor| anchor.box_id)
    .expect("expected an anchor node");
  assert_eq!(
    anchor, 2,
    "anchor selection should skip position: fixed fragments and choose a normal-flow fragment"
  );

  // Sanity-check: if the fixed fragment were not excluded, it would be selected first.
  let fixed_nonfixed = FragmentNode::new_with_style(
    fixed_rect,
    FragmentContent::Block { box_id: Some(1) },
    vec![],
    Arc::new(ComputedStyle::default()),
  );
  let normal_again = FragmentNode::new_with_style(
    normal_rect,
    FragmentContent::Block { box_id: Some(2) },
    vec![],
    normal_style,
  );
  let scroller_without_fixed_position = FragmentNode::new_with_style(
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    FragmentContent::Block { box_id: Some(10) },
    vec![fixed_nonfixed, normal_again],
    scroller_style,
  );
  let root = FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 0.0, 0.0),
    vec![scroller_without_fixed_position],
  );
  let tree = FragmentTree::with_viewport(root, Size::new(100.0, 100.0));
  let snapshot = capture_scroll_anchors(&tree, &scroll);
  let anchor_without_fixed = snapshot
    .elements
    .get(&10)
    .map(|anchor| anchor.box_id)
    .expect("expected an anchor node when fixed is not excluded");
  assert_eq!(
    anchor_without_fixed, 1,
    "expected the first fully-visible child to be selected when it is not position: fixed"
  );
}
