use crate::geometry::{Point, Rect, Size};
use crate::scroll::{capture_scroll_anchors, ScrollState};
use crate::style::types::Overflow;
use crate::tree::fragment_tree::{FragmentContent, FragmentNode, FragmentTree};
use crate::ComputedStyle;
use std::collections::HashMap;
use std::sync::Arc;

fn node_with_id(
  bounds: Rect,
  id: usize,
  style: Arc<ComputedStyle>,
  children: Vec<FragmentNode>,
) -> FragmentNode {
  FragmentNode::new_with_style(
    bounds,
    FragmentContent::Block { box_id: Some(id) },
    children,
    style,
  )
}

#[test]
fn viewport_anchor_selection_depends_on_viewport_scroll_offset() {
  // Document: three blocks stacked vertically (A, B, C). Viewport height is 100px.
  //
  // Use non-contiguous y positions so the "touching edge" semantics of `Rect::intersects` do not
  // allow A to remain a candidate at scroll_y=200.
  let style = Arc::new(ComputedStyle::default());
  let a = node_with_id(
    Rect::from_xywh(0.0, 0.0, 100.0, 150.0),
    1,
    style.clone(),
    vec![],
  );
  let b = node_with_id(
    Rect::from_xywh(0.0, 200.0, 100.0, 150.0),
    2,
    style.clone(),
    vec![],
  );
  let c = node_with_id(Rect::from_xywh(0.0, 400.0, 100.0, 150.0), 3, style, vec![]);

  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 0.0, 0.0), vec![a, b, c]);
  let tree = FragmentTree::with_viewport(root, Size::new(100.0, 100.0));

  let top = ScrollState::with_viewport(Point::new(0.0, 0.0));
  let top_snapshot = capture_scroll_anchors(&tree, &top);
  let top_anchor = top_snapshot
    .viewport
    .expect("expected an anchor at scroll_y=0");
  assert_eq!(top_anchor.box_id, 1);
  assert!(
    (top_anchor.origin.y - 0.0).abs() < 1e-3,
    "anchor should be near the visible block-start edge at scroll_y=0; got {:?}",
    top_anchor.origin
  );

  let scrolled = ScrollState::with_viewport(Point::new(0.0, 200.0));
  let scrolled_snapshot = capture_scroll_anchors(&tree, &scrolled);
  let scrolled_anchor = scrolled_snapshot
    .viewport
    .expect("expected an anchor at scroll_y=200");
  assert_eq!(scrolled_anchor.box_id, 2);
  assert!(
    (scrolled_anchor.origin.y - 200.0).abs() < 1e-3,
    "anchor should be near the visible block-start edge at scroll_y=200; got {:?}",
    scrolled_anchor.origin
  );
}

#[test]
fn element_anchor_selection_depends_on_element_scroll_offset() {
  // Element scroll container with three stacked blocks (A, B, C) and a 100px scrollport.
  let mut scroller_style = ComputedStyle::default();
  scroller_style.overflow_y = Overflow::Scroll;
  let scroller_style = Arc::new(scroller_style);

  let child_style = Arc::new(ComputedStyle::default());
  let a = node_with_id(
    Rect::from_xywh(0.0, 0.0, 100.0, 150.0),
    2,
    child_style.clone(),
    vec![],
  );
  let b = node_with_id(
    Rect::from_xywh(0.0, 200.0, 100.0, 150.0),
    3,
    child_style.clone(),
    vec![],
  );
  let c = node_with_id(
    Rect::from_xywh(0.0, 400.0, 100.0, 150.0),
    4,
    child_style,
    vec![],
  );

  let scroller = node_with_id(
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    1,
    scroller_style,
    vec![a, b, c],
  );
  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 0.0, 0.0), vec![scroller]);
  let tree = FragmentTree::with_viewport(root, Size::new(100.0, 100.0));

  let top = ScrollState::from_parts(Point::ZERO, HashMap::from([(1usize, Point::new(0.0, 0.0))]));
  let top_snapshot = capture_scroll_anchors(&tree, &top);
  let top_anchor = top_snapshot
    .elements
    .get(&1)
    .copied()
    .expect("expected an element anchor at scroll_y=0");
  assert_eq!(top_anchor.box_id, 2);
  assert!(
    (top_anchor.origin.y - 0.0).abs() < 1e-3,
    "element anchor should be near the visible block-start edge at scroll_y=0; got {:?}",
    top_anchor.origin
  );

  let scrolled = ScrollState::from_parts(
    Point::ZERO,
    HashMap::from([(1usize, Point::new(0.0, 200.0))]),
  );
  let scrolled_snapshot = capture_scroll_anchors(&tree, &scrolled);
  let scrolled_anchor = scrolled_snapshot
    .elements
    .get(&1)
    .copied()
    .expect("expected an element anchor at scroll_y=200");
  assert_eq!(scrolled_anchor.box_id, 3);
  assert!(
    (scrolled_anchor.origin.y - 200.0).abs() < 1e-3,
    "element anchor should be near the visible block-start edge at scroll_y=200; got {:?}",
    scrolled_anchor.origin
  );
}
