use std::sync::Arc;

use fastrender::interaction::scroll_wheel::{apply_wheel_scroll_at_point, ScrollWheelInput};
use fastrender::scroll::ScrollState;
use fastrender::style::types::Overflow;
use fastrender::style::ComputedStyle;
use fastrender::{FragmentContent, FragmentNode, FragmentTree, Point, Rect};

fn build_scroll_container_tree() -> FragmentTree {
  let mut style = ComputedStyle::default();
  style.overflow_y = Overflow::Scroll;
  let style = Arc::new(style);

  let mut fragment = FragmentNode::new_with_style(
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    FragmentContent::Block { box_id: Some(1) },
    vec![],
    style,
  );
  fragment.scroll_overflow = Rect::from_xywh(0.0, 0.0, 100.0, 300.0);

  FragmentTree::new(fragment)
}

#[test]
fn wheel_scroll_updates_element_offset() {
  let fragment_tree = build_scroll_container_tree();
  let scroll_state = ScrollState::default();

  let next = apply_wheel_scroll_at_point(
    &fragment_tree,
    &scroll_state,
    Point::new(50.0, 50.0),
    ScrollWheelInput {
      delta_x: 0.0,
      delta_y: 50.0,
    },
  );

  assert_eq!(next.element_offset(1), Point::new(0.0, 50.0));
}

#[test]
fn wheel_scroll_clamps_to_max() {
  let fragment_tree = build_scroll_container_tree();
  let scroll_state = ScrollState::default();

  let next = apply_wheel_scroll_at_point(
    &fragment_tree,
    &scroll_state,
    Point::new(50.0, 50.0),
    ScrollWheelInput {
      delta_x: 0.0,
      delta_y: 500.0,
    },
  );

  // max_scroll_y = 300 - 100 = 200
  assert_eq!(next.element_offset(1), Point::new(0.0, 200.0));
}

#[test]
fn wheel_scroll_point_outside_fragment_does_nothing() {
  let fragment_tree = build_scroll_container_tree();
  let scroll_state = ScrollState::default();

  let next = apply_wheel_scroll_at_point(
    &fragment_tree,
    &scroll_state,
    Point::new(150.0, 150.0),
    ScrollWheelInput {
      delta_x: 0.0,
      delta_y: 50.0,
    },
  );

  assert_eq!(next, scroll_state);
}
