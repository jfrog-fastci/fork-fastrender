#![cfg(test)]

use crate::css::types::Transform;
use crate::geometry::{Point, Rect, Size};
use crate::scroll::{apply_scroll_snap, build_scroll_chain, ScrollState};
use crate::style::types::{BorderStyle, Overflow, ScrollSnapAxis};
use crate::tree::fragment_tree::{FragmentNode, FragmentTree};
use crate::{ComputedStyle, Length};
use std::sync::Arc;

#[test]
fn scroll_overflow_ignores_grandchild_clipped_by_overflow_hidden() {
  let mut scroller_style = ComputedStyle::default();
  scroller_style.overflow_y = Overflow::Scroll;
  let scroller_style = Arc::new(scroller_style);

  let mut child_style = ComputedStyle::default();
  child_style.overflow_x = Overflow::Hidden;
  child_style.overflow_y = Overflow::Hidden;
  let child_style = Arc::new(child_style);

  let grandchild = FragmentNode::new_block(Rect::from_xywh(0.0, 200.0, 100.0, 100.0), vec![]);
  let child = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    vec![grandchild],
    child_style,
  );
  let scroller = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    vec![child],
    scroller_style,
  );

  let mut tree = FragmentTree::with_viewport(scroller, Size::new(100.0, 100.0));
  tree.ensure_scroll_metadata();

  // The grandchild overflows the intermediate `overflow: hidden` child, so it must not inflate the
  // parent's scrollable overflow.
  assert!(
    (tree.root.scroll_overflow.max_y() - 100.0).abs() < 1e-3,
    "clipped descendant should not inflate scroll_overflow: {:#?}",
    tree.root.scroll_overflow
  );

  let chain = build_scroll_chain(&tree.root, tree.viewport_size(), &[]);
  assert_eq!(chain.len(), 1);
  assert!(
    chain[0].bounds.max_y.abs() < 1e-3,
    "scroll bounds should not include clipped overflow: {:#?}",
    chain[0].bounds
  );
}

#[test]
fn scroll_overflow_respects_axis_specific_overflow_clipping() {
  let mut scroller_style = ComputedStyle::default();
  scroller_style.overflow_x = Overflow::Scroll;
  scroller_style.overflow_y = Overflow::Scroll;
  let scroller_style = Arc::new(scroller_style);

  let mut child_style = ComputedStyle::default();
  child_style.overflow_x = Overflow::Hidden;
  child_style.overflow_y = Overflow::Visible;
  let child_style = Arc::new(child_style);

  // Overflows the child only in X (and should be clipped).
  let grandchild_x = FragmentNode::new_block(Rect::from_xywh(200.0, 0.0, 100.0, 100.0), vec![]);
  // Overflows the child only in Y and should remain visible (since overflow-y is visible).
  let grandchild_y = FragmentNode::new_block(Rect::from_xywh(0.0, 200.0, 100.0, 100.0), vec![]);

  let child = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    vec![grandchild_x, grandchild_y],
    child_style,
  );

  let scroller = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    vec![child],
    scroller_style,
  );

  let mut tree = FragmentTree::with_viewport(scroller, Size::new(100.0, 100.0));
  tree.ensure_scroll_metadata();

  assert!(
    (tree.root.scroll_overflow.max_x() - 100.0).abs() < 1e-3,
    "x overflow should be clipped by overflow-x: hidden: {:#?}",
    tree.root.scroll_overflow
  );
  assert!(
    (tree.root.scroll_overflow.max_y() - 300.0).abs() < 1e-3,
    "y overflow should remain visible when overflow-y is visible: {:#?}",
    tree.root.scroll_overflow
  );

  let chain = build_scroll_chain(&tree.root, tree.viewport_size(), &[]);
  assert_eq!(chain.len(), 1);
  assert!(
    chain[0].bounds.max_x.abs() < 1e-3,
    "x scroll range should not include clipped overflow: {:#?}",
    chain[0].bounds
  );
  assert!(
    (chain[0].bounds.max_y - 200.0).abs() < 1e-3,
    "y scroll range should still include visible overflow: {:#?}",
    chain[0].bounds
  );
}

