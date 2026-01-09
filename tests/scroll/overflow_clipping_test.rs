use fastrender::geometry::{Rect, Size};
use fastrender::scroll::build_scroll_chain;
use fastrender::style::types::Overflow;
use fastrender::tree::fragment_tree::{FragmentNode, FragmentTree};
use fastrender::ComputedStyle;
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
