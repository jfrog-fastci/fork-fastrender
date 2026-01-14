use crate::geometry::{Rect, Size};
use crate::scroll::select_scroll_anchor;
use crate::style::position::Position;
use crate::tree::fragment_tree::{FragmentContent, FragmentNode};
use crate::ComputedStyle;
use std::sync::Arc;

fn block_with_id(bounds: Rect, box_id: usize, children: Vec<FragmentNode>, style: Arc<ComputedStyle>) -> FragmentNode {
  FragmentNode::new_with_style(
    bounds,
    FragmentContent::Block {
      box_id: Some(box_id),
    },
    children,
    style,
  )
}

#[test]
fn scroll_anchoring_examines_abspos_descendants_with_containing_block_when_dom_parent_is_clipped() {
  let scroller_style = Arc::new(ComputedStyle::default());
  let n_style = Arc::new(ComputedStyle::default());
  let p_style = Arc::new(ComputedStyle::default());

  let mut abs_style = ComputedStyle::default();
  abs_style.position = Position::Absolute;
  let abs_style = Arc::new(abs_style);

  // A is positioned relative to N (box_id=2) but remains in the subtree of P, which is fully
  // clipped. Step 2.2 should still examine A when examining N.
  let mut a = block_with_id(
    Rect::from_xywh(0.0, -200.0, 10.0, 10.0),
    4,
    vec![],
    abs_style,
  );
  a.abs_containing_block_box_id = Some(2);

  let p = block_with_id(
    Rect::from_xywh(0.0, 200.0, 10.0, 10.0),
    3,
    vec![a],
    p_style,
  );

  // N is partially visible within the 100x100 scrollport (y=80..130).
  let n = block_with_id(
    Rect::from_xywh(0.0, 80.0, 100.0, 50.0),
    2,
    vec![p],
    n_style,
  );

  let scroller = block_with_id(
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    1,
    vec![n],
    scroller_style,
  );

  assert_eq!(
    select_scroll_anchor(&scroller, Size::new(100.0, 100.0)),
    Some(4),
    "expected scroll anchoring to select the visible abspos descendant"
  );
}

#[test]
fn scroll_anchoring_does_not_examine_abspos_descendants_with_other_containing_blocks() {
  let scroller_style = Arc::new(ComputedStyle::default());
  let n_style = Arc::new(ComputedStyle::default());
  let p_style = Arc::new(ComputedStyle::default());

  let mut abs_style = ComputedStyle::default();
  abs_style.position = Position::Absolute;
  let abs_style = Arc::new(abs_style);

  let mut a = block_with_id(
    Rect::from_xywh(0.0, -200.0, 10.0, 10.0),
    4,
    vec![],
    abs_style,
  );
  // A's containing block is not N, so it should not be examined during N's step 2.2 handling.
  a.abs_containing_block_box_id = Some(1);

  let p = block_with_id(
    Rect::from_xywh(0.0, 200.0, 10.0, 10.0),
    3,
    vec![a],
    p_style,
  );

  let n = block_with_id(
    Rect::from_xywh(0.0, 80.0, 100.0, 50.0),
    2,
    vec![p],
    n_style,
  );

  let scroller = block_with_id(
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    1,
    vec![n],
    scroller_style,
  );

  assert_eq!(
    select_scroll_anchor(&scroller, Size::new(100.0, 100.0)),
    Some(2),
    "expected scroll anchoring to ignore abspos descendants whose containing block is not the examined candidate"
  );
}