#[test]
fn scroll_snap_bounds_ignore_clipped_descendant_overflow() {
  let mut scroller_style = ComputedStyle::default();
  scroller_style.overflow_y = Overflow::Scroll;
  scroller_style.scroll_snap_type.axis = ScrollSnapAxis::Y;
  let scroller_style = Arc::new(scroller_style);

  let mut child_style = ComputedStyle::default();
  child_style.overflow_x = Overflow::Hidden;
  child_style.overflow_y = Overflow::Hidden;
  let child_style = Arc::new(child_style);

  let grandchild = FragmentNode::new_block(Rect::from_xywh(0.0, 200.0, 100.0, 100.0), vec![]);
  let child = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    vec![grandchild],
    child_style,
  );
  let scroller = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    vec![child],
    scroller_style,
  );

  let mut tree = FragmentTree::with_viewport(scroller, Size::new(100.0, 100.0));
  // Scroll snap with no targets falls back to clamping within the scroll bounds. Overflow hidden
  // descendants must not inflate those bounds.
  let state = ScrollState::with_viewport(Point::new(0.0, 500.0));
  let snapped = apply_scroll_snap(&mut tree, &state);
  assert!(
    snapped.state.viewport.y.abs() < 1e-3,
    "expected scroll snap to clamp to 0 when overflow is clipped; got {:#?}",
    snapped.state.viewport
  );
}

#[test]
fn scroll_overflow_clips_to_padding_box_not_border_box() {
  let mut root_style = ComputedStyle::default();
  root_style.overflow_x = Overflow::Scroll;
  root_style.overflow_y = Overflow::Scroll;
  let root_style = Arc::new(root_style);

  let mut child_style = ComputedStyle::default();
  child_style.overflow_x = Overflow::Hidden;
  child_style.overflow_y = Overflow::Hidden;
  child_style.border_left_style = BorderStyle::Solid;
  child_style.border_right_style = BorderStyle::Solid;
  child_style.border_top_style = BorderStyle::Solid;
  child_style.border_bottom_style = BorderStyle::Solid;
  child_style.border_left_width = Length::px(10.0);
  child_style.border_right_width = Length::px(10.0);
  child_style.border_top_width = Length::px(10.0);
  child_style.border_bottom_width = Length::px(10.0);
  let child_style = Arc::new(child_style);

  // The grandchild starts at (0,0) which places it in the border area (outside the padding box).
  // When propagating overflow into the root, it should be clipped to the child's padding box,
  // i.e. max_x/max_y should be 90 rather than 100.
  let grandchild = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 100.0), vec![]);
  let child = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    vec![grandchild],
    child_style,
  );

  let root =
    FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 0.0, 0.0), vec![child], root_style);
  let mut tree = FragmentTree::with_viewport(root, Size::new(80.0, 80.0));
  tree.ensure_scroll_metadata();

  assert!(
    (tree.root.scroll_overflow.max_x() - 90.0).abs() < 1e-3,
    "expected scroll_overflow to be clipped to the padding edge (border excluded); got {:#?}",
    tree.root.scroll_overflow
  );
  assert!(
    (tree.root.scroll_overflow.max_y() - 90.0).abs() < 1e-3,
    "expected scroll_overflow to be clipped to the padding edge (border excluded); got {:#?}",
    tree.root.scroll_overflow
  );

  let chain = build_scroll_chain(&tree.root, tree.viewport_size(), &[]);
  assert_eq!(chain.len(), 1);
  assert!(
    (chain[0].bounds.max_x - 10.0).abs() < 1e-3,
    "scroll bounds should be derived from the clipped scrollport; got {:#?}",
    chain[0].bounds
  );
  assert!(
    (chain[0].bounds.max_y - 10.0).abs() < 1e-3,
    "scroll bounds should be derived from the clipped scrollport; got {:#?}",
    chain[0].bounds
  );
}

