use crate::geometry::{Point, Rect};
use crate::style::types::Overflow;
use crate::tree::fragment_tree::{FragmentContent, FragmentNode};
use crate::{ComputedStyle, Length};
use std::sync::Arc;

use super::super::{scrollport_rect_for_fragment, select_scroll_anchoring_anchor_box_id};

fn rect_contains(outer: Rect, inner: Rect) -> bool {
  inner.min_x() >= outer.min_x()
    && inner.max_x() <= outer.max_x()
    && inner.min_y() >= outer.min_y()
    && inner.max_y() <= outer.max_y()
}

fn build_scroller(scroll_padding_top: f32) -> FragmentNode {
  let mut scroller_style = ComputedStyle::default();
  scroller_style.overflow_y = Overflow::Scroll;
  scroller_style.scroll_padding_top = Length::px(scroll_padding_top);
  let scroller_style = Arc::new(scroller_style);

  // Element that is fully visible in the raw scrollport, but can become fully clipped relative to
  // the scroll-padding-inset optimal viewing region.
  let in_padding = FragmentNode::new(
    Rect::from_xywh(0.0, 0.0, 100.0, 10.0),
    FragmentContent::Block { box_id: Some(1) },
    vec![],
  );

  // Element that sits below the top scroll-padding inset, so it remains fully visible within the
  // optimal viewing region.
  let in_optimal = FragmentNode::new(
    Rect::from_xywh(0.0, 30.0, 100.0, 10.0),
    FragmentContent::Block { box_id: Some(2) },
    vec![],
  );

  FragmentNode::new_with_style(
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    FragmentContent::Block { box_id: Some(99) },
    vec![in_padding, in_optimal],
    scroller_style,
  )
}

#[test]
fn scroll_anchoring_anchor_selection_uses_scroll_padding_optimal_viewing_region() {
  let no_padding = build_scroller(0.0);
  assert_eq!(
    select_scroll_anchoring_anchor_box_id(&no_padding, Point::ZERO),
    Some(1),
    "without scroll-padding, the top element is fully visible and should be selected"
  );

  let scroller = build_scroller(20.0);
  let style = scroller.style.as_deref().expect("scroller style");
  let scrollport = scrollport_rect_for_fragment(&scroller, style);
  let optimal_viewing_region = Rect::from_xywh(
    scrollport.x(),
    scrollport.y() + 20.0,
    scrollport.width(),
    (scrollport.height() - 20.0).max(0.0),
  );

  let in_padding = &scroller.children[0];
  let in_padding_rect = in_padding
    .scroll_overflow
    .translate(in_padding.bounds.origin);
  assert!(
    rect_contains(scrollport, in_padding_rect),
    "test setup: element should be fully visible within the raw scrollport"
  );
  assert!(
    !rect_contains(optimal_viewing_region, in_padding_rect),
    "test setup: element should NOT be fully visible within the scroll-padding-inset optimal viewing region"
  );

  let in_optimal = &scroller.children[1];
  let in_optimal_rect = in_optimal
    .scroll_overflow
    .translate(in_optimal.bounds.origin);
  assert!(
    rect_contains(optimal_viewing_region, in_optimal_rect),
    "test setup: second element should be fully visible within the optimal viewing region"
  );

  assert_eq!(
    select_scroll_anchoring_anchor_box_id(&scroller, Point::ZERO),
    Some(2),
    "anchor selection must treat the scroll-padding insets as clipped when evaluating visibility"
  );
}