#[test]
fn scroll_overflow_accounts_for_child_transforms() {
  let mut root_style = ComputedStyle::default();
  root_style.overflow_x = Overflow::Scroll;
  root_style.overflow_y = Overflow::Scroll;
  let root_style = Arc::new(root_style);

  let mut child_style = ComputedStyle::default();
  // Place the child partially outside the scrollport, then translate it left by 50% of its own
  // size. The transformed box should not inflate the parent's scrollable overflow.
  child_style.transform.push(Transform::Translate(
    Length::percent(-50.0),
    Length::percent(0.0),
  ));
  let child_style = Arc::new(child_style);

  let child =
    FragmentNode::new_block_styled(Rect::from_xywh(80.0, 0.0, 40.0, 20.0), vec![], child_style);
  let root = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    vec![child],
    root_style,
  );
  let mut tree = FragmentTree::with_viewport(root, Size::new(100.0, 100.0));
  tree.ensure_scroll_metadata();

  assert!(
    (tree.root.scroll_overflow.max_x() - 100.0).abs() < 1e-3,
    "expected transformed child to fit within the scrollport; got {:#?}",
    tree.root.scroll_overflow
  );

  let chain = build_scroll_chain(&tree.root, tree.viewport_size(), &[]);
  assert_eq!(chain.len(), 1);
  assert!(
    chain[0].bounds.max_x.abs() < 1e-3,
    "expected no horizontal scroll range; got {:#?}",
    chain[0].bounds
  );
}

#[test]
fn scroll_overflow_respects_scrollbar_reservation_when_clipping() {
  let mut root_style = ComputedStyle::default();
  root_style.overflow_x = Overflow::Scroll;
  root_style.overflow_y = Overflow::Scroll;
  let root_style = Arc::new(root_style);

  let mut child_style = ComputedStyle::default();
  child_style.overflow_x = Overflow::Hidden;
  child_style.overflow_y = Overflow::Hidden;
  let child_style = Arc::new(child_style);

  // The grandchild overflows into the 10px reserved gutter on the right. That gutter is not part of
  // the scrollport and must not inflate ancestor overflow.
  let grandchild = FragmentNode::new_block(Rect::from_xywh(80.0, 0.0, 20.0, 100.0), vec![]);
  let mut child = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    vec![grandchild],
    child_style,
  );
  child.scrollbar_reservation.right = 10.0;

  let root =
    FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 0.0, 0.0), vec![child], root_style);
  let mut tree = FragmentTree::with_viewport(root, Size::new(80.0, 80.0));
  tree.ensure_scroll_metadata();

  assert!(
    (tree.root.scroll_overflow.max_x() - 90.0).abs() < 1e-3,
    "expected scroll_overflow to clip to the scrollport excluding reserved scrollbar gutters; got {:#?}",
    tree.root.scroll_overflow
  );

  let chain = build_scroll_chain(&tree.root, tree.viewport_size(), &[]);
  assert_eq!(chain.len(), 1);
  assert!(
    (chain[0].bounds.max_x - 10.0).abs() < 1e-3,
    "scroll bounds should not include overflow into reserved scrollbar gutters; got {:#?}",
    chain[0].bounds
  );
}

#[test]
fn element_scroll_bounds_use_padding_box_when_border_is_present() {
  let mut scroller_style = ComputedStyle::default();
  scroller_style.overflow_x = Overflow::Scroll;
  scroller_style.overflow_y = Overflow::Scroll;
  scroller_style.border_left_style = BorderStyle::Solid;
  scroller_style.border_right_style = BorderStyle::Solid;
  scroller_style.border_top_style = BorderStyle::Solid;
  scroller_style.border_bottom_style = BorderStyle::Solid;
  scroller_style.border_left_width = Length::px(10.0);
  scroller_style.border_right_width = Length::px(10.0);
  scroller_style.border_top_width = Length::px(10.0);
  scroller_style.border_bottom_width = Length::px(10.0);
  let scroller_style = Arc::new(scroller_style);

  // Child content starts at the padding edge (inside the 10px border) and overflows 20px beyond
  // the padding box. The scrollport width is 80px (100 - 10 - 10), so the horizontal scroll range
  // should be 20px, not 10px.
  let child = FragmentNode::new_block(Rect::from_xywh(10.0, 10.0, 100.0, 50.0), vec![]);
  let scroller = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    vec![child],
    scroller_style,
  );

  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 0.0, 0.0), vec![scroller]);
  let mut tree = FragmentTree::with_viewport(root, Size::new(200.0, 200.0));
  tree.ensure_scroll_metadata();

  let chain = build_scroll_chain(&tree.root, tree.viewport_size(), &[0]);
  assert!(
    chain.len() >= 1,
    "expected scroll chain to include the element scroller"
  );
  assert!(
    (chain[0].bounds.max_x - 20.0).abs() < 1e-3,
    "element scroll bounds should use the padding box as the scrollport; got {:#?}",
    chain[0].bounds
  );
}

#[test]
fn viewport_scroll_bounds_respect_left_scrollbar_reservation() {
  // Simulate `scrollbar-gutter: stable both-edges` by reserving space on both inline edges. The
  // effective scrollport should be shifted right by the left gutter; that shift must not inflate
  // the scroll range.
  let mut root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 0.0, 0.0), vec![]);
  root.scrollbar_reservation.left = 10.0;
  root.scrollbar_reservation.right = 10.0;

  // Content exactly fills the effective scrollport (100 - 10 - 10 = 80px) and is positioned at the
  // scrollport start edge (x=10).
  let child = FragmentNode::new_block(Rect::from_xywh(10.0, 0.0, 80.0, 10.0), vec![]);
  root.children = vec![child].into();

  let mut tree = FragmentTree::with_viewport(root, Size::new(100.0, 100.0));
  tree.ensure_scroll_metadata();

  let chain = build_scroll_chain(&tree.root, tree.viewport_size(), &[]);
  assert_eq!(chain.len(), 1);
  assert!(
    chain[0].bounds.max_x.abs() < 1e-3,
    "viewport scroll bounds should not be inflated by left gutter reservation; got {:#?}",
    chain[0].bounds
  );
}

#[test]
fn viewport_scroll_bounds_ignore_negative_descendant_overflow() {
  // Wikipedia's search button uses `text-indent:-9999px` to visually hide the label. That creates a
  // descendant fragment far to the left of the viewport, but browsers do not expand the document's
  // horizontal scroll range for negative overflow.
  let hidden_label = FragmentNode::new_block(Rect::from_xywh(-9999.0, 0.0, 10.0, 10.0), vec![]);
  let icon = FragmentNode::new_block(Rect::from_xywh(250.0, 0.0, 10.0, 10.0), vec![hidden_label]);
  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 0.0, 0.0), vec![icon]);

  let mut tree = FragmentTree::with_viewport(root, Size::new(800.0, 600.0));
  tree.ensure_scroll_metadata();

  assert!(
    tree.root.scroll_overflow.min_x() < 0.0,
    "test requires negative scrollable overflow; got {:#?}",
    tree.root.scroll_overflow
  );

  let chain = build_scroll_chain(&tree.root, tree.viewport_size(), &[]);
  assert_eq!(chain.len(), 1);
  assert!(
    chain[0].bounds.min_x.abs() < 1e-3 && chain[0].bounds.max_x.abs() < 1e-3,
    "viewport scroll bounds should ignore negative overflow; got {:#?}",
    chain[0].bounds
  );
}

#[test]
fn element_scroll_bounds_ignore_negative_overflow_origin() {
  // The scrollable range should be determined by the bottom/right-most edge of the content. Any
  // content overflowing to the left/top should be clipped but not reachable via negative scroll
  // offsets.
  let mut scroller_style = ComputedStyle::default();
  scroller_style.overflow_x = Overflow::Scroll;
  let scroller_style = Arc::new(scroller_style);

  // Content extends 50px past the right edge ([-50,150] inside a 100px viewport).
  let child = FragmentNode::new_block(Rect::from_xywh(-50.0, 0.0, 200.0, 10.0), vec![]);
  let scroller = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 100.0, 10.0),
    vec![child],
    scroller_style,
  );
  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 0.0, 0.0), vec![scroller]);

  let mut tree = FragmentTree::with_viewport(root, Size::new(100.0, 10.0));
  tree.ensure_scroll_metadata();

  let overflow = tree.root.children_ref()[0].scroll_overflow;
  assert!(
    overflow.min_x() < 0.0,
    "test requires negative scrollable overflow; got {:#?}",
    overflow
  );

  let chain = build_scroll_chain(&tree.root, tree.viewport_size(), &[0]);
  assert!(
    chain.len() >= 1,
    "expected scroll chain to include the element scroller"
  );
  assert!(
    chain[0].bounds.min_x.abs() < 1e-3 && (chain[0].bounds.max_x - 50.0).abs() < 1e-3,
    "element scroll bounds should ignore negative overflow while preserving positive range; got {:#?}",
    chain[0].bounds
  );
}
